//! `amh`, the agent manager helper: parse the command line, log the action, run it, exit.
// 🧑 "we shall gradually replace parts into Rust rewrites and see if the whole thing still works ... eventually replace the whole thing with Rust"

mod actions;
mod agents;
mod cli;
mod decide;
mod guest;
mod log;
mod mail;
mod omnigent;
mod settings;
mod store;
mod taskfile;
mod tasks;
mod view;
mod watch;
mod work;

use std::process::ExitCode;

use clap::{ArgAction, ArgMatches, Command};
use serde_json::{Map, Value};

/// Turn the chosen action and its values into one JSON object: `group`, `action`, and each value under its name.
fn parsed(command: &Command, matches: &ArgMatches) -> Value {
    let (group, group_matches) = matches.subcommand().expect("clap requires a group");
    let (action, values) = group_matches.subcommand().expect("clap requires an action");
    let leaf = command
        .find_subcommand(group)
        .and_then(|found| found.find_subcommand(action))
        .expect("clap matched this action");
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
        object.insert(id.to_owned(), value);
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
