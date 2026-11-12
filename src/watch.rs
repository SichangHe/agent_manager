//! The one background watcher: takes in human/guest mail, delivers queued messages, and nudges managers and idle agents.
//!
//! Every decision and text is a pure function of `decide`, `guest`, or `work`; this module does the mail, the agents, the files, and the clock around them.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agents::{self, NewTask};
use crate::decide::{self, Chase, Nudge, Outcome, Plan, Source};
use crate::mail::{self, Mailbox};
use crate::store::Store;
use crate::tasks::Task;
use crate::{guest, omnigent, settings};

// 🧑 "Why so slow ... Improve it"
const MAIL_POLL: Duration = Duration::from_secs(5);
const RECONNECT: Duration = Duration::from_secs(30);
/// How often queued emails are tried again.
const FLUSH_EVERY: Duration = Duration::from_secs(600);
/// How often the human's unread agent emails are counted.
const COMPRESS_EVERY: Duration = Duration::from_secs(300);
/// The record of the last notice to compress: `EPOCH COUNT`.
const COMPRESSED: &str = "amh-compress.txt";
const NUDGE_EVERY: Duration = Duration::from_secs(60);
/// How often the generated Markdown of the work logs is committed.
// 🧑 "Why is work_logs not a clean repo? Since it is fully generated, should we auto-commit too? Do avoid committing huge files and artifacts, binaries, tho."
const COMMIT_EVERY: Duration = Duration::from_secs(600);
pub const AWAITING: &str = "amh-awaiting.txt";
const TOLD_HUMAN: &str = "amh-told-human.txt";
/// The agent questions already sent to the human: lines of prompt IDs.
const ASKED_HUMAN: &str = "amh-asked-human.txt";
const PROBLEM_NOTICES: &str = "amh-problem-notices.txt";
const STALLS: &str = "amh-stalls.txt";
const PROBLEM_REPEATS: &str = "amh-problem-repeats.txt";

type Settings = BTreeMap<String, String>;

/// The connection to the agent account's mailbox, kept across polls.
enum Link {
    Up(Mailbox),
    /// Not logged in; the next login is tried at `retry_at`.
    Down {
        retry_at: Instant,
    },
}

/// Why mail intake stopped.
enum IntakeError {
    /// The mailbox connection failed; it is dropped and made again.
    Connection(String),
    /// Something else failed; the connection is kept.
    Other(String),
}

/// What the watcher remembers between polls; a restart forgets it, which only repeats a retry or a reminder early.
struct Watcher {
    /// When the delivery of a task's blocks last reached nobody.
    failed_at: BTreeMap<String, Instant>,
    pending_events: Option<i64>,
}

/// The email intake thread owns both persistent IMAP connections.
struct MailIntake {
    link: Link,
    human_link: Link,
}

// 🧑 "Zulip delay feels very large now": the agent scan asks Omnigent about every agent, which takes about a
// minute with ~600 agents, so it runs on its own thread and never holds up taking in and delivering messages.
/// What the agent scan remembers between scans.
struct Scanner {
    started: Instant,
    /// When each task's idle agent was last reminded, in seconds since `started`.
    reminded_s: BTreeMap<String, u64>,
    /// Each idle task's reminders since its task file last changed.
    stalls: BTreeMap<String, decide::Stall>,
    /// Whether `stalls` was read back from the record after a restart.
    stalls_loaded: bool,
}

