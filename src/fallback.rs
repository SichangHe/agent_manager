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
/// Capacity failures within the window that trigger the swap of one agent and mark Codex unhealthy.
const N_FAILURES: usize = 3;
/// Seconds between probes while Codex is unhealthy.
const PROBE_S: i64 = 900;
/// Probe successes in a row that make Codex healthy again.
const N_PROBES_OK: u32 = 2;

// 🧑 "I think we just need to retry aggressively on 429 if they actually succeed sometimes we need to strike a balance between aggressive retries and if it’s taking too many tries, and the progress is way too slow you should switch to Claude"
/// Whether a session error is Codex giving up on 429s, or the model being at capacity.
// Real `last_task_error` values of codex-native sessions in Omnigent: `{code: rate_limit_exceeded, message: "exceeded retry limit, last status: 429 Too Many Requests"}` and `{code: codex_turn_error, message: "Selected model is at capacity. Please try a different model."}`.
pub fn capacity_429(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("last status: 429")
        || lower.contains("at capacity")
        || lower.contains("stream closed before response.completed")
}

/// Codex health and the capacity failures behind it, kept across watcher restarts in `amh-codex-health.json`.
#[derive(Default, PartialEq, Clone)]
pub struct Health {
    /// `(agent address, failure time in s)`, deduplicated.
    failures: Vec<(String, i64)>,
    unhealthy: bool,
    probes_ok: u32,
    probed_s: i64,
    /// Agents whose swap was refused and whose manager was told.
    refused: Vec<String>,
}

