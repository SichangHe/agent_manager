//! `amh`, the agent manager helper: parse the command line, log the action, run it, exit.
// 🧑 "we shall gradually replace parts into Rust rewrites and see if the whole thing still works ... eventually replace the whole thing with Rust"

// Inside amhd, standard error belongs to the request being served; `eprintln!` everywhere goes through it.
macro_rules! eprintln {
    ($($arg:tt)*) => {
        $crate::request::err(&format!($($arg)*))
    };
}

mod actions;
mod agents;
mod archive;
mod cli;
mod daemon;
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
mod quota_apikey;
mod quota_core;
mod quota_observer;
mod quota_pace;
mod remote;
mod release;
mod replies;
mod request;
mod requests;
mod revive;
mod settings;
mod store;
mod taskfile;
mod tasks;
mod urgent;
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
/// Send this command to amhd and print its answer; `None` when it should run here (no daemon, opted out, or a command that needs this terminal).
fn through_daemon(values: &Value) -> Option<ExitCode> {
    use std::io::{BufRead, BufReader, Write};
    // Clients use the daemon with `AMH_CLIENT=1`, or after the cut-over, which leaves `amhd.client` in the state dir; `AMH_CLIENT=0` runs here regardless.
    match std::env::var("AMH_CLIENT").as_deref() {
        Ok("1") => {}
        Ok("0") => return None,
        _ if !settings::state_dir().join(daemon::CLIENT_MARK).exists() => return None,
        _ => {}
    }
    let local = matches!(
        (values["group"].as_str(), values["action"].as_str()),
        (
            Some("short"),
            Some("daemon" | "edit" | "view" | "stop" | "release")
        ) | (Some("manager"), Some("watch" | "watchers"))
            | (Some("agent"), Some("view"))
            | (Some("quota"), Some("serve"))
    ) || (
        // Commands that may ask the human y/N at a terminal run here, where the terminal is.
        std::io::IsTerminal::is_terminal(&std::io::stdin())
            && matches!(
                (values["group"].as_str(), values["action"].as_str()),
                (Some("task"), Some("close" | "start" | "reopen"))
            )
    );
    // A message read from standard input stays local, so the daemon never waits on a terminal.
    let stdin_body =
        values["group"] == "tell" && values["text"].is_null() && values["file"].is_null();
    if local || stdin_body {
        return None;
    }
    let socket = daemon::socket_path(&settings::state_dir());
    // Without the daemon running, `AMH_CLIENT` unset and no marker ran here above; with them, wait out a restart (about 30 s) instead of writing beside the daemon.
    let mut stream = None;
    for wait_ms in [0, 100, 200, 500, 1000, 2000, 4000, 8000, 15000] {
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        if let Ok(connected) = std::os::unix::net::UnixStream::connect(&socket) {
            stream = Some(connected);
            break;
        }
    }
    let Some(mut stream) = stream else {
        eprintln!(
            "amh: amhd is not answering at {}; nothing ran. To run without it: AMH_CLIENT=0 amh ..., or remove {} (rollback)",
            socket.display(),
            daemon::CLIENT_MARK
        );
        return Some(ExitCode::from(75));
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    // Paths are the caller's: make them absolute, since the daemon has its own working directory.
    const PATH_FLAGS: [&str; 6] = [
        "--file", "--attach", "--prompt", "--dir", "--image", "--repo",
    ];
    let absolute = |path: &str| match path {
        "-" => path.to_owned(),
        _ if path.starts_with('/') => path.to_owned(),
        _ => cwd.join(path).display().to_string(),
    };
    let mut argv: Vec<String> = Vec::new();
    let mut path_next = false;
    for arg in std::env::args() {
        let given = match arg.split_once('=') {
            Some((flag, path)) if PATH_FLAGS.contains(&flag) => {
                format!("{flag}={}", absolute(path))
            }
            _ if path_next => absolute(&arg),
            _ => arg.clone(),
        };
        path_next = PATH_FLAGS.contains(&arg.as_str());
        argv.push(given);
    }
    let mut env = serde_json::Map::new();
    for name in request::ALLOWED {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.to_owned(), value.into());
        }
    }
    if !env.contains_key("AMH_HUMAN_STARTED") {
        let human = if actions::human_terminal() { "1" } else { "0" };
        env.insert("AMH_HUMAN_STARTED".to_owned(), human.into());
    }
    // The daemon names an unidentified caller by the client's pid, not its own.
    env.insert(
        "AMH_CLIENT_PID".to_owned(),
        std::process::id().to_string().into(),
    );
    let id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let request = serde_json::json!({"argv": argv, "env": env, "cwd": cwd, "id": id});
    // Once connected, never fall back to running here: the daemon may have applied the request. A retry with the same id gets the stored outcome.
    let mut reply = None;
    // About 30 s in all, longer than a daemon restart.
    for wait_ms in [0, 200, 500, 1000, 2000, 4000, 8000, 15000] {
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        if wait_ms > 0 {
            match std::os::unix::net::UnixStream::connect(&socket) {
                Ok(again) => stream = again,
                Err(_) => continue,
            }
        }
        let mut line = String::new();
        let sent = stream
            .write_all((request.to_string() + "\n").as_bytes())
            .and_then(|()| BufReader::new(&stream).read_line(&mut line).map(|_| ()));
        if let Ok(Ok(answer)) = sent.map(|()| serde_json::from_str::<Value>(&line)) {
            reply = Some(answer);
            break;
        }
    }
    let Some(reply) = reply else {
        eprintln!("amh: amhd did not answer; the request may or may not have run");
        return Some(ExitCode::from(75));
    };
    print!("{}", reply["out"].as_str().unwrap_or_default());
    eprint!("{}", reply["err"].as_str().unwrap_or_default());
    Some(ExitCode::from(reply["code"].as_u64().unwrap_or(1) as u8))
}