/// Serializes changes to the awaited-answers record, which delivery appends to while the scan rewrites it.
static AWAITING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn log(text: &str) {
    println!(
        "{} {text}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

fn epoch_s() -> f64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.unwrap_or_default().as_secs_f64()
}

/// Report slow serial watcher phases without writing an event for every idle poll.
fn log_elapsed(phase: &str, began: Instant) {
    let elapsed = began.elapsed();
    if elapsed >= Duration::from_secs(1) {
        log(&format!(
            "watcher phase {phase}: {:.1} ms",
            elapsed.as_secs_f64() * 1000.0
        ));
    }
}

/// An epoch as the local time of day `HH:MM`.
fn hour_minute(epoch_s: f64) -> String {
    chrono::DateTime::from_timestamp(epoch_s as i64, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// The text of a record of the watcher; empty when there is none yet or it cannot be read.
fn record(store: &Store, name: &str) -> String {
    store.record(name).unwrap_or_else(|error| {
        log(&format!("cannot read the record {name}: {error}"));
        String::new()
    })
}

fn append(store: &Store, name: &str, line: &str) -> Result<(), String> {
    store.set_record(name, &(store.record(name)? + line))
}

/// Write a file only its owner can read.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    let mut file = options
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// Email the human about one of their messages, at most once per `key`.
impl Scanner {
    /// Remind idle agents of their open items; report each agent failure once to its manager and, for harness errors, to the human.
    fn nudge(&mut self, settings: &Settings, store: &Store) -> Result<(), String> {
        let reported: BTreeSet<String> = record(store, PROBLEM_NOTICES)
            .lines()
            .map(str::to_owned)
            .collect();
        for line in agents::follow_moves(settings, store)? {
            log(&line);
        }
        if !self.stalls_loaded {
            self.stalls = decide::parse_stalls(&record(store, STALLS));
            self.stalls_loaded = true;
        }
        let found = agents::problems(settings, store)?;
        for line in crate::quota_observer::watcher_pass(settings, store) {
            log(&line);
        }
        let wall_s = chrono::Utc::now().timestamp();
        for line in crate::fallback::watch(settings, store, &found, wall_s) {
            log(&line);
        }
        for line in crate::revive::pass(settings, store, &found, wall_s) {
            log(&line);
        }
        let now_s = self.started.elapsed().as_secs();
        // 🧑 "We have 96% quota left for Codex and it resets in 5 days. Try to use them up in those days": Codex workers stop after each small step, so the setting `remind.codex_idle_s` wakes them sooner than the default.
        let remind_every_s = |task: &Task| {
            let tool = task.field("tool").unwrap_or_default();
            [format!("remind.{tool}_idle_s"), "remind.idle_s".to_owned()]
                .iter()
                .find_map(|key| settings::get(settings, key).parse().ok())
                .unwrap_or(decide::REMINDER_S)
        };
        let decided = decide::nudges(
            &found,
            &reported,
            &store.active()?,
            &self.reminded_s,
            now_s,
            remind_every_s,
        );
        let Some((nudges, mut now)) = decided else {
            return Ok(());
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let every_s = settings::get(settings, "remind.repeat_report_s")
            .parse()
            .unwrap_or(2 * 3600);
        let (repeats, repeat_state) = decide::repeat_reports(
            &found,
            &reported,
            &store.active()?,
            &decide::parse_repeats(&record(store, PROBLEM_REPEATS)),
            now_unix,
            every_s,
            settings::get(settings, "main_manager"),
        );
        let mut by_manager: Vec<(String, Vec<String>)> = Vec::new();
        let mut stalled: Vec<(String, String)> = Vec::new();
        for nudge in nudges {
            match nudge {
                Nudge::Remind {
                    task,
                    address,
                    text,
                } => {
                    let reminded = omnigent::send(settings, &address, &text);
                    self.reminded_s.insert(task.clone(), now_s);
                    if let Err(error) = reminded {
                        log(&format!("reminder to {task} failed: {error}"));
                        continue;
                    }
                    log(&format!("reminded idle {task} of its open items"));
                    let Ok(loaded) = store.load(&task) else {
                        continue;
                    };
                    let limit = settings::get(settings, "remind.stall_reminders")
                        .parse()
                        .unwrap_or(decide::STALL_REMINDERS);
                    let previous = self.stalls.get(&task);
                    let mark = decide::progress_mark(
                        &loaded.items,
                        loaded.field("request_records").unwrap_or_default(),
                    );
                    if previous.is_some_and(|stall| stall.reported && stall.task_text != mark) {
                        log(&format!("stalled {task} made progress again"));
                    }
                    let (stall, escalate) = decide::stall_step(previous, &mark, limit);
                    if escalate {
                        log(&format!(
                            "stalled {task}: {} reminders without progress",
                            stall.reminders
                        ));
                        let route = decide::notice_route(
                            loaded.field("managerat").unwrap_or_default(),
                            loaded.address(),
                            settings::get(settings, "main_manager"),
                        );
                        let (manager, note) = match route {
                            decide::Route::To(to, note) => (to, note.unwrap_or_default()),
                            decide::Route::Root => {
                                // The main manager answers only to the human; no agent above it gets stall notices.
                                log(&format!(
                                    "stall of main manager {task} not escalated: it reports to the human"
                                ));
                                let mut stall = stall;
                                stall.reported = true;
                                self.stalls.insert(task, stall);
                                continue;
                            }
                            decide::Route::Unroutable => {
                                log(&format!(
                                    "stall notice for {task} has no Omnigent recipient"
                                ));
                                self.stalls.insert(task, stall);
                                continue;
                            }
                        };
                        let text = format!(
                            "{note}{}",
                            decide::stall_text(&task, stall.reminders, loaded.items.len())
                        );
                        match by_manager.iter_mut().find(|(known, _)| *known == manager) {
                            Some((_, texts)) => texts.push(text),
                            None => by_manager.push((manager.clone(), vec![text])),
                        }
                        stalled.push((manager, task.clone()));
                    }
                    self.stalls.insert(task, stall);
                }
                Nudge::Report {
                    key,
                    log: line,
                    task,
                    manager,
                    text,
                    email,
                } => {
                    log(&line);
                    let address = store.load(&task).map(|loaded| loaded.address().to_owned());
                    let route = decide::notice_route(
                        &manager,
                        address.as_deref().unwrap_or_default(),
                        settings::get(settings, "main_manager"),
                    );
                    match route {
                        decide::Route::To(to, note) => {
                            let text = format!("{}{text}", note.unwrap_or_default());
                            match by_manager.iter_mut().find(|(known, _)| *known == to) {
                                Some((_, texts)) => texts.push(text),
                                None => by_manager.push((to, vec![text])),
                            }
                        }
                        decide::Route::Root | decide::Route::Unroutable => {
                            log(&format!(
                                "problem notice for {task} has no agent recipient (manager {manager})"
                            ));
                        }
                    }
                    let Some((tag, body)) = email else {
                        continue;
                    };
                    let sent = mail::send(
                        settings,
                        &store.state_dir,
                        &tag,
                        "",
                        &body,
                        mail::WATCHER,
                        &[],
                    );
                    if let Err(error) = sent {
                        log(&format!("failure email for {task} was not sent: {error}"));
                        // Not remembered, so the problem is reported again once email works.
                        now.remove(&key);
                    }
                }
            }
        }
        let mut repeat_state = repeat_state;
        let mut pending_repeats = Vec::new();
        for repeat in repeats {
            match by_manager.iter_mut().find(|(known, _)| *known == repeat.to) {
                Some((_, texts)) => texts.push(repeat.text.clone()),
                None => by_manager.push((repeat.to.clone(), vec![repeat.text.clone()])),
            }
            pending_repeats.push((repeat.to, repeat.state_key, repeat.entry));
        }
        for (manager, texts) in by_manager {
            if let Err(error) = omnigent::send(settings, &manager, &decide::problems_text(&texts)) {
                log(&format!("problem notice to {manager} failed: {error}"));
                continue;
            }
            for (_, state_key, entry) in pending_repeats.iter().filter(|(to, _, _)| *to == manager)
            {
                log(&format!(
                    "repeated an unresolved agent problem to {manager}"
                ));
                repeat_state.insert(state_key.clone(), *entry);
            }
            for (_, task) in stalled.iter().filter(|(known, _)| *known == manager) {
                if let Some(stall) = self.stalls.get_mut(task) {
                    stall.reported = true;
                }
            }
        }
        if let Err(error) = store.set_record(PROBLEM_REPEATS, &decide::repeats_text(&repeat_state))
        {
            log(&format!("cannot save repeated problem records: {error}"));
        }
        if let Err(error) = store.set_record(STALLS, &decide::stalls_text(&self.stalls)) {
            log(&format!("cannot save stall records: {error}"));
        }
        let keys: String = now.iter().map(|key| format!("{key}\n")).collect();
        store.set_record(PROBLEM_NOTICES, &keys)
    }
}

// 🧑 "if I message an agent and somehow they are not whatever state they are, I should get some sort of response"
fn tell_human_once(settings: &Settings, store: &Store, task: &Task, key: &str, text: &str) {
    let key = decide::told_key(task, key);
    if record(store, TOLD_HUMAN).lines().any(|told| told == key) {
        return;
    }
    let body = format!("{text}\n");
    let sent = mail::send(
        settings,
        &store.state_dir,
        task.tag(),
        "",
        &body,
        mail::WATCHER,
        &[],
    );
    match sent {
        Ok(_) => {
            if let Err(error) = append(store, TOLD_HUMAN, &format!("{key}\n")) {
                log(&format!(
                    "{error}; the human may be emailed again about {key}"
                ));
            }
        }
        Err(error) => log(&format!(
            "email to the human about {} was not sent: {error}",
            task.name
        )),
    }
}

/// Record in the event log that a message of the human was taken in for a task.
fn received(store: &Store, task: &str, stored: &str) {
    let data = serde_json::json!({"stored": format!("manager_mail/{stored}")});
    if let Err(error) = crate::log::event(&store.state_dir, "human mail received", task, data) {
        log(&error);
    }
}

/// Store one human email and queue it for the agent of the right task file; returns that task file's name.
fn take_in(
    settings: &Settings,
    store: &Store,
    stored: &Path,
    name: &str,
    mail: &mail::Parsed,
) -> Result<String, String> {
    let _lock = store.lock()?;
    let tasks = store.all()?;
    let main_manager = settings::get(settings, "main_manager");
    let task = &decide::route(&tasks, main_manager, &mail.subject, &mail.body)?.name;
    write_private(stored, &decide::stored_mail(&mail.subject, &mail.body))
        .map_err(|error| format!("cannot write {}: {error}", stored.display()))?;
    let mut task = store.load(task)?;
    task.body = decide::queued(&task.body, name);
    store.queue_mail(Some(name), &task, name)?;
    Ok(task.name)
}

/// Take in each new human email; one that cannot be routed stays unread and unstored.
fn take_in_mail(
    settings: &Settings,
    store: &Store,
    mailbox: &mut Mailbox,
    routed: &BTreeSet<String>,
) -> Result<(), String> {
    let prefix = match settings::get(settings, "mail.stored_prefix") {
        "" => "mail",
        prefix => prefix,
    };
    for (uid, raw) in mailbox.unseen_from(settings::get(settings, "mail.human_address"))? {
        if routed.contains(&uid) {
            continue;
        }
        let mail = mail::parse(&raw);
        let name = format!("{prefix}-{uid}.txt");
        let stored = store.root.join("manager_mail").join(&name);
        let subject = decide::quoted(&mail.subject);
        // A stored file means the email was taken in before and only marking it read failed.
        if !stored.exists() {
            match take_in(settings, store, &stored, &name, &mail) {
                Ok(task) => {
                    log(&format!("mail {name} {subject} -> {task}"));
                    received(store, &task, &name);
                }
                Err(error) => {
                    // A committed receipt means the email is queued; its stored file must stay, and the next round only marks it read.
                    let queued =
                        crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))
                            .and_then(|database| database.received_mail(&name))
                            .unwrap_or(true);
                    if !queued {
                        let _ = fs::remove_file(&stored);
                    }
                    log(&format!(
                        "mail {name} {subject} left unread, not taken in: {error}"
                    ));
                    continue;
                }
            }
        }
        mailbox.mark_read(&uid)?;
    }
    Ok(())
}

fn take_in_zulip(settings: &Settings, store: &Store) -> Result<(), String> {
    let poll_started_at = chrono::Local::now().to_rfc3339();
    let mut cursor = store
        .record("amh-zulip-cursor.txt")?
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    for message in crate::zulip::incoming(settings, cursor)? {
        let id = message["id"].as_u64().ok_or("Zulip message has no ID")?;
        let body = message["content"]
            .as_str()
            .ok_or("Zulip message has no content")?;
        let subject = if settings::get(settings, "zulip.stream_id").is_empty() {
            crate::zulip::subject(body)
        } else {
            let topic = message["subject"]
                .as_str()
                .ok_or("Zulip stream message has no topic")?;
            format!("[{}] Zulip message", crate::zulip::task_topic(topic))
        };
        let name = format!("zulip-{id}.txt");
        let stored = store.root.join("manager_mail").join(&name);
        {
            let _lock = store.lock()?;
            let key = format!("zulip:{id}");
            if !crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?
                .received_mail(&key)?
            {
                let tasks = store.all()?;
                let mut task = decide::route(
                    &tasks,
                    settings::get(settings, "main_manager"),
                    &subject,
                    &crate::zulip::own_text(body),
                )?
                .clone();
                write_private(&stored, &decide::stored_mail(&subject, body))
                    .map_err(|error| format!("cannot store Zulip message: {error}"))?;
                task.body = decide::queued(&task.body, &name);
                store.queue_mail(Some(&key), &task, &name)?;
                log(&format!("Zulip {id} -> {}", task.name));
                received(store, &task.name, &name);
                let _ = crate::log::event(
                    &store.state_dir,
                    "zulip pickup",
                    &task.name,
                    serde_json::json!({"zulip_message_id": id.to_string(),
                        "source_timestamp_s": message["timestamp"], "poll_started_at": poll_started_at}),
                );
            }
        }
        cursor = id;
        store.set_record("amh-zulip-cursor.txt", &cursor.to_string())?;
    }
    Ok(())
}

/// Hold the watcher's lifetime lock, or fail when another watcher already holds it.
fn watcher_lock(state_dir: &std::path::Path) -> Result<fs::File, String> {
    let failed = |error: std::io::Error| format!("amh watch: cannot lock: {error}");
    fs::create_dir_all(state_dir).map_err(failed)?;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_dir.join("amh-watch.lock"))
        .map_err(failed)?;
    lock.try_lock()
        .map_err(|_| "amh watch: another watcher is already running; exiting".to_owned())?;
    Ok(lock)
}

/// Commit the task notification and receipt before changing the external mailbox.
fn route_mail(
    store: &Store,
    mailbox: &mut Mailbox,
    kind: &str,
    rules: &[crate::mail_routes::Rule],
) -> Result<BTreeSet<String>, String> {
    let mut routed = BTreeSet::new();
    let senders: BTreeSet<&str> = rules
        .iter()
        .filter(|rule| rule.mailbox == kind)
        .map(|rule| rule.from.as_str())
        .collect();
    for sender in senders {
        for (uid, raw) in mailbox.unseen_from(sender)? {
            let parsed = mail::parse(&raw);
            let Some(rule) = crate::mail_routes::choose(rules, kind, &parsed) else {
                continue;
            };
            routed.insert(uid.clone());
            let generation = mailbox.uid_validity()?;
            let key = if parsed.message_id.is_empty() {
                format!("{kind}:{generation}:{uid}")
            } else {
                format!("{kind}:{}", parsed.message_id)
            };
            let name = format!("routed-{kind}-{generation}-{uid}.txt");
            let captured = (|| -> Result<bool, String> {
                let _lock = store.lock()?;
                if crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?
                    .received_mail(&key)?
                {
                    store.refresh_exports()?;
                    return Ok(false);
                }
                let mut task = store.load(&crate::work::task_name(&rule.to))?;
                if !task.is_active() {
                    return Err(format!(
                        "mail route {} target {} is done; notification stays unread",
                        rule.name, task.name
                    ));
                }
                let path = store.root.join("manager_mail").join(&name);
                write_private(
                    &path,
                    &format!(
                        "From: {}\nMessage-ID: {}\n{}",
                        parsed.sender,
                        parsed.message_id,
                        decide::stored_mail(&parsed.subject, &parsed.body)
                    ),
                )
                .map_err(|error| format!("cannot store routed notification: {error}"))?;
                let target = task.name.clone();
                crate::requests::queue(
                    &mut task,
                    &target,
                    &crate::mail_routes::message(rule, &path, &parsed),
                )?;
                store.receive_mail(&key, &task)
            })();
            let captured = match captured {
                Ok(captured) => captured,
                Err(error) => {
                    log(&format!("routed {kind} mail {uid} retained: {error}"));
                    continue;
                }
            };
            if captured {
                log(&format!(
                    "routed {kind} mail {uid} with rule {} -> {}",
                    rule.name, rule.to
                ));
            }
            match rule.disposition.as_str() {
                "read" => mailbox.mark_read(&uid)?,
                "trash" => mailbox.trash(&uid)?,
                _ => {}
            }
        }
    }
    Ok(routed)
}

/// Reuse the guest agent when it is alive; otherwise start one under the main manager.
// 🧑 "Make it lazy. Make the watcher start an agent. If there's no agent for that guest or reuse an agent if they already exist."
fn ensure_guest_agent(settings: &Settings, store: &Store, address: &str) -> Result<(), String> {
    let name = guest::task_name(settings);
    if store.contains(&name)? && agents::has_live_agent(settings, &store.load(&name)?) {
        return Ok(());
    }
    let new = NewTask {
        account: None,
        name: &name,
        workdir: settings::get(settings, "guest.workdir"),
        goal: &guest::goal(address),
        manager: settings::get(settings, "main_manager"),
        tool: None,
        model: None,
        effort: None,
        as_manager: false,
        long_running: false,
        email: None,
        lines: None,
    };
    agents::start_task(settings, store, &new).map(|_| ())
}

/// Save one guest email as `stored` with its images beside it, make sure the guest agent exists, and add the email to its open work.
fn store_guest_mail(
    settings: &Settings,
    store: &Store,
    address: &str,
    mail: &mail::Parsed,
    stored: &str,
) -> Result<(), String> {
    let mail_dir = guest::mail_dir(settings);
    let key = format!("guest:{mail_dir}/{stored}");
    {
        let _lock = store.lock()?;
        if crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?
            .received_mail(&key)?
        {
            return Ok(());
        }
        let name = guest::task_name(settings);
        if store.contains(&name)? {
            let task = store.load(&name)?;
            let source = format!("{}{mail_dir}/{stored})", guest::SOURCE);
            if task.body.lines().any(|line| line == source) {
                store.receive_mail(&key, &task)?;
                return Ok(());
            }
        }
    }
    let folder = store.root.join(&mail_dir);
    let failed = |error: std::io::Error| format!("cannot write in {}: {error}", folder.display());
    if !folder.is_dir() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&folder)
            .map_err(failed)?;
    }
    let mut saved = Vec::new();
    for (name, data) in guest::kept_images(&mail.images, stored) {
        fs::write(folder.join(&name), data).map_err(failed)?;
        saved.push(name);
    }
    ensure_guest_agent(settings, store, address)?;
    write_private(
        &folder.join(stored),
        &guest::stored_text(mail, &mail_dir, &saved),
    )
    .map_err(failed)?;
    let _lock = store.lock()?;
    let mut task = store.load(&guest::task_name(settings))?;
    guest::queue(&mut task, &mail_dir, stored);
    store.receive_mail(&key, &task).map(|_| ())
}

