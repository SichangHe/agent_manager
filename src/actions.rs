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

/// Commit dependency releases and delivery intents; the watcher sends them with `deliver_requests`.
pub fn request_effects(store: &Store) -> Result<(), String> {
    let database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
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
    Ok(())
}

/// What to wait on instead of someone.
// 🧑 "Let's disallow blocking on someone. First they need to create a task and let that someone own it, and block on this task"
const WAIT_HOW: &str = "amh: wait only on a task file (OTHER.md), an item ID (NAME.md#N), or a date (--until 2026-11-20). To wait on the human, give them an item with `amh todo ask-human 'WHAT YOU NEED'` and wait for the ID it prints; to wait on another agent, add an item to its task with `amh task give OTHER.md 'WHAT YOU NEED'` and wait for that item. Without a dependency, do not block: keep the item open.";

/// Add an item the human owns, asked by `asker`, to the human's task (made on first use); returns its ID.
fn ask_human(store: &Store, asker: &str, what: &str) -> Result<String, String> {
    let mut human = match store.contains(tasks::HUMAN_TASK)? {
        true => store.load(tasks::HUMAN_TASK)?,
        false => crate::tasks::Task::parse(
            tasks::HUMAN_TASK,
            "---\nstatus: long_running\nrunat: retired\nis_manager: false\npending_task_items: []\n---\nItems the human owns. Agents add them with `amh todo ask-human` and wait for them; the human resolves them with `amh human --done ID`.\n",
        )?,
    };
    let text = format!("{} (asked by {asker})", what.trim());
    let item = match tasks::add_item(&mut human, &text, false)? {
        Some(item) => item,
        None => tasks::open_match(&human, &taskfile::mark(&text, false)?)
            .ok_or("amh: the human's item vanished")?,
    };
    requests::register(&mut human, &item, asker, "amh todo ask-human")?;
    let id = requests::read(&human)?
        .into_iter()
        .find(|r| r.text == item && r.stage != requests::Stage::Resolved)
        .map(|r| r.id)
        .ok_or("amh: the human's item has no request")?;
    store.save(&human)?;
    store.place(&human)?;
    Ok(id)
}

/// Release tasks blocked on a task that closed, remind about other blocks and outside waits with backoff, and date undated requests; the watcher calls this every few minutes under the store lock.
// 🧑 "What are the waiting on? Previously I asked for the task blocked on to block on other tasks, was that ever implemented?"
// 🧑 "Blocked tasks that are not blocked on other tasks should have long reminders with exponential backoff. Maybe 1hr, base 1.5?"
pub fn maintain(store: &Store) -> Result<Vec<String>, String> {
    let database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
    let now = chrono::Utc::now();
    let all = store.all()?;
    let mut failed = Vec::new();
    // A task with no live Omnigent agent gets no reminders: they could never arrive.
    let live = |task: &Task| task.address().starts_with(omnigent::PREFIX);
    for task in store.active()?.into_iter().filter(live) {
        let name = task.name.clone();
        if let Err(error) = maintain_task(store, &database, &all, task, now) {
            failed.push(format!("{name}: {error}"));
        }
    }
    Ok(failed)
}

/// One task's part of `maintain`; `all` is every task as the pass began.
fn maintain_task(
    store: &Store,
    database: &crate::database::Database,
    all: &[Task],
    mut task: Task,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    {
        let added = |id: &str| {
            database
                .first_event_time("request added", id)
                .ok()
                .flatten()
        };
        let mut changed = requests::remind_waits(&mut task, now, added)?;
        // A short-lived build of 2026-11-13 gave blocks an end; reminders replace it.
        if task.field("blocked_until").is_some() {
            task.fields.retain(|(key, _)| key != "blocked_until");
            changed = true;
        }
        let on = task.field("blocked_on").unwrap_or_default().to_owned();
        let name = task.name.clone();
        // What a block on a task, an item or a date waits for, and whether it is over; `None` for free text.
        let over = match tasks::block_of(&on) {
            Some(tasks::Block::Task(other)) if store.contains(other)? => {
                Some((store.load(other)?.status() == "done").then(|| format!("{other} is closed")))
            }
            Some(tasks::Block::Item(id)) => {
                let open = all.iter().any(|owner| {
                    requests::read(owner).is_ok_and(|records| {
                        records
                            .iter()
                            .any(|r| r.id == id && r.stage != requests::Stage::Resolved)
                    })
                });
                Some((!open).then(|| format!("{id} is resolved")))
            }
            Some(tasks::Block::Date(at)) => Some((at <= now).then(|| format!("{on} has come"))),
            _ => None,
        };
        match over {
            Some(Some(why)) => {
                let status = match task.status() {
                    "long_running" => "long_running",
                    _ => "running",
                };
                tasks::set_status(&mut task, status, None)?;
                let text = format!(
                    "Your task's block is over ({why}), so it runs again; continue its open items."
                );
                requests::queue(&mut task, &name, &text)?;
                changed = true;
            }
            Some(None) => {}
            None if task.status() == "blocked" => {
                let schedule = task
                    .field(tasks::BLOCK_REMIND)
                    .and_then(requests::parse_schedule);
                let next = match schedule {
                    None => Some(requests::schedule_text(requests::next_reminder(now, 0), 0)),
                    Some((at, n)) if at <= now => {
                        let text = format!(
                            "Your task is still blocked on: {on}. If that is over, set it running (`amh task status {name} running`) and continue; if it waits on another task, block it on that task file instead (`amh task status {name} blocked --on OTHER.md`). Otherwise nothing is needed; the next reminder comes later."
                        );
                        requests::queue(&mut task, &name, &text)?;
                        Some(requests::schedule_text(
                            requests::next_reminder(now, n + 1),
                            n + 1,
                        ))
                    }
                    Some(_) => None,
                };
                if let Some(next) = next {
                    task.fields.retain(|(key, _)| key != tasks::BLOCK_REMIND);
                    task.fields.push((tasks::BLOCK_REMIND.to_owned(), next));
                    changed = true;
                }
            }
            None => {}
        }
        if changed {
            store.save(&task)?;
            store.place(&task)?;
        }
    }
    Ok(())
}

