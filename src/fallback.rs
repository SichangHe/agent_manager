//! Codex capacity fallback: retry a few times, then replace the agent with a Claude one; start new agents on Claude until Codex probes healthy.
//!
//! Design: `docs/codex-claude-fallback.md`.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

use crate::store::Store;
use crate::tasks::Task;
use crate::work::Problem;
use crate::{agents, omnigent, settings, taskfile};

type Settings = BTreeMap<String, String>;

/// Failures older than this no longer count.
const WINDOW_S: i64 = 600;
/// Failures of one agent within the window that allow its swap, and of all agents that trigger a probe.
const N_FAILURES: usize = 3;
/// Seconds between probes while Codex is unhealthy.
const PROBE_S: i64 = 900;
/// Probe successes in a row that make Codex healthy again.
const N_PROBES_OK: u32 = 2;
/// Seconds before a probe may be repeated because failures piled up.
const RECHECK_S: i64 = 300;
const HOUR_S: i64 = 3600;
/// Swaps per hour, over all agents.
const MAX_SWAPS_H: usize = 3;
/// Attempts per failed swap, `RETRY_SWAP_S` apart.
const MAX_TRIES: u32 = 3;
const RETRY_SWAP_S: i64 = 900;
/// Seconds a probe may take.
const PROBE_TIMEOUT_S: u64 = 20;

// 🧑 "I think we just need to retry aggressively on 429 if they actually succeed sometimes we need to strike a balance between aggressive retries and if it’s taking too many tries, and the progress is way too slow you should switch to Claude"
/// Whether a session error is Codex giving up on 429s, the model being at capacity, or a dropped stream.
// Real `last_task_error` values of codex-native sessions in Omnigent: `{code: rate_limit_exceeded, message: "exceeded retry limit, last status: 429 Too Many Requests"}` and `{code: codex_turn_error, message: "Selected model is at capacity. Please try a different model."}`.
pub fn capacity_429(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("last status: 429") || lower.contains("at capacity") || stream_drop(error)
}

/// Whether a session error is Codex's stream ending early, which Codex retries itself first.
fn stream_drop(error: &str) -> bool {
    error
        .to_lowercase()
        .contains("stream closed before response.completed")
}

/// A Codex agent's failure: `(agent address, time in s, whether a stream drop rather than a 429)`.
type Failure = (String, i64, bool);

/// Codex health, the failures behind it, and the swap brakes, kept across watcher restarts in `amh-codex-health.json`.
#[derive(Default, PartialEq, Clone)]
pub struct Health {
    failures: Vec<Failure>,
    unhealthy: bool,
    /// The latest probe: `Some(false)` is a failure; `None` is unknown.
    last_ok: Option<bool>,
    probes_ok: u32,
    probed_s: i64,
    /// Agents whose swap was refused and whose manager was told.
    refused: Vec<String>,
    /// Times of swaps in the last hour.
    swaps_s: Vec<i64>,
    cap_told_s: i64,
    config_told: bool,
    /// Failed swaps: `(address, failure time in s, attempts, last attempt in s)`.
    swap_tries: Vec<(String, i64, u32, i64)>,
}

impl Health {
    fn from_json(value: &Value) -> Self {
        let text = |item: &Value| item.as_str().map(str::to_owned);
        let list = |key: &str| value[key].as_array().cloned().unwrap_or_default();
        let int = |key: &str| value[key].as_i64().unwrap_or_default();
        Self {
            failures: list("failures")
                .iter()
                .filter_map(|f| Some((text(&f[0])?, f[1].as_i64()?, f[2] == true)))
                .collect(),
            unhealthy: value["unhealthy"] == true,
            last_ok: value["last_ok"].as_bool(),
            probes_ok: u32::try_from(int("probes_ok")).unwrap_or_default(),
            probed_s: int("probed_s"),
            refused: list("refused").iter().filter_map(text).collect(),
            swaps_s: list("swaps_s").iter().filter_map(Value::as_i64).collect(),
            cap_told_s: int("cap_told_s"),
            config_told: value["config_told"] == true,
            swap_tries: list("swap_tries")
                .iter()
                .filter_map(|t| {
                    Some((
                        text(&t[0])?,
                        t[1].as_i64()?,
                        u32::try_from(t[2].as_i64()?).ok()?,
                        t[3].as_i64()?,
                    ))
                })
                .collect(),
        }
    }