/// Take in each new guest email; one whose sender is not verified is dropped.
fn take_in_guest_mail(
    settings: &Settings,
    store: &Store,
    mailbox: &mut Mailbox,
    routed: &BTreeSet<String>,
) -> Result<(), IntakeError> {
    let address = settings::get(settings, "guest.address");
    if address.is_empty() {
        return Ok(());
    }
    let unseen = mailbox.unseen_from(address);
    for (uid, raw) in unseen.map_err(IntakeError::Connection)? {
        if routed.contains(&uid) {
            continue;
        }
        let mail = mail::parse(&raw);
        let stored = guest::stored_name(settings, &uid);
        if !guest::verified(&mail, address) {
            println!("guest mail uid {uid} rejected: the sender is not verified as {address}");
        } else {
            store_guest_mail(settings, store, address, &mail, &stored)
                .map_err(IntakeError::Other)?;
        }
        mailbox.mark_read(&uid).map_err(IntakeError::Connection)?;
    }
    Ok(())
}

/// The message for an agent from a pending block; a stored human email is inlined verbatim.
fn delivery_text(
    settings: &Settings,
    store: &Store,
    task: &Task,
    block: &[String],
) -> Result<String, String> {
    // 🧑 "Pending blocks in task files should not be `manager_delegation`, they should be dispatched naked, randomly followed by a reminder to add to pending task items"
    // A fresh `RandomState` hashes with fresh random keys, which makes it a random number without another crate.
    let remind = RandomState::new().build_hasher().finish().is_multiple_of(8);
    Ok(match decide::source_of(&block[0]) {
        Source::Guest => {
            guest::delivery_text(&store.root.join(guest::mail_dir(settings)), &block[0])
        }
        Source::Mail(name) => {
            let file = Path::new(name).file_name().unwrap_or_default();
            let stored = fs::read_to_string(store.root.join("manager_mail").join(file))
                .map_err(|error| format!("{name}: {error}"))?;
            decide::mail_text(name, &stored, task.tag(), !task.items.is_empty(), remind)
        }
        Source::Plain => decide::plain_text(block, remind),
    })
}