/// Send the queued request notifications; only the watcher calls this, so each message has one sender.
///
/// The network send runs without the amh lock; the lock is taken only to record each delivery.
// 🧑 "One problem you had was you would start a bunch of stuff and they would all stop at some point."
pub fn deliver_requests(settings: &Settings, store: &Store) -> Result<(), String> {
    let mut database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
    let mut failed = Vec::new();
    let mut awakened = Vec::new();
    for message in database.pending_messages("agent")? {
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
        let now_s = chrono::Utc::now().timestamp();
        if message["retry_at"].as_i64().unwrap_or_default() > now_s {
            continue;
        }
        // A notice for a task with no live Omnigent agent (retired, elsewhere, closed) can never arrive: it is dropped once, with one event.
        let target = match to.ends_with(".md") {
            true => store.load(to).ok(),
            false => None,
        };
        let address = agents::address_of(store, to).unwrap_or_default();
        let dead = !address.starts_with(omnigent::PREFIX)
            || target.as_ref().is_some_and(|task| task.status() == "done");
        let kind = if dead {
            "message dropped"
        } else {
            match omnigent::send(settings, &address, text) {
                Ok(()) => {
                    awakened.push(address.clone());
                    "message delivered"
                }
                Err(error) => {
                    // One event per message: the first failure; later attempts only back off.
                    if message["attempts"].as_i64().unwrap_or_default() == 0 {
                        crate::log::event(
                            &store.state_dir,
                            "message failed",
                            source,
                            serde_json::json!({"to": to, "text": text, "error": error}),
                        )?;
                    }
                    database.defer_message(id, &error, now_s)?;
                    failed.push(format!("{source}: {error}"));
                    continue;
                }
            }
        };
        let _lock = store.lock()?;
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
            kind,
            source,
            serde_json::json!({"id":id,"to":to,"text":text,"address":address}),
        );
        database.complete_message(id, &[(task.name.clone(), task.render())], &[event])?;
        store.refresh_exports()?;
    }
    let _lock = store.lock()?;
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
    let mut command = Command::new("systemctl");
    // Agent sessions often lack the variable `systemctl --user` needs to find the user's bus.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        use std::os::unix::fs::MetadataExt;
        let uid = fs::metadata("/proc/self").map(|own| own.uid());
        let uid = uid.map_err(|error| format!("amh: cannot read own user id: {error}"))?;
        command.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }
    let status = command.arg("--user").args(args).status();
    let status = status.map_err(|error| format!("amh: cannot run systemctl: {error}"))?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

const BATCH_HINT: &str = "This messaged the human by itself. Next time batch: give several items in one call, and when you also have something to tell the human, send it all in one message with `amh tell human --add ITEM --done ITEM`.\n";

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