    fn to_json(&self) -> Value {
        json!({"failures": self.failures, "unhealthy": self.unhealthy, "last_ok": self.last_ok, "probes_ok": self.probes_ok, "probed_s": self.probed_s, "refused": self.refused, "swaps_s": self.swaps_s, "cap_told_s": self.cap_told_s, "config_told": self.config_told, "swap_tries": self.swap_tries})
    }

    /// Count a failure of `address` at `at_s`; true when it is new.
    pub fn record(&mut self, address: &str, at_s: i64, stream: bool, now_s: i64) -> bool {
        self.failures.retain(|(_, at, _)| now_s - at < WINDOW_S);
        self.swaps_s.retain(|at| now_s - at < HOUR_S);
        self.swap_tries.retain(|t| now_s - t.1 < HOUR_S);
        if at_s <= now_s - WINDOW_S
            || self
                .failures
                .iter()
                .any(|(who, at, _)| who == address && *at == at_s)
        {
            return false;
        }
        self.failures.push((address.to_owned(), at_s, stream));
        if self.unhealthy {
            self.probes_ok = 0;
        }
        true
    }

    /// The agent's failures in the window: 429s and stream drops.
    pub fn n_failures(&self, address: &str) -> (usize, usize) {
        let own = || self.failures.iter().filter(|f| f.0 == address);
        (
            own().filter(|f| !f.2).count(),
            own().filter(|f| f.2).count(),
        )
    }

    pub fn unhealthy(&self) -> bool {
        self.unhealthy
    }

    fn probe_due(&self, now_s: i64) -> bool {
        self.unhealthy && now_s >= self.probed_s + PROBE_S
    }

    /// Whether failures piled up and the last probe is too old to say whether Codex is down.
    fn probe_wanted(&self, now_s: i64) -> bool {
        self.failures.len() >= N_FAILURES
            && (self.last_ok != Some(false) || !self.unhealthy)
            && now_s >= self.probed_s + RECHECK_S
    }

    /// Take a probe result. A failure marks Codex unhealthy; `N_PROBES_OK` successes in a row make it healthy. Failures and refusals stay.
    fn probed(&mut self, ok: bool, now_s: i64) {
        self.probed_s = now_s;
        self.last_ok = Some(ok);
        self.config_told = false;
        if !ok {
            (self.unhealthy, self.probes_ok) = (true, 0);
            return;
        }
        self.probes_ok += 1;
        if self.probes_ok >= N_PROBES_OK || !self.unhealthy {
            (self.unhealthy, self.probes_ok, self.last_ok) = (false, 0, None);
        }
    }