/// Try to deliver one message: to the target, else to the fallback manager.
fn attempt(
    settings: &Settings,
    task: &Task,
    plan: &Plan,
    text: &str,
    reply: Option<&str>,
    trace: Option<&omnigent::DeliveryTrace<'_>>,
) -> Outcome {
    let (mut state, mut evidence) = omnigent::status(settings, &plan.target);
    // The human's message to an agent that waits on a question it was sent is the answer to that question.
    if let ("stuck", Some(reply)) = (state, reply) {
        let waiting = omnigent::pending_questions(settings, &plan.target).unwrap_or_default();
        for (elicitation, parts) in &waiting {
            let answers = decide::question_answers(parts, reply);
            match omnigent::answer_question(settings, &plan.target, elicitation, &answers) {
                Ok(()) => log(&format!(
                    "gave the human's answer to the question of {}: {answers:?}",
                    task.name
                )),
                Err(error) => log(&format!(
                    "answering the question of {} failed: {error}",
                    task.name
                )),
            }
        }
        if !waiting.is_empty() {
            (state, evidence) = omnigent::status(settings, &plan.target);
        }
    }
    // A missing or stuck agent reads no messages, so its mail goes to its manager.
    let sent = match state {
        "missing" | "stuck" | "error" => Err(format!("the agent is {state}: {evidence}")),
        _ => omnigent::send_now_traced(settings, &plan.target, text, trace),
    };
    let Err(error) = sent else {
        return Outcome::Delivered;
    };
    let second = match &plan.fallback {
        Some(manager) => omnigent::send_now_traced(
            settings,
            manager,
            &decide::rerouted(task, &error, text),
            trace,
        ),
        None => Err("no manager to fall back to".to_owned()),
    };
    match second {
        Ok(()) => Outcome::Rerouted { error },
        Err(second) => Outcome::Unreachable { error, second },
    }
}

