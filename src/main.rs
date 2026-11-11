//! `amh`, the agent manager helper: parse the command line, log the action, run it, exit.
// 🧑 "we shall gradually replace parts into Rust rewrites and see if the whole thing still works ... eventually replace the whole thing with Rust"

mod actions;
mod agents;
mod archive;
mod cli;
mod database;
mod decide;
mod fallback;
mod guest;
mod history;
mod jobs;
mod log;
mod mail;
mod mail_routes;
mod omnigent;
mod org;
mod quota;
mod quota_core;
mod quota_observer;
mod requests;
mod settings;
mod store;
mod taskfile;
mod tasks;
mod view;
mod wake;
mod watch;
mod work;
mod zulip;
mod zulip_cleanup;

use std::process::ExitCode;

use clap::{ArgAction, ArgMatches, Command};
use serde_json::{Map, Value};

/// The values that name a task file.
const TASK_KEYS: [&str; 5] = ["task", "who", "source", "to", "task_file"];

/// Turn the chosen action and its values into one JSON object: `group`, `action`, and each value under its name.
fn parsed(command: &Command, matches: &ArgMatches) -> Value {
    let (group, group_matches) = matches.subcommand().expect("clap requires a group");
    let top = command
        .find_subcommand(group)
        .expect("clap matched this command");
    // A short command for the human has no actions under it; it is its own action in the group `short`.
    let (group, action, values, leaf) = match group_matches.subcommand() {
        Some((action, values)) => {
            let leaf = top
                .find_subcommand(action)
                .expect("clap matched this action");
            (group, action, values, leaf)
        }
        None => ("short", group, group_matches, top),
    };
    let mut object = Map::from_iter([
        ("group".to_owned(), Value::from(group)),
        ("action".to_owned(), Value::from(action)),
    ]);
    for arg in leaf.get_arguments() {
        let id = arg.get_id().as_str();
        let value = match arg.get_action() {
            ArgAction::SetTrue => Value::from(values.get_flag(id)),
            ArgAction::Append => values
                .get_many::<String>(id)
                .map(|many| many.cloned().collect::<Vec<_>>())
                .unwrap_or_default()
                .into(),
            ArgAction::Set => values.get_one::<String>(id).cloned().into(),
            _ => continue,
        };
        // 🧑 "Make it lenient and allow both having and not having .md and allow full paths."
        let named = |given: &Value| {
            given
                .as_str()
                .map_or(Value::Null, |given| work::task_name(given).into())
        };
        let value = match (TASK_KEYS.contains(&id), &value) {
            (true, Value::String(_)) => named(&value),
            (true, Value::Array(many)) => many.iter().map(named).collect(),
            _ => value,
        };
        object.insert(id.to_owned(), value);
    }
    if let Ok(Some(task)) = values.try_get_one::<String>("task_file") {
        object.insert("task_file".to_owned(), work::task_name(task).into());
    }
    Value::Object(object)
}

fn main() -> ExitCode {
    let command = cli::command();
    let values = parsed(&command, &command.clone().get_matches());
    let line = values.to_string();
    // These two do not end by themselves, so their start is logged too.
    if matches!(
        (values["group"].as_str(), values["action"].as_str()),
        (Some("agent"), Some("view"))
            | (Some("manager"), Some("watch"))
            | (Some("quota"), Some("serve"))
    ) {
        log::record(&line, "started");
    }
    // 🤖 A start that must wait for a slot (2 per minute on the machine) blocked its caller for many minutes during restart storms, and callers' tool timeouts killed them (exit 144). Such a start now goes on in the background and the caller gets an answer at once.
    if matches!(
        (values["group"].as_str(), values["action"].as_str()),
        (Some("task"), Some("start")) | (Some("agent"), Some("replace")) | (Some("manager"), Some("rotate"))
    ) && std::env::var_os("AMH_DETACHED").is_none()
    {
        let ahead = omnigent::start_backlog();
        if ahead > 0 {
            match detach() {
                Ok(out) => {
                    println!(
                        "queued: {ahead} start(s) ahead (2 agent starts per minute on the machine); it goes on in the background, output in {out}; check later with `amh agent status`"
                    );
                    log::record(&line, "detached");
                    return ExitCode::SUCCESS;
                }
                Err(error) => eprintln!("amh: cannot run the start in the background ({error}); waiting here"),
            }
        }
    }
    let code = actions::run(&values).unwrap_or_else(|message| {
        eprintln!("{message}");
        1
    });
    log::record(&line, &format!("exit={code}"));
    ExitCode::from(code)
}

/// Run this same command again in its own session, so the caller's exit or kill does not stop it; returns the output file.
fn detach() -> Result<String, String> {
    let out = std::env::temp_dir().join(format!("amh-start-{}.txt", std::process::id()));
    let file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new("setsid")
        .arg(exe)
        .args(std::env::args_os().skip(1))
        .env("AMH_DETACHED", "1")
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone().map_err(|e| e.to_string())?)
        .stderr(file)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(out.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tell_accepts_task_file_before_and_after_recipient() -> Result<(), clap::Error> {
        for args in [
            vec!["amh", "--task-file", "a.md", "tell", "manager", "hello"],
            vec!["amh", "tell", "--task-file", "a.md", "manager", "hello"],
            vec!["amh", "tell", "manager", "--task-file", "a.md", "hello"],
            vec![
                "amh",
                "tell",
                "--task-file",
                "a.md",
                "human",
                "--subject",
                "hello",
                "body",
            ],
            vec![
                "amh",
                "tell",
                "--task-file",
                "a.md",
                "agent",
                "b.md",
                "--file",
                "/tmp/message",
            ],
        ] {
            let command = cli::command();
            let matches = command.clone().try_get_matches_from(args)?;
            let values = parsed(&command, &matches);
            assert_eq!(values["task_file"], "a.md");
        }
        Ok(())
    }
}