    /// Whether to swap `address` (failed at `at_s`) now; pure.
    fn swap(&self, address: &str, at_s: i64, now_s: i64) -> Swap {
        let (n_429, n_stream) = self.n_failures(address);
        if n_429 + n_stream < N_FAILURES {
            return Swap::Wait;
        }
        if self.last_ok != Some(false) {
            return Swap::CodexAlive;
        }
        if self
            .swaps_s
            .iter()
            .filter(|at| now_s - **at < HOUR_S)
            .count()
            >= MAX_SWAPS_H
        {
            return Swap::Capped;
        }
        match self
            .swap_tries
            .iter()
            .find(|t| t.0 == address && t.1 == at_s)
        {
            Some((_, _, n, last)) if *n >= MAX_TRIES || now_s < last + RETRY_SWAP_S => {
                Swap::Backoff
            }
            _ => Swap::Go,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Swap {
    Wait,
    CodexAlive,
    Capped,
    Backoff,
    Go,
}

// 🧑 (via the manager) "Human authorized Claude replacements. Preserve experiment-specific model/account requirements; agent-runtime fallback must not silently substitute experiment models."
/// Why this agent must not be swapped to Claude, if so. Pure.
///
/// An experiment's model is not in the task file, but a task started with `--model` or `--effort` carries `model_pinned`, and a session running a model other than the current default was chosen by hand. A task started before `model_pinned` with the default model looks unpinned and is swapped.
pub fn refusal(task: &Task, session: &Value, default_model: &str) -> Option<String> {
    if task.field("is_manager") != Some("false") {
        return Some("it is a manager or its role is unrecorded".to_owned());
    }
    if task.field("model_pinned").is_some() {
        return Some("its task was started with an explicit model or effort".to_owned());
    }
    match session["model_override"].as_str() {
        Some(model) if model == default_model => None,
        other => Some(format!(
            "it runs {}, not the default {default_model}",
            other.unwrap_or("an unrecorded model")
        )),
    }
}

fn enabled(settings: &Settings) -> bool {
    settings::get(settings, "fallback.codex_to_claude") == "true"
}

fn path() -> std::path::PathBuf {
    settings::state_dir().join("amh-codex-health.json")
}

/// The saved health; no file is healthy. A corrupt file is moved aside and counts as unhealthy with no known probe, so nothing is swapped on it, and its error is returned.
fn load() -> (Health, Option<String>) {
    match std::fs::read_to_string(path()) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(value) => (Health::from_json(&value), None),
            Err(error) => {
                let _ = std::fs::rename(path(), path().with_extension("corrupt"));
                let unknown = Health {
                    unhealthy: true,
                    ..Health::default()
                };
                (
                    unknown,
                    Some(format!("corrupt codex health record: {error}")),
                )
            }
        },
        Err(_) => (Health::default(), None),
    }
}

// 🧑 "later somehow decide when to switch back"
/// The tool a new worker starts with: Claude while the fallback is on and Codex is unhealthy, else `tool`.
pub fn default_tool(settings: &Settings, tool: &'static str) -> &'static str {
    if tool == "codex" && enabled(settings) && load().0.unhealthy() {
        "claude"
    } else {
        tool
    }
}

#[derive(Debug, PartialEq)]
enum Probe {
    Alive,
    Down,
    /// The probe cannot run: missing key file or unreachable proxy; Codex may be fine.
    Config(String),
}

/// One tiny request to Codex through the proxy. The proxy's client key is read from `fallback.proxy_key_file` per probe and never kept or printed.
fn probe(settings: &Settings) -> Probe {
    let file = settings::get(settings, "fallback.proxy_key_file");
    let key = match std::fs::read_to_string(file) {
        Ok(key) => key,
        Err(error) => {
            return Probe::Config(format!(
                "cannot read the proxy key file `fallback.proxy_key_file`: {error}"
            ));
        }
    };
    let url = Some(settings::get(settings, "fallback.proxy_url"))
        .filter(|url| !url.is_empty())
        .unwrap_or("http://127.0.0.1:18318");
    let model = match omnigent::launch_default(settings, "codex", false) {
        Ok((model, _)) => model,
        Err(error) => return Probe::Config(error),
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(PROBE_TIMEOUT_S)))
        .build()
        .into();
    match agent
        .post(&format!("{url}/v1/responses"))
        .header("Authorization", &format!("Bearer {}", key.trim()))
        .send_json(json!({"model": model, "input": "ok", "max_output_tokens": 16, "stream": false}))
    {
        Ok(answer) if answer.status().as_u16() == 200 => Probe::Alive,
        Ok(_) | Err(ureq::Error::Timeout(_)) => Probe::Down,
        Err(error) => Probe::Config(format!(
            "the Codex probe could not reach the proxy at {url}: {error}"
        )),
    }
}

/// Act on the watcher's findings: ask a Codex agent that failed to retry; swap it to Claude once it failed 3 times, the latest probe failed, and the hourly cap allows; probe Codex while unhealthy. Returns log lines.
pub fn watch(
    settings: &Settings,
    store: &Store,
    found: &[(Task, Problem, String)],
    now_s: i64,
) -> Vec<String> {
    let mut logs = Vec::new();
    if !enabled(settings) {
        return logs;
    }
    let (mut health, corrupt) = load();
    logs.extend(corrupt);
    let before = health.clone();
    let (claude_model, claude_effort) = match omnigent::launch_default(settings, "claude", false) {
        Ok(chosen) => chosen,
        Err(error) => return vec![format!("codex fallback: {error}")],
    };
    let default_model = omnigent::launch_default(settings, "codex", false).map(|(model, _)| model);
    let tell = |logs: &mut Vec<String>, manager: &str, name: &str, text: &str| {
        let told = agents::report(
            settings,
            store,
            manager,
            &crate::work::envelope(name, text),
            false,
        );
        logs.push(format!("{text} (manager told: {})", told.is_ok()));
    };
    // `(task, session, failure time in s, new)` of agents that reached the failures a swap needs.
    let mut due = Vec::new();
    for (task, problem, _) in found {
        let address = task.address();
        if *problem != Problem::Error || task.field("tool") != Some("codex") {
            continue;
        }
        let Ok(id) = omnigent::session(address) else {
            continue;
        };
        let Ok(session) = omnigent::api(
            settings,
            "GET",
            &format!("/v1/sessions/{id}?include_items=false"),
            None,
        ) else {
            continue;
        };
        let error = omnigent::session_error(&session);
        if session["harness"] != "codex-native" || !capacity_429(error) {
            continue;
        }
        let Some(at_s) = omnigent::updated_at(&session).map(|at| at.timestamp()) else {
            logs.push(format!("{}: unreadable updated_at; skipped", task.name));
            continue;
        };
        let fresh = health.record(address, at_s, stream_drop(error), now_s);
        let (n_429, n_stream) = health.n_failures(address);
        if n_429 + n_stream < N_FAILURES {
            if fresh {
                let text =
                    "Your last turn failed on a provider error (429 or dropped stream). Retry it.";
                logs.push(match omnigent::send(settings, address, text) {
                    Ok(()) => format!("{}: failure {n_429} 429s, {n_stream} stream drops; asked it to retry", task.name),
                    Err(error) => format!("{}: failure {n_429} 429s, {n_stream} stream drops; retry message failed: {error}", task.name),
                });
            }
        } else {
            due.push((task, session, at_s, fresh));
        }
    }
    if health.probe_due(now_s) || health.probe_wanted(now_s) {
        match probe(settings) {
            Probe::Config(error) => {
                health.probed_s = now_s;
                logs.push(format!("codex probe cannot run: {error}"));
                if !health.config_told {
                    health.config_told = true;
                    let manager = settings::get(settings, "main_manager");
                    let text = format!(
                        "amh's Codex probe cannot run, so Codex is not judged unhealthy and no agent is swapped to Claude: {error}"
                    );
                    tell(&mut logs, manager, "amh", &text);
                }
            }
            result => {
                let ok = result == Probe::Alive;
                health.probed(ok, now_s);
                logs.push(format!(
                    "codex probe {}; {}",
                    if ok { "succeeded" } else { "failed" },
                    if health.unhealthy() {
                        "unhealthy"
                    } else {
                        "healthy"
                    }
                ));
            }
        }
    }
    for (task, session, at_s, fresh) in due {
        let address = task.address();
        let (n_429, n_stream) = health.n_failures(address);
        let counts = format!(
            "{n_429} 429s and {n_stream} stream drops in {} min",
            WINDOW_S / 60
        );
        let manager = task.field("managerat").unwrap_or_default();
        match health.swap(address, at_s, now_s) {
            Swap::Wait | Swap::Backoff => {}
            Swap::CodexAlive => {
                if fresh {
                    let text = "Your last turn failed on a provider error, but Codex answers a probe. Retry it.";
                    logs.push(match omnigent::send(settings, address, text) {
                        Ok(()) => format!(
                            "{}: {counts}, Codex answers probes; asked it to retry",
                            task.name
                        ),
                        Err(error) => {
                            format!("{}: {counts}; retry message failed: {error}", task.name)
                        }
                    });
                }
            }
            Swap::Capped => {
                if now_s >= health.cap_told_s + HOUR_S {
                    health.cap_told_s = now_s;
                    let text = format!(
                        "{}: amh did not swap its Codex agent to Claude ({counts}): {MAX_SWAPS_H} swaps already happened in the last hour. It is left as is.",
                        task.name
                    );
                    tell(&mut logs, manager, address, &text);
                }
            }
            Swap::Go => {
                let why = match &default_model {
                    Ok(model) => refusal(task, &session, model),
                    Err(error) => Some(error.clone()),
                };
                if let Some(why) = why {
                    if !health.refused.iter().any(|known| known == address) {
                        health.refused.push(address.to_owned());
                        let text = format!(
                            "{}: amh did not swap its Codex agent to Claude ({counts}): {why}. It is left as is.",
                            task.name
                        );
                        tell(&mut logs, manager, address, &text);
                    }
                    continue;
                }
                let reason = format!("because Codex failed {counts} and a probe failed too");
                let message = format!(
                    "amh swapped this agent from Codex to Claude {reason}. Continue the task."
                );
                let to = Some(("claude", claude_model.as_str(), claude_effort.as_str()));
                match agents::replace(settings, store, &task.name, &message, to, &reason) {
                    Ok(new) => {
                        health.swaps_s.push(now_s);
                        logs.push(format!(
                            "{}: replaced its Codex agent with Claude {new} {reason}",
                            task.name
                        ));
                    }
                    Err(error) => {
                        let tries = health
                            .swap_tries
                            .iter()
                            .find(|t| t.0 == address && t.1 == at_s)
                            .map_or(0, |t| t.2);
                        health
                            .swap_tries
                            .retain(|t| !(t.0 == address && t.1 == at_s));
                        health
                            .swap_tries
                            .push((address.to_owned(), at_s, tries + 1, now_s));
                        logs.push(format!(
                            "{}: swap to Claude failed (attempt {}): {error}",
                            task.name,
                            tries + 1
                        ));
                    }
                }
            }
        }
    }
    if health.unhealthy() != before.unhealthy() {
        logs.push(format!(
            "codex marked {}",
            if health.unhealthy() {
                "unhealthy: new agents start on Claude"
            } else {
                "healthy"
            }
        ));
    }
    if health != before
        && let Err(error) = taskfile::write(&path(), &health.to_json().to_string())
    {
        logs.push(format!("cannot write the codex health record: {error}"));
    }
    logs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(fields: &[(&str, &str)]) -> Task {
        Task {
            name: "t.md".to_owned(),
            fields: fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            items: Vec::new(),
            body: String::new(),
        }
    }

    #[test]
    #[ignore = "sends one tiny request to the live Codex proxy"]
    fn probe_reaches_the_live_proxy() -> Result<(), String> {
        let mut settings = settings::load()?;
        settings.insert(
            "fallback.proxy_key_file".to_owned(),
            format!(
                "{}/.local/share/cliproxyapi-trial/client-key",
                std::env::var("HOME").unwrap_or_default()
            ),
        );
        assert_eq!(probe(&settings), Probe::Alive);
        settings.insert(
            "fallback.proxy_url".to_owned(),
            "http://127.0.0.1:9".to_owned(),
        );
        assert!(matches!(probe(&settings), Probe::Config(_)));
        Ok(())
    }

    #[test]
    fn live_session_errors_are_capacity_failures() {
        assert!(capacity_429(
            "exceeded retry limit, last status: 429 Too Many Requests"
        ));
        assert!(capacity_429(
            "Selected model is at capacity. Please try a different model."
        ));
        assert!(capacity_429(
            "stream disconnected before completion: stream closed before response.completed"
        ));
        assert!(!capacity_429("Session limit reached"));
        assert!(!capacity_429("Runner disconnected unexpectedly."));
    }

    fn down(h: &mut Health, now_s: i64) {
        h.probed(false, now_s);
    }

    #[test]
    fn three_failures_alone_do_not_swap_and_a_failed_probe_allows_it() {
        let mut h = Health::default();
        assert!(h.record("a", 100, false, 100));
        assert!(!h.record("a", 100, false, 101), "same failure counts once");
        h.record("a", 160, true, 160);
        assert_eq!(h.swap("a", 220, 220), Swap::Wait);
        h.record("a", 220, true, 220);
        assert_eq!(h.n_failures("a"), (1, 2));
        assert!(h.probe_wanted(400), "pile-up triggers a probe");
        assert_eq!(h.swap("a", 220, 220), Swap::CodexAlive, "no probe yet");
        h.probed(true, 400);
        assert!(!h.unhealthy());
        assert_eq!(
            h.swap("a", 220, 401),
            Swap::CodexAlive,
            "a proxy restart blip"
        );
        down(&mut h, 600);
        assert!(h.unhealthy());
        assert_eq!(h.swap("a", 220, 601), Swap::Go);
    }

    #[test]
    fn swaps_are_capped_per_hour_and_failed_swaps_back_off() {
        let mut h = Health::default();
        for at in [10, 20, 30] {
            h.record("a", at, false, at);
        }
        down(&mut h, 40);
        h.swap_tries.push(("a".to_owned(), 30, 1, 100));
        assert_eq!(h.swap("a", 30, 150), Swap::Backoff);
        assert_eq!(h.swap("a", 30, 100 + RETRY_SWAP_S), Swap::Go);
        h.swap_tries[0].2 = MAX_TRIES;
        assert_eq!(h.swap("a", 30, 100_000), Swap::Backoff);
        assert_eq!(
            h.swap("a", 31, 150),
            Swap::Go,
            "a newer failure is a new attempt"
        );
        h.swaps_s = vec![100, 200, 300];
        assert_eq!(h.swap("a", 31, 400), Swap::Capped);
        assert_eq!(h.swap("a", 31, 100 + HOUR_S), Swap::Go);
    }

    #[test]
    fn two_probes_in_a_row_recover_and_keep_this_passes_failures() {
        let mut h = Health::default();
        h.record("a", 100, false, 100);
        h.refused.push("a".to_owned());
        down(&mut h, 100);
        assert!(!h.probe_due(100 + PROBE_S - 1));
        assert!(h.probe_due(100 + PROBE_S));
        h.probed(true, 1000);
        h.probed(false, 1900);
        h.probed(true, 2800);
        assert!(h.unhealthy(), "a failed probe breaks the run");
        h.probed(true, 3700);
        assert!(!h.unhealthy());
        assert_eq!(h.n_failures("a"), (1, 0));
        assert_eq!(h.refused, ["a"]);
    }

    #[test]
    fn failures_outside_the_window_do_not_count() {
        let mut h = Health::default();
        h.record("a", 0, false, 0);
        h.record("a", 300, false, 300);
        h.record("a", 700, false, 700);
        assert_eq!(h.n_failures("a"), (2, 0));
        assert!(!h.record("a", 50, false, 700), "a stale failure is ignored");
    }

    #[test]
    fn health_survives_a_restart() {
        let mut h = Health::default();
        for (i, at) in [10, 20, 30].into_iter().enumerate() {
            h.record(&format!("agent{i}"), at, i == 1, at);
        }
        down(&mut h, 40);
        h.refused.push("agent0".to_owned());
        h.swaps_s.push(5);
        h.swap_tries.push(("agent0".to_owned(), 10, 2, 50));
        let back =
            Health::from_json(&serde_json::from_str(&h.to_json().to_string()).unwrap_or_default());
        assert!(back == h && back.unhealthy());
    }

    #[test]
    fn a_key_file_that_is_missing_is_a_config_error_not_a_dead_codex() {
        let mut settings = BTreeMap::new();
        settings.insert(
            "fallback.proxy_key_file".to_owned(),
            "/nonexistent/key".to_owned(),
        );
        assert!(matches!(probe(&settings), Probe::Config(_)));
    }

    #[test]
    fn experiments_and_managers_are_never_swapped() {
        let live = json!({"model_override": "gpt-6-astra"});
        let worker = [("is_manager", "false"), ("tool", "codex")];
        assert_eq!(refusal(&task(&worker), &live, "gpt-6-astra"), None);
        assert!(refusal(&task(&[("is_manager", "true")]), &live, "gpt-6-astra").is_some());
        assert!(
            refusal(&task(&[]), &live, "gpt-6-astra").is_some(),
            "role unrecorded fails closed"
        );
        let pinned = [("is_manager", "false"), ("model_pinned", "true")];
        assert!(refusal(&task(&pinned), &live, "gpt-6-astra").is_some());
        assert!(
            refusal(
                &task(&worker),
                &json!({"model_override": "gpt-6.1-sol"}),
                "gpt-6-astra"
            )
            .is_some()
        );
        assert!(refusal(&task(&worker), &json!({}), "gpt-6-astra").is_some());
    }
}