/// Try to deliver one pending block of a task and record the outcome; `false` when the task is to be retried later.
fn deliver_block(
    settings: &Settings,
    store: &Store,
    task: &Task,
    plan: &Plan,
    block: &[String],
) -> bool {
    let name = &task.name;
    let zulip_id = match decide::source_of(&block[0]) {
        Source::Mail(file) => Path::new(file)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("zulip-"))
            .and_then(|name| name.strip_suffix(".txt"))
            .filter(|id| {
                !id.is_empty() && id.len() <= 20 && id.bytes().all(|byte| byte.is_ascii_digit())
            }),
        _ => None,
    };
    // 🧑 "Coordinate with the relevant agents to relentlessly monitor and minimize E2E latency"
    let trace = zulip_id.map(|id| omnigent::DeliveryTrace {
        state_dir: &store.state_dir,
        task: name,
        zulip_message_id: id,
    });
    let began = Instant::now();
    if let Some(id) = zulip_id
        && let Err(error) = crate::log::event(
            &store.state_dir,
            "delivery started",
            name,
            serde_json::json!({"zulip_message_id": id, "to": plan.target, "block": block[0]}),
        )
    {
        log(&error);
    }
    if omnigent::status(settings, &plan.target).0 == "paused" {
        if let Some(id) = zulip_id
            && let Err(error) = crate::log::event(
                &store.state_dir,
                "delivery paused",
                name,
                serde_json::json!({"zulip_message_id": id, "to": plan.target,
                    "elapsed_ms": began.elapsed().as_secs_f64() * 1000.0}),
            )
        {
            log(&error);
        }
        return false;
    }
    let reply = match decide::source_of(&block[0]) {
        Source::Mail(file) => {
            let file = Path::new(file).file_name().unwrap_or_default();
            fs::read_to_string(store.root.join("manager_mail").join(file)).ok()
        }
        _ => None,
    };
    let text = match delivery_text(settings, store, task, block) {
        Ok(text) => decide::addressed(task, plan, &text),
        Err(error) => {
            log(&format!(
                "a block in {name} names a stored email that cannot be read: {error}"
            ));
            return false;
        }
    };
    let sent_s = crate::replies::now_s();
    let outcome = attempt(
        settings,
        task,
        plan,
        &text,
        reply.as_deref(),
        trace.as_ref(),
    );
    if let (Outcome::Delivered, Source::Mail(_)) = (&outcome, decide::source_of(&block[0]))
        && let Err(error) = crate::replies::expect(
            &store.state_dir,
            &plan.target,
            crate::replies::HUMAN,
            sent_s,
        )
    {
        log(&error);
    }
    let settled = decide::settle(task, plan, &block[0], &outcome);
    // 🧑 "log all the events (resolution requested, etc.) and who did those"
    let (kind, mut data) = match &outcome {
        Outcome::Delivered => (
            "message delivered",
            serde_json::json!({"to": plan.target, "block": block[0]}),
        ),
        Outcome::Rerouted { error } => (
            "message rerouted",
            serde_json::json!({"to": plan.fallback, "block": block[0], "error": error}),
        ),
        Outcome::Unreachable { error, second } => (
            "message failed",
            serde_json::json!({"to": plan.target, "block": block[0], "error": error, "fallback_error": second}),
        ),
    };
    if let Some(id) = zulip_id {
        data["zulip_message_id"] = serde_json::json!(id);
        data["elapsed_ms"] = serde_json::json!(began.elapsed().as_secs_f64() * 1000.0);
    }
    if let Err(error) = crate::log::event(&store.state_dir, kind, name, data) {
        log(&error);
    }
    for line in &settled.logs {
        log(line);
    }
    if let Some((key, text)) = &settled.human {
        tell_human_once(settings, store, task, key, text);
    }
    if let Some(mail) = &settled.awaiting {
        let row = format!("{:.0} {name} {mail} 0\n", epoch_s());
        let _awaiting = AWAITING_LOCK.lock();
        if let Err(error) = append(store, AWAITING, &row) {
            log(&format!("{error}; the answer to {mail} will not be chased"));
        }
    }
    settled.delivered
}

impl MailIntake {
    /// Take in the human's and the guest's unread mail on the kept connection, logging in again after a failure.
    fn take_in(&mut self, settings: &Settings, store: &Store) {
        let rules = match crate::mail_routes::rules(settings) {
            Ok(rules) => rules,
            Err(error) => {
                log(&format!("mail routing configuration rejected: {error}"));
                Vec::new()
            }
        };
        if rules.iter().any(|rule| rule.mailbox == "human") {
            if let Link::Down { retry_at } = self.human_link
                && Instant::now() >= retry_at
            {
                match Mailbox::human(settings) {
                    Ok(mailbox) => self.human_link = Link::Up(mailbox),
                    Err(error) => {
                        log(&format!("human route mailbox failed: {error}"));
                        self.human_link = Link::Down {
                            retry_at: Instant::now() + RECONNECT,
                        };
                    }
                }
            }
            if let Link::Up(mailbox) = &mut self.human_link
                && let Err(error) = route_mail(store, mailbox, "human", &rules)
            {
                log(&format!("human email routing failed: {error}"));
                self.human_link = Link::Down {
                    retry_at: Instant::now() + RECONNECT,
                };
            }
        }
        if let Link::Down { retry_at } = self.link
            && Instant::now() >= retry_at
        {
            match Mailbox::agent(settings) {
                Ok(mailbox) => self.link = Link::Up(mailbox),
                Err(error) => self.disconnected(&error),
            }
        }
        let Link::Up(mailbox) = &mut self.link else {
            return;
        };
        let taken = route_mail(store, mailbox, "agent", &rules)
            .map_err(IntakeError::Connection)
            .and_then(|routed| {
                take_in_mail(settings, store, mailbox, &routed).map_err(IntakeError::Connection)?;
                take_in_guest_mail(settings, store, mailbox, &routed)
            });
        match taken {
            Ok(()) => {}
            Err(IntakeError::Connection(error)) => self.disconnected(&error),
            Err(IntakeError::Other(error)) => log(&format!("mail intake failed: {error}")),
        }
    }

    fn disconnected(&mut self, error: &str) {
        log(&format!(
            "mail connection failed, reconnecting in {} s: {error}",
            RECONNECT.as_secs()
        ));
        self.link = Link::Down {
            retry_at: Instant::now() + RECONNECT,
        };
    }
}

/// Poll email independently so IMAP waits do not stall Zulip pickup or delivery.
fn poll_mail() {
    let mut intake = MailIntake {
        link: Link::Down {
            retry_at: Instant::now(),
        },
        human_link: Link::Down {
            retry_at: Instant::now(),
        },
    };
    loop {
        match settings::load() {
            Ok(settings) => {
                let began = Instant::now();
                intake.take_in(&settings, &Store::new(&settings));
                log_elapsed("IMAP intake", began);
            }
            Err(error) => log(&format!("mail settings failed: {error}")),
        }
        thread::sleep(MAIL_POLL);
    }
}

