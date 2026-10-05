//! Every action end to end: find the task files, change them under the lock, reach the agents and the mail, print the result.
//!
//! The decisions and texts are the pure functions of `tasks`, `work`, and `mail`; this module does the input and output around them.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::agents::{self, NewTask};
use crate::mail::{self, Attached, Draft};
use crate::store::Store;
use crate::tasks::{self, Task};
use crate::{guest, omnigent, settings, view, watch, work};

const UNIT: &str = "amh-watch.service";

type Settings = BTreeMap<String, String>;

/// Print to standard output; a reader that went away, as in `amh agent list | head`, does not fail the action.
fn say(text: &str) {
    let _ = io::stdout().write_all(text.as_bytes());
}

/// Read a text file as Python's text mode did, so the texts sent on stay the same.
fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(path)
        .map(|text| work::unix_lines(&text))
        .map_err(|error| format!("amh: cannot read {path}: {error}"))
}

/// Run `systemctl --user` with the caller's terminal and return its exit status.
fn systemctl(args: &[&str]) -> Result<u8, String> {
    let status = Command::new("systemctl").arg("--user").args(args).status();
    let status = status.map_err(|error| format!("amh: cannot run systemctl: {error}"))?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

/// Email the human about their items on a task's list.
///
/// A task's earlier notice that the human has not read yet is folded into the new one and trashed, so at most one notice per task waits unread.
// 🧑 "At all time, try to keep 1 unread email for the human from yourself."
fn notify(settings: &Settings, task: &Task, change: &str, items: &[String]) -> Result<(), String> {
    let Some(mut body) = tasks::human_notice(change, items) else {
        return Ok(());
    };
    let state_dir = settings::state_dir();
    let record = state_dir.join("amh-notices.json");
    let mut notices: BTreeMap<String, (String, String)> = fs::read_to_string(&record)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let unread_ids = |trash: &[String]| -> Result<Vec<String>, String> {
        let unread = mail::unread(settings, task.tag(), trash)?;
        Ok(unread.into_iter().map(|header| header.message_id).collect())
    };
    let earlier = notices
        .get(task.tag())
        .filter(|(id, _)| unread_ids(&[]).is_ok_and(|ids| ids.contains(id)))
        .cloned();
    if let Some((_, text)) = &earlier {
        body = format!("{text}{body}");
    }
    let sender = format!("{} {}", mail::NOTICE_SENDER, task.address());
    let sent = mail::send(settings, &state_dir, task.tag(), "", &body, &sender, &[])
        .map_err(|error| format!("amh: {error}"))?;
    if let Some((id, _)) = earlier {
        // The new notice is sent; failing to trash the old one only leaves one more unread email.
        let _ = unread_ids(&[id]);
    }
    notices.insert(task.tag().to_owned(), (sent, body));
    let _ = fs::write(&record, serde_json::to_string(&notices).unwrap_or_default());
    Ok(())
}

/// Email the human on the caller's thread, then say which earlier emails of the caller the human has not read.
fn tell_human(
    settings: &Settings,
    task: &Task,
    subject: &str,
    body: &str,
    replaces: &[String],
    attachments: &[PathBuf],
) -> Result<(), String> {
    let sent = mail::send(
        settings,
        &settings::state_dir(),
        task.tag(),
        subject,
        body,
        task.address(),
        attachments,
    );
    let message_id = sent.map_err(|error| format!("amh: {error}"))?;
    say(&format!("Email sent.\nMessage-ID: {message_id}\n"));
    say(&match mail::unread(settings, task.tag(), replaces) {
        Ok(unread) => mail::replace_reminder(&message_id, &unread),
        Err(error) => mail::unread_check_failed(&error),
    });
    Ok(())
}

/// Email an answer to the guest alone, on the thread of the stored guest email `mail_name`; returns what to print.
fn tell_guest(
    settings: &BTreeMap<String, String>,
    store: &Store,
    mail_name: &str,
    file: &str,
    images: &[&str],
) -> Result<String, String> {
    let read = |path: &Path| {
        fs::read_to_string(path)
            .map_err(|error| format!("amh: cannot read {}: {error}", path.display()))
    };
    let body = read(Path::new(file))?;
    let name = Path::new(mail_name).file_name().unwrap_or_default();
    let stored = read(&store.root.join(guest::mail_dir(settings)).join(name))?;
    let plan = guest::reply_plan(&stored, &body)?;
    let mut attachments = Vec::new();
    for image in images {
        let path = Path::new(image);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        attachments.push(Attached {
            name: name.into_owned(),
            kind: guest::image_kind(image)?.to_owned(),
            data: fs::read(path).map_err(|error| format!("amh: cannot read {image}: {error}"))?,
        });
    }
    let to = settings::get(settings, "guest.address");
    let draft = Draft {
        from: settings::get(settings, "mail.agent_address"),
        to,
        subject: &plan.subject,
        body: &body,
        reply_to: &plan.reply_to,
        references: &plan.references,
        sender: "",
        attachments: &attachments,
    };
    let message_id = mail::new_id();
    let raw = mail::compose(&draft, &message_id).map_err(|error| format!("amh: {error}"))?;
    mail::deliver(settings, to, &raw).map_err(|error| format!("amh: {error}"))?;
    Ok(format!(
        "Email sent to the guest.\nMessage-ID: {message_id}\n"
    ))
}

/// Run one action from the parsed command line: a JSON object of `group`, `action`, and the action's values. Returns the exit status, or the refusal to print.
pub fn run(values: &Value) -> Result<u8, String> {
    let text = |key: &str| values[key].as_str().filter(|value| !value.is_empty());
    let need = |key: &str| values[key].as_str().unwrap_or_default();
    let flag = |key: &str| values[key].as_bool().unwrap_or(false);
    let many = |key: &str| -> Vec<&str> {
        let given = values[key].as_array().into_iter().flatten();
        given.filter_map(Value::as_str).collect()
    };
    let action = (need("group"), need("action"));
    match action {
        ("agent", "view") => return Err(view::open(need("who"), flag("read_only"))),
        ("manager", "watch") => return Ok(watch::run()),
        _ => {}
    }
    let settings = settings::load().map_err(|message| format!("amh: {message}"))?;
    let store = Store::new(&settings);
    let failed = |error: String| format!("amh: {error}");
    let output = match action {
        ("tell", "manager") => {
            let task = store.own(text("task_file"))?;
            let state = match (flag("blocked"), flag("done")) {
                (true, _) => "blocked",
                (_, true) => "done",
                _ => "",
            };
            let (manager, message) = work::manager_message(&task, need("text"), state)?;
            omnigent::send(&settings, manager, &message).map_err(failed)?;
            "message sent to your manager; do not send it again\n".to_owned()
        }
        ("tell", "human") => {
            let body = match text("file") {
                Some(file) => read(file)?,
                None => io::read_to_string(io::stdin())
                    .map(|body| work::unix_lines(&body))
                    .map_err(|error| format!("amh: cannot read standard input: {error}"))?,
            };
            let task = store.own(text("task_file"))?;
            let replaces: Vec<String> = many("replaces").into_iter().map(str::to_owned).collect();
            let attach: Vec<PathBuf> = many("attach").into_iter().map(PathBuf::from).collect();
            tell_human(&settings, &task, need("subject"), &body, &replaces, &attach)?;
            String::new()
        }
        ("tell", "agent") => {
            let message = match (text("text"), text("file")) {
                (Some(text), None) => text.to_owned(),
                (None, Some(file)) => read(file)?,
                _ => return Err("amh: give the message as TEXT or as --file, not both".to_owned()),
            };
            let to = agents::address_of(&store, need("who"))?;
            let message = work::envelope(&omnigent::own_address()?, &message);
            omnigent::send(&settings, &to, &message).map_err(failed)?;
            String::new()
        }
        ("tell", "guest") => tell_guest(
            &settings,
            &store,
            need("mail"),
            need("file"),
            &many("image"),
        )?,
        ("todo", "list") => store.own(text("task_file"))?.items.join("\n") + "\n",
        ("todo", "add") | ("task", "give") => {
            let lock = store.lock()?;
            let mut task = match action.0 {
                "task" => store.load(need("task"))?,
                _ => store.own(text("task_file"))?,
            };
            let new: Vec<String> = tasks::add_item(&mut task, need("item"), flag("from_human"))?
                .into_iter()
                .collect();
            if !new.is_empty() {
                store.save(&task)?;
            }
            drop(lock);
            notify(&settings, &task, "created", &new)?;
            let to = if action.0 == "task" {
                format!(" to {}", task.name)
            } else {
                String::new()
            };
            format!("added {} open item(s){to}\n", new.len())
        }
        ("todo", "done") => {
            let lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            tasks::remove_item(&mut task, need("item"), need("evidence"))?;
            store.save(&task)?;
            drop(lock);
            notify(&settings, &task, "deleted", &[need("item").to_owned()])?;
            "removed 1 open item\n".to_owned()
        }
        ("todo", "replace") => {
            let _lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            tasks::replace_item(&mut task, need("old"), need("new"))?;
            store.save(&task)?;
            "replaced 1 open item\n".to_owned()
        }
        ("task", "start") => {
            if text("email").is_some() != text("lines").is_some() {
                return Err("amh: --email and --lines go together".to_owned());
            }
            let goal = read(need("prompt"))?;
            let manager = match text("manager") {
                Some(manager) => manager.to_owned(),
                None => omnigent::own_address()?,
            };
            let new = NewTask {
                name: need("task"),
                workdir: need("dir"),
                goal: &goal,
                manager: &manager,
                tool: text("tool"),
                model: text("model"),
                effort: text("effort"),
                as_manager: flag("as_manager"),
                email: text("email"),
                lines: text("lines"),
            };
            agents::start_task(&settings, &store, &new)? + "\n"
        }
        ("task", "show") => {
            let shown: Result<Vec<String>, String> = many("task")
                .into_iter()
                .map(|name| Ok(tasks::show(&store.load(name)?)))
                .collect();
            shown?.concat()
        }
        ("task", "close") => {
            let name = match text("task") {
                Some(name) => name.to_owned(),
                None => store.own(text("task_file"))?.name,
            };
            let at = agents::close_task(
                &settings,
                &store,
                &name,
                flag("agent_gone"),
                !flag("no_email"),
                text("task").is_none(),
            )?;
            format!("closed {name} at {at}\n")
        }
        ("task", "note") => {
            let _lock = store.lock()?;
            let mut task = store.load(need("task"))?;
            tasks::add_note(&mut task, need("text"));
            store.save(&task)?;
            String::new()
        }
        ("task", "move") => {
            let _lock = store.lock()?;
            let (mut source, mut to) = (store.load(need("source"))?, store.load(need("to"))?);
            tasks::move_item(&mut source, &mut to, need("item"))?;
            store.save(&to)?;
            store.save(&source)?;
            String::new()
        }
        ("task", "status") => {
            let _lock = store.lock()?;
            let mut task = store.load(need("task"))?;
            tasks::set_status(&mut task, need("status"), text("on"))?;
            store.save(&task)?;
            store.place(&task)?;
            String::new()
        }
        ("task", "check") => {
            let findings = tasks::check(&store.list()?, &store.active()?);
            say(&findings
                .iter()
                .map(|finding| format!("{finding}\n"))
                .collect::<String>());
            return Ok(u8::from(!findings.is_empty()));
        }
        ("agent", "problems") => {
            let found = agents::problems(&settings, &store)?;
            for (task, problem, evidence) in &found {
                say(&(work::problem_line(task, *problem, evidence) + "\n"));
            }
            return Ok(if found.is_empty() { 0 } else { 3 });
        }
        ("agent", "list") => {
            for task in store.active()? {
                let (state, evidence) = match task.address() {
                    "retired" => ("retired", String::new()),
                    at => omnigent::status(&settings, at),
                };
                say(&(work::listed(&task, state, &evidence) + "\n"));
            }
            String::new()
        }
        ("agent", "tree") => {
            work::tree(&store.active()?, settings::get(&settings, "main_manager")) + "\n"
        }
        ("agent", "status") => {
            let at = agents::address_of(&store, need("who"))?;
            let (state, evidence) = omnigent::status(&settings, &at);
            format!("{state}: {evidence}\n")
        }
        ("agent", "stop") => {
            let at = agents::address_of(&store, need("who"))?;
            omnigent::stop(&settings, &at).map_err(failed)?;
            String::new()
        }
        ("agent", "compact") => {
            let at = agents::address_of(&store, need("who"))?;
            omnigent::send(&settings, &at, "/compact").map_err(failed)?;
            String::new()
        }
        ("mail", "unread") => {
            let task = store.own(text("task_file"))?;
            let unread = mail::unread(&settings, task.tag(), &[]).map_err(failed)?;
            let theirs = unread.iter().filter(|header| !header.is_notice());
            theirs.map(|header| header.listed() + "\n").collect()
        }
        ("manager", "watchers") => {
            let restarted = systemctl(&["restart", UNIT])?;
            return match restarted {
                0 => systemctl(&["--no-pager", "--lines=3", "status", UNIT]),
                failed => Ok(failed),
            };
        }
        ("manager", "rotate") => {
            let (tool, model, effort) = (text("tool"), text("model"), text("effort"));
            agents::rotate(&settings, &store, need("who"), tool, model, effort)? + "\n"
        }
        ("manager", "worktree") => {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            let mut report = String::new();
            for repo in [home.join(".config"), store.root.clone()] {
                let git = Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["status", "--porcelain"])
                    .output()
                    .map_err(|error| format!("amh: cannot run git: {error}"))?;
                let dirty = String::from_utf8_lossy(&git.stdout);
                report += &work::worktree(&repo.display().to_string(), &dirty);
            }
            report
        }
        (group, action) => return Err(format!("amh: no action `{group} {action}`")),
    };
    say(&output);
    Ok(0)
}