impl Health {
    fn from_json(value: &Value) -> Self {
        let text = |item: &Value| item.as_str().map(str::to_owned);
        Self {
            failures: value["failures"]
                .as_array()
                .map(|all| {
                    all.iter()
                        .filter_map(|f| Some((text(&f[0])?, f[1].as_i64()?)))
                        .collect()
                })
                .unwrap_or_default(),
            unhealthy: value["unhealthy"] == true,
            probes_ok: value["probes_ok"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or_default(),
            probed_s: value["probed_s"].as_i64().unwrap_or_default(),
            refused: value["refused"]
                .as_array()
                .map(|all| all.iter().filter_map(text).collect())
                .unwrap_or_default(),
        }
    }

    fn to_json(&self) -> Value {
        json!({"failures": self.failures, "unhealthy": self.unhealthy, "probes_ok": self.probes_ok, "probed_s": self.probed_s, "refused": self.refused})
    }

    /// Count a capacity failure of `address` at `at_s`; true when it is new. The third failure in the window, from any agents, marks Codex unhealthy.
    pub fn record(&mut self, address: &str, at_s: i64, now_s: i64) -> bool {
        self.failures.retain(|(_, at)| now_s - at < WINDOW_S);
        let key = (address.to_owned(), at_s);
        if at_s <= now_s - WINDOW_S || self.failures.contains(&key) {
            return false;
        }
        self.failures.push(key);
        if self.unhealthy {
            self.probes_ok = 0;
        } else if self.failures.len() >= N_FAILURES {
            (self.unhealthy, self.probes_ok, self.probed_s) = (true, 0, now_s);
        }
        true
    }

    pub fn n_failures(&self, address: &str) -> usize {
        self.failures
            .iter()
            .filter(|(who, _)| who == address)
            .count()
    }

    pub fn unhealthy(&self) -> bool {
        self.unhealthy
    }

    fn probe_due(&self, now_s: i64) -> bool {
        self.unhealthy && now_s >= self.probed_s + PROBE_S
    }

    /// Take a probe result: `N_PROBES_OK` successes in a row make Codex healthy and forget the failures.
    fn probed(&mut self, ok: bool, now_s: i64) {
        self.probed_s = now_s;
        self.probes_ok = if ok { self.probes_ok + 1 } else { 0 };
        if self.probes_ok >= N_PROBES_OK {
            *self = Self::default();
        }
    }
}

// 🧑 (via the manager) "Human authorized Claude replacements. Preserve experiment-specific model/account requirements; agent-runtime fallback must not silently substitute experiment models."
/// Why this agent must not be swapped to Claude, if so. Pure; fails closed.
///
/// An experiment's model is not in the task file, but a task started with `--model` carries `model_pinned`, and a session running a model other than the current default was chosen by hand.
pub fn refusal(task: &Task, session: &Value, default_model: &str) -> Option<String> {
    if task.field("is_manager") != Some("false") {
        return Some("it is a manager or its role is unrecorded".to_owned());
    }
    if task.field("model_pinned").is_some() {
        return Some("its task was started with an explicit model".to_owned());
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

fn load() -> Health {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .map(|value| Health::from_json(&value))
        .unwrap_or_default()
}

// 🧑 "later somehow decide when to switch back"
/// The tool a new worker starts with: Claude while the fallback is on and Codex is unhealthy, else `tool`.
pub fn default_tool(settings: &Settings, tool: &'static str) -> &'static str {
    if tool == "codex" && enabled(settings) && load().unhealthy() {
        "claude"
    } else {
        tool
    }
}

/// One tiny request to Codex through the proxy; true when it answers 200. The proxy's client key is read from `fallback.proxy_key_file` per probe and never kept or printed.
fn probe(settings: &Settings) -> Result<bool, String> {
    let file = settings::get(settings, "fallback.proxy_key_file");
    let key = std::fs::read_to_string(file).map_err(|error| {
        format!("cannot read the proxy key file `fallback.proxy_key_file`: {error}")
    })?;
    let url = Some(settings::get(settings, "fallback.proxy_url"))
        .filter(|url| !url.is_empty())
        .unwrap_or("http://127.0.0.1:18318");
    let (model, _) = omnigent::launch_default(settings, "codex", false)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
    agent
        .post(&format!("{url}/v1/responses"))
        .header("Authorization", &format!("Bearer {}", key.trim()))
        .send_json(json!({"model": model, "input": "ok", "max_output_tokens": 16, "stream": false}))
        .map(|answer| answer.status().as_u16() == 200)
        .map_err(|error| format!("the Codex probe could not reach the proxy: {error}"))
}

/// Act on the watcher's findings: retry a Codex agent that failed on capacity, swap it to Claude on the third failure, and probe Codex while unhealthy. Returns log lines.
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
    let mut health = load();
    let before = health.clone();
    let (claude_model, claude_effort) = match omnigent::launch_default(settings, "claude", false) {
        Ok(chosen) => chosen,
        Err(error) => return vec![format!("codex fallback: {error}")],
    };
    let default_model = omnigent::launch_default(settings, "codex", false).map(|(model, _)| model);
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
        if session["harness"] != "codex-native" || !capacity_429(omnigent::session_error(&session))
        {
            continue;
        }
        let at_s = session["updated_at"].as_i64().unwrap_or(now_s);
        let fresh = health.record(address, at_s, now_s);
        let tried = health.n_failures(address);
        if tried < N_FAILURES {
            if fresh {
                let text = "Your last turn failed on a provider capacity error (429). Retry it.";
                logs.push(match omnigent::send(settings, address, text) {
                    Ok(()) => format!(
                        "{}: capacity failure {tried}/{N_FAILURES}; asked it to retry",
                        task.name
                    ),
                    Err(error) => format!(
                        "{}: capacity failure {tried}/{N_FAILURES}; retry message failed: {error}",
                        task.name
                    ),
                });
            }
            continue;
        }
        let why = match &default_model {
            Ok(model) => refusal(task, &session, model),
            Err(error) => Some(error.clone()),
        };
        let Some(why) = why else {
            let reason = format!(
                "because Codex failed on capacity errors {tried} times in {} min",
                WINDOW_S / 60
            );
            let message =
                format!("amh swapped this agent from Codex to Claude {reason}. Continue the task.");
            logs.push(
                match agents::replace(
                    settings,
                    store,
                    &task.name,
                    &message,
                    Some(("claude", &claude_model, &claude_effort)),
                    &reason,
                ) {
                    Ok(new) => format!(
                        "{}: replaced its Codex agent with Claude {new} {reason}",
                        task.name
                    ),
                    Err(error) => format!("{}: swap to Claude failed: {error}", task.name),
                },
            );
            continue;
        };
        if !health.refused.iter().any(|known| known == address) {
            health.refused.push(address.to_owned());
            let text = format!(
                "{}: amh did not swap its Codex agent to Claude although it failed on capacity errors {tried} times: {why}. It is left as is.",
                task.name
            );
            let manager = task.field("managerat").unwrap_or_default();
            let told = agents::report(
                settings,
                store,
                manager,
                &crate::work::envelope(address, &text),
                false,
            );
            logs.push(format!("{text} (manager told: {})", told.is_ok()));
        }
    }
    if health.probe_due(now_s) {
        match probe(settings) {
            Ok(ok) => {
                health.probed(ok, now_s);
                logs.push(format!(
                    "codex probe {}; {}",
                    if ok { "succeeded" } else { "failed" },
                    if health.unhealthy() {
                        "still unhealthy"
                    } else {
                        "healthy again"
                    }
                ));
            }
            Err(error) => {
                health.probed(false, now_s);
                logs.push(format!("codex probe: {error}"));
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
        assert!(probe(&settings)?);
        settings.insert(
            "fallback.proxy_url".to_owned(),
            "http://127.0.0.1:9".to_owned(),
        );
        assert!(probe(&settings).is_err());
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

    #[test]
    fn three_failures_mark_unhealthy_and_two_probes_in_a_row_recover() {
        let mut h = Health::default();
        assert!(h.record("a", 100, 100));
        assert!(!h.record("a", 100, 101), "same failure counts once");
        assert!(h.record("b", 160, 160));
        assert!(!h.unhealthy());
        assert!(h.record("a", 220, 220));
        assert_eq!(h.n_failures("a"), 2);
        assert!(h.unhealthy());
        assert!(!h.probe_due(220 + PROBE_S - 1));
        assert!(h.probe_due(220 + PROBE_S));
        h.probed(true, 1120);
        h.probed(false, 2020);
        h.probed(true, 2920);
        assert!(h.unhealthy(), "a failed probe breaks the run");
        h.probed(true, 3820);
        assert!(!h.unhealthy());
        assert_eq!(h.n_failures("a"), 0);
    }

    #[test]
    fn failures_outside_the_window_do_not_count() {
        let mut h = Health::default();
        h.record("a", 0, 0);
        h.record("a", 300, 300);
        h.record("a", 700, 700);
        assert_eq!(h.n_failures("a"), 2);
        assert!(!h.unhealthy());
        assert!(!h.record("a", 50, 700), "a stale failure is ignored");
    }

    #[test]
    fn health_survives_a_restart() {
        let mut h = Health::default();
        for (i, at) in [10, 20, 30].into_iter().enumerate() {
            h.record(&format!("agent{i}"), at, at);
        }
        h.refused.push("agent0".to_owned());
        let back =
            Health::from_json(&serde_json::from_str(&h.to_json().to_string()).unwrap_or_default());
        assert!(back == h && back.unhealthy());
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
