//! The action log: one line for every `amh` action, saying when, who, what, and how it ended.
// 🧑 "We should also log all the events (resolution requested, etc.) and who did those (agent ID or tmux window)."
// 🧑 "We need auditability and observability instead so agents can diagnose when bugs do occur"

use std::fs::{self, OpenOptions};
use std::io::Write;

use crate::settings;

/// Who is calling: the agent's Omnigent session, else its tmux pane, else the login name.
pub fn caller() -> String {
    let set = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    if let Some(session) = set("OMNIGENT_RUNNER_PRIMARY_SESSION_ID") {
        format!("omnigent://{session}")
    } else if let Some(task) = task_caller() {
        task
    } else if let Some(pane) = set("TMUX_PANE") {
        format!("tmux-pane:{pane}")
    } else {
        set("USER").map_or_else(
            || format!("terminal:{}", std::process::id()),
            |user| format!("user:{user}"),
        )
    }
}

/// A caller that explicitly supplies its launch identity when the runner environment is absent.
fn task_caller() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let named = args
        .windows(2)
        .find(|pair| pair[0] == "--task-file")
        .map(|pair| pair[1].clone())
        .or_else(|| {
            args.iter()
                .find_map(|arg| arg.strip_prefix("--task-file=").map(str::to_owned))
        })
        .or_else(|| std::env::var("OMO_AGENT_TASK_FILE").ok())?;
    let values = settings::load().ok()?;
    let name = crate::work::task_name(&named);
    let text = fs::read_to_string(settings::work_logs(&values).join(&name)).ok()?;
    let task = crate::tasks::Task::parse(&name, &text).ok()?;
    (!task.address().is_empty()).then(|| task.address().to_owned())
}

/// Append a structured domain event; unlike diagnostic action logging, report write failure.
pub fn event(
    folder: &std::path::Path,
    kind: &str,
    subject: &str,
    data: serde_json::Value,
) -> Result<(), String> {
    if folder.join("amh.sqlite3").exists() {
        return crate::database::Database::open(&folder.join("amh.sqlite3"))?
            .commit(&[], &[event_value(kind, subject, data)]);
    }
    fs::create_dir_all(folder).map_err(|e| format!("amh: cannot create event directory: {e}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join("amh-events.jsonl"))
        .map_err(|e| format!("amh: cannot open event log: {e}"))?;
    writeln!(file, "{}", serde_json::json!({"time": chrono::Local::now().to_rfc3339(), "actor": caller(), "kind": kind, "subject": subject, "data": data}))
        .map_err(|e| format!("amh: cannot append event: {e}"))
}

pub fn event_value(kind: &str, subject: &str, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"time": chrono::Local::now().to_rfc3339(), "actor": caller(), "kind": kind, "subject": subject, "data": data})
}

/// Append one line to `amh-actions.log` in the state folder; a failure to log never fails the action.
pub fn record(parsed: &str, outcome: &str) {
    let folder = settings::state_dir();
    let _ = fs::create_dir_all(&folder);
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join("amh-actions.log"))
    {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(log, "{now} {} {outcome} {parsed}", caller());
    }
}
