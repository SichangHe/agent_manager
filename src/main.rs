//! `amh`, the agent manager helper: parse the command line, log the action, run it, exit.
// 🧑 "we shall gradually replace parts into Rust rewrites and see if the whole thing still works ... eventually replace the whole thing with Rust"

mod actions;
mod agents;
mod archive;
mod cli;
mod database;
mod decide;
mod guest;
mod jobs;
mod log;
mod mail;
mod mail_routes;
mod omnigent;
mod quota;
mod requests;
mod settings;
mod store;
mod taskfile;
mod tasks;
mod view;
mod watch;
mod work;
mod zulip;

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
        (Some("agent"), Some("view")) | (Some("manager"), Some("watch"))
    ) {
        log::record(&line, "started");
    }
    let code = actions::run(&values).unwrap_or_else(|message| {
        eprintln!("{message}");
        1
    });
    log::record(&line, &format!("exit={code}"));
    ExitCode::from(code)
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
