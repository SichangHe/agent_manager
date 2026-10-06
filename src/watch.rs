//! The one background watcher: takes in the human's and the guest's email, delivers `(pending)` blocks, and nudges managers and idle agents.
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
use crate::taskfile::{self, MARKER};
use crate::tasks::Task;
use crate::{guest, omnigent, settings};

// 🧑 "Why so slow ... Improve it"
const MAIL_POLL: Duration = Duration::from_secs(5);
const RECONNECT: Duration = Duration::from_secs(30);
/// How often queued emails are tried again.
const FLUSH_EVERY: Duration = Duration::from_secs(600);
/// How often the human's unread agent emails are counted.
const COMPRESS_EVERY: Duration = Duration::from_secs(300);
/// The record in the state folder of the last notice to compress: `EPOCH COUNT`.
const COMPRESSED: &str = "amh-compress.txt";
const NUDGE_EVERY: Duration = Duration::from_secs(60);
const AWAITING: &str = "amh-awaiting.txt";
const TOLD_HUMAN: &str = "amh-told-human.txt";
const PROBLEM_NOTICES: &str = "amh-problem-notices.txt";

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
    started: Instant,
    link: Link,
    /// When the delivery of a task's blocks last reached nobody.
    failed_at: BTreeMap<String, Instant>,
    /// When each task's idle agent was last reminded, in seconds since `started`.
    reminded_s: BTreeMap<String, u64>,
    last_nudge: Option<Instant>,
}

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

/// The text of a record in the state folder; empty when there is none yet.
fn record(store: &Store, name: &str) -> String {
    fs::read_to_string(store.state_dir.join(name)).unwrap_or_default()
}

fn append(store: &Store, name: &str, line: &str) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    let opened = options
        .create(true)
        .append(true)
        .open(store.state_dir.join(name));
    opened
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .map_err(|error| format!("cannot write {name}: {error}"))
}

fn rewrite(store: &Store, name: &str, text: &str) -> Result<(), String> {
    taskfile::write(&store.state_dir.join(name), text)
        .map_err(|error| format!("cannot write {name}: {error}"))
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

/// Store one human email and append a `(pending)` block naming it to the right task file; returns that task file's name.
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
    let file = fs::read_to_string(store.root.join(task))
        .map_err(|error| format!("cannot read {task}: {error}"))?;
    store.write(task, &decide::queued(&file, name))?;
    Ok(task.clone())
}

