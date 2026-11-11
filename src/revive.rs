//! Revive agents that failed or went away: first try to resume the session, then replace it if that did not work.
// 🧑 (via human_mgr, relayed by the main manager, 2026-11-11) "Agent should handle all agent resumptions, or maybe just restart those agents if resumption doesn't work"
// Never touches held or quota-paused agents: `omnigent::status` reports them `paused`, so they never reach `found`.

use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::store::Store;
use crate::tasks::Task;
use crate::work::Problem;
use crate::{agents, fallback, omnigent};

type Settings = BTreeMap<String, String>;

const RECORD: &str = "amh-revive.json";
/// Seconds a resume gets before the agent is replaced instead.
const RESUME_WAIT_S: i64 = 600;
/// No replacement within this many seconds of the agent's start (no-thrash rule: "not within 30 min").
const MIN_AGE_S: i64 = 1800;
/// Replacements per hour over all agents, so an outage does not restart everything at once.
const MAX_REPLACES_H: usize = 6;
const HOUR_S: i64 = 3600;
/// More agents down at once than this looks like an Omnigent or provider outage: leave them to come back.
const MAX_DOWN: usize = 10;

/// Per task: the agent address last seen, when it was first seen failed-or-not, and when it was resumed.
#[derive(Default, Clone, PartialEq, Debug)]
struct Seen {
    at: String,
    seen_s: i64,
    resumed_s: Option<i64>,
}

#[derive(Default, Clone, PartialEq, Debug)]
struct State {
    tasks: BTreeMap<String, Seen>,
    replaces_s: Vec<i64>,
}

