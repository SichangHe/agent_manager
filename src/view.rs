//! The human's command-line view of an agent: its live terminal, opened in the caller's terminal.
// 🧑 "I do want a CLI view for Omnigent tho"
// 🧑 "I don't want to use a browser give me a terminal option. Ideally, it would be something like I can type a command within tmux and connect to. And then I can disconnect while it still runs."

use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::tasks::Task;
use crate::{omnigent, settings};

/// Replace this process with a terminal attached to the agent named by a task file or an address; only returns on failure.
pub fn open(who: &str, read_only: bool) -> String {
    match terminal(who) {
        Ok((socket, target)) => {
            let mut attach = Command::new("tmux");
            // The agent's terminal lives on its own tmux server, so the caller's `TMUX` must not apply.
            attach
                .env_remove("TMUX")
                .args(["-S", &socket, "attach-session", "-t", &target]);
            if read_only {
                attach.arg("-r");
            }
            format!("amh: cannot start tmux: {}", attach.exec())
        }
        Err(message) => format!("amh: {message}"),
    }
}

/// Find the tmux socket and target of the agent's running terminal.
fn terminal(who: &str) -> Result<(String, String), String> {
    let values = settings::load()?;
    let address = if who.ends_with(".md") {
        let text = fs::read_to_string(settings::work_logs(&values).join(who))
            .map_err(|_| format!("no task file {who} in the work-log root"))?;
        Task::parse(who, &text)?.address().to_owned()
    } else {
        who.to_owned()
    };
    let session = address
        .strip_prefix(omnigent::PREFIX)
        .ok_or_else(|| format!("{address} is not an Omnigent session"))?;
    let parked = format!(
        "{address} has no running terminal; Omnigent parks idle agents. Message it to wake it (`amh tell agent`), then view it again"
    );
    let resources = omnigent::api(
        &values,
        "GET",
        &format!("/v1/sessions/{session}/resources"),
        None,
    )
    .map_err(|error| format!("{parked} ({error})"))?;
    let running = resources["data"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|resource| resource["type"] == "terminal" && resource["metadata"]["running"] == true);
    let text = |value: &serde_json::Value| value.as_str().map(str::to_owned);
    running
        .and_then(|resource| {
            Some((
                text(&resource["metadata"]["tmux_socket"])?,
                text(&resource["metadata"]["tmux_target"])?,
            ))
        })
        .ok_or(parked)
}
