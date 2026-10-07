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
use crate::{decide, guest, omnigent, requests, settings, taskfile, view, watch, work};

fn creator(store: &Store) -> Result<String, String> {
    Ok(store.own(None)?.name)
}

pub fn clear_stopped(store: &Store, address: &str) -> Result<(), String> {
    for mut task in store
        .all()?
        .into_iter()
        .filter(|task| task.address() == address && task.field("agent_stopped") == Some("true"))
    {
        task.fields.retain(|(key, _)| key != "agent_stopped");
        store.save(&task)?;
    }
    Ok(())
}

/// Commit dependency releases and delivery intents before attempting agent messages.
pub fn request_effects(settings: &Settings, store: &Store) -> Result<(), String> {
    let mut database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
    let through = database.latest_event_id()?;
    if database.cursor("request_effects")? < through {
        let mut all = store.all()?;
        let mut resolved = Vec::new();
        for task in &mut all {
            resolved.extend(
                requests::sync(task)?
                    .into_iter()
                    .filter(|r| r.stage == requests::Stage::Resolved)
                    .map(|r| r.id),
            );
        }
        for task in &mut all {
            if requests::unblock(task, &resolved)? {
                store.save(task)?;
            }
        }
        database.set_cursor("request_effects", through)?;
    }
    let mut failed = Vec::new();
    let mut awakened = Vec::new();
    for message in database.pending_messages()? {
        let to = message["target"]
            .as_str()
            .ok_or("amh: notification lacks recipient")?;
        let text = message["text"]
            .as_str()
            .ok_or("amh: notification lacks text")?;
        let source = message["source"]
            .as_str()
            .ok_or("amh: notification lacks owner")?;
        let id = message["id"].as_i64().ok_or("amh: notification lacks ID")?;
        let delivered = agents::address_of(store, to)
            .and_then(|address| omnigent::send(settings, &address, text).map(|()| address));
        if let Err(error) = delivered {
            database.finish_message(id, false, &error)?;
            crate::log::event(
                &store.state_dir,
                "message failed",
                source,
                serde_json::json!({"to": to, "text": text, "error": error}),
            )?;
            failed.push(format!("{source}: {error}"));
            continue;
        }
        if let Ok(address) = delivered {
            awakened.push(address);
        }
        let mut task = store.load(source)?;
        let mut remaining = requests::notifications(&task)?;
        if let Some(at) = remaining
            .iter()
            .position(|queued| queued["to"] == to && queued["text"] == text)
        {
            remaining.remove(at);
        }
        requests::set_notifications(&mut task, remaining);
        let event = crate::log::event_value(
            "message delivered",
            source,
            serde_json::json!({"id":id,"to":to,"text":text}),
        );
        database.complete_message(id, &[(task.name.clone(), task.render())], &[event])?;
        store.refresh_exports()?;
    }
    for address in awakened {
        clear_stopped(store, &address)?;
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "amh: requests saved; notifications retained for retry: {}",
            failed.join("; ")
        ))
    }
}

fn finish_request(task: &mut Task, item: &str, evidence: &str) -> Result<String, String> {
    let request = requests::finish(task, item, evidence)?;
    if request.stage == requests::Stage::Requested {
        requests::queue(
            task,
            &request.creator,
            &format!(
                "Resolution requested for {}: {}\n{}\nDecide with `amh todo approve {}` or `amh todo reject {} --reason REASON`.",
                request.id, request.text, request.evidence, request.id, request.id
            ),
        )?;
        Ok(String::new())
    } else {
        Ok(request.text)
    }
}

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

/// Run the human's editor on `path` and wait for it to exit.
fn edit(path: &Path) -> Result<(), String> {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|set| !set.is_empty()))
        .unwrap_or_else(|| "vi".to_owned());
    let command = format!("{editor} \"$1\"");
    let edited = Command::new("sh")
        .args(["-c", &command, "sh"])
        .arg(path)
        .status();
    match edited {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(format!(
            "amh: {editor} did not exit cleanly; nothing was taken"
        )),
        Err(error) => Err(format!("amh: cannot run {editor}: {error}")),
    }
}

/// A temporary file for the human to edit, named after what it is for.
fn scratch(purpose: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    std::env::temp_dir().join(format!("amh-{}-{purpose}", now.as_nanos()))
}

