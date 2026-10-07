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
    } else if let Some(pane) = set("TMUX_PANE") {
        format!("tmux-pane:{pane}")
    } else {
        format!("user:{}", set("USER").unwrap_or_default())
    }
}

/// Append a structured domain event; unlike diagnostic action logging, report write failure.
pub fn event(
    folder: &std::path::Path,
    kind: &str,
    subject: &str,
    data: serde_json::Value,
) -> Result<(), String> {
    fs::create_dir_all(folder).map_err(|e| format!("amh: cannot create event directory: {e}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join("amh-events.jsonl"))
        .map_err(|e| format!("amh: cannot open event log: {e}"))?;
    writeln!(file, "{}", serde_json::json!({"time": chrono::Local::now().to_rfc3339(), "actor": caller(), "kind": kind, "subject": subject, "data": data}))
        .map_err(|e| format!("amh: cannot append event: {e}"))
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