pub(crate) fn parsed(command: &Command, matches: &ArgMatches) -> Value {
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
    // The detached child launches agents; they must not inherit the marker, or their own starts would block again.
    let detached = std::env::var_os("AMH_DETACHED").is_some();
    // SAFETY: single-threaded at this point.
    unsafe { std::env::remove_var("AMH_DETACHED") };
    let command = cli::command();
    let values = parsed(&command, &command.clone().get_matches());
    // Stage 1 of the rework: with `AMH_CLIENT=1`, commands go to a running amhd instead of opening the store here.
    if let Some(code) = through_daemon(&values) {
        return code;
    }
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
        (Some("task"), Some("start"))
            | (Some("agent"), Some("replace"))
            | (Some("manager"), Some("rotate"))
    ) && !detached
    {
        let ahead = omnigent::start_backlog();
        if ahead > 0 {
            match detach() {
                Ok(out) => {
                    println!(
                        "queued: {ahead} start(s) ahead (machine-wide limit: launch.starts_per_minute in config.toml, default 2); it goes on in the background, output in {out}; check later with `amh agent status`"
                    );
                    log::record(&line, "detached");
                    return ExitCode::SUCCESS;
                }
                Err(error) => {
                    eprintln!("amh: cannot run the start in the background ({error}); waiting here")
                }
            }
        }
    }
    let began = std::time::Instant::now();
    let code = actions::run(&values).unwrap_or_else(|message| {
        eprintln!("{message}");
        1
    });
    // 🧑 "Task file rebuild should be lazy. Locking should be in us not ms": files of the tasks this command changed are written now, after the lock is released.
    if store::LOCKED.load(std::sync::atomic::Ordering::Relaxed)
        && let Err(error) =
            settings::load().and_then(|settings| store::Store::new(&settings).refresh_exports())
    {
        eprintln!("amh: task files not refreshed yet (the watcher will): {error}");
    }
    let us = |us: &std::sync::atomic::AtomicU64| us.load(std::sync::atomic::Ordering::Relaxed);
    log::record(
        &line,
        &format!(
            "exit={code} duration_ms={} lock_wait_us={} lock_hold_us={} build={}",
            began.elapsed().as_millis(),
            us(&store::LOCK_WAIT_US),
            us(&store::LOCK_HOLD_US),
            env!("AMH_BUILD")
        ),
    );
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
        // The detached start has no terminal; it keeps whether the human ran the command, for the owed first reply.
        .env(
            "AMH_HUMAN_STARTED",
            if actions::human_terminal() { "1" } else { "0" },
        )
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