/// Resolve every open item and drop every undelivered message of `name`, so it can be closed; `why` is recorded as the evidence.
pub fn drop_open_work(store: &Store, name: &str, why: &str) -> Result<(), String> {
    let _lock = store.lock()?;
    let mut task = store.load(name)?;
    let mut records = requests::sync(&mut task)?;
    for request in &mut records {
        request.stage = requests::Stage::Resolved;
        request.resolved_at = Some(chrono::Utc::now().to_rfc3339());
        request.evidence = why.to_owned();
        request.blockers.clear();
    }
    requests::write(&mut task, &records);
    for item in task.items.clone() {
        tasks::remove_item(&mut task, &item, why)?;
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
    store.drop_queued_mail(name)?;
    request_effects(store)?;
    Ok(())
}

/// Said after every `amh tell` that sends.
// 🧑 "after agent sends something. Amh would say. Never repeat this. In. Your terminal output. Again"
const NO_REPEAT: &str = "Never repeat this in your final text.\n";

/// Whether the agent of `task` may message the human: its tool is in `human.writers`, or its model is admitted for that tool.
pub fn writes_to_human(settings: &Settings, task: &Task) -> Result<bool, String> {
    let tool = task.field("tool").unwrap_or_default();
    if settings::allows(settings, "human.writers", tool) {
        return Ok(true);
    }
    // 🧑 (via the main manager) "Allow Codex Astra agents to message the human": an entry `TOOL:MODEL_PART` admits that tool on matching models only.
    let listed = settings::get(settings, "human.writers");
    let wanted: Vec<&str> = listed
        .split(',')
        .filter_map(|entry| entry.trim().strip_prefix(&format!("{tool}:")))
        .collect();
    let exception = settings::get(
        settings,
        &format!(
            "human.writer_sessions.{}",
            task.address()
                .strip_prefix(omnigent::PREFIX)
                .unwrap_or_default()
        ),
    );
    // 🧑 (via the manager) "Pin the allowlist by task file, not session, so restarts don't break it": `human.writer_tasks.TAG = "TOOL:MODEL"`.
    let by_task = settings::get(settings, &format!("human.writer_tasks.{}", task.tag()));
    if wanted.is_empty() && exception.is_empty() && by_task.is_empty() {
        return Ok(false);
    }
    let Some(id) = task.address().strip_prefix(omnigent::PREFIX) else {
        return Ok(false);
    };
    // 🤖 human_mgr.md: a slow or failed session lookup used to read as "wrong model" and refuse the human-facing reporter; retry, then say what failed.
    let path = format!("/v1/sessions/{id}?include_items=false");
    let session = omnigent::api(settings, "GET", &path, None)
        .or_else(|_| omnigent::api(settings, "GET", &path, None))
        .map_err(|error| {
            format!(
                "amh: nothing sent: cannot check which model you run on ({error}); send it again"
            )
        })?;
    let model = omnigent::session_model(&session).unwrap_or_default();
    Ok(wanted.iter().any(|part| model.contains(part))
            // 🧑 "Try 6 Sol" for this direct-human writing role.
            || (!model.is_empty() && exception == format!("{}:{tool}:{model}", task.tag()))
            || (!model.is_empty() && by_task == format!("{tool}:{model}")))
}

/// Email the human on the caller's thread; returns the receipt and the reminder to merge the caller's earlier unread updates (empty when none).
// 🧑 "Make the emailing script combine emails into one. ... Merge those, which also requires caller interface to change and encourage batched calling of amh tell human"
pub fn tell_human(
    settings: &Settings,
    task: &Task,
    subject: &str,
    body: &str,
    replaces: &[String],
    attachments: &[PathBuf],
    report_label: Option<&str>,
) -> Result<(String, String), String> {
    let tag = report_label.unwrap_or_else(|| task.tag());
    let sent = mail::send(
        settings,
        &settings::state_dir(),
        tag,
        subject,
        body,
        task.address(),
        attachments,
    );
    let message_id = sent.map_err(|error| format!("amh: {error}"))?;
    if crate::zulip::enabled(settings) {
        let receipt = format!("Zulip message sent.\nMessage-ID: {message_id}\n{NO_REPEAT}");
        let reminder = zulip_sent_followup(crate::zulip::replace(settings, replaces), || {
            crate::zulip::consolidation_reminder(settings, tag, &message_id)
        })
        .unwrap_or_else(|error| {
            eprintln!("amh: message {message_id} was sent; {error}");
            String::new()
        });
        return Ok((receipt, reminder));
    }
    mail::replace_queued(&settings::state_dir(), &message_id, replaces)
        .map_err(|error| format!("amh: {error}"))?;
    if mail::is_queued(&settings::state_dir(), &message_id) {
        // The emails it replaces stay until it is really sent, so the human loses nothing meanwhile.
        return Ok((
            format!(
                "Email NOT sent yet: Gmail refused it, see the line above. It is queued and the watcher will send it when Gmail allows. Do not send it again.\nMessage-ID: {message_id}\n"
            ),
            String::new(),
        ));
    }
    let reminder = match mail::unread(settings, tag, replaces) {
        Ok(unread) => mail::replace_reminder(&message_id, &unread),
        Err(error) => mail::unread_check_failed(&error),
    };
    Ok((
        format!("Email sent.\nMessage-ID: {message_id}\n{NO_REPEAT}"),
        reminder,
    ))
}

/// Cleanup failure never changes delivery success or requests another copy.
fn zulip_sent_followup(
    cleanup: Result<(), String>,
    reminder: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    cleanup.map_err(|error| format!(
        "earlier-message cleanup failed: {error}. Do not resend or send a merged copy. Report only new information in future updates; retain successful Message-IDs."
    ))?;
    reminder().map_err(|error| {
        format!("checking earlier Zulip updates failed: {error}; do not resend it")
    })
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
    let mut task = store.load(name)?;
    task.body = decide::queued(&task.body, &mail_name);
    store.queue_mail(None, &task, &mail_name)?;
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
        // 🧑 "Let's call it `amh human`"
        "human" => {
            let settings = settings::load().map_err(|message| format!("amh: {message}"))?;
            let store = Store::new(&settings);
            let _lock = store.lock()?;
            let owned = store.contains(tasks::HUMAN_TASK)?;
            match (text("text"), values["done"] == true) {
                (Some(item), true)
                    if owned
                        && requests::read(&store.load(tasks::HUMAN_TASK)?)?
                            .iter()
                            .any(|r| {
                                r.stage == requests::Stage::Added
                                    && (r.id == item || r.text == item)
                            }) =>
                {
                    let mut human = store.load(tasks::HUMAN_TASK)?;
                    requests::finish(&mut human, item, "the human resolved it")?;
                    store.save(&human)?;
                    request_effects(&store)?;
                }
                (Some(item), true) => store.list_remove(item)?,
                (Some(item), false) => store.list_add(item, "human pending")?,
                (None, _) => {
                    say(&store.list_entries("human pending")?);
                    if owned {
                        say(&requests::listed(&mut store.load(tasks::HUMAN_TASK)?)?);
                    }
                }
            }
            Ok(0)
        }
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
                account: None,
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
            let undelivered =
                taskfile::pending_blocks(&found.body).len() + store.queued_mail(name)?.len();
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
                drop_open_work(&store, name, "the human closed the task with amh stop")?;
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
        ("quota", "serve") => return Ok(crate::quota_observer::serve(flag("live"))),
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
            let body = match (text("text"), text("file")) {
                (Some(message), None) => message.to_owned(),
                (None, Some(file)) => read(file)?,
                (None, None) => io::read_to_string(io::stdin())
                    .map(|body| work::unix_lines(&body))
                    .map_err(|error| format!("amh: cannot read standard input: {error}"))?,
                _ => return Err("amh: give TEXT or --file, not both".to_owned()),
            };
            let (manager, message) = work::manager_message(&task, &body, state)?;
            match agents::report(&settings, &store, manager, &message, !state.is_empty())
                .map_err(failed)?
            {
                Some(sink) => format!(
                    "recorded in {sink}, which your manager reads; it is not woken. Do not send it again. Only a blocker (--blocked) or your final result (--done) wakes your manager\n"
                ),
                None => format!("message sent to your manager; do not send it again\n{NO_REPEAT}"),
            }
        }
        ("tell", "human") => {
            let body = match (text("text"), text("file")) {
                (Some(message), None) => message.to_owned(),
                (None, Some(file)) => read(file)?,
                (None, None) => io::read_to_string(io::stdin())
                    .map(|body| work::unix_lines(&body))
                    .map_err(|error| format!("amh: cannot read standard input: {error}"))?,
                _ => return Err("amh: give TEXT or --file, not both".to_owned()),
            };
            let lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            if !writes_to_human(&settings, &task)? {
                return Err(format!(
                    "amh: nothing sent: the human reads only agents running on {}, and you run on {}. Send what the human should hear to your manager with `amh tell manager` (details in a file); your manager writes to the human. Change your items with `amh todo`.",
                    settings::get(&settings, "human.writers"),
                    task.field("tool").unwrap_or_default()
                ));
            }
            // 🧑 "fix the messages to come through pb new and pb urgent instead of this tag"
            if text("report_label").is_some() && task.tag() != "stock_news_watch" {
                return Err("amh: --report-label requires stock_news_watch".to_owned());
            }
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
            // 🧑 "Send the news via email like done before instead of through Zulip"
            let mut by_email = settings.clone();
            by_email.insert("human.transport".to_owned(), "email".to_owned());
            // 🧑 "Zulip does not need a title, it just needs a thread. And the thread is basically determined, on the agent name or file name. And we can do the same for emails."
            // --subject stays accepted for old callers; only a report label that names a report stream (PB news) is kept.
            let given = text("subject").unwrap_or_default();
            let report_stream = format!("zulip.report_streams.{}", mail::bare(given).trim());
            let subject = match (
                text("report_label"),
                settings::get(&settings, &report_stream).is_empty(),
            ) {
                (Some(label), _) => label,
                (None, true) => "",
                (None, false) => given,
            };
            let (receipt, reminder) = tell_human(
                if flag("email") { &by_email } else { &settings },
                &task,
                subject,
                &body,
                &replaces,
                &attach,
                text("report_label"),
            )?;
            say(&receipt);
            say(&reminder);
            let _lock = store.lock()?;
            request_effects(&store)?;
            match done.is_empty() {
                true => String::new(),
                false => tasks::close_hint(&task).to_owned(),
            }
        }
        ("tell", "agent") => {
            let message = match (text("text"), text("file")) {
                (Some(text), None) => text.to_owned(),
                (None, Some(file)) => read(file)?,
                (None, None) => io::read_to_string(io::stdin())
                    .map(|body| work::unix_lines(&body))
                    .map_err(|error| format!("amh: cannot read standard input: {error}"))?,
                _ => return Err("amh: give the message as TEXT or as --file, not both".to_owned()),
            };
            let to = agents::address_of(&store, need("who"))?;
            // 🧑 "allow some sort of scripts or commands that should be run like a cron job, but is tied to AMH server"
            // Until that exists, a script that is no agent names itself with --from.
            let from = match (omnigent::own_address(), text("from")) {
                (Ok(address), _) => address,
                (Err(_), Some(script)) => format!("script {script}"),
                (Err(_), None) => {
                    // 🧑 "repair manager caller recognition" ... "despite --task-file"
                    let task = store.own(text("task_file")).map_err(|error| {
                        format!("{error}; a script that is no agent passes --from NAME")
                    })?;
                    if task.address().is_empty() {
                        return Err(format!(
                            "amh: caller task {} has no runat address; restart its agent or pass --from NAME for a script that is no agent",
                            task.name
                        ));
                    }
                    task.address().to_owned()
                }
            };
            let message = work::envelope(&from, &message);
            omnigent::send(&settings, &to, &message).map_err(failed)?;
            say(NO_REPEAT);
            let _lock = store.lock()?;
            clear_stopped(&store, &to)?;
            // mdbook_pr3086.md: "routing ownership should match the task that handles them"
            if need("who").ends_with(".md") {
                hand_over_mail(&store, text("task_file"), need("who"), &message)?;
            }
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
                store.own(text("task_file"))?.name
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
                Some(_) => "the human is not messaged, as they asked\n",
                None => report_items(&settings, &store, &task, &[], &done)?,
            };
            let _lock = store.lock()?;
            request_effects(&store)?;
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
            request_effects(&store)?;
            format!("{}: {}\n", id, request.stage.text())
        }
        ("todo", "wait") => {
            let _lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            let blockers = many("blocker")
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !blockers.is_empty() {
                requests::wait(&mut task, need("id"), &blockers, &store.all()?)?;
            }
            if text("on").is_some() {
                return Err(WAIT_HOW.to_owned());
            }
            if flag("clear_on") {
                requests::wait_on(&mut task, need("id"), None)?;
            }
            if let Some(at) = text("until") {
                requests::wait_until(&mut task, need("id"), requests::parse_date(at)?)?;
            }
            let mut said = String::new();
            if flag("ask_human") {
                let reason = requests::read(&task)?
                    .into_iter()
                    .find(|r| r.id == need("id"))
                    .and_then(|r| {
                        r.blockers
                            .iter()
                            .find_map(|b| b.starts_with(requests::EXTERNAL).then(|| requests::wait_schedule(b).0.to_owned()))
                    })
                    .ok_or("amh: that request waits on nothing in words; give the human an item with `amh todo ask-human`")?;
                let item = ask_human(&store, &task.name, &reason)?;
                requests::wait_to_item(&mut task, need("id"), &item)?;
                said = format!(
                    "{} now waits for the human's item {item}: {reason}\n",
                    need("id")
                );
            }
            store.save(&task)?;
            said
        }
        ("todo", "ask-human") => {
            let _lock = store.lock()?;
            let task = store.own(text("task_file"))?;
            let item = ask_human(&store, &task.name, need("text"))?;
            format!(
                "{item}\nWait for it with `amh todo wait YOUR_ID --for {item}` or `amh task status {} blocked --on {item}`. It does not message the human; tell them with `amh tell human`.\n",
                task.name
            )
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
                None => omnigent::own_address().or_else(|_| {
                    let caller = store.own(text("task_file"))?;
                    if caller.address().is_empty() {
                        return Err(format!(
                            "amh: caller task {} has no runat address; pass --manager ADDRESS",
                            caller.name
                        ));
                    }
                    Ok(caller.address().to_owned())
                })?,
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
                account: text("account"),
            };
            agents::start_task(&settings, &store, &new)? + "\n"
        }
        ("quota", "status") => {
            crate::quota_observer::status(&store.state_dir, chrono::Utc::now().timestamp())?
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
            request_effects(&store)?;
            "ownership offered; the target must run `amh todo take ID`\n".to_owned()
        }
        ("task", "status") => {
            let _lock = store.lock()?;
            let mut task = store.load(need("task"))?;
            // 🧑 "Previously I asked for the task blocked on to block on other tasks, was that ever implemented?"
            // 🧑 "Let's disallow blocking on someone."
            let until = text("until")
                .map(requests::parse_date)
                .transpose()?
                .map(|at| {
                    format!(
                        "until {}",
                        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    )
                });
            let on = until.as_deref().or(text("on"));
            let known = match on.and_then(tasks::block_of) {
                Some(tasks::Block::Task(other)) => store.contains(other)?,
                Some(tasks::Block::Item(id)) => store.all()?.iter().any(|owner| {
                    requests::read(owner).is_ok_and(|records| records.iter().any(|r| r.id == id))
                }),
                Some(tasks::Block::Date(_)) => true,
                None => on.is_none(),
            };
            if !known {
                return Err(WAIT_HOW.to_owned());
            }
            tasks::set_status(&mut task, need("status"), on)?;
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
        // 🧑 "Organize everything and improve amh to give a company-like structure where human is CEO."
        ("task", "role") => {
            let _lock = store.lock()?;
            let mut task = store.load(need("task"))?;
            let wanted = if need("role") == "manager" {
                "true"
            } else {
                "false"
            };
            match task.fields.iter_mut().find(|(key, _)| key == "is_manager") {
                Some((_, value)) => *value = wanted.to_owned(),
                None => task
                    .fields
                    .push(("is_manager".to_owned(), wanted.to_owned())),
            }
            store.save(&task)?;
            format!("{} is now recorded as a {}\n", task.name, need("role"))
        }
        ("task", "adopt") => {
            let _lock = store.lock()?;
            let caller = store.own(text("task_file"))?;
            let task = store.load(need("task"))?;
            let old = task.field("managerat").unwrap_or_default().to_owned();
            for report in work::repointed(vec![task], &old, caller.address()) {
                store.save(&report)?;
            }
            String::new()
        }
        ("task", "list-remove") => {
            let _lock = store.lock()?;
            store.list_remove(need("entry"))?;
            String::new()
        }
        ("task", "list-add") => {
            let _lock = store.lock()?;
            store.list_add(need("entry"), need("under"))?;
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
        ("agent", "org") => crate::org::chart(
            &store.active()?,
            settings::get(&settings, "main_manager"),
            |task| omnigent::held(task.address()),
            &decide::parse_stalls(&store.record("amh-stalls.txt").unwrap_or_default()),
        ),
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
            let said = omnigent::stop(&settings, &at).map_err(failed)?;
            let _lock = store.lock()?;
            for mut task in store.all()?.into_iter().filter(|task| task.address() == at) {
                task.fields.retain(|(key, _)| key != "agent_stopped");
                task.fields
                    .push(("agent_stopped".to_owned(), "true".to_owned()));
                store.save(&task)?;
            }
            said.to_owned()
        }
        ("agent", "hold") => {
            let at = agents::address_of(&store, need("who"))?;
            omnigent::set_hold(&at, Some(need("reason")))?;
            let said = omnigent::stop(&settings, &at).map_err(failed)?;
            format!("{at} is held: stopped, and messages wait until amh agent release\n{said}")
        }
        ("agent", "release") => {
            let at = agents::address_of(&store, need("who"))?;
            if omnigent::held(&at).is_none() {
                return Err(format!("amh: {at} is not held"));
            }
            omnigent::set_hold(&at, None)?;
            clear_stopped(&store, &at)?;
            let message = text("message").unwrap_or(
                "You were paused on purpose and are released now: resume the work you were doing.",
            );
            omnigent::send(&settings, &at, message).map_err(failed)?;
            format!("{at} is released and told to resume\n")
        }
        ("agent", "replace") => {
            let name = work::task_name(need("who"));
            let message = text("message").unwrap_or_default();
            if let Some(account) = text("account") {
                let _lock = store.lock()?;
                let mut task = store.load(&name)?;
                task.fields.retain(|(key, _)| key != "account");
                task.fields.push(("account".to_owned(), account.to_owned()));
                store.save(&task)?;
            }
            let new = agents::replace(
                &settings,
                &store,
                &name,
                message,
                None,
                "on the human's order",
            )?;
            format!("{name} now runs at {new}\n")
        }
        ("agent", "retire") => {
            let at = need("who").to_owned();
            if store
                .all()?
                .iter()
                .any(|task| task.address() == at && task.status() != "done")
            {
                return Err(format!(
                    "amh: {at} is a task's current agent; use amh agent stop or amh task close"
                ));
            }
            omnigent::retire(&settings, &at).map_err(failed)?;
            format!("{at} is stopped and archived; its history stays readable\n")
        }
        ("agent", "compact") => {
            let at = agents::address_of(&store, need("who"))?;
            omnigent::send(&settings, &at, "/compact").map_err(failed)?;
            let _lock = store.lock()?;
            clear_stopped(&store, &at)?;
            String::new()
        }
        ("mail", "recent") => {
            let hours: u64 = need("hours")
                .parse()
                .map_err(|_| "amh: --hours takes a whole number".to_owned())?;
            let since = chrono::Utc::now().timestamp() - (hours as i64) * 3600;
            let dir = store.root.join("manager_mail");
            let mut entries = Vec::new();
            for entry in fs::read_dir(&dir)
                .map_err(|e| format!("amh: cannot read {}: {e}", dir.display()))?
            {
                let Ok(entry) = entry else { continue };
                let Ok(at) = entry.metadata().and_then(|m| m.modified()) else {
                    continue;
                };
                let at = at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                if at < since {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let text = fs::read_to_string(entry.path()).unwrap_or_default();
                entries.push((name, at, text));
            }
            let mail_dir = settings::work_logs(&settings).join("manager_mail");
            let out = decide::recent_human(&entries, since, &mail_dir.to_string_lossy());
            if out.is_empty() {
                format!("nothing from the human in the last {hours} h\n")
            } else {
                out
            }
        }
        ("mail", "unread") => {
            let task = store.own(text("task_file"))?;
            if crate::zulip::enabled(&settings) {
                let waiting = crate::zulip::unread(&settings, &store.state_dir).map_err(failed)?;
                let shown = waiting
                    .iter()
                    .filter(|(_, topic, _, _)| flag("all") || topic == task.tag());
                shown
                    .map(|(id, topic, body, _)| format!("== zulip:{id} topic {topic}\n{body}\n"))
                    .collect()
            } else {
                let unread = mail::unread(&settings, task.tag(), &[]).map_err(failed)?;
                let theirs = unread.iter().filter(|header| header.is_own());
                theirs.map(|header| header.listed() + "\n").collect()
            }
        }
        ("mail", "handled") => {
            let _lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            let (kept, n) =
                decide::without_awaiting(&store.record(watch::AWAITING)?, &task.name, need("mail"));
            if n == 0 {
                return Err(format!(
                    "amh: no message {} of the human is waiting for an answer from {}",
                    need("mail"),
                    task.name
                ));
            }
            store.set_record(watch::AWAITING, &kept)?;
            tasks::add_note(
                &mut task,
                &format!(
                    "handled the human's message {} without replying, because the human wrote: {}",
                    need("mail"),
                    need("because")
                ),
            );
            store.save(&task)?;
            "noted; you will not be reminded to answer that message\n".to_owned()
        }
        ("mail", "done") => {
            let source = Path::new(need("mail"));
            let name = source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let path = store.root.join("manager_mail").join(name);
            let stored = fs::read_to_string(&path)
                .map_err(|error| format!("amh: cannot read {}: {error}", path.display()))?;
            let (kind, generation, uid, key) = decide::routed_mail(name, &stored)?;
            let task = store.own(text("task_file"))?;
            let database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
            let owner = database.mail_owner(&key)?;
            if owner.as_deref() != Some(task.name.as_str()) {
                return Err(format!(
                    "amh: {name} was not routed to {} (recorded owner: {}); the owner hands it over by naming the file in `amh tell agent` to your task",
                    task.name,
                    owner.as_deref().unwrap_or("none")
                ));
            }
            let mut mailbox = match kind.as_str() {
                "human" => mail::Mailbox::human(&settings)?,
                "agent" => mail::Mailbox::agent(&settings)?,
                other => return Err(format!("amh: unknown mailbox {other} in {name}")),
            };
            if mailbox.uid_validity()? != generation {
                return Err(format!(
                    "amh: the {kind} mailbox was renumbered since {name} arrived; mark it read by hand"
                ));
            }
            mailbox.mark_read(&uid)?;
            let _lock = store.lock()?;
            let mut task = store.load(&task.name)?;
            tasks::add_note(
                &mut task,
                &format!("marked routed email {name} read after handling it"),
            );
            store.save(&task)?;
            format!("marked {name} read\n")
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

#[cfg(test)]
mod maintain_tests {
    use crate::store::Store;
    use crate::taskfile;

    #[test]
    fn undeliverable_notices_drop_once_and_failures_back_off()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!(
            "amh-notices-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir(&root)?;
        let store = Store {
            root: root.clone(),
            state_dir: root.join("state"),
        };
        for (name, at) in [
            ("retired.md", "retired"),
            ("live.md", "omnigent://nowhere"),
            ("src.md", "omnigent://src"),
        ] {
            taskfile::write(
                &root.join(name),
                &format!("---\nstatus: running\nrunat: {at}\n---\n"),
            )?;
        }
        taskfile::write(
            &root.join("TODO.md"),
            "current:\n\nhuman pending:\n\nlow priority:\n\nprevious:\n",
        )?;
        {
            let _lock = store.lock()?;
            let mut source = store.load("src.md")?;
            crate::requests::queue(&mut source, "retired.md", "to nobody")?;
            crate::requests::queue(&mut source, "live.md", "to a dead port")?;
            store.save(&source)?;
        }
        // Nothing listens on port 9, so every send fails at once.
        let settings =
            super::Settings::from([("omnigent.url".to_owned(), "http://127.0.0.1:9".to_owned())]);
        for _ in 0..3 {
            let _ = super::deliver_requests(&settings, &store);
        }
        let database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
        let pending = database.pending_messages("agent")?;
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (
                pending[0]["target"].as_str(),
                pending[0]["attempts"].as_i64()
            ),
            (Some("live.md"), Some(1))
        );
        let kinds: Vec<String> = database
            .events()?
            .iter()
            .filter_map(|event| event["kind"].as_str().map(str::to_owned))
            .filter(|kind| kind.starts_with("message ") && kind != "message queued")
            .collect();
        assert_eq!(kinds, ["message dropped", "message failed"]);
        assert!(
            !crate::requests::notifications(&store.load("src.md")?)?
                .iter()
                .any(|n| n["to"] == "retired.md")
        );
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn closing_a_task_releases_tasks_blocked_on_it_and_other_blocks_get_reminders()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!(
            "amh-maintain-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir(&root)?;
        let store = Store {
            root: root.clone(),
            state_dir: root.join("state"),
        };
        let file = |name: &str, status: &str, on: &str| {
            taskfile::write(
                &root.join(name),
                &format!(
                    "---\nstatus: {status}\nrunat: omnigent://{name}\nblocked_on: {on}\n---\n"
                ),
            )
        };
        file("other.md", "running", "")?;
        file("waiter.md", "blocked", "other.md")?;
        file("standing.md", "long_running", "other.md")?;
        file("human.md", "blocked", "human reviewing docs/plan.md")?;
        taskfile::write(
            &root.join("TODO.md"),
            "current:\n\nhuman pending:\n\nlow priority:\n\nprevious:\n",
        )?;
        let _lock = store.lock()?;
        assert!(super::maintain(&store)?.is_empty());
        assert_eq!(store.load("waiter.md")?.status(), "blocked");
        let human = store.load("human.md")?;
        assert!(
            human
                .field(crate::tasks::BLOCK_REMIND)
                .is_some_and(|at| at.ends_with(" #0"))
        );
        let mut other = store.load("other.md")?;
        crate::tasks::set_status(&mut other, "done", None)?;
        store.save(&other)?;
        assert!(super::maintain(&store)?.is_empty());
        let waiter = store.load("waiter.md")?;
        assert_eq!(
            (waiter.status(), waiter.field("blocked_on")),
            ("running", None)
        );
        let standing = store.load("standing.md")?;
        assert_eq!(
            (standing.status(), standing.field("blocked_on")),
            ("long_running", None)
        );
        let database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
        let queued = database.pending_messages("agent")?;
        assert!(queued.iter().any(|m| {
            m["target"] == "waiter.md"
                && m["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("block is over (other.md is closed)"))
        }));
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}

#[cfg(test)]
mod zulip_followup_tests {
    #[test]
    fn delivered_message_with_cleanup_failure_does_not_request_merge() {
        let result = super::zulip_sent_followup(
            Err("You don't have permission to delete this message".to_owned()),
            || panic!("must not request consolidation after failed cleanup"),
        );
        let Err(error) = result else {
            panic!("cleanup failure must be reported")
        };
        assert!(error.contains("permission to delete"));
        assert!(error.contains("Do not resend or send a merged copy"));
        assert!(error.contains("Report only new information"));
        assert!(!error.contains("Merge them now"));
    }

    #[test]
    fn successful_cleanup_keeps_normal_reminder() {
        assert_eq!(
            super::zulip_sent_followup(Ok(()), || Ok("normal reminder".to_owned())),
            Ok("normal reminder".to_owned())
        );
        assert_eq!(
            super::zulip_sent_followup(Ok(()), || Ok(String::new())),
            Ok(String::new())
        );
    }
}

/// A routed email named in a forwarded message now belongs to the receiving task, so it can run `amh mail done`.
/// Only the sender that owns it, or anyone for a receipt with no recorded owner, hands it over.
fn hand_over_mail(
    store: &Store,
    sender: Option<&str>,
    receiver: &str,
    message: &str,
) -> Result<(), String> {
    let sender = store.own(sender).ok().map(|task| task.name.clone());
    let database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
    for name in decide::routed_names(message) {
        let Ok(stored) = fs::read_to_string(store.root.join("manager_mail").join(&name)) else {
            continue;
        };
        let Ok((_, _, _, key)) = decide::routed_mail(&name, &stored) else {
            continue;
        };
        let owner = database.mail_owner(&key)?;
        if owner.is_none() || owner == sender {
            database.set_mail_owner(&key, receiver)?;
        }
    }
    Ok(())
}