/// Text the human types in their editor, starting from an empty file; refused when left empty.
fn typed(purpose: &str) -> Result<String, String> {
    let path = scratch(purpose);
    fs::write(&path, "")
        .map_err(|error| format!("amh: cannot write {}: {error}", path.display()))?;
    let text = edit(&path).and_then(|()| {
        fs::read_to_string(&path)
            .map_err(|error| format!("amh: cannot read {}: {error}", path.display()))
    });
    let _ = fs::remove_file(&path);
    match text? {
        text if text.trim().is_empty() => {
            Err("amh: nothing was typed; nothing was done".to_owned())
        }
        text => Ok(text),
    }
}

/// Queue text from the human for a task's agent, as a stored email is queued: it is delivered within seconds like an email from the human.
fn queue_from_human(
    settings: &Settings,
    store: &Store,
    name: &str,
    text: &str,
    how: &str,
) -> Result<String, String> {
    let prefix = match settings::get(settings, "mail.stored_prefix") {
        "" => "mail",
        prefix => prefix,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mail_name = format!("{prefix}-edit-{}.txt", now.as_millis());
    let tag = name.strip_suffix(".md").unwrap_or(name);
    let _lock = store.lock()?;
    let _ = fs::create_dir_all(store.root.join("manager_mail"));
    let stored = store.root.join("manager_mail").join(&mail_name);
    fs::write(
        &stored,
        decide::stored_mail(&format!("[{tag}] {how}"), text),
    )
    .map_err(|error| format!("amh: cannot store what you wrote: {error}"))?;
    // The file may have changed meanwhile, so the block is appended to its current text.
    let current = store.load(name)?.render();
    store.write(name, &decide::queued(&current, &mail_name))?;
    Ok(format!(
        "queued for the agent of {name}; the watcher delivers it within seconds\n"
    ))
}

/// Let the human add to a task file in their editor and queue what they added for the task's agent.
// 🧑 "some sort of shortcut that opens the editor with a copy of the task file and pushes the appended stuff on close, just like what things like cron do"
fn edit_task(settings: &Settings, store: &Store, name: &str) -> Result<String, String> {
    let before = store.load(name)?.render();
    let copy = scratch(name);
    fs::write(&copy, &before)
        .map_err(|error| format!("amh: cannot write {}: {error}", copy.display()))?;
    let added = loop {
        if let Err(error) = edit(&copy) {
            let _ = fs::remove_file(&copy);
            return Err(error);
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
    match added {
        Some(added) => queue_from_human(
            settings,
            store,
            name,
            &added,
            "added to the task file in the editor",
        ),
        None => Ok("nothing was added\n".to_owned()),
    }
}

/// The short commands for the human; each maps onto the long action it stands for or does a little more.
// 🧑 "there needs to be sane short commands for common tasks like spawning agents, without needing tons of flags"
fn short(values: &Value) -> Result<u8, String> {
    let text = |key: &str| values[key].as_str().filter(|value| !value.is_empty());
    let name = text("task").unwrap_or_default();
    let long = |group: &str, action: &str, more: Value| {
        let mut object = more;
        object["group"] = group.into();
        object["action"] = action.into();
        run(&object)
    };
    match values["action"].as_str().unwrap_or_default() {
        "ls" => long("agent", "tree", serde_json::json!({})),
        "view" => long(
            "agent",
            "view",
            serde_json::json!({"who": values["who"], "read_only": values["read_only"]}),
        ),
        "edit" => long("task", "edit", serde_json::json!({"task": name})),
        "say" => {
            let settings = settings::load().map_err(|message| format!("amh: {message}"))?;
            let store = Store::new(&settings);
            store.load(name)?;
            let said = match text("text") {
                Some(said) => format!("{}\n", said.trim_end()),
                None => typed(name)?,
            };
            say(&queue_from_human(
                &settings,
                &store,
                name,
                &said,
                "said with amh say",
            )?);
            Ok(0)
        }
        "new" => {
            let settings = settings::load().map_err(|message| format!("amh: {message}"))?;
            let store = Store::new(&settings);
            let goal = match text("goal") {
                Some(goal) => goal.to_owned(),
                None => typed(name)?,
            };
            let here = std::env::current_dir()
                .map_err(|error| format!("amh: cannot tell the current directory: {error}"))?;
            let new = NewTask {
                name,
                workdir: &here.to_string_lossy(),
                goal: &goal,
                manager: settings::get(&settings, "main_manager"),
                tool: Some(text("tool").unwrap_or("claude")),
                model: None,
                effort: None,
                as_manager: values["as_manager"].as_bool().unwrap_or(false),
                long_running: false,
                email: None,
                lines: None,
            };
            say(&format!(
                "{} started at {}\n",
                name,
                agents::start_task(&settings, &store, &new)?
            ));
            Ok(0)
        }
        "stop" => {
            let settings = settings::load().map_err(|message| format!("amh: {message}"))?;
            let store = Store::new(&settings);
            let found = store.load(name)?;
            let undelivered = taskfile::pending_blocks(&found.body).len();
            if !found.items.is_empty() || undelivered > 0 {
                let listed: String = found
                    .items
                    .iter()
                    .map(|item| format!("  - {item}\n"))
                    .collect();
                say(&format!(
                    "{name} still has {} open item(s) and {undelivered} undelivered message(s):\n{listed}Close it anyway? [y/N] ",
                    found.items.len()
                ));
                let mut answer = String::new();
                let _ = io::stdin().read_line(&mut answer);
                if !answer.trim().to_lowercase().starts_with('y') {
                    return Err("amh: nothing was closed".to_owned());
                }
                let _lock = store.lock()?;
                let mut task = store.load(name)?;
                let mut records = requests::sync(&mut task)?;
                for request in &mut records {
                    request.stage = requests::Stage::Resolved;
                    request.resolved_at = Some(chrono::Utc::now().to_rfc3339());
                    request.evidence = "the human closed the task with amh stop".to_owned();
                    request.blockers.clear();
                }
                requests::write(&mut task, &records);
                for item in task.items.clone() {
                    tasks::remove_item(
                        &mut task,
                        &item,
                        "the human closed the task with amh stop",
                    )?;
                }
                // An undelivered block keeps its text in the file; only its marker goes, so nothing tries to deliver it later.
                let markers: Vec<usize> = taskfile::pending_blocks(&task.body)
                    .into_iter()
                    .map(|(at, _)| at)
                    .collect();
                let kept: Vec<&str> = task
                    .body
                    .split('\n')
                    .enumerate()
                    .filter(|(at, _)| !markers.contains(at))
                    .map(|(_, line)| line)
                    .collect();
                task.body = kept.join("\n");
                store.save(&task)?;
                request_effects(&settings, &store)?;
            }
            let at = agents::close_task(&settings, &store, name, true, false, false)?;
            say(&format!("closed {name} at {at}\n"));
            Ok(0)
        }
        "log" => {
            let settings = settings::load().map_err(|message| format!("amh: {message}"))?;
            let store = Store::new(&settings);
            let at = store.load(name)?.address().to_owned();
            let n = text("lines").and_then(|n| n.parse().ok()).unwrap_or(60);
            let read =
                |file: &str| fs::read_to_string(store.state_dir.join(file)).unwrap_or_default();
            say(&work::task_log(
                &read("amh-watch.log"),
                &read("amh-actions.log"),
                name,
                &at,
                n,
            ));
            let events = if store.state_dir.join("amh.sqlite3").exists() {
                crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?
                    .events()?
                    .into_iter()
                    .map(|event| event.to_string())
                    .collect::<Vec<_>>()
            } else {
                read("amh-events.jsonl")
                    .lines()
                    .map(str::to_owned)
                    .collect()
            };
            let matching: Vec<&String> = events
                .iter()
                .filter(|line| line.contains(name) || line.contains(&at))
                .collect();
            for line in matching.iter().skip(matching.len().saturating_sub(n)) {
                say(&format!("{line}\n"));
            }
            Ok(0)
        }
        other => Err(format!("amh: no short command {other}")),
    }
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
    if action.0 == "short" {
        return short(values);
    }
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
                if let Some(item) = tasks::add_item(&mut task, item, true)? {
                    requests::register(&mut task, &item, "human", "amh tell human --add")?;
                    added.push(item);
                }
            }
            let mut done = Vec::new();
            for item in many("done") {
                let finished = finish_request(&mut task, item, "")?;
                if !finished.is_empty() {
                    done.push(finished);
                }
            }
            if !many("add").is_empty() || !many("done").is_empty() {
                store.save(&task)?;
            }
            drop(lock);
            // The item changes made by this same call go out in this email, under the agent's own text.
            let body = tasks::with_changes(&body, &tasks::changes_text(&added, &done));
            let replaces: Vec<String> = many("replaces").into_iter().map(str::to_owned).collect();
            let attach: Vec<PathBuf> = many("attach").into_iter().map(PathBuf::from).collect();
            tell_human(&settings, &task, need("subject"), &body, &replaces, &attach)?;
            let _lock = store.lock()?;
            request_effects(&settings, &store)?;
            match done.is_empty() {
                true => String::new(),
                false => tasks::close_hint(&task).to_owned(),
            }
        }
        ("tell", "agent") => {
            let message = match (text("text"), text("file")) {
                (Some(text), None) => text.to_owned(),
                (None, Some(file)) => read(file)?,
                _ => return Err("amh: give the message as TEXT or as --file, not both".to_owned()),
            };
            let to = agents::address_of(&store, need("who"))?;
            // 🧑 "allow some sort of scripts or commands that should be run like a cron job, but is tied to AMH server"
            // Until that exists, a script that is no agent names itself with --from.
            let from = match (omnigent::own_address(), text("from")) {
                (Ok(address), _) => address,
                (Err(_), Some(script)) => format!("script {script}"),
                (Err(_), None) => {
                    return Err("amh: cannot tell which agent is calling; a script that is no agent passes --from NAME".to_owned());
                }
            };
            let message = work::envelope(&from, &message);
            omnigent::send(&settings, &to, &message).map_err(failed)?;
            let _lock = store.lock()?;
            clear_stopped(&store, &to)?;
            String::new()
        }
        ("tell", "guest") => tell_guest(
            &settings,
            &store,
            need("mail"),
            need("file"),
            &many("image"),
        )?,
        ("todo", "list") => {
            let _lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            let before = task.render();
            let listed = requests::listed(&mut task)?;
            if task.render() != before {
                store.save(&task)?;
            }
            listed
        }
        ("todo", "add") | ("task", "give") => {
            let lock = store.lock()?;
            let mut task = match action.0 {
                "task" => store.load(need("task"))?,
                _ => store.own(text("task_file"))?,
            };
            let mut new: Vec<String> = Vec::new();
            let author = if flag("from_human") {
                "human".to_owned()
            } else if action.0 == "todo" {
                task.name.clone()
            } else {
                creator(&store)?
            };
            for item in many("item") {
                if let Some(item) = tasks::add_item(&mut task, item, flag("from_human"))? {
                    requests::register(
                        &mut task,
                        &item,
                        &author,
                        &format!(
                            "amh {} {}; caller {}",
                            action.0,
                            action.1,
                            crate::log::caller()
                        ),
                    )?;
                    new.push(item);
                }
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
                let finished = finish_request(
                    &mut task,
                    item,
                    &tasks::quiet_evidence(need("evidence"), text("quiet_because")),
                )?;
                if !finished.is_empty() {
                    done.push(finished);
                }
            }
            store.save(&task)?;
            drop(lock);
            // 🧑 "Get some agent to fix it, and don’t bother me afterwards"
            let hint = match text("quiet_because") {
                Some(_) => "the human is not emailed, as they asked\n",
                None => report_items(&settings, &store, &task, &[], &done)?,
            };
            let _lock = store.lock()?;
            request_effects(&settings, &store)?;
            format!(
                "resolved {} human request(s); agent requests await their creator\n{hint}{}",
                done.len(),
                tasks::close_hint(&task)
            )
        }
        ("todo", "replace") => {
            let _lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            tasks::replace_item(&mut task, need("old"), need("new"))?;
            store.save(&task)?;
            "replaced 1 open item\n".to_owned()
        }
        ("todo", "approve") | ("todo", "reject") => {
            let _lock = store.lock()?;
            let caller = store.own(text("task_file"))?;
            let id = need("id");
            let owner = store
                .all()?
                .into_iter()
                .find(|task| {
                    requests::read(task).is_ok_and(|records| records.iter().any(|r| r.id == id))
                })
                .ok_or("amh: no such request ID")?;
            let mut owner = owner;
            let request = requests::decide(
                &mut owner,
                id,
                &caller.name,
                action.1 == "approve",
                need("reason"),
            )?;
            let recipient = owner.name.clone();
            requests::queue(
                &mut owner,
                &recipient,
                &format!(
                    "{}: {} ({})",
                    request.id,
                    request.stage.text(),
                    request.evidence
                ),
            )?;
            store.save(&owner)?;
            request_effects(&settings, &store)?;
            format!("{}: {}\n", id, request.stage.text())
        }
        ("todo", "wait") => {
            let _lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            let blockers = many("blocker")
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            requests::wait(&mut task, need("id"), &blockers, &store.all()?)?;
            store.save(&task)?;
            String::new()
        }
        ("todo", "take") => {
            let _lock = store.lock()?;
            let mut to = store.own(text("task_file"))?;
            let id = need("id");
            let mut source = store
                .all()?
                .into_iter()
                .find(|task| {
                    requests::read(task).is_ok_and(|records| records.iter().any(|r| r.id == id))
                })
                .ok_or("amh: no such request ID")?;
            requests::transfer(&mut source, &mut to, id)?;
            store.save_transfer(&source, &to)?;
            String::new()
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
                long_running: false,
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
            let (mut source, to) = (store.load(need("source"))?, store.load(need("to"))?);
            let request = requests::sync(&mut source)?
                .into_iter()
                .find(|r| {
                    r.stage == requests::Stage::Added
                        && (r.text == need("item") || r.id == need("item"))
                })
                .ok_or("amh: no such added request")?;
            if source.name == to.name || to.status() == "done" {
                return Err("amh: destination is done or the current owner".to_owned());
            }
            let message = format!(
                "{} offers you request {}: {}. Take ownership with `amh todo take {}`.",
                source.name, request.id, request.text, request.id
            );
            requests::queue(&mut source, &to.name, &message)?;
            store.save(&source)?;
            request_effects(&settings, &store)?;
            "ownership offered; the target must run `amh todo take ID`\n".to_owned()
        }
        ("task", "status") => {
            let _lock = store.lock()?;
            let mut task = store.load(need("task"))?;
            tasks::set_status(&mut task, need("status"), text("on"))?;
            store.save(&task)?;
            store.place(&task)?;
            String::new()
        }
        ("task", "place") => {
            let _lock = store.lock()?;
            let mut task = store.load(need("task"))?;
            tasks::pin(&mut task, need("under"))?;
            store.save(&task)?;
            store.place(&task)?;
            String::new()
        }
        ("task", "check") => {
            let findings = tasks::check(&store.list()?, &store.all()?);
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
            let _lock = store.lock()?;
            for mut task in store.all()?.into_iter().filter(|task| task.address() == at) {
                task.fields.retain(|(key, _)| key != "agent_stopped");
                task.fields
                    .push(("agent_stopped".to_owned(), "true".to_owned()));
                store.save(&task)?;
            }
            String::new()
        }
        ("agent", "compact") => {
            let at = agents::address_of(&store, need("who"))?;
            omnigent::send(&settings, &at, "/compact").map_err(failed)?;
            let _lock = store.lock()?;
            clear_stopped(&store, &at)?;
            String::new()
        }
        ("mail", "unread") => {
            let task = store.own(text("task_file"))?;
            let unread = mail::unread(&settings, task.tag(), &[]).map_err(failed)?;
            let theirs = unread.iter().filter(|header| header.is_own());
            theirs.map(|header| header.listed() + "\n").collect()
        }
        ("mail", "handled") => {
            let _lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            let path = store.state_dir.join(watch::AWAITING);
            let (kept, n) = decide::without_awaiting(
                &fs::read_to_string(&path).unwrap_or_default(),
                &task.name,
                need("mail"),
            );
            if n == 0 {
                return Err(format!(
                    "amh: no email {} of the human is waiting for an answer from {}",
                    need("mail"),
                    task.name
                ));
            }
            taskfile::write(&path, &kept)
                .map_err(|error| format!("amh: cannot update the waiting list: {error}"))?;
            tasks::add_note(
                &mut task,
                &format!(
                    "handled the human's email {} without emailing, because the human wrote: {}",
                    need("mail"),
                    need("because")
                ),
            );
            store.save(&task)?;
            "noted; you will not be reminded to answer that email\n".to_owned()
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
