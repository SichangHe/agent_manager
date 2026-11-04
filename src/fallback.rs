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

/// Failures older than this no longer count toward the 3-failure swap and the probe.
const WINDOW_S: i64 = 600;
/// Failures of one agent within the window that allow its swap, and of all agents that trigger a probe.
const N_FAILURES: usize = 3;
/// Seconds between probes while Codex is unhealthy.
const PROBE_S: i64 = 900;
/// Probe successes in a row that make Codex healthy again.
const N_PROBES_OK: u32 = 2;
/// Failures of one agent within `LONG_WINDOW_S` that allow its swap even while Codex answers probes.
const N_FAILURES_LONG: usize = 6;
const LONG_WINDOW_S: i64 = 1800;
/// Seconds before a probe may be repeated because failures piled up, and after which a failed probe no longer allows swaps.
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
        self.failures
            .retain(|(_, at, _)| now_s - at < LONG_WINDOW_S);
        self.swaps_s.retain(|at| now_s - at < HOUR_S);
        self.swap_tries.retain(|t| now_s - t.1 < HOUR_S);
        if at_s <= now_s - LONG_WINDOW_S
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

    /// The agent's failures (429s, stream drops) in the last `window_s`.
    pub fn n_within(&self, address: &str, now_s: i64, window_s: i64) -> (usize, usize) {
        let own = || {
            self.failures
                .iter()
                .filter(|f| f.0 == address && now_s - f.1 < window_s)
        };
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

    /// Whether failures piled up (3 in the window, from any agents) and the last probe is too old to say whether Codex is down.
    fn probe_wanted(&self, now_s: i64) -> bool {
        self.failures
            .iter()
            .filter(|f| now_s - f.1 < WINDOW_S)
            .count()
            >= N_FAILURES
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

    /// Whether to swap `address` (failed at `at_s`) now; pure. Needs 3 failures in 10 min and a probe failed within 5 min, or 6 failures in 30 min whatever the probe says; then the hourly cap and backoff apply.
    fn swap(&self, address: &str, at_s: i64, now_s: i64) -> Swap {
        let n = |window_s| {
            let (n_429, n_stream) = self.n_within(address, now_s, window_s);
            n_429 + n_stream
        };
        let slow = n(LONG_WINDOW_S) >= N_FAILURES_LONG;
        if n(WINDOW_S) < N_FAILURES && !slow {
            return Swap::Wait;
        }
        let probe_failed = self.last_ok == Some(false) && now_s < self.probed_s + RECHECK_S;
        if !probe_failed && !slow {
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

/// The saved health, read only; no file is healthy. A corrupt file counts as unhealthy with no known probe, so nothing is swapped on it, and its error is returned. Only the watcher moves it aside.
fn load() -> (Health, Option<String>) {
    read(&path())
}

fn read(path: &std::path::Path) -> (Health, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(value) => (Health::from_json(&value), None),
            Err(error) => {
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
        Ok(answer) => match answer.status().as_u16() {
            200 => Probe::Alive,
            429 | 500..=599 => Probe::Down,
            code => Probe::Config(format!("the Codex probe got HTTP {code} from {url}")),
        },
        Err(ureq::Error::Timeout(_)) => Probe::Down,
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
    if let Some(error) = corrupt {
        let _ = std::fs::rename(path(), path().with_extension("corrupt"));
        logs.push(format!("{error}; moved aside to .corrupt"));
    }
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
    // `(task, session, failure time in s, new)` of every failed Codex agent.
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
        due.push((task, session, at_s, fresh));
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
        let (n_429, n_stream) = health.n_within(address, now_s, WINDOW_S);
        let (long_429, long_stream) = health.n_within(address, now_s, LONG_WINDOW_S);
        let counts = format!(
            "{n_429} 429s and {n_stream} stream drops in {} min ({} in {} min)",
            WINDOW_S / 60,
            long_429 + long_stream,
            LONG_WINDOW_S / 60
        );
        let manager = task.field("managerat").unwrap_or_default();
        let mut swapped = false;
        match health.swap(address, at_s, now_s) {
            Swap::Wait | Swap::Backoff | Swap::CodexAlive => {}
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
                } else {
                    let reason = format!("because Codex failed {counts} and a probe failed too");
                    let message = format!(
                        "amh swapped this agent from Codex to Claude {reason}. Continue the task."
                    );
                    let to = Some(("claude", claude_model.as_str(), claude_effort.as_str()));
                    // An attempt counts against the cap even when it fails.
                    health.swaps_s.push(now_s);
                    match agents::replace(settings, store, &task.name, &message, to, &reason) {
                        Ok(new) => {
                            swapped = true;
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
        // Every fresh failure of an agent that stays gets a retry message, whatever held back its swap.
        if fresh && !swapped {
            let text =
                "Your last turn failed on a provider error (429 or dropped stream). Retry it.";
            logs.push(match omnigent::send(settings, address, text) {
                Ok(()) => format!("{}: {counts}; asked it to retry", task.name),
                Err(error) => format!("{}: {counts}; retry message failed: {error}", task.name),
            });
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
        assert_eq!(h.n_within("a", 220, WINDOW_S), (1, 2));
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
        h.swap_tries[0].2 = MAX_TRIES;
        assert_eq!(h.swap("a", 30, 300), Swap::Backoff);
        assert_eq!(
            h.swap("a", 31, 150),
            Swap::Go,
            "a newer failure is a new attempt"
        );
        h.swaps_s = vec![100, 200, 300];
        assert_eq!(h.swap("a", 31, 200), Swap::Capped);
        h.swaps_s = vec![-HOUR_S, -HOUR_S + 1, -HOUR_S + 2];
        assert_eq!(
            h.swap("a", 31, 200),
            Swap::Go,
            "swaps older than an hour do not count"
        );
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
        assert_eq!(h.n_within("a", 3700, LONG_WINDOW_S), (0, 0));
        assert_eq!(h.failures.len(), 1, "failures recorded in the pass stay");
        assert_eq!(h.refused, ["a"]);
    }

    #[test]
    fn failures_outside_the_window_do_not_count() {
        let mut h = Health::default();
        h.record("a", 0, false, 0);
        h.record("a", 300, false, 300);
        h.record("a", 700, false, 700);
        assert_eq!(h.n_within("a", 700, WINDOW_S), (2, 0));
        assert_eq!(h.n_within("a", 700, LONG_WINDOW_S), (3, 0));
        assert!(
            !h.record("a", -2000, false, 700),
            "a stale failure is ignored"
        );
    }

    #[test]
    fn six_failures_in_30_min_swap_even_while_probes_succeed_but_the_cap_holds() {
        let mut h = Health::default();
        for i in 0..6 {
            h.record("a", i * 280, false, i * 280);
        }
        h.probed(true, 1400);
        assert_eq!(h.swap("a", 1400, 1401), Swap::Go);
        h.swaps_s = vec![1000, 1100, 1200];
        assert_eq!(h.swap("a", 1400, 1401), Swap::Capped);
        let mut few = Health::default();
        for i in 0..5 {
            few.record("a", i * 280, false, i * 280);
        }
        few.probed(true, 1400);
        assert_eq!(
            few.swap("a", 1120, 1401),
            Swap::Wait,
            "5 failures spread over 20 min: only 2 in 10"
        );
    }

    #[test]
    fn a_failed_probe_older_than_5_min_does_not_unlock_swaps() {
        let mut h = Health::default();
        for at in [10, 20, 30] {
            h.record("a", at, false, at);
        }
        h.probed(false, 40);
        assert_eq!(h.swap("a", 30, 41), Swap::Go);
        assert_eq!(h.swap("a", 30, 40 + RECHECK_S), Swap::CodexAlive);
        assert!(h.probe_wanted(40 + RECHECK_S));
    }

    /// Serve one canned HTTP answer on a local port and return its URL.
    fn proxy_answering(code: u16) -> Result<String, std::io::Error> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = [0; 4096];
                let _ = conn.set_read_timeout(Some(std::time::Duration::from_millis(300)));
                let _ = conn.read(&mut buf);
                let _ = conn.read(&mut buf);
                let _ = write!(
                    conn,
                    "HTTP/1.1 {code} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                );
            }
        });
        Ok(url)
    }

    #[test]
    fn only_429_5xx_and_timeouts_are_a_down_codex() -> Result<(), Box<dyn std::error::Error>> {
        let key = std::env::temp_dir().join(format!("amh-probe-key-{}", std::process::id()));
        std::fs::write(&key, "k\n")?;
        let mut settings = BTreeMap::new();
        settings.insert(
            "fallback.proxy_key_file".to_owned(),
            key.display().to_string(),
        );
        for (code, want) in [(200, Probe::Alive), (429, Probe::Down), (503, Probe::Down)] {
            settings.insert("fallback.proxy_url".to_owned(), proxy_answering(code)?);
            assert_eq!(probe(&settings), want, "HTTP {code}");
        }
        for code in [401, 403, 404] {
            settings.insert("fallback.proxy_url".to_owned(), proxy_answering(code)?);
            assert!(matches!(probe(&settings), Probe::Config(_)), "HTTP {code}");
        }
        std::fs::remove_file(key)?;
        Ok(())
    }

    #[test]
    fn reading_a_corrupt_health_file_leaves_it_alone_and_counts_unhealthy()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = std::env::temp_dir().join(format!("amh-health-{}.json", std::process::id()));
        std::fs::write(&file, "{not json")?;
        let (health, error) = read(&file);
        assert!(health.unhealthy() && health.last_ok.is_none() && error.is_some());
        assert!(file.exists(), "only the watcher moves it aside");
        std::fs::remove_file(file)?;
        Ok(())
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