/// Take in each new human email; one that cannot be routed stays unread and unstored.
fn take_in_mail(settings: &Settings, store: &Store, mailbox: &mut Mailbox) -> Result<(), String> {
    let prefix = match settings::get(settings, "mail.stored_prefix") {
        "" => "mail",
        prefix => prefix,
    };
    for (uid, raw) in mailbox.unseen_from(settings::get(settings, "mail.human_address"))? {
        let mail = mail::parse(&raw);
        let name = format!("{prefix}-{uid}.txt");
        let stored = store.root.join("manager_mail").join(&name);
        let subject = decide::quoted(&mail.subject);
        // A stored file means the email was taken in before and only marking it read failed.
        if !stored.exists() {
            match take_in(settings, store, &stored, &name, &mail) {
                Ok(task) => log(&format!("mail {name} {subject} -> {task}")),
                Err(error) => {
                    let _ = fs::remove_file(&stored);
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

/// Reuse the guest agent when it is alive; otherwise start one under the main manager.
// 🧑 "Make it lazy. Make the watcher start an agent. If there's no agent for that guest or reuse an agent if they already exist."
fn ensure_guest_agent(settings: &Settings, store: &Store, address: &str) -> Result<(), String> {
    let name = guest::task_name(settings);
    if store.root.join(&name).exists() && agents::has_live_agent(settings, &store.load(&name)?) {
        return Ok(());
    }
    let new = NewTask {
        name: &name,
        workdir: settings::get(settings, "guest.workdir"),
        goal: &guest::goal(address),
        manager: settings::get(settings, "main_manager"),
        tool: None,
        model: None,
        effort: None,
        as_manager: false,
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
    store.save(&task)
}

/// Take in each new guest email; one whose sender is not verified is dropped.
fn take_in_guest_mail(
    settings: &Settings,
    store: &Store,
    mailbox: &mut Mailbox,
) -> Result<(), IntakeError> {
    let address = settings::get(settings, "guest.address");
    if address.is_empty() {
        return Ok(());
    }
    let unseen = mailbox.unseen_from(address);
    for (uid, raw) in unseen.map_err(IntakeError::Connection)? {
        let mail = mail::parse(&raw);
        let stored = guest::stored_name(settings, &uid);
        if !guest::verified(&mail, address) {
            println!("guest mail uid {uid} rejected: the sender is not verified as {address}");
        } else if !store
            .root
            .join(guest::mail_dir(settings))
            .join(&stored)
            .exists()
        {
            store_guest_mail(settings, store, address, &mail, &stored)
                .map_err(IntakeError::Other)?;
        }
        mailbox.mark_read(&uid).map_err(IntakeError::Connection)?;
    }
    Ok(())
}

/// The message for an agent from a pending block; a stored human email is inlined verbatim.
fn delivery_text(settings: &Settings, store: &Store, block: &[String]) -> Result<String, String> {
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
            decide::mail_text(name, &stored, remind)
        }
        Source::Plain => decide::plain_text(block, remind),
    })
}

/// Try to deliver one message: to the target, else to the fallback manager.
fn attempt(settings: &Settings, task: &Task, plan: &Plan, text: &str) -> Outcome {
    let (state, evidence) = omnigent::status(settings, &plan.target);
    // A missing or stuck agent reads no messages, so its mail goes to its manager.
    let sent = match state {
        "missing" | "stuck" => Err(format!("the agent is {state}: {evidence}")),
        _ => omnigent::send(settings, &plan.target, text),
    };
    let Err(error) = sent else {
        return Outcome::Delivered;
    };
    let second = match &plan.fallback {
        Some(manager) => omnigent::send(settings, manager, &decide::rerouted(task, &error, text)),
        None => Err("no manager to fall back to".to_owned()),
    };
    match second {
        Ok(()) => Outcome::Rerouted { error },
        Err(second) => Outcome::Unreachable { error, second },
    }
}

/// Deliver one pending block of a task and delete its marker line; `false` when the task is to be retried later.
fn deliver_block(
    settings: &Settings,
    store: &Store,
    task: &Task,
    plan: &Plan,
    block: &[String],
) -> Result<bool, String> {
    let name = &task.name;
    let text = match delivery_text(settings, store, block) {
        Ok(text) => decide::addressed(task, plan, &text),
        Err(error) => {
            log(&format!(
                "a block in {name} names a stored email that cannot be read: {error}"
            ));
            return Ok(false);
        }
    };
    let settled = decide::settle(task, plan, &block[0], &attempt(settings, task, plan, &text));
    for line in &settled.logs {
        log(line);
    }
    if let Some((key, text)) = &settled.human {
        tell_human_once(settings, store, task, key, text);
    }
    if let Some(mail) = &settled.awaiting {
        let row = format!("{:.0} {name} {mail} 0\n", epoch_s());
        if let Err(error) = append(store, AWAITING, &row) {
            log(&format!("{error}; the answer to {mail} will not be chased"));
        }
    }
    if !settled.delivered {
        return Ok(false);
    }
    let _lock = store.lock()?;
    let mut fresh = store.load(name)?;
    match decide::without_marker(&fresh.body, &block[0]) {
        Some(body) => {
            fresh.body = body;
            store.save(&fresh).map(|()| true)
        }
        None => {
            log(&format!(
                "delivered block is no longer in {name}; its marker was not deleted"
            ));
            Ok(false)
        }
    }
}

impl Watcher {
    /// Take in the human's and the guest's unread mail on the kept connection, logging in again after a failure.
    fn take_in(&mut self, settings: &Settings, store: &Store) {
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
        let taken = take_in_mail(settings, store, mailbox)
            .map_err(IntakeError::Connection)
            .and_then(|()| take_in_guest_mail(settings, store, mailbox));
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

    /// Send every pending block to its task's agent, then delete the marker line.
    ///
    /// A block in a task that has no agent (done or retired) goes to that task's manager, else to the main manager.
    fn deliver_pending(&mut self, settings: &Settings, store: &Store) -> Result<(), String> {
        let tasks = store.all()?;
        let agent_at = decide::agent_addresses(&tasks);
        let retry = Duration::from_secs(decide::RETRY_S);
        for task in &tasks {
            let waits = |failed: &Instant| failed.elapsed() < retry;
            if !task.body.contains(MARKER) || self.failed_at.get(&task.name).is_some_and(waits) {
                continue;
            }
            let plan = decide::plan(task, &agent_at, settings::get(settings, "main_manager"));
            for (_, block) in taskfile::pending_blocks(&task.body) {
                if !block.is_empty() && !deliver_block(settings, store, task, &plan, &block)? {
                    self.failed_at.insert(task.name.clone(), Instant::now());
                    break;
                }
            }
        }
        Ok(())
    }

    /// Remind idle agents of their open items; report each agent failure once to its manager and, for harness errors, to the human.
    fn nudge(&mut self, settings: &Settings, store: &Store) -> Result<(), String> {
        let reported: BTreeSet<String> = record(store, PROBLEM_NOTICES)
            .lines()
            .map(str::to_owned)
            .collect();
        let found = agents::problems(settings, store)?;
        let now_s = self.started.elapsed().as_secs();
        let decided = decide::nudges(&found, &reported, &store.active()?, &self.reminded_s, now_s);
        let Some((nudges, mut now)) = decided else {
            return Ok(());
        };
        let mut by_manager: Vec<(String, Vec<String>)> = Vec::new();
        for nudge in nudges {
            match nudge {
                Nudge::Remind {
                    task,
                    address,
                    text,
                } => {
                    if let Err(error) = omnigent::send(settings, &address, &text) {
                        log(&format!("reminder to {task} failed: {error}"));
                    }
                    self.reminded_s.insert(task, now_s);
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
                    match by_manager.iter_mut().find(|(known, _)| *known == manager) {
                        Some((_, texts)) => texts.push(text),
                        None => by_manager.push((manager, vec![text])),
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
        for (manager, texts) in by_manager {
            if let Err(error) = omnigent::send(settings, &manager, &decide::problems_text(&texts)) {
                log(&format!("problem notice to {manager} failed: {error}"));
            }
        }
        let keys: String = now.iter().map(|key| format!("{key}\n")).collect();
        rewrite(store, PROBLEM_NOTICES, &keys)
    }
}

/// Answer yes to permission prompts that agents wait on, and tell each agent's manager what was approved.
// 🧑 "Sounds good, but should not be the default, which is no question any time at all"
fn answer_prompts(settings: &Settings, store: &Store) -> Result<(), String> {
    for task in store.active()? {
        if task.address() == "retired" || omnigent::status(settings, task.address()).0 != "stuck" {
            continue;
        }
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

/// Make sure each delivered human email gets an answer: remind the agent after `ANSWER_S`, then tell its manager and the human.
// 🧑 "if I message an agent and somehow they are not whatever state they are, I should get some sort of response"
fn chase_answers(settings: &Settings, store: &Store) -> Result<(), String> {
    if !store.state_dir.join(AWAITING).exists() {
        return Ok(());
    }
    let active = store.active()?;
    let (awaiting, sent) = (record(store, AWAITING), record(store, mail::SENT_LOG));
    let mut keep = Vec::new();
    for chase in decide::chase(&awaiting, &sent, &active, epoch_s(), hour_minute) {
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
    let rows: String = keep.iter().map(|row| format!("{row}\n")).collect();
    rewrite(store, AWAITING, &rows)
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
    let unread = mail::unread_all(settings)?;
    let counted: Vec<&mail::Header> = unread
        .iter()
        .filter(|header| decide::countable(&header.sender))
        .collect();
    let path = store.state_dir.join(COMPRESSED);
    let noted = fs::read_to_string(&path).unwrap_or_default();
    let mut parts = noted.split_whitespace().map(str::parse::<u64>);
    let last = match (parts.next(), parts.next()) {
        (Some(Ok(at)), Some(Ok(count))) => Some((at, count as usize)),
        _ => None,
    };
    let now_s = epoch_s() as u64;
    if counted.len() <= over {
        // Below the threshold the record goes, so the next pile-up is noticed at once.
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    if !decide::compress_due(counted.len(), over, urgent, last, now_s) {
        return Ok(());
    }
    let tags: Vec<String> = counted
        .iter()
        .map(|header| {
            mail::tag_of(&header.subject)
                .unwrap_or("(no tag)")
                .to_owned()
        })
        .collect();
    let task = store.load(name)?;
    omnigent::send(
        settings,
        task.address(),
        &decide::compress_text(counted.len(), over, &tags),
    )?;
    fs::write(&path, format!("{now_s} {}\n", counted.len()))
        .map_err(|error| format!("cannot write {COMPRESSED}: {error}"))?;
    log(&format!(
        "told {name} to compress the mailbox: {} unread",
        counted.len()
    ));
    Ok(())
}

/// Loop forever; the service manager restarts the process if it dies. Returns the exit status when the settings cannot be read.
pub fn run() -> u8 {
    let mut watcher = Watcher {
        started: Instant::now(),
        link: Link::Down {
            retry_at: Instant::now(),
        },
        failed_at: BTreeMap::new(),
        reminded_s: BTreeMap::new(),
        last_nudge: None,
    };
    let mut last_flush: Option<Instant> = None;
    let mut last_compress: Option<Instant> = None;
    log("watcher started");
    loop {
        let settings = match settings::load() {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("amh: {error}");
                return 1;
            }
        };
        let store = Store::new(&settings);
        watcher.take_in(&settings, &store);
        let mut scanned = watcher.deliver_pending(&settings, &store);
        if scanned.is_ok()
            && watcher
                .last_nudge
                .is_none_or(|last| last.elapsed() > NUDGE_EVERY)
        {
            watcher.last_nudge = Some(Instant::now());
            scanned = answer_prompts(&settings, &store)
                .and_then(|()| watcher.nudge(&settings, &store))
                .and_then(|()| chase_answers(&settings, &store));
        }
        if last_compress.is_none_or(|last| last.elapsed() > COMPRESS_EVERY) {
            last_compress = Some(Instant::now());
            if let Err(error) = notify_compression(&settings, &store) {
                log(&format!("mailbox compression check failed: {error}"));
            }
        }
        if last_flush.is_none_or(|last| last.elapsed() > FLUSH_EVERY) {
            last_flush = Some(Instant::now());
            for line in mail::flush_outbox(&settings, &store.state_dir) {
                log(&line);
            }
        }
        if let Err(error) = scanned {
            log(&format!("task scan failed: {error}"));
        }
        thread::sleep(MAIL_POLL);
    }
}