impl Watcher {
    /// Deliver queued typed/guest blocks and stored human messages, then mark their rows sent.
    ///
    /// A block in a task that has no agent (done or retired) goes to that task's manager, else to the main manager.
    fn deliver_pending(&mut self, settings: &Settings, store: &Store) -> Result<(), String> {
        let database = crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
        let through = database.latest_event_id()?;
        if self.pending_events == Some(through) && self.failed_at.is_empty() {
            return Ok(());
        }
        let tasks = store.all()?;
        let agent_at = decide::agent_addresses(&tasks);
        let retry = Duration::from_secs(decide::RETRY_S);
        let mut queued = database.pending_messages("block")?;
        queued.extend(database.pending_messages("human")?);
        for message in &queued {
            let (Some(id), Some(name), Some(mail)) = (
                message["id"].as_i64(),
                message["target"].as_str(),
                message["text"].as_str(),
            ) else {
                continue;
            };
            if self
                .failed_at
                .get(name)
                .is_some_and(|failed| failed.elapsed() < retry)
            {
                continue;
            }
            let Some(task) = tasks.iter().find(|task| task.name == name) else {
                log(&format!(
                    "{mail} is queued for {name}, which does not exist"
                ));
                continue;
            };
            let plan = decide::plan(task, &agent_at, settings::get(settings, "main_manager"));
            let block: Vec<String> = if message["channel"] == "block" {
                mail.split('\n').map(str::to_owned).collect()
            } else {
                vec![decide::mail_line(mail)]
            };
            let file = Path::new(mail).file_name().unwrap_or_default();
            let stored =
                fs::read_to_string(store.root.join("manager_mail").join(file)).unwrap_or_default();
            let (_, own) = stored.split_once("\n\n").unwrap_or_default();
            let own = if mail.starts_with("zulip-") {
                crate::zulip::own_text(own)
            } else {
                own.to_owned()
            };
            if message["channel"] == "human"
                && decide::replace_order(&own)
                && plan.target == task.address()
                && task.address().starts_with(omnigent::PREFIX)
            {
                match crate::agents::replace(
                    settings,
                    store,
                    name,
                    &decide::mail_text(mail, &stored, task.tag(), true, false),
                    None,
                    "on the human's order",
                ) {
                    Ok(new) => {
                        log(&format!(
                            "{name}: replaced its agent with {new} on the human's order {mail}"
                        ));
                        let _lock = store.lock()?;
                        if let Err(error) = database.finish_message(id, true, "") {
                            log(&error);
                        }
                    }
                    Err(error) => {
                        // A fresh agent that started but did not show its prompt is the replacement; never start a second one.
                        let started = store
                            .load(name)
                            .is_ok_and(|fresh| fresh.address() != task.address());
                        log(&format!(
                            "{name}: replacing its agent on {mail} failed{}: {error}",
                            if started {
                                " after the new agent started; not retried"
                            } else {
                                ", retried later"
                            }
                        ));
                        if started {
                            let _lock = store.lock()?;
                            if let Err(error) = database.finish_message(id, true, "") {
                                log(&error);
                            }
                        } else {
                            self.failed_at.insert(name.to_owned(), Instant::now());
                        }
                    }
                }
                continue;
            }
            if deliver_block(settings, store, task, &plan, &block) {
                self.failed_at.remove(name);
                let _lock = store.lock()?;
                // `amh stop` may have dropped the message meanwhile; a crash before this line delivers it again.
                if let Err(error) = database.finish_message(id, true, "") {
                    log(&error);
                }
                crate::actions::clear_stopped(store, &plan.target)?;
            } else {
                database.finish_message(
                    id,
                    false,
                    "delivery did not settle; see delivery events",
                )?;
                self.failed_at.insert(name.to_owned(), Instant::now());
            }
        }
        self.failed_at
            .retain(|name, _| queued.iter().any(|message| message["target"] == *name));
        self.pending_events = Some(through);
        Ok(())
    }
}

/// Answer yes to permission prompts that agents wait on, and tell each agent's manager what was approved.
// 🧑 "Sounds good, but should not be the default, which is no question any time at all"
fn answer_prompts(settings: &Settings, store: &Store) -> Result<(), String> {
    for task in store.active()? {
        if task.address() == "retired" || omnigent::status(settings, task.address()).0 != "stuck" {
            continue;
        }
        ask_human(settings, store, &task);
        omnigent::forget(task.address());
        let answered = omnigent::approve_prompts(settings, task.address()).and_then(|approved| {
            for prompt in &approved {
                log(&format!(
                    "answered yes for {} at {}: {prompt}",
                    task.name,
                    task.address()
                ));
            }
            if approved.is_empty() {
                return Ok(());
            }
            let manager = task.field("managerat").unwrap_or_default();
            omnigent::send(settings, manager, &decide::approved_text(&task, &approved))
        });
        if let Err(error) = answered {
            log(&format!(
                "answering the prompt of {} failed: {error}",
                task.name
            ));
        }
    }
    Ok(())
}

/// Send the human each question with choices that an agent waits on, once.
// 🧑 "Choices like that should have been automatically sent to me."
fn ask_human(settings: &Settings, store: &Store, task: &Task) {
    let waiting = match omnigent::pending_questions(settings, task.address()) {
        Ok(waiting) => waiting,
        Err(error) => {
            return log(&format!(
                "reading the question of {} failed: {error}",
                task.name
            ));
        }
    };
    for (elicitation, parts) in waiting {
        if record(store, ASKED_HUMAN)
            .lines()
            .any(|asked| asked == elicitation)
        {
            continue;
        }
        let body = decide::question_text(&task.name, &parts);
        let sent = mail::send(
            settings,
            &store.state_dir,
            task.tag(),
            "Agent question",
            &body,
            mail::WATCHER,
            &[],
        );
        let noted = sent.and_then(|_| append(store, ASKED_HUMAN, &format!("{elicitation}\n")));
        match noted {
            Ok(()) => log(&format!("sent the human the question of {}", task.name)),
            Err(error) => log(&format!("question of {} was not sent: {error}", task.name)),
        }
    }
}

