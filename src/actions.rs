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
use crate::{decide, guest, omnigent, settings, taskfile, view, watch, work};

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

const BATCH_HINT: &str = "This emailed the human by itself. Next time batch: give several items in one call, and when you also have something to tell the human, send it all in one email with `amh tell human --add ITEM --done ITEM`.\n";

/// Email the human at once about the changes one call made to their items on a task; returns the hint to print when an email went out.
// 🧑 “Pending items originated from the human need emails, ones from agents do not.”
fn report_items(
    settings: &Settings,
    store: &Store,
    task: &Task,
    created: &[String],
    deleted: &[String],
) -> Result<&'static str, String> {
    let text = tasks::changes_text(created, deleted);
    if text.is_empty() {
        return Ok("");
    }
    let sender = format!("{} {}", mail::NOTICE_SENDER, task.address());
    mail::send(
        settings,
        &store.state_dir,
        task.tag(),
        "",
        &text,
        &sender,
        &[],
    )
    .map_err(|error| format!("amh: the items are saved, but {error}"))?;
    Ok(BATCH_HINT)
}

/// Email the human on the caller's thread, then say which earlier emails of the caller the human has not read.
// 🧑 "Make the emailing script combine emails into one. ... Merge those, which also requires caller interface to change and encourage batched calling of amh tell human"
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
    if mail::is_queued(&settings::state_dir(), &message_id) {
        // The emails it replaces stay until it is really sent, so the human loses nothing meanwhile.
        say(&format!(
            "Email NOT sent yet: Gmail refused it, see the line above. It is queued and the watcher will send it when Gmail allows. Do not send it again.\nMessage-ID: {message_id}\n"
        ));
        return Ok(());
    }
    say(&format!("Email sent.\nMessage-ID: {message_id}\n"));
    say(&match mail::unread(settings, task.tag(), replaces) {
        Ok(unread) => mail::replace_reminder(&message_id, &unread),
        Err(error) => mail::unread_check_failed(&error),
    });
    Ok(())
}

/// Let the human add to a task file in their editor and queue what they added for the task's agent, as a stored email is queued.
fn edit_task(settings: &Settings, store: &Store, name: &str) -> Result<String, String> {
    let before = store.load(name)?.render();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let copy = std::env::temp_dir().join(format!("amh-edit-{}-{name}", now.as_nanos()));
    fs::write(&copy, &before)
        .map_err(|error| format!("amh: cannot write {}: {error}", copy.display()))?;
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|set| !set.is_empty()))
        .unwrap_or_else(|| "vi".to_owned());
    let added = loop {
        let command = format!("{editor} \"$1\"");
        let edited = Command::new("sh")
            .args(["-c", &command, "sh"])
            .arg(&copy)
            .status();
        if !edited
            .map_err(|error| format!("amh: cannot run {editor}: {error}"))?
            .success()
        {
            let _ = fs::remove_file(&copy);
            return Err(format!(
                "amh: {editor} did not exit cleanly; nothing was taken"
            ));
        }
        let after = fs::read_to_string(&copy)
            .map_err(|error| format!("amh: cannot read {}: {error}", copy.display()))?;
        match work::appended(&before, &after) {
            Ok(added) => break added,
            Err(problem) => {
                say(&format!(
                    "amh: {problem}. Reopen the copy to fix it? [Y/n] "
                ));
                let mut answer = String::new();
                let _ = io::stdin().read_line(&mut answer);
                if answer.trim().to_lowercase().starts_with('n') {
                    let _ = fs::remove_file(&copy);
                    return Err("amh: nothing was taken".to_owned());
                }
            }
        }
    };
    let _ = fs::remove_file(&copy);
    let Some(added) = added else {
        return Ok("nothing was added\n".to_owned());
    };
    let prefix = match settings::get(settings, "mail.stored_prefix") {
        "" => "mail",
        prefix => prefix,
    };
    let mail_name = format!("{prefix}-edit-{}.txt", now.as_secs());
    let tag = name.strip_suffix(".md").unwrap_or(name);
    let _lock = store.lock()?;
    let _ = fs::create_dir_all(store.root.join("manager_mail"));
    let stored = store.root.join("manager_mail").join(&mail_name);
    fs::write(
        &stored,
        decide::stored_mail(
            &format!("[{tag}] added to the task file in the editor"),
            &added,
        ),
    )
    .map_err(|error| format!("amh: cannot store what you added: {error}"))?;
    // The file may have changed while the editor was open, so the block is appended to its current text.
    let current = fs::read_to_string(store.root.join(name))
        .map_err(|error| format!("amh: cannot read {name}: {error}"))?;
    taskfile::write(
        &store.root.join(name),
        &decide::queued(&current, &mail_name),
    )
    .map_err(|error| format!("amh: cannot write {name}: {error}"))?;
    Ok(format!(
        "queued for the agent of {name}; the watcher delivers it within seconds\n"
    ))
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
            let lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            let mut added = Vec::new();
            for item in many("add") {
                added.extend(tasks::add_item(&mut task, item, true)?);
            }
            let mut done = Vec::new();
            for item in many("done") {
                done.push(tasks::remove_item(&mut task, item, "")?);
            }
            if !added.is_empty() || !done.is_empty() {
                store.save(&task)?;
            }
            drop(lock);
            // The item changes made by this same call go out in this email, under the agent's own text.
            let body = tasks::with_changes(&body, &tasks::changes_text(&added, &done));
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
            let mut new: Vec<String> = Vec::new();
            for item in many("item") {
                new.extend(tasks::add_item(&mut task, item, flag("from_human"))?);
            }
            if !new.is_empty() {
                store.save(&task)?;
            }
            drop(lock);
            let hint = report_items(&settings, &store, &task, &new, &[])?;
            let to = if action.0 == "task" {
                format!(" to {}", task.name)
            } else {
                String::new()
            };
            format!("added {} open item(s){to}\n{hint}", new.len())
        }
        ("todo", "done") => {
            let lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            let mut done = Vec::new();
            for item in many("item") {
                done.push(tasks::remove_item(&mut task, item, need("evidence"))?);
            }
            store.save(&task)?;
            drop(lock);
            let hint = report_items(&settings, &store, &task, &[], &done)?;
            format!("removed {} open item(s)\n{hint}", done.len())
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
        ("task", "edit") => edit_task(&settings, &store, need("task"))?,
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
            let theirs = unread.iter().filter(|header| header.is_own());
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
