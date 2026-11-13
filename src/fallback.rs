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
/// A restart refused only because no agent start slot was free is retried this soon: slots free up within a minute.
const SLOT_RETRY_S: i64 = 60;
/// Slot-refused restarts of one failure are retried for at most this long, then left to the normal backoff.
const SLOT_RETRY_FOR_S: i64 = 1800;
/// Same-tool restarts per task per hour, attempts included; separate from `MAX_SWAPS_H`.
const MAX_RESTARTS_H: usize = 3;
/// The attempts value of a restart that worked.
const RESTARTED: u32 = u32::MAX;
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

/// A Codex agent's failure: `(task name, kept across restarts into new sessions, time in s, whether a stream drop rather than a 429)`.
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
    /// Failed swaps: `(task name, failure time in s, attempts, last attempt in s)`.
    swap_tries: Vec<(String, i64, u32, i64)>,
    /// Restart attempts `(task name, time in s)` in the last hour.
    restarts_s: Vec<(String, i64)>,
    /// Failed restarts: `(task name, failure time in s, attempts, last attempt in s)`.
    restart_tries: Vec<(String, i64, u32, i64)>,
    /// Restart notices to managers, once per hour: `(key, time in s)`.
    restart_told: Vec<(String, i64)>,
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
            restarts_s: list("restarts_s")
                .iter()
                .filter_map(|r| Some((text(&r[0])?, r[1].as_i64()?)))
                .collect(),
            restart_tries: list("restart_tries")
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
            restart_told: list("restart_told")
                .iter()
                .filter_map(|r| Some((text(&r[0])?, r[1].as_i64()?)))
                .collect(),
        }
    }

    fn to_json(&self) -> Value {
        json!({"failures": self.failures, "unhealthy": self.unhealthy, "last_ok": self.last_ok, "probes_ok": self.probes_ok, "probed_s": self.probed_s, "refused": self.refused, "swaps_s": self.swaps_s, "cap_told_s": self.cap_told_s, "config_told": self.config_told, "swap_tries": self.swap_tries, "restarts_s": self.restarts_s, "restart_tries": self.restart_tries, "restart_told": self.restart_told})
    }

    /// Count a failure of `task` at `at_s`; true when it is new.
    pub fn record(&mut self, task: &str, at_s: i64, stream: bool, now_s: i64) -> bool {
        self.failures
            .retain(|(_, at, _)| now_s - at < LONG_WINDOW_S);
        self.swaps_s.retain(|at| now_s - at < HOUR_S);
        self.swap_tries.retain(|t| now_s - t.1 < HOUR_S);
        self.restart_tries.retain(|t| now_s - t.1 < HOUR_S);
        self.restarts_s.retain(|r| now_s - r.1 < HOUR_S);
        self.restart_told.retain(|r| now_s - r.1 < HOUR_S);
        if at_s <= now_s - LONG_WINDOW_S
            || self
                .failures
                .iter()
                .any(|(who, at, _)| who == task && *at == at_s)
        {
            return false;
        }
        self.failures.push((task.to_owned(), at_s, stream));
        if self.unhealthy {
            self.probes_ok = 0;
        }
        true
    }

    /// The agent's failures (429s, stream drops) in the last `window_s`.
    pub fn n_within(&self, task: &str, now_s: i64, window_s: i64) -> (usize, usize) {
        let own = || {
            self.failures
                .iter()
                .filter(|f| f.0 == task && now_s - f.1 < window_s)
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

    /// Whether to restart `task` on Codex now, for its failure at `at_s`; pure. A fresh failure restarts once; a failed restart is retried up to `MAX_TRIES` times, `RETRY_SWAP_S` apart; at most `MAX_RESTARTS_H` attempts per hour. Managers are never restarted by amh.
    fn restart(&self, task: &str, is_manager: bool, at_s: i64, fresh: bool, now_s: i64) -> Restart {
        if is_manager {
            return Restart::Manager;
        }
        let due = match self
            .restart_tries
            .iter()
            .find(|t| t.0 == task && t.1 == at_s)
        {
            Some((_, _, n, _)) if *n == RESTARTED => false,
            Some((_, _, n, _)) if *n >= MAX_TRIES => return Restart::GaveUp,
            Some((_, _, _, last)) => now_s >= last + RETRY_SWAP_S,
            None => fresh,
        };
        if !due {
            return Restart::No;
        }
        let n_hour = self
            .restarts_s
            .iter()
            .filter(|r| r.0 == task && now_s - r.1 < HOUR_S)
            .count();
        if n_hour >= MAX_RESTARTS_H {
            Restart::Capped
        } else {
            Restart::Go
        }
    }

    /// Note a restart attempt of `task` for its failure at `at_s`; a failed one stays retryable with backoff.
    ///
    /// A refusal only for want of a start slot started nothing: it uses no attempt and no hourly restart, and is due again after `SLOT_RETRY_S` (for up to `SLOT_RETRY_FOR_S` after the failure).
    fn restarted(&mut self, task: &str, at_s: i64, outcome: Attempt, now_s: i64) {
        if outcome == Attempt::NoSlot && now_s - at_s < SLOT_RETRY_FOR_S {
            let tries = self
                .restart_tries
                .iter()
                .find(|t| t.0 == task && t.1 == at_s)
                .map_or(0, |t| t.2);
            self.restart_tries.retain(|t| !(t.0 == task && t.1 == at_s));
            // Stored as if the last real attempt was long enough ago that the next is due after SLOT_RETRY_S.
            self.restart_tries.push((
                task.to_owned(),
                at_s,
                tries,
                now_s + SLOT_RETRY_S - RETRY_SWAP_S,
            ));
            return;
        }
        let ok = outcome == Attempt::Ok;
        self.restarts_s.push((task.to_owned(), now_s));
        let tries = self
            .restart_tries
            .iter()
            .find(|t| t.0 == task && t.1 == at_s)
            .map_or(0, |t| t.2);
        self.restart_tries.retain(|t| !(t.0 == task && t.1 == at_s));
        // A success needs no retry, but the entry keeps this failure from counting as fresh again.
        let n = if ok { RESTARTED } else { tries + 1 };
        self.restart_tries.push((task.to_owned(), at_s, n, now_s));
    }

    /// Whether the notice `key` is due, and if so mark it told for an hour.
    fn tell_once(&mut self, key: String, now_s: i64) -> bool {
        if self.restart_told.iter().any(|r| r.0 == key) {
            return false;
        }
        self.restart_told.push((key, now_s));
        true
    }

    /// Whether to swap `task` (failed at `at_s`) now; pure. Needs 3 failures in 10 min and a probe failed within 5 min, or 6 failures in 30 min whatever the probe says; then the hourly cap and backoff apply.
    fn swap(&self, task: &str, at_s: i64, now_s: i64) -> Swap {
        let n = |window_s| {
            let (n_429, n_stream) = self.n_within(task, now_s, window_s);
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
        match self.swap_tries.iter().find(|t| t.0 == task && t.1 == at_s) {
            Some((_, _, n, _)) if *n >= MAX_TRIES => Swap::GaveUp,
            Some((_, _, _, last)) if now_s < last + RETRY_SWAP_S => Swap::Backoff,
            _ => Swap::Go,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Restart {
    No,
    Go,
    Capped,
    Manager,
    GaveUp,
}

/// Whether the task still runs the agent whose session was checked; a task reloaded after an owner replaced its agent does not.
fn same_agent(reloaded_address: &str, checked_address: &str) -> bool {
    reloaded_address == checked_address
}

/// Whether the session is still the failure seen at `at_s`: failed, with the same `updated_at`.
fn still_failed(session: &Value, at_s: i64) -> bool {
    session["status"] == "failed"
        && omnigent::updated_at(session).is_some_and(|at| at.timestamp() == at_s)
}

#[derive(Debug, PartialEq)]
enum Swap {
    Wait,
    CodexAlive,
    Capped,
    Backoff,
    GaveUp,
    Go,
}

// 🧑 (via the manager) "Human authorized Claude replacements. Preserve experiment-specific model/account requirements; agent-runtime fallback must not silently substitute experiment models."
/// Why this agent must not be swapped to Claude, if so. Pure.
///
/// An experiment's model is not in the task file, but a task started with `--model` or `--effort` carries `model_pinned`, and a session running a model other than the current default was chosen by hand. Only a task with `model_pinned: false` can be swapped; one started before the marker is refused.
pub fn refusal(task: &Task, session: &Value, default_model: &str) -> Option<String> {
    if task.field("is_manager") != Some("false") {
        return Some("it is a manager or its role is unrecorded".to_owned());
    }
    // Flip back to fail-open by refusing only `Some("true")`.
    match task.field("model_pinned") {
        Some("false") => {}
        Some(_) => return Some("its task was started with an explicit model or effort".to_owned()),
        None => {
            return Some(
                "its task has no `model_pinned` marker (started before the fallback)".to_owned(),
            );
        }
    }
    match omnigent::session_model(session) {
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
    // `name` is the failed agent's address: a notice never goes to it, nor to nobody; the problem notice of the watcher already emails the human about a failed agent.
    let tell = |logs: &mut Vec<String>, manager: &str, name: &str, text: &str| {
        let main_manager = settings::get(settings, "main_manager");
        let manager = if manager.starts_with("omnigent://") {
            manager
        } else {
            main_manager
        };
        if manager == name || manager.is_empty() {
            logs.push(format!("{text} (no agent above it to tell; not sent)"));
            return;
        }
        let told = agents::report(
            settings,
            store,
            manager,
            &crate::work::envelope(name, text),
            false,
        );
        logs.push(format!("{text} (manager told: {})", told.is_ok()));
    };
    // Just before a replace: the task still has the checked agent, and that session is still failed with the same `updated_at`.
    let current = |task: &Task, at_s: i64| {
        store
            .load(&task.name)
            .is_ok_and(|fresh| same_agent(fresh.address(), task.address()))
            && omnigent::session(task.address()).is_ok_and(|id| {
                omnigent::api(
                    settings,
                    "GET",
                    &format!("/v1/sessions/{id}?include_items=false"),
                    None,
                )
                .is_ok_and(|session| still_failed(&session, at_s))
            })
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
        let fresh = health.record(&task.name, at_s, stream_drop(error), now_s);
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
        let (n_429, n_stream) = health.n_within(&task.name, now_s, WINDOW_S);
        let (long_429, long_stream) = health.n_within(&task.name, now_s, LONG_WINDOW_S);
        let counts = format!(
            "{n_429} 429s and {n_stream} stream drops in {} min ({} in {} min)",
            WINDOW_S / 60,
            long_429 + long_stream,
            LONG_WINDOW_S / 60
        );
        let manager = task.field("managerat").unwrap_or_default();
        // True when the swap path handled this failure, so no restart follows.
        let mut swapped = false;
        match health.swap(&task.name, at_s, now_s) {
            Swap::Wait | Swap::CodexAlive => {}
            // The swap backoff owns the retry of a failed swap; no Codex restart meanwhile.
            Swap::Backoff => swapped = true,
            Swap::GaveUp => {
                swapped = true;
                if health.tell_once(format!("swapgaveup:{}:{at_s}", task.name), now_s) {
                    let text = format!(
                        "{}: amh's {MAX_TRIES} attempts to swap its Codex agent to Claude failed ({counts}); it is left failed for you to handle.",
                        task.name
                    );
                    tell(&mut logs, manager, address, &text);
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
                    if !health.refused.contains(&task.name) {
                        health.refused.push(task.name.clone());
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
                    if !current(task, at_s) {
                        logs.push(format!(
                            "{}: its agent or session changed since the check; swap skipped",
                            task.name
                        ));
                        continue;
                    }
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
                            swapped = true;
                            let tries = health
                                .swap_tries
                                .iter()
                                .find(|t| t.0 == task.name && t.1 == at_s)
                                .map_or(0, |t| t.2);
                            health
                                .swap_tries
                                .retain(|t| !(t.0 == task.name && t.1 == at_s));
                            health
                                .swap_tries
                                .push((task.name.clone(), at_s, tries + 1, now_s));
                            logs.push(format!(
                                "{}: swap to Claude failed (attempt {}): {error}",
                                task.name,
                                tries + 1
                            ));
                            if health.tell_once(format!("swapfail:{}:{at_s}", task.name), now_s) {
                                let text = format!(
                                    "{}: amh's swap of its Codex agent to Claude failed: {error}. The agent is left failed for you to handle; amh does not restart it, since a half-launched agent may exist.",
                                    task.name
                                );
                                tell(&mut logs, manager, address, &text);
                            }
                        }
                    }
                }
            }
        }
        // A fresh failure (or a failed restart due again) of an agent that stays is restarted on Codex, whatever held back its swap.
        // A message to a failed `codex-native` session starts no turn (amh_support_codex.md: cx_maint's retries at 06:40, 06:43 and 06:49 on Nov 4 sat unanswered), so the retry is a fresh agent on the same tool, model and effort.
        if swapped {
            continue;
        }
        let is_manager = task.field("is_manager") != Some("false");
        match health.restart(&task.name, is_manager, at_s, fresh, now_s) {
            Restart::No => {}
            Restart::Manager => {
                if fresh && !health.refused.contains(&format!("restart:{}", task.name)) {
                    health.refused.push(format!("restart:{}", task.name));
                    let text = format!(
                        "{}: amh does not restart a manager or a task of unrecorded role after a Codex failure ({counts}); restart it by hand.",
                        task.name
                    );
                    tell(
                        &mut logs,
                        settings::get(settings, "main_manager"),
                        address,
                        &text,
                    );
                }
            }
            Restart::GaveUp => {
                if health.tell_once(format!("gaveup:{}:{at_s}", task.name), now_s) {
                    let text = format!(
                        "{}: amh's {MAX_TRIES} attempts to restart its Codex agent failed ({counts}); it is left failed.",
                        task.name
                    );
                    tell(&mut logs, manager, address, &text);
                }
            }
            Restart::Capped => {
                if health.tell_once(format!("cap:{}", task.name), now_s) {
                    let text = format!(
                        "{}: amh restarted its Codex agent {MAX_RESTARTS_H} times in the last hour ({counts}) and stops; it is left failed.",
                        task.name
                    );
                    tell(&mut logs, manager, address, &text);
                }
            }
            Restart::Go if !current(task, at_s) => logs.push(format!(
                "{}: its agent or session changed since the check; restart skipped",
                task.name
            )),
            Restart::Go => {
                let reason = format!("because its Codex session failed ({counts})");
                let message = "Your previous session failed on a provider error (429 or dropped stream). Continue the task.";
                let done = agents::replace(settings, store, &task.name, message, None, &reason);
                let attempt = attempt_of(&done);
                health.restarted(&task.name, at_s, attempt, now_s);
                logs.push(match done {
                    Ok(new) => format!("{}: {counts}; restarted on Codex as {new}", task.name),
                    Err(error) if attempt == Attempt::NoSlot => format!(
                        "{}: {counts}; restart on Codex waits for a start slot (retried in {SLOT_RETRY_S} s): {error}",
                        task.name
                    ),
                    Err(error) => format!(
                        "{}: {counts}; restart on Codex failed (retried with backoff): {error}",
                        task.name
                    ),
                });
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

/// The result of one restart attempt.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Attempt {
    Ok,
    /// Refused only because no agent start slot was free; nothing was started.
    NoSlot,
    Failed,
}

/// Classify a restart result; a missing start slot is recognized by `omnigent`'s message.
fn attempt_of(done: &Result<String, String>) -> Attempt {
    match done {
        Ok(_) => Attempt::Ok,
        Err(error) if error.contains(omnigent::NO_SLOT) => Attempt::NoSlot,
        Err(_) => Attempt::Failed,
    }
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
        assert_eq!(h.swap("a", 30, 300), Swap::GaveUp, "attempts run out");
        assert!(h.tell_once("swapgaveup:a:30".to_owned(), 300));
        assert!(!h.tell_once("swapgaveup:a:30".to_owned(), 301));
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
        h.restarted("agent0", 10, Attempt::Failed, 60);
        h.tell_once("cap:agent0".to_owned(), 70);
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
    fn failures_of_successive_sessions_of_one_task_reach_a_swap() {
        let mut h = Health::default();
        // Each restart makes a new session, so the failures come from different addresses but one task name.
        for at in [100, 200, 300] {
            assert!(h.record("t.md", at, true, at));
        }
        h.probed(false, 310);
        assert_eq!(h.swap("t.md", 300, 320), Swap::Go);
        assert_eq!(h.swap("other.md", 300, 320), Swap::Wait);
    }

    #[test]
    fn a_fresh_failure_restarts_once_and_a_deduplicated_one_does_not() {
        let mut h = Health::default();
        assert!(h.record("t.md", 100, false, 100));
        assert_eq!(h.restart("t.md", false, 100, true, 101), Restart::Go);
        h.restarted("t.md", 100, Attempt::Ok, 101);
        assert!(
            !h.record("t.md", 100, false, 160),
            "same failure is not fresh"
        );
        assert_eq!(h.restart("t.md", false, 100, false, 160), Restart::No);
        assert_eq!(
            h.restart("t.md", false, 100, false, 5000),
            Restart::No,
            "a done restart is not retried"
        );
    }

    #[test]
    fn restarts_stop_at_the_hourly_cap_and_the_manager_is_told_once() {
        let mut h = Health::default();
        for (i, at) in [100, 200, 300].into_iter().enumerate() {
            h.record("t.md", at, false, at);
            assert_eq!(
                h.restart("t.md", false, at, true, at),
                Restart::Go,
                "restart {i}"
            );
            h.restarted("t.md", at, Attempt::Ok, at);
        }
        h.record("t.md", 400, false, 400);
        assert_eq!(h.restart("t.md", false, 400, true, 400), Restart::Capped);
        assert!(h.tell_once("cap:t.md".to_owned(), 400));
        assert!(!h.tell_once("cap:t.md".to_owned(), 401));
        assert_eq!(h.swaps_s.len(), 0, "restarts do not use the swap cap");
        h.record("t.md", 100 + HOUR_S, false, 100 + HOUR_S);
        assert_eq!(
            h.restart("t.md", false, 100 + HOUR_S, true, 100 + HOUR_S),
            Restart::Go,
            "an hour later it may restart again"
        );
    }

    #[test]
    fn a_start_slot_refusal_is_retried_within_a_minute_without_using_attempts() {
        // As seen live for cx_builtins.md (15:46) and vl_cx_engine_hunt.md (17:14): the restart was refused for a start slot.
        let mut h = Health::default();
        let at = 100;
        assert_eq!(h.restart("t.md", false, at, true, at), Restart::Go);
        h.restarted("t.md", at, Attempt::NoSlot, at);
        assert_eq!(
            h.restart("t.md", false, at, false, at + SLOT_RETRY_S - 1),
            Restart::No
        );
        assert_eq!(
            h.restart("t.md", false, at, false, at + SLOT_RETRY_S),
            Restart::Go
        );
        // Many slot refusals use neither attempts nor the hourly cap.
        for k in 1..20 {
            h.restarted("t.md", at, Attempt::NoSlot, at + k * SLOT_RETRY_S);
        }
        assert!(h.restarts_s.is_empty());
        assert_eq!(
            h.restart("t.md", false, at, false, at + 20 * SLOT_RETRY_S),
            Restart::Go
        );
        // A real failure after that still uses the long backoff.
        h.restarted("t.md", at, Attempt::Failed, at + 20 * SLOT_RETRY_S);
        assert_eq!(
            h.restart("t.md", false, at, false, at + 21 * SLOT_RETRY_S),
            Restart::No
        );
        assert_eq!(
            h.restart(
                "t.md",
                false,
                at,
                false,
                at + 20 * SLOT_RETRY_S + RETRY_SWAP_S
            ),
            Restart::Go
        );
        // Past SLOT_RETRY_FOR_S a slot refusal counts as a normal failed attempt.
        let mut late = Health::default();
        late.restarted("u.md", at, Attempt::NoSlot, at + SLOT_RETRY_FOR_S);
        assert_eq!(late.restarts_s.len(), 1);
        // Managers stay excluded.
        assert_eq!(h.restart("m.md", true, at, true, at), Restart::Manager);
        assert_eq!(
            attempt_of(&Err(format!("amh: {}", omnigent::NO_SLOT))),
            Attempt::NoSlot
        );
        assert_eq!(
            attempt_of(&Err(
                "amh: the manager's session has no model_override".into()
            )),
            Attempt::Failed
        );
        assert_eq!(attempt_of(&Ok("omnigent://new".into())), Attempt::Ok);
    }

    #[test]
    fn a_failed_restart_is_retried_with_backoff_up_to_a_limit() {
        let mut h = Health::default();
        h.record("t.md", 100, false, 100);
        h.restarted("t.md", 100, Attempt::Failed, 100);
        assert_eq!(h.restart("t.md", false, 100, false, 101), Restart::No);
        assert_eq!(
            h.restart("t.md", false, 100, false, 100 + RETRY_SWAP_S),
            Restart::Go
        );
        h.restarted("t.md", 100, Attempt::Failed, 100 + RETRY_SWAP_S);
        h.restarted("t.md", 100, Attempt::Failed, 100 + 2 * RETRY_SWAP_S);
        assert_eq!(
            h.restart("t.md", false, 100, false, 100 + 4 * RETRY_SWAP_S),
            Restart::GaveUp,
            "attempts are limited"
        );
    }

    #[test]
    fn exhausted_restart_attempts_are_reported_not_silent() {
        let mut h = Health::default();
        h.record("t.md", 100, false, 100);
        for k in 0..MAX_TRIES {
            h.restarted(
                "t.md",
                100,
                Attempt::Failed,
                100 + i64::from(k) * RETRY_SWAP_S,
            );
        }
        assert_eq!(h.restart("t.md", false, 100, false, 200), Restart::GaveUp);
        assert!(h.tell_once("gaveup:t.md:100".to_owned(), 200));
        assert!(!h.tell_once("gaveup:t.md:100".to_owned(), 300));
    }

    #[test]
    fn a_session_revived_or_changed_is_not_replaced() {
        let failed = json!({"status": "failed", "updated_at": 500});
        assert!(still_failed(&failed, 500));
        assert!(!still_failed(
            &json!({"status": "running", "updated_at": 500}),
            500
        ));
        assert!(!still_failed(
            &json!({"status": "failed", "updated_at": 501}),
            500
        ));
        assert!(!still_failed(&json!({"status": "failed"}), 500));
    }

    #[test]
    fn managers_are_never_restarted() {
        let h = Health::default();
        assert_eq!(h.restart("m.md", true, 100, true, 101), Restart::Manager);
    }

    #[test]
    fn a_task_whose_agent_changed_is_not_restarted() {
        assert!(same_agent("omnigent://a", "omnigent://a"));
        assert!(!same_agent("omnigent://new", "omnigent://a"));
    }

    #[test]
    fn a_same_tool_restart_keeps_the_pin_and_the_model() {
        let mut t = task(&[
            ("is_manager", "false"),
            ("model_pinned", "true"),
            ("tool", "codex"),
        ]);
        let session = json!({"harness": "codex-native", "model_override": "gpt-6.1-sol", "reasoning_effort": "high", "workspace": "/w"});
        let [tool, model, effort, _] =
            crate::work::relaunch(&session, None, None, None).unwrap_or_default();
        assert_eq!(
            (tool.as_str(), model.as_str(), effort.as_str()),
            ("codex", "gpt-6.1-sol", "high")
        );
        crate::work::record_start(&mut t, &[], "omnigent://new", &tool, "restarted\n");
        assert_eq!(t.field("model_pinned"), Some("true"));
        assert_eq!(t.address(), "omnigent://new");
    }

    #[test]
    fn experiments_and_managers_are_never_swapped() {
        let live = json!({"model_override": "gpt-6-astra"});
        let worker = [
            ("is_manager", "false"),
            ("tool", "codex"),
            ("model_pinned", "false"),
        ];
        assert_eq!(refusal(&task(&worker), &live, "gpt-6-astra"), None);
        assert!(refusal(&task(&[("is_manager", "true")]), &live, "gpt-6-astra").is_some());
        assert!(
            refusal(&task(&[]), &live, "gpt-6-astra").is_some(),
            "role unrecorded fails closed"
        );
        let old = [("is_manager", "false"), ("tool", "codex")];
        assert!(
            refusal(&task(&old), &live, "gpt-6-astra").is_some(),
            "unmarked task is refused"
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