/// Make sure each delivered human email gets an answer: remind the agent after `ANSWER_S`, then tell its manager and the human.
// 🧑 "if I message an agent and somehow they are not whatever state they are, I should get some sort of response"
fn chase_answers(settings: &Settings, store: &Store) -> Result<(), String> {
    let all_rows = record(store, AWAITING);
    let awaiting = all_rows.clone();
    if awaiting.is_empty() {
        return Ok(());
    }
    let active = store.active()?;
    let sent = store.record(mail::SENT_LOG)?;
    let paused: BTreeSet<&str> = active
        .iter()
        .filter(|task| omnigent::status(settings, task.address()).0 == "paused")
        .map(|task| task.name.as_str())
        .collect();
    let (held, ready): (Vec<&str>, Vec<&str>) = awaiting.lines().partition(|row| {
        row.split_whitespace()
            .nth(1)
            .is_some_and(|name| paused.contains(name))
    });
    let awaiting = ready.join("\n");
    let mut keep: Vec<String> = held.into_iter().map(str::to_owned).collect();
    let mail_dir = settings::work_logs(settings).join("manager_mail");
    for chase in decide::chase(&awaiting, &sent, &active, epoch_s(), hour_minute, &mail_dir.to_string_lossy()) {
        let failed = |task: &str, error: String| {
            log(&format!("chasing an answer from {task} failed: {error}"));
        };
        match chase {
            Chase::Keep(row) => keep.push(row),
            Chase::Remind {
                task,
                address,
                text,
                row,
            } => match omnigent::send(settings, &address, &text) {
                Ok(()) => keep.push(row),
                Err(error) => failed(&task, error),
            },
            Chase::Escalate {
                task,
                manager,
                text,
                human,
            } => {
                if let Err(error) = omnigent::send(settings, &manager, &text) {
                    failed(&task, error);
                }
                let found = active.iter().find(|known| known.name == task);
                if let (Some((key, text)), Some(task)) = (human, found) {
                    tell_human_once(settings, store, task, &key, &text);
                }
            }
        }
    }
    let _awaiting = AWAITING_LOCK.lock();
    // Rows delivery added while this chase ran stay.
    let original: BTreeSet<String> = record(store, AWAITING).lines().map(str::to_owned).collect();
    let before: BTreeSet<&str> = all_rows.lines().collect();
    keep.extend(
        original
            .into_iter()
            .filter(|row| !before.contains(row.as_str())),
    );
    let rows: String = keep.iter().map(|row| format!("{row}\n")).collect();
    store.set_record(AWAITING, &rows)
}

/// Every minute: answer permission prompts, nudge idle or failed agents, chase awaited answers.
fn scan_agents() {
    let mut scanner = Scanner {
        started: Instant::now(),
        reminded_s: BTreeMap::new(),
        stalls: BTreeMap::new(),
        stalls_loaded: false,
    };
    loop {
        let began = Instant::now();
        let scanned = settings::load().and_then(|settings| {
            let store = Store::new(&settings);
            let addresses: Vec<String> = store
                .active()?
                .iter()
                .map(|task| task.address().to_owned())
                .collect();
            omnigent::begin_scan(&settings, &addresses);
            let scanned = answer_prompts(&settings, &store)
                .and_then(|()| scanner.nudge(&settings, &store))
                .and_then(|()| chase_answers(&settings, &store));
            omnigent::end_scan();
            scanned
        });
        if let Err(error) = scanned {
            log(&format!("agent scan failed: {error}"));
        }
        thread::sleep(NUDGE_EVERY.saturating_sub(began.elapsed()));
    }
}

/// Tell the main manager `notice` once per `token`, remembered under the record `key`; an empty token clears it so the next problem is told again.
fn notify_manager(
    settings: &Settings,
    store: &Store,
    key: &str,
    token: &str,
    notice: &str,
) -> Result<(), String> {
    if token == record(store, key) {
        return Ok(());
    }
    if !token.is_empty() {
        omnigent::send(settings, settings::get(settings, "main_manager"), notice)?;
        log(&format!("told the main manager: {notice}"));
    }
    store.set_record(key, token)
}

/// The notice for files `amh` refuses to commit for exceeding the size limit.
fn oversize_notice(files: &[(String, u64)]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let list: String = files
        .iter()
        .map(|(path, bytes)| format!("{path} ({} MB)\n", bytes >> 20))
        .collect();
    format!(
        "amh will not commit these work-log files because each is over {} MB; shrink, split, or gitignore them:\n{list}",
        crate::history::MAX_BYTES >> 20
    )
}

/// Tell the task named by `compress.task` to compress the human's mailbox when too many agent emails wait unread there.
fn notify_compression(settings: &Settings, store: &Store) -> Result<(), String> {
    let name = settings::get(settings, "compress.task");
    if name.is_empty() {
        return Ok(());
    }
    let number =
        |key: &str, default: usize| settings::get(settings, key).parse().unwrap_or(default);
    let (over, urgent) = (
        number("compress.unread_over", 16),
        number("compress.unread_urgent", 30),
    );
    // The human manager: "the compression notice counts PB news and PB urgent emails (shown as '(no tag)'). The compression rules exclude them"
    let counted: Vec<String> = if crate::zulip::enabled(settings) {
        let waiting = crate::zulip::unread(settings, &store.state_dir)?;
        // The human manager: "the watcher counts 'pending item deleted' notices although its text says it does not."
        let notice = |content: &str| {
            let body = content.split_once("\n\n").map_or("", |(_, body)| body);
            body.starts_with("pending item ")
        };
        let counted = waiting
            .into_iter()
            .filter(|(_, _, content)| !notice(content));
        counted
            .map(|(_, topic, _)| topic)
            .filter(|topic| !mail::REPORT_LABELS.contains(&topic.as_str()))
            .collect()
    } else {
        // Every email an agent sends through `amh` has a task tag, so an untagged one is not the human manager's to compress.
        let unread = mail::unread_all(settings)?;
        let countable = unread
            .iter()
            .filter(|header| decide::countable(&header.sender));
        countable
            .filter_map(|header| mail::tag_of(&header.subject).map(str::to_owned))
            .collect()
    };
    let noted = record(store, COMPRESSED);
    let mut parts = noted.split_whitespace().map(str::parse::<u64>);
    let last = match (parts.next(), parts.next()) {
        (Some(Ok(at)), Some(Ok(count))) => Some((at, count as usize)),
        _ => None,
    };
    let now_s = epoch_s() as u64;
    if counted.len() <= over {
        // Below the threshold the record goes, so the next pile-up is noticed at once.
        if !noted.is_empty() {
            store.set_record(COMPRESSED, "")?;
        }
        return Ok(());
    }
    if !decide::compress_due(counted.len(), over, urgent, last, now_s) {
        return Ok(());
    }
    let task = store.load(name)?;
    omnigent::send(
        settings,
        task.address(),
        &decide::compress_text(counted.len(), over, &counted),
    )?;
    store.set_record(COMPRESSED, &format!("{now_s} {}\n", counted.len()))?;
    log(&format!(
        "told {name} to compress the mailbox: {} unread",
        counted.len()
    ));
    Ok(())
}