impl State {
    fn from_json(value: &Value) -> Self {
        let mut state = Self::default();
        if let Some(tasks) = value["tasks"].as_object() {
            for (name, seen) in tasks {
                state.tasks.insert(
                    name.clone(),
                    Seen {
                        at: seen["at"].as_str().unwrap_or_default().to_owned(),
                        seen_s: seen["seen_s"].as_i64().unwrap_or_default(),
                        resumed_s: seen["resumed_s"].as_i64(),
                    },
                );
            }
        }
        state.replaces_s = value["replaces_s"]
            .as_array()
            .map(|all| all.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        state
    }

    fn to_json(&self) -> Value {
        let tasks: serde_json::Map<String, Value> = self
            .tasks
            .iter()
            .map(|(name, seen)| {
                (
                    name.clone(),
                    json!({"at": seen.at, "seen_s": seen.seen_s, "resumed_s": seen.resumed_s}),
                )
            })
            .collect();
        json!({"tasks": tasks, "replaces_s": self.replaces_s})
    }
}

#[derive(Debug, PartialEq)]
enum Step {
    Resume,
    Wait,
    Replace,
    Capped,
}

/// What to do now with a failed or missing agent; pure.
fn step(seen: &Seen, replaces_s: &[i64], now_s: i64) -> Step {
    let Some(resumed_s) = seen.resumed_s else {
        return Step::Resume;
    };
    if now_s < resumed_s + RESUME_WAIT_S || now_s < seen.seen_s + MIN_AGE_S {
        return Step::Wait;
    }
    if replaces_s.iter().filter(|&&s| s > now_s - HOUR_S).count() >= MAX_REPLACES_H {
        return Step::Capped;
    }
    Step::Replace
}

/// Whether the fallback owns this failure: a Codex agent failing on 429s, capacity, or a dropped stream.
fn fallback_owns(task: &Task, evidence: &str) -> bool {
    task.field("tool") == Some("codex") && fallback::capacity_429(evidence)
}

/// One watcher pass: resume each unheld failed or gone agent once, then replace it if it is still down.
pub fn pass(
    settings: &Settings,
    store: &Store,
    found: &[(Task, Problem, String)],
    now_s: i64,
) -> Vec<String> {
    let mut logs = Vec::new();
    let mut state = State::from_json(
        &serde_json::from_str(&store.record(RECORD).unwrap_or_default()).unwrap_or(Value::Null),
    );
    let before = state.clone();
    let active = store.active().unwrap_or_default();
    // Remember when each agent address was first seen, so a new agent is never replaced within `MIN_AGE_S`.
    state
        .tasks
        .retain(|name, _| active.iter().any(|t| &t.name == name));
    for task in &active {
        let at = task.address();
        let entry = state.tasks.entry(task.name.clone()).or_default();
        if entry.at != at {
            *entry = Seen {
                at: at.to_owned(),
                seen_s: now_s,
                resumed_s: None,
            };
        }
    }
    let down: BTreeMap<&str, (&Task, &String)> = found
        .iter()
        .filter(|(task, problem, evidence)| {
            matches!(problem, Problem::Missing | Problem::Error)
                && !fallback_owns(task, evidence)
                && task.address().starts_with(omnigent::PREFIX)
                // Omnigent not answering says nothing about the agent.
                && !evidence.contains("Omnigent is unreachable")
        })
        .map(|(task, _, evidence)| (task.name.as_str(), (task, evidence)))
        .collect();
    // An agent that stayed up for `MIN_AGE_S` after a resume gets a fresh resume next time; one that falls over sooner is replaced.
    for (name, seen) in &mut state.tasks {
        if !down.contains_key(name.as_str())
            && seen.resumed_s.is_some_and(|s| now_s >= s + MIN_AGE_S)
        {
            seen.resumed_s = None;
        }
    }
    if down.len() > MAX_DOWN {
        logs.push(format!(
            "revive: {} agents down at once; looks like an outage, nothing done",
            down.len()
        ));
        if state != before {
            let _ = store.set_record(RECORD, &state.to_json().to_string());
        }
        return logs;
    }
    let mut replaced = false;
    state.replaces_s.retain(|&s| s > now_s - HOUR_S);
    for (name, (task, evidence)) in down {
        let Some(seen) = state.tasks.get(name).cloned() else {
            continue;
        };
        match step(&seen, &state.replaces_s, now_s) {
            Step::Wait | Step::Capped => {}
            Step::Resume => {
                let text = "amh: your session failed or stopped. Resume your task from where you left off: check your open items with `amh todo list`.";
                let sent = omnigent::send_now_traced(settings, task.address(), text, None);
                logs.push(format!(
                    "{name}: down ({evidence}); resume sent: {}",
                    sent.as_ref()
                        .map_or_else(Clone::clone, |()| "ok".to_owned())
                ));
                if let Some(seen) = state.tasks.get_mut(name) {
                    seen.resumed_s = Some(now_s);
                }
            }
            Step::Replace if replaced => {}
            Step::Replace => {
                replaced = true;
                // Re-check just before: still the same agent, still down, still not held.
                let fresh = store.load(name).ok();
                let still = fresh.as_ref().is_some_and(|t| t.address() == seen.at)
                    && omnigent::held(&seen.at).is_none()
                    && crate::quota_pause::paused(&seen.at).is_none()
                    && matches!(
                        omnigent::fetched_status(settings, &seen.at).0,
                        "missing" | "error"
                    );
                if !still {
                    logs.push(format!(
                        "{name}: changed or recovered before replacement; skipped"
                    ));
                    continue;
                }
                let reason = format!("because its agent stayed down after a resume ({evidence})");
                let message = "Your previous agent failed or went away and did not come back after a resume. Continue the task.";
                match agents::replace(settings, store, name, message, None, &reason) {
                    Ok(new) => {
                        state.replaces_s.push(now_s);
                        logs.push(format!("{name}: replaced by {new} {reason}"));
                    }
                    Err(error) => {
                        logs.push(format!("{name}: replacement failed: {error}"));
                        // Retry later only after another full resume wait.
                        if let Some(seen) = state.tasks.get_mut(name) {
                            seen.resumed_s = Some(now_s);
                        }
                    }
                }
            }
        }
    }
    if state != before
        && let Err(error) = store.set_record(RECORD, &state.to_json().to_string())
    {
        logs.push(format!("revive: cannot save {RECORD}: {error}"));
    }
    logs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(seen_s: i64, resumed_s: Option<i64>) -> Seen {
        Seen {
            at: "omnigent://a".into(),
            seen_s,
            resumed_s,
        }
    }

    #[test]
    fn first_resume_then_wait_then_replace() {
        assert_eq!(step(&seen(0, None), &[], 5000), Step::Resume);
        assert_eq!(step(&seen(0, Some(5000)), &[], 5000 + 599), Step::Wait);
        assert_eq!(step(&seen(0, Some(5000)), &[], 5000 + 600), Step::Replace);
    }

    #[test]
    fn a_young_agent_is_never_replaced() {
        assert_eq!(step(&seen(1000, Some(1000)), &[], 1000 + 1799), Step::Wait);
        assert_eq!(
            step(&seen(1000, Some(1000)), &[], 1000 + 1800),
            Step::Replace
        );
    }

    #[test]
    fn replacements_are_capped_per_hour() {
        let recent = [100, 200, 300, 400, 500, 600];
        assert_eq!(step(&seen(0, Some(0)), &recent, 3000), Step::Capped);
        assert_eq!(step(&seen(0, Some(0)), &recent, 3650), Step::Capped);
        assert_eq!(step(&seen(0, Some(0)), &recent, 4300), Step::Replace);
    }

    #[test]
    fn state_round_trips() {
        let mut state = State::default();
        state.tasks.insert("x.md".into(), seen(3, Some(4)));
        state.replaces_s = vec![9];
        assert_eq!(State::from_json(&state.to_json()), state);
    }
}