/// Loop forever; the service manager restarts the process if it dies. Returns the exit status when the settings cannot be read.
pub fn run() -> u8 {
    let mut watcher = Watcher {
        failed_at: BTreeMap::new(),
        pending_events: None,
    };
    let mut jobs = crate::jobs::Jobs::default();
    let mut last_flush: Option<Instant> = None;
    let mut last_compress: Option<Instant> = None;
    let mut last_archive: Option<Instant> = None;
    let mut last_zulip_cleanup: Option<Instant> = None;
    let mut last_commit: Option<Instant> = None;
    let mut last_poll: Option<Instant> = None;
    let mut last_state_profile: Option<Instant> = None;
    // Only one watcher may run: request notifications are read and sent without the amh lock, so a second watcher would send them twice.
    let _single = match watcher_lock(&settings::state_dir()) {
        Ok(lock) => lock,
        Err(error) => {
            log(&error);
            return 1;
        }
    };
    let wake = match crate::wake::listen(&settings::state_dir()) {
        Ok(socket) => Some(socket),
        Err(error) => {
            log(&format!(
                "event wakeups unavailable; using polling: {error}"
            ));
            None
        }
    };
    omnigent::NO_WAIT.store(true, std::sync::atomic::Ordering::Relaxed);
    log("watcher started");
    std::thread::spawn(scan_agents);
    std::thread::spawn(poll_mail);
    loop {
        let settings = match settings::load() {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("amh: {error}");
                return 1;
            }
        };
        let store = Store::new(&settings);
        if last_archive.is_none_or(|last| last.elapsed().as_secs() >= 86400) {
            let phase = Instant::now();
            let archived = store.lock().and_then(|_lock| {
                let mut database =
                    crate::database::Database::open(&store.state_dir.join("amh.sqlite3"))?;
                crate::archive::archive_events(
                    &mut database,
                    &store.root.join("archive"),
                    chrono::Utc::now(),
                )?;
                let count = crate::archive::archive_requests(&store, chrono::Utc::now())?;
                store.refresh_exports()?;
                Ok(count)
            });
            match archived {
                Ok(_) => last_archive = Some(Instant::now()),
                Err(error) => log(&format!("archival failed: {error}")),
            }
            log_elapsed("archive", phase);
        }
        if settings::get(&settings, "archive.commit") != "false"
            && last_commit.is_none_or(|last| last.elapsed() >= COMMIT_EVERY)
        {
            let phase = Instant::now();
            last_commit = Some(Instant::now());
            // A failed run says nothing about oversize files or merges, so their records stay as they are; one failure streak is told once.
            let mut notices: Vec<(&str, String, String)> = Vec::new();
            match crate::history::commit_and_push(&store.root) {
                Ok(done) => {
                    let oversize = oversize_notice(&done.oversize);
                    notices.push(("oversize-notified", oversize.clone(), oversize));
                    notices.push(("push-failure-notified", String::new(), String::new()));
                    if !done.notes.is_empty() {
                        let text = format!("amh merge notes:\n{}", done.notes.join("\n"));
                        notices.push(("merge-notes-notified", text.clone(), text));
                    }
                }
                Err(error) => {
                    log(&format!("Markdown commit failed: {error}"));
                    notices.push((
                        "push-failure-notified",
                        "failing".to_owned(),
                        format!("amh could not commit or push the work logs: {error}"),
                    ));
                }
            }
            let filing = record(&store, "filing-problems");
            notices.push((
                "filing-problems-notified",
                filing.clone(),
                format!("amh left these task files unfiled or unreconciled:\n{filing}"),
            ));
            for (key, token, notice) in &notices {
                if let Err(error) = notify_manager(&settings, &store, key, token, notice) {
                    log(&format!("{key} notice failed: {error}"));
                }
            }
            log_elapsed("git commit", phase);
        }
        if last_poll.is_none_or(|last| last.elapsed() >= MAIL_POLL) {
            let phase = Instant::now();
            if crate::zulip::enabled(&settings)
                && let Err(error) = take_in_zulip(&settings, &store)
            {
                log(&format!("Zulip intake failed: {error}"));
            }
            log_elapsed("Zulip intake", phase);
            last_poll = Some(Instant::now());
        }
        let phase = Instant::now();
        let request_effects = store
            .lock_timed()
            .and_then(|(_lock, timing)| {
                let computing = Instant::now();
                let result = crate::actions::request_effects(&store);
                if phase.elapsed() >= Duration::from_secs(1)
                    || last_state_profile.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                    last_state_profile = Some(Instant::now());
                    log(&format!("request state timing: acquire_ms={:.3} import_ms={:.3} export_ms={:.3} dependencies_ms={:.3}",
                        timing.acquire_ms, timing.import_ms, timing.export_ms,
                        computing.elapsed().as_secs_f64() * 1000.0));
                }
                result
            });
        if let Err(error) = request_effects {
            log(&format!("request effects failed: {error}"));
        }
        log_elapsed("request state updates", phase);
        let phase = Instant::now();
        let scanned = watcher.deliver_pending(&settings, &store);
        log_elapsed("message delivery", phase);
        let phase = Instant::now();
        for line in crate::replies::forward(&settings, &store) {
            log(&line);
        }
        log_elapsed("turn replies", phase);
        let phase = Instant::now();
        if let Err(error) = crate::actions::deliver_requests(&settings, &store) {
            log(&format!("request notifications failed: {error}"));
        }
        log_elapsed("request notifications", phase);
        if last_compress.is_none_or(|last| last.elapsed() > COMPRESS_EVERY) {
            let phase = Instant::now();
            last_compress = Some(Instant::now());
            if let Err(error) = notify_compression(&settings, &store) {
                log(&format!("mailbox compression check failed: {error}"));
            }
            log_elapsed("compression check", phase);
        }
        if last_flush.is_none_or(|last| last.elapsed() > FLUSH_EVERY) {
            let phase = Instant::now();
            last_flush = Some(Instant::now());
            if crate::zulip::enabled(&settings)
                && let Err(error) = crate::zulip::replace(&settings, &[])
            {
                log(&format!("queued Zulip deletion failed: {error}"));
            }
            for line in mail::flush_outbox(&settings, &store.state_dir) {
                log(&line);
            }
            if crate::zulip::enabled(&settings) {
                for line in crate::zulip::drain_pending(&settings, &store.state_dir) {
                    log(&line);
                }
            }
            log_elapsed("outbox flush", phase);
        }
        let phase = Instant::now();
        for line in jobs.tick(&settings, &store) {
            log(&line);
        }
        log_elapsed("scheduled jobs", phase);
        if crate::zulip::enabled(&settings)
            && last_zulip_cleanup.is_none_or(|last| last.elapsed().as_secs() >= 86400)
        {
            let phase = Instant::now();
            last_zulip_cleanup = Some(Instant::now());
            match crate::zulip_cleanup::run(&settings, &store) {
                Ok(summary) => log(&summary),
                Err(error) => log(&format!("Zulip cleanup failed: {error}")),
            }
            log_elapsed("Zulip cleanup", phase);
        }
        if let Err(error) = scanned {
            log(&format!("task scan failed: {error}"));
        }
        let timeout = last_poll.map_or(MAIL_POLL, |last| MAIL_POLL.saturating_sub(last.elapsed()));
        match &wake {
            Some(socket) => {
                if let Err(error) = crate::wake::wait(socket, timeout) {
                    log(&format!("event wakeup failed; polling: {error}"));
                    thread::sleep(timeout);
                }
            }
            None => thread::sleep(timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_watcher_holds_the_lock() {
        let dir = std::env::temp_dir().join(format!("amh-watch-lock-{}", std::process::id()));
        let first = watcher_lock(&dir).unwrap();
        assert!(watcher_lock(&dir).unwrap_err().contains("already running"));
        drop(first);
        assert!(watcher_lock(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
