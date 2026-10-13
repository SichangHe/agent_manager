//! What the watcher decides: where a human email goes, who gets a `(pending)` block and what follows from the attempt, which agent problems to report, and which unanswered emails to chase.
//!
//! Pure functions over plain data; `watch` does the mail, the agents, the files, and the clock around them.
// 🧑 "make only the logic part synchronous and let's also have the logic separate from IO"

use std::collections::{BTreeMap, BTreeSet};

use crate::taskfile;
use crate::tasks::Task;
use crate::work::{self, Problem};

/// The start of the line that names a stored human email.
pub const MAIL_SOURCE: &str = "(record and delegate manager_mail/";
pub const RETRY_S: u64 = 600;
pub const ANSWER_S: u64 = 900;
pub const REMINDER_S: u64 = 1800;
pub const ACCEPT: &str = "Immediately acknowledge acceptance to the Human and record every open item, in ONE call: `amh tell human --subject ... --add 'ITEM'` once per open item, quoting the human's words. Keep the Message-ID it prints: your next email replaces this acknowledgement with `--replaces ID` if the human has not read it by then:";
pub const ADD_REMINDER: &str = "\nAdd every still-open request to your open items.";

/// Text in quotes as the watcher log shows it.
pub fn quoted(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut shown = String::from(quote);
    for letter in text.chars() {
        match letter {
            '\\' => shown += "\\\\",
            '\n' => shown += "\\n",
            '\r' => shown += "\\r",
            '\t' => shown += "\\t",
            _ if letter == quote => shown.extend(['\\', letter]),
            _ => shown.push(letter),
        }
    }
    shown.push(quote);
    shown
}

/// Whether the human's own text starts or ends with `for manager`, `for a manager`, or `for the manager`, or starts by asking to terminate or replace the agent.
// 🧑 “We should only dispatch “for manager” type and replace/terminate type messages to managers.”
pub fn for_manager(body: &str) -> bool {
    let breaks = [
        '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
    ];
    let edges = [
        ' ', '\t', '.', ',', ':', ';', '!', '(', ')', '[', ']', '*', '_',
    ];
    let own: Vec<String> = body
        .split(breaks)
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('>'))
        .map(|line| line.trim_matches(edges).to_lowercase())
        .collect();
    let (Some(first), Some(last)) = (own.first(), own.last()) else {
        return false;
    };
    let phrases = ["for manager", "for a manager", "for the manager"];
    let word_edge = |rest: Option<char>| rest.is_none_or(|c| !c.is_alphanumeric());
    phrases.iter().any(|phrase| {
        first
            .strip_prefix(phrase)
            .is_some_and(|rest| word_edge(rest.chars().next()))
            || last
                .strip_suffix(phrase)
                .is_some_and(|rest| word_edge(rest.chars().next_back()))
    }) || first.starts_with("terminate this agent")
        || first.starts_with("replace this agent")
}

/// Whether an agent works on the task: it is active and not retired.
fn staffed(task: &Task) -> bool {
    task.is_active() && task.address() != "retired"
}

/// Pick the task a human email belongs to, among every task file: its `[tag]`, else that task's manager, else the main manager.
// 🧑 "it did not get routed to the correct Omnigent agent or even its task file"
pub fn route<'a>(
    tasks: &'a [Task],
    main_manager: &str,
    subject: &str,
    body: &str,
) -> Result<&'a Task, String> {
    let at = |address: &str| {
        let mut staffed = tasks.iter().rev().filter(|task| staffed(task));
        staffed.find(|task| task.address() == address)
    };
    let manager_of = |task: &Task| at(task.field("managerat").unwrap_or_default());
    let tagged = crate::mail::tag_of(subject)
        .map(crate::zulip::task_topic)
        .and_then(|tag| tasks.iter().find(|task| task.tag() == tag));
    let chosen = match tagged {
        Some(tagged) if staffed(tagged) && for_manager(body) => manager_of(tagged).or(Some(tagged)),
        Some(tagged) if staffed(tagged) => Some(tagged),
        Some(tagged) => manager_of(tagged),
        None => None,
    };
    chosen
        .or_else(|| at(main_manager))
        .ok_or_else(|| "no active task file runs at the main manager address".to_owned())
}

/// A human email as it is stored.
pub fn stored_mail(subject: &str, body: &str) -> String {
    format!("Subject: {subject}\n\n{body}")
}

/// The line of a task body that names the stored human email.
pub fn mail_line(mail_name: &str) -> String {
    format!("{MAIL_SOURCE}{mail_name})")
}

/// A task body with the line naming the stored human email appended; the email itself is queued in the `messages` table.
pub fn queued(body: &str, mail_name: &str) -> String {
    format!(
        "{}\n\n{}\n",
        body.trim_end_matches('\n'),
        mail_line(mail_name)
    )
}

/// What the first line of a pending block says the block is.
#[derive(Debug, PartialEq)]
pub enum Source<'a> {
    /// A stored guest email.
    Guest,
    /// The stored human email of that file name.
    Mail(&'a str),
    /// Text to pass on as it is.
    Plain,
}

pub fn source_of(first: &str) -> Source<'_> {
    if first.starts_with(crate::guest::SOURCE) {
        Source::Guest
    } else if let Some(name) = first.strip_prefix(MAIL_SOURCE) {
        Source::Mail(name.trim_end_matches(')'))
    } else {
        Source::Plain
    }
}

/// The message for an agent from a pending block of plain text.
pub fn plain_text(block: &[String], remind: bool) -> String {
    block.join("\n").trim().to_owned() + if remind { ADD_REMINDER } else { "" }
}

/// The message for an agent from a stored human email, which is inlined verbatim.
// 🧑 "human requests should be sent verbatim"
pub fn mail_text(name: &str, stored: &str, remind: bool) -> String {
    format!(
        "{ACCEPT}\n<human_instruction authoritative=\"true\" source=\"manager_mail/{name}\">\n{}\n</human_instruction>{}",
        work::unix_lines(stored).trim(),
        if remind { ADD_REMINDER } else { "" }
    )
}

/// What the human reads when an agent asks a question with choices.
pub fn question_text(task: &str, parts: &[(String, Vec<(String, String)>)]) -> String {
    let mut text = format!(
        "The agent of {task} asks and waits for your answer. Reply in this topic with a choice's number or your own words{}.\n",
        if parts.len() > 1 {
            ", one line per question"
        } else {
            ""
        }
    );
    for (question, choices) in parts {
        text.push_str(&format!("\n{question}\n"));
        for (number, (label, description)) in choices.iter().enumerate() {
            let description = if description.is_empty() || description == "null" {
                String::new()
            } else {
                format!(": {description}")
            };
            text.push_str(&format!("{}. {label}{description}\n", number + 1));
        }
    }
    text
}

/// The human's answer to each part of a question, from a stored message: a choice's number means that choice, other text is the answer itself.
///
/// With several parts, each line that is not a quote answers the next part; with one part, the whole reply does.
pub fn question_answers(
    parts: &[(String, Vec<(String, String)>)],
    stored: &str,
) -> Vec<(String, String)> {
    let body = stored.split_once("\n\n").map_or(stored, |(_, body)| body);
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('>'))
        .collect();
    let whole = lines.join("\n");
    let mut answers = Vec::new();
    for (index, (question, choices)) in parts.iter().enumerate() {
        let reply = if parts.len() > 1 && lines.len() >= parts.len() {
            lines[index]
        } else {
            &whole
        };
        let number = reply.trim_end_matches('.').parse::<usize>().ok();
        let chosen = number.and_then(|number| choices.get(number.wrapping_sub(1)));
        let answer = chosen.map_or(reply, |(label, _)| label);
        answers.push((question.clone(), answer.to_owned()));
    }
    answers
}

/// Who gets the pending blocks of one task.
#[derive(Debug, PartialEq)]
pub struct Plan {
    /// The task has no agent (done or retired), so its blocks go to a manager.
    pub orphan: bool,
    pub target: String,
    /// The manager to try when the target cannot be reached.
    pub fallback: Option<String>,
}

/// The addresses agents work at: those of every task that is neither done nor retired.
pub fn agent_addresses(tasks: &[Task]) -> BTreeSet<&str> {
    let staffed = tasks
        .iter()
        .filter(|task| task.status() != "done" && task.address() != "retired");
    staffed.map(Task::address).collect()
}

/// Decide who gets a task's pending blocks: its agent; for a task with no agent, its manager, else the main manager.
pub fn plan(task: &Task, agent_at: &BTreeSet<&str>, main_manager: &str) -> Plan {
    let manager = task.field("managerat").unwrap_or_default();
    let orphan = !agent_at.contains(task.address()) || task.status() == "done";
    let target = match (orphan, agent_at.contains(manager)) {
        (false, _) => task.address(),
        (true, true) => manager,
        (true, false) => main_manager,
    };
    let fallback = [manager, main_manager]
        .into_iter()
        .find(|address| agent_at.contains(address) && *address != target);
    Plan {
        orphan,
        target: target.to_owned(),
        fallback: fallback.map(str::to_owned),
    }
}

/// The message as its target gets it: a manager is told when it comes from a task that has no agent.
pub fn addressed(task: &Task, plan: &Plan, text: &str) -> String {
    let note = if plan.orphan {
        format!(
            "This was left in {}, a task that has no agent; it comes to you as its manager.\n",
            task.name
        )
    } else {
        String::new()
    };
    note + text
}

/// The message as the fallback manager gets it.
pub fn rerouted(task: &Task, error: &str, text: &str) -> String {
    format!(
        "This could not be delivered to the agent of {} ({error}); it comes to you as its manager. Handle it or get that agent working again.\n{text}",
        task.name
    )
}

/// How the attempt to deliver one pending block ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The target took it.
    Delivered,
    /// The target failed with `error`; the fallback manager took it.
    Rerouted { error: String },
    /// The target failed with `error`, and the fallback with `second`.
    Unreachable { error: String, second: String },
}

/// What follows from a delivery attempt.
#[derive(Debug, Default, PartialEq)]
pub struct Settled {
    pub logs: Vec<String>,
    /// An email to the human, sent at most once per key: the key and the text.
    pub human: Option<(String, String)>,
    /// The stored human email that now awaits the agent's answer.
    pub awaiting: Option<String>,
    /// The block reached someone, so its marker line is deleted; otherwise the task is retried after `RETRY_S`.
    pub delivered: bool,
}

/// Decide what follows from the attempt to deliver the block starting with the line `first`.
pub fn settle(task: &Task, plan: &Plan, first: &str, outcome: &Outcome) -> Settled {
    let name = &task.name;
    let shown = quoted(&first.chars().take(80).collect::<String>());
    let failed = |error: &str| {
        format!(
            "delivery of a block in {name} to {} failed: {error}",
            plan.target
        )
    };
    match outcome {
        Outcome::Delivered => Settled {
            logs: vec![format!("delivered {shown} in {name} to {}", plan.target)],
            human: None,
            awaiting: match source_of(first) {
                Source::Mail(mail) if !plan.orphan => Some(mail.to_owned()),
                _ => None,
            },
            delivered: true,
        },
        Outcome::Rerouted { error } => Settled {
            logs: vec![
                failed(error),
                format!(
                    "delivered {shown} in {name} to the manager at {} instead",
                    plan.fallback.as_deref().unwrap_or_default()
                ),
            ],
            human: Some((
                format!("rerouted {first}"),
                format!(
                    "Your message for {name} could not be delivered to its agent ({error}). I gave it to its manager instead, who is to handle it or get that agent working again."
                ),
            )),
            awaiting: None,
            delivered: true,
        },
        Outcome::Unreachable { error, second } => Settled {
            logs: vec![
                failed(error),
                format!("fallback delivery for {name} failed too: {second}"),
            ],
            human: Some((
                format!("undelivered {first}"),
                format!(
                    "Your message for {name} has not been delivered to anyone: its agent could not be reached ({error}), and neither could a manager. The watcher retries every 10 minutes."
                ),
            )),
            awaiting: None,
            delivered: false,
        },
    }
}

/// A task body without the marker line of the first pending block that starts with the line `first`; `None` when no such block is left.
pub fn without_marker(body: &str, first: &str) -> Option<String> {
    let blocks = taskfile::pending_blocks(body);
    let (at, _) = blocks
        .iter()
        .find(|(_, block)| block.first().is_some_and(|line| line == first))?;
    let mut lines: Vec<&str> = body.split('\n').collect();
    lines.remove(*at);
    Some(lines.join("\n"))
}

/// The line that records in `amh-told-human.txt` that the human was emailed about `key` of a task.
pub fn told_key(task: &Task, key: &str) -> String {
    format!("{} {key}", task.name)
}

/// What to do about one delivered human email that awaits an answer.
#[derive(Debug, PartialEq)]
pub enum Chase {
    /// Not due yet: the row stays as it is.
    Keep(String),
    /// Remind the agent; the row then waits for the second stage.
    Remind {
        task: String,
        address: String,
        text: String,
        row: String,
    },
    /// Tell the manager and the human; the row is dropped.
    Escalate {
        task: String,
        manager: String,
        text: String,
        /// The key and text of the email to the human.
        human: Option<(String, String)>,
    },
}

/// Decide what to do about each row `EPOCH TASK MAIL STAGE` of `amh-awaiting.txt`, given the rows `EPOCH TAG` of `amh-sent.txt`.
///
/// A row is dropped when its task is no longer active or has emailed the human since. `given` shows an epoch as local `HH:MM`.
pub fn chase(
    awaiting: &str,
    sent: &str,
    active: &[Task],
    now_s: f64,
    given: impl Fn(f64) -> String,
) -> Vec<Chase> {
    let sent: Vec<(f64, &str)> = sent
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(when, tag)| Some((when.parse().ok()?, tag)))
        .collect();
    let decide = |row: &str| {
        let &[at, name, mail, stage] = &row.split(' ').collect::<Vec<_>>()[..] else {
            return None;
        };
        let (at_s, stage) = (at.parse::<f64>().ok()?, stage.parse::<u64>().ok()?);
        let task = active.iter().find(|task| task.name == name)?;
        if sent
            .iter()
            .any(|(when, tag)| *tag == task.tag() && *when >= at_s)
        {
            return None;
        }
        if now_s - at_s < (ANSWER_S * (stage + 1)) as f64 {
            return Some(Chase::Keep(row.to_owned()));
        }
        let given = given(at_s);
        Some(if stage == 0 {
            Chase::Remind {
                task: name.to_owned(),
                address: task.address().to_owned(),
                text: format!(
                    "The human's email manager_mail/{mail} was given to you at {given} and you have not emailed the human since. Email the human now with `amh tell human`, even if only to say you got it and what you are doing. If the human wrote that no email is needed, run `amh mail handled {mail} --because 'THE HUMAN'S WORDS'` instead."
                ),
                row: format!("{at} {name} {mail} 1"),
            }
        } else {
            Chase::Escalate {
                task: name.to_owned(),
                manager: task.field("managerat").unwrap_or_default().to_owned(),
                text: format!(
                    "The agent of {name} was given the human's email manager_mail/{mail} at {given}, was reminded, and still has not emailed the human. Find out why and make sure the human gets an answer."
                ),
                human: (stage == 1).then(|| {
                    (
                        format!("unanswered {mail}"),
                        format!(
                            "The agent of {name} was given your message at {given} and was reminded once, but has not emailed you since. Its manager has been told to find out why."
                        ),
                    )
                }),
            }
        })
    };
    awaiting.lines().filter_map(decide).collect()
}

/// The harness error or the prompt waited on, on one line, from the evidence of an agent's state.
fn error_of(problem: Problem, evidence: &str) -> String {
    let mark = match problem {
        Problem::Error => "last_task_error=",
        Problem::Stuck => "waiting_on=",
        _ => return String::new(),
    };
    let after = evidence.split_once(mark).map_or("", |(_, after)| after);
    let error = after.split("; pane_tail:").next().unwrap_or_default();
    error.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Say in plain words which task's agent has which problem.
// 🧑 “This message sucks. I don’t know the task file. Has been recurring.”
fn failure_text(task: &Task, problem: Problem, error: &str, active: &[Task]) -> String {
    let manager_at = task.field("managerat");
    let manager = active
        .iter()
        .find(|other| Some(other.address()) == manager_at)
        .map_or(manager_at.unwrap_or("none"), |other| &other.name);
    let what = match problem {
        Problem::Error => "has failed and is not working any more",
        Problem::Stuck => {
            "is stuck: its tool waits for someone to answer a prompt, and it reads no messages until then"
        }
        _ => "is gone: nothing runs at its address",
    };
    let cause = match (error, problem) {
        ("", _) => String::new(),
        (_, Problem::Stuck) => format!("The prompt it waits on:\n{error}\n"),
        _ => format!("The error its harness reported:\n{error}\n"),
    };
    format!(
        "The agent working on task file {} {what}.\nTool: {}, running at {}\nIts manager: {manager}\nOpen items on the task: {}\n{cause}",
        task.name,
        task.field("tool").unwrap_or("unknown"),
        task.address(),
        task.items.len()
    )
}

/// One thing the watcher does about an agent problem.
#[derive(Debug, PartialEq)]
pub enum Nudge {
    /// Remind an idle agent of its open items.
    Remind {
        task: String,
        address: String,
        text: String,
    },
    /// Report a problem that was not reported yet to the task's manager.
    Report {
        /// The line that records in `amh-problem-notices.txt` that this problem was reported.
        key: String,
        log: String,
        task: String,
        manager: String,
        text: String,
        /// The task's tag and the text of the email to the human, for a failed or stuck agent.
        // 🧑 "Harness errors should directly email me too"
        email: Option<(String, String)>,
    },
}

/// Decide what to do about the agent problems found, and which problem keys exist now.
///
/// `reported` holds the keys already reported; a key that is no longer returned is forgotten, so the problem is reported again if it comes back.
/// `reminded_s` holds when each task's idle agent was last reminded, on the clock of `now_s`.
/// `None` while Omnigent is unreachable: then nothing is known, so nothing is done or forgotten.
pub fn nudges(
    found: &[(Task, Problem, String)],
    reported: &BTreeSet<String>,
    active: &[Task],
    reminded_s: &BTreeMap<String, u64>,
    now_s: u64,
    remind_every_s: impl Fn(&Task) -> u64,
) -> Option<(Vec<Nudge>, BTreeSet<String>)> {
    if found
        .iter()
        .any(|(_, _, evidence)| evidence.contains("unreachable"))
    {
        return None;
    }
    let mut nudges = Vec::new();
    let mut now = BTreeSet::new();
    for (task, problem, evidence) in found {
        if *problem == Problem::Idle {
            let due = reminded_s
                .get(&task.name)
                .is_none_or(|at| now_s - at >= remind_every_s(task));
            if due {
                nudges.push(Nudge::Remind {
                    task: task.name.clone(),
                    address: task.address().to_owned(),
                    text: format!(
                        "You have {} open items. See them with `amh todo list`. Continue until each is finished or cancelled. This reminder is automatic: send nobody a message because of it; if you are waiting for a subagent, keep waiting.",
                        task.items.len()
                    ),
                });
            }
            continue;
        }
        let error = error_of(*problem, evidence);
        let key = format!(
            "{} {} {} {}",
            task.name,
            task.address(),
            problem.name(),
            error.chars().take(200).collect::<String>()
        );
        if now.insert(key.clone()) && !reported.contains(&key) {
            let text = failure_text(task, *problem, &error, active);
            let emailed = matches!(problem, Problem::Error | Problem::Stuck)
                && crate::requests::from_human(task);
            nudges.push(Nudge::Report {
                key,
                log: format!(
                    "agent problem: {} at {}: {} ({evidence})",
                    task.name,
                    task.address(),
                    problem.name()
                ),
                task: task.name.clone(),
                manager: task.field("managerat").unwrap_or_default().to_owned(),
                email: emailed.then(|| {
                    (
                        task.tag().to_owned(),
                        format!("{text}\nIts manager has been told to handle it.\n"),
                    )
                }),
                text,
            });
        }
    }
    Some((nudges, now))
}

/// The message telling a manager about the problems of its agents.
pub fn problems_text(texts: &[String]) -> String {
    format!(
        "Handle these agent problems; only email the human if you cannot:\n\n{}",
        texts.join("\n")
    )
}

/// The message telling a manager which permission prompts of its agent the watcher approved.
pub fn approved_text(task: &Task, approved: &[String]) -> String {
    format!(
        "The agent of {} waited on a permission prompt; the watcher answered yes: {}",
        task.name,
        approved.join(" | ")
    )
}

/// What the watcher knows about the last notice to compress the mailbox: when it was sent and the unread count then.
pub type Compressed = Option<(u64, usize)>;

/// Whether to tell the human manager now to compress the human's mailbox, given the unread agent emails that count.
// 🧑 "do a dedicated agent “human manager” who directly gets notification to compress the mailbox and does it"
// 🧑 "Automatic manager-mail cleanup is triggered only by unread manager mail, never by total retained Inbox thread count."
// Hourly while more than `over` are unread; at once when the count reaches `urgent` after a notice sent below it.
pub fn compress_due(
    count: usize,
    over: usize,
    urgent: usize,
    last: Compressed,
    now_s: u64,
) -> bool {
    match last {
        _ if count <= over => false,
        None => true,
        Some((at, counted)) => {
            now_s.saturating_sub(at) >= 3600 || (count >= urgent && counted < urgent)
        }
    }
}

/// The names of the config tables under `group`, e.g. the `NAME` of every `[job.NAME]`.
pub fn table_names(settings: &BTreeMap<String, String>, group: &str) -> Vec<String> {
    let mut names: Vec<String> = settings
        .keys()
        .filter_map(|key| key.strip_prefix(group)?.strip_prefix('.'))
        .filter_map(|rest| Some(rest.rsplit_once('.')?.0.to_owned()))
        .collect();
    names.dedup();
    names
}

/// A span such as `30s`, `15m`, `2h`, `1d` in seconds.
pub fn span_s(text: &str) -> Option<u64> {
    let text = text.trim();
    let unit = text.chars().last()?;
    let number = text.strip_suffix(unit)?;
    let each = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return None,
    };
    number
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .and_then(|n| n.checked_mul(each))
}

/// Whether a scheduled job runs now: `every_s` after its last run, or once `at_today_s` has passed and it has not run since.
// 🧑 "allow some sort of scripts or commands that should be run like a cron job, but is tied to AMH server"
/// A job never seen before runs at once when it repeats, and waits for its next time of day otherwise.
pub fn job_due(
    every_s: Option<u64>,
    at_today_s: Option<u64>,
    last_s: Option<u64>,
    now_s: u64,
) -> bool {
    match (every_s, at_today_s, last_s) {
        (Some(_), _, None) => true,
        (Some(every), _, Some(last)) => now_s
            .checked_sub(last)
            .is_some_and(|elapsed| elapsed >= every),
        (None, Some(at), Some(last)) => now_s >= at && last < at,
        _ => false,
    }
}

/// The emails that count toward compression: not the "pending item created/deleted" notices.
// 🧑 "Pending-item created emails, and pending-item removed or deleted emails, are not countable manager mail."
pub fn countable(sender: &str) -> bool {
    !sender.starts_with(crate::mail::NOTICE_SENDER)
}

/// The notice to the human manager: how many emails wait unread, and how many per task tag, most first.
pub fn compress_text(count: usize, over: usize, tags: &[String]) -> String {
    let mut per_tag: Vec<(usize, &str)> = Vec::new();
    for tag in tags {
        match per_tag.iter_mut().find(|(_, known)| known == tag) {
            Some(found) => found.0 += 1,
            None => per_tag.push((1, tag)),
        }
    }
    per_tag.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let listed: Vec<String> = per_tag
        .iter()
        .map(|(n, tag)| format!("{tag} {n}"))
        .collect();
    format!(
        "Mailbox compression is due: the human has {count} unread messages from agents, more than {over}; pending-item notices and messages without a task tag, such as PB news, are not counted. Compress the mailbox now; under Zulip, `amh mail unread --all` lists them with their text. Unread per task tag: {}.",
        listed.join(", ")
    )
}

/// The waiting list without the rows of `task` for the stored human email `mail`, and how many rows went.
// 🧑 "No need to email me for this, just handle it"
pub fn without_awaiting(awaiting: &str, task: &str, mail: &str) -> (String, usize) {
    let mail = mail.rsplit('/').next().unwrap_or(mail);
    let waits = |row: &&str| {
        let mut parts = row.split(' ');
        (parts.nth(1), parts.next()) == (Some(task), Some(mail))
    };
    let kept: String = awaiting
        .lines()
        .filter(|row| !waits(row))
        .map(|row| format!("{row}\n"))
        .collect();
    (kept, awaiting.lines().filter(waits).count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::tests::task;

    const MAIN: &str = "omnigent://main";

    /// A main manager, a worker under it, a submanager under the worker, a done task, and a retired one.
    fn tasks() -> Vec<Task> {
        let made = |name: &str, status: &str, at: &str, manager: &str| {
            let fields = [
                ("status", status),
                ("runat", at),
                ("managerat", manager),
                ("tool", "codex"),
            ];
            task(name, &fields, &["x"], "")
        };
        vec![
            made("t_done.md", "done", "omnigent://gone", "omnigent://s1"),
            task(
                "t_main.md",
                &[("status", "long_running"), ("runat", MAIN)],
                &[],
                "",
            ),
            made("t_old.md", "running", "retired", "omnigent://s2"),
            made("t_sub.md", "long_running", "omnigent://s2", "omnigent://s1"),
            made("t_work.md", "running", "omnigent://s1", MAIN),
        ]
    }

    fn by_name<'a>(tasks: &'a [Task], name: &str) -> &'a Task {
        tasks.iter().find(|task| task.name == name).unwrap()
    }

    #[test]
    fn question_goes_to_the_human_and_the_reply_comes_back() {
        let choice = |label: &str, about: &str| (label.to_owned(), about.to_owned());
        let parts = vec![
            (
                "Which way?".to_owned(),
                vec![choice("Left", "the short one"), choice("Right", "")],
            ),
            (
                "How fast?".to_owned(),
                vec![choice("Slow", ""), choice("Fast", "")],
            ),
        ];
        let text = question_text("t.md", &parts);
        assert!(
            text.contains("Which way?\n1. Left: the short one\n2. Right\n"),
            "{text}"
        );
        let answer = |stored: &str| question_answers(&parts, stored);
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            answer("Subject: [t] Zulip message\n\n> Which way?\n\n2\nvery slow"),
            [pair("Which way?", "Right"), pair("How fast?", "very slow")]
        );
        assert_eq!(
            question_answers(&parts[..1], "Subject: x\n\n1."),
            [pair("Which way?", "Left")]
        );
        assert_eq!(
            question_answers(&parts[..1], "Subject: x\n\nNeither,\ngo back"),
            [pair("Which way?", "Neither,\ngo back")]
        );
    }

    #[test]
    fn mail_is_routed_by_tag_and_manager_request() {
        let tasks = tasks();
        let cases = [
            ("Re: [t_sub] hello", "do it", "t_sub.md"),
            ("[t_sub] hello", "For manager.\r\nplease", "t_work.md"),
            (
                "[t_sub] hello",
                "please\n> quoted\n**for manager**\n> for worker",
                "t_work.md",
            ),
            ("[t_sub] hello", "Terminate this agent now", "t_work.md"),
            (
                "[t_sub] hello",
                "replace this agent, it is stuck",
                "t_work.md",
            ),
            ("[t_sub] hello", "not for manager really\nok", "t_sub.md"),
            ("[t_sub] hello", "please\nterminate this agent", "t_sub.md"),
            ("[t_main] hello", "for manager", "t_main.md"),
            ("[t_done] hello", "more work", "t_work.md"),
            ("[t_old] hello", "more work", "t_sub.md"),
            ("[absent_tag] hello", "x", "t_main.md"),
            ("no tag", "x", "t_main.md"),
            ("[TODO] hello", "x", "t_main.md"),
        ];
        for (subject, body, want) in cases {
            let got = route(&tasks, MAIN, subject, body).map(|task| task.name.as_str());
            assert_eq!(got, Ok(want), "{subject:?} {body:?}");
        }
        let unrouted = route(&tasks, "omnigent://nobody", "untagged", "x");
        assert!(unrouted.unwrap_err().contains("main manager"));
        assert!(!for_manager("") && !for_manager("> for manager\n\n"));
        // 🧑 "For a manager, this agent sounds stupid, isn't running on Luna."
        assert!(
            for_manager("For a manager, this agent sounds stupid")
                && for_manager("park it.\nThis is for the manager.")
        );
        assert!(!for_manager("for managers in general") && !for_manager("a platform manager"));
    }

    #[test]
    fn a_human_email_is_stored_and_queued() {
        assert_eq!(
            stored_mail("Re: [t_sub] do more", "Please do Z.\n> old\n"),
            "Subject: Re: [t_sub] do more\n\nPlease do Z.\n> old\n"
        );
        assert_eq!(
            queued("---\nx\n---\nbody\n\n\n", "m-11.txt"),
            "---\nx\n---\nbody\n\n(record and delegate manager_mail/m-11.txt)\n"
        );
    }

    #[test]
    fn a_block_becomes_the_message_for_its_agent() {
        let body = "note\n\n(pending)\n(record and delegate manager_mail/m-11.txt)\n\n(pending)\nplain block line one\nline two\n";
        let blocks = taskfile::pending_blocks(body);
        assert_eq!(source_of(&blocks[0].1[0]), Source::Mail("m-11.txt"));
        assert_eq!(source_of(&blocks[1].1[0]), Source::Plain);
        assert_eq!(source_of("(guest mail d/guest-3.txt)"), Source::Guest);
        assert_eq!(
            mail_text(
                "m-11.txt",
                "Subject: Re: [t_sub] do more\n\nPlease do Z.\r\n> old\r\n",
                false
            ),
            format!(
                "{ACCEPT}\n<human_instruction authoritative=\"true\" source=\"manager_mail/m-11.txt\">\nSubject: Re: [t_sub] do more\n\nPlease do Z.\n> old\n</human_instruction>"
            )
        );
        assert_eq!(
            plain_text(&blocks[1].1, false),
            "plain block line one\nline two"
        );
        assert_eq!(
            plain_text(&["do it".to_owned()], true),
            format!("do it{ADD_REMINDER}")
        );
        assert!(
            mail_text("m.txt", "x", true).ends_with(&format!("</human_instruction>{ADD_REMINDER}"))
        );
    }

    #[test]
    fn blocks_go_to_the_agent_and_orphans_to_a_manager() {
        let tasks = tasks();
        let agent_at = agent_addresses(&tasks);
        assert_eq!(
            agent_at,
            BTreeSet::from([MAIN, "omnigent://s1", "omnigent://s2"])
        );
        let planned = |name: &str| {
            let plan = plan(by_name(&tasks, name), &agent_at, MAIN);
            (plan.orphan, plan.target, plan.fallback)
        };
        let some = |address: &str| Some(address.to_owned());
        assert_eq!(
            planned("t_sub.md"),
            (false, "omnigent://s2".to_owned(), some("omnigent://s1"))
        );
        assert_eq!(
            planned("t_work.md"),
            (false, "omnigent://s1".to_owned(), some(MAIN))
        );
        assert_eq!(planned("t_main.md"), (false, MAIN.to_owned(), None));
        assert_eq!(
            planned("t_done.md"),
            (true, "omnigent://s1".to_owned(), some(MAIN))
        );
        assert_eq!(
            planned("t_old.md"),
            (true, "omnigent://s2".to_owned(), some(MAIN))
        );
        // A manager that is itself gone leaves only the main manager.
        let lone = task(
            "t_lone.md",
            &[
                ("status", "done"),
                ("runat", "omnigent://x"),
                ("managerat", "omnigent://y"),
            ],
            &[],
            "",
        );
        assert_eq!(
            plan(&lone, &agent_at, MAIN),
            Plan {
                orphan: true,
                target: MAIN.to_owned(),
                fallback: None
            }
        );
        let done = by_name(&tasks, "t_done.md");
        assert_eq!(
            addressed(done, &plan(done, &agent_at, MAIN), "note"),
            "This was left in t_done.md, a task that has no agent; it comes to you as its manager.\nnote"
        );
        let sub = by_name(&tasks, "t_sub.md");
        assert_eq!(addressed(sub, &plan(sub, &agent_at, MAIN), "note"), "note");
        assert_eq!(
            rerouted(sub, "the agent is missing: gone", "second note"),
            "This could not be delivered to the agent of t_sub.md (the agent is missing: gone); it comes to you as its manager. Handle it or get that agent working again.\nsecond note"
        );
    }

    #[test]
    fn a_delivery_attempt_settles_by_its_outcome() {
        let tasks = tasks();
        let agent_at = agent_addresses(&tasks);
        let sub = by_name(&tasks, "t_sub.md");
        let planned = plan(sub, &agent_at, MAIN);
        let first = "(record and delegate manager_mail/m-11.txt)";
        let delivered = settle(sub, &planned, first, &Outcome::Delivered);
        assert_eq!(
            delivered,
            Settled {
                logs: vec![
                    "delivered '(record and delegate manager_mail/m-11.txt)' in t_sub.md to omnigent://s2".to_owned()
                ],
                human: None,
                awaiting: Some("m-11.txt".to_owned()),
                delivered: true,
            }
        );
        assert_eq!(
            settle(sub, &planned, "it's plain", &Outcome::Delivered).awaiting,
            None
        );
        let done = by_name(&tasks, "t_done.md");
        let orphan = settle(
            done,
            &plan(done, &agent_at, MAIN),
            first,
            &Outcome::Delivered,
        );
        assert!(orphan.delivered && orphan.awaiting.is_none());
        let error = "the agent is missing: gone".to_owned();
        let moved = settle(
            sub,
            &planned,
            first,
            &Outcome::Rerouted {
                error: error.clone(),
            },
        );
        assert_eq!(
            moved.logs,
            [
                "delivery of a block in t_sub.md to omnigent://s2 failed: the agent is missing: gone",
                "delivered '(record and delegate manager_mail/m-11.txt)' in t_sub.md to the manager at omnigent://s1 instead"
            ]
        );
        let (key, text) = moved.human.unwrap();
        assert_eq!(key, format!("rerouted {first}"));
        assert!(text.contains("gave it to its manager") && moved.delivered);
        let lost = settle(
            sub,
            &planned,
            first,
            &Outcome::Unreachable {
                error,
                second: "omnigent://s1 is down".to_owned(),
            },
        );
        assert_eq!(
            lost.logs[1],
            "fallback delivery for t_sub.md failed too: omnigent://s1 is down"
        );
        let (key, text) = lost.human.unwrap();
        assert_eq!(told_key(sub, &key), format!("t_sub.md undelivered {first}"));
        assert!(text.contains("not been delivered to anyone") && !lost.delivered);
        assert_eq!(quoted("it's"), "\"it's\"");
        assert_eq!(quoted("a 'b' \"c\"\n"), "'a \\'b\\' \"c\"\\n'");
    }

    #[test]
    fn a_delivered_block_loses_only_its_marker() {
        let body = "a\n\n(pending)\n(record and delegate manager_mail/m-11.txt)\n\n(a note)\n\n(pending)\nplain\n";
        let once = without_marker(body, "(record and delegate manager_mail/m-11.txt)").unwrap();
        assert_eq!(
            once,
            "a\n\n(record and delegate manager_mail/m-11.txt)\n\n(a note)\n\n(pending)\nplain\n"
        );
        assert_eq!(
            without_marker(&once, "plain").unwrap(),
            "a\n\n(record and delegate manager_mail/m-11.txt)\n\n(a note)\n\nplain\n"
        );
        assert_eq!(without_marker(&once, "gone"), None);
    }

    #[test]
    fn an_unanswered_email_is_chased_in_two_stages() {
        let tasks: Vec<Task> = tasks().into_iter().filter(Task::is_active).collect();
        let given = |at_s: f64| format!("T{at_s}");
        let now = 100_000.0;
        let chased = |awaiting: &str, sent: &str| chase(awaiting, sent, &tasks, now, given);
        let fresh = format!("{} t_sub.md m-11.txt 0", now - 899.0);
        assert_eq!(chased(&fresh, ""), [Chase::Keep(fresh.clone())]);
        let silent = chased("99095 t_sub.md m-11.txt 0\n", "99000 t_sub\n99500 t_work\n");
        let [
            Chase::Remind {
                task,
                address,
                text,
                row,
            },
        ] = &silent[..]
        else {
            panic!("{silent:?}");
        };
        assert_eq!(
            (task.as_str(), address.as_str(), row.as_str()),
            ("t_sub.md", "omnigent://s2", "99095 t_sub.md m-11.txt 1")
        );
        assert_eq!(
            text,
            "The human's email manager_mail/m-11.txt was given to you at T99095 and you have not emailed the human since. Email the human now with `amh tell human`, even if only to say you got it and what you are doing. If the human wrote that no email is needed, run `amh mail handled m-11.txt --because 'THE HUMAN'S WORDS'` instead."
        );
        let reminded = "99095 t_sub.md m-11.txt 1";
        assert_eq!(chased(reminded, ""), [Chase::Keep(reminded.to_owned())]);
        let late = chased("98195 t_sub.md m-11.txt 1", "");
        let [
            Chase::Escalate {
                task,
                manager,
                text,
                human,
            },
        ] = &late[..]
        else {
            panic!("{late:?}");
        };
        assert_eq!(
            (task.as_str(), manager.as_str()),
            ("t_sub.md", "omnigent://s1")
        );
        assert!(text.contains("at T98195, was reminded, and still has not emailed the human"));
        let (key, text) = human.clone().unwrap();
        assert_eq!(key, "unanswered m-11.txt");
        assert!(text.contains("has not emailed you since"));
        assert_eq!(chased("99095 t_sub.md m-11.txt 0", "99095 t_sub\n"), []);
        assert_eq!(
            chased(
                "99095 t_done.md m-1.txt 0\n99095 t_gone.md m-2.txt 0\nbroken row\n",
                ""
            ),
            []
        );
    }

    #[test]
    fn a_problem_is_reported_once_and_again_after_recovery() {
        let tasks: Vec<Task> = tasks().into_iter().filter(Task::is_active).collect();
        let (plain, worker) = (by_name(&tasks, "t_sub.md"), by_name(&tasks, "t_work.md"));
        let error = "session_status=failed last_task_error=model  overloaded; pane_tail: junk";
        let none = BTreeSet::new();
        // The human hears of a failure only on a task they gave a request themselves.
        let unasked = vec![(plain.clone(), Problem::Error, error.to_owned())];
        let (quiet, _) =
            nudges(&unasked, &none, &tasks, &BTreeMap::new(), 0, |_| REMINDER_S).unwrap();
        assert!(matches!(&quiet[..], [Nudge::Report { email: None, .. }]));
        let mut sub = plain.clone();
        sub.items = vec![format!("{}x", taskfile::HUMAN_MARK)];
        crate::requests::sync(&mut sub).unwrap();
        let sub = &sub;
        let found = vec![
            (sub.clone(), Problem::Error, error.to_owned()),
            (worker.clone(), Problem::Idle, "idle".to_owned()),
        ];
        let mut reminded = BTreeMap::new();
        let (first, now) = nudges(&found, &none, &tasks, &reminded, 5000, |_| REMINDER_S).unwrap();
        let failure = "The agent working on task file t_sub.md has failed and is not working any more.\nTool: codex, running at omnigent://s2\nIts manager: t_work.md\nOpen items on the task: 1\nThe error its harness reported:\nmodel overloaded\n";
        let key = "t_sub.md omnigent://s2 error model overloaded";
        assert_eq!(
            first,
            [
                Nudge::Report {
                    key: key.to_owned(),
                    log: format!("agent problem: t_sub.md at omnigent://s2: error ({error})"),
                    task: "t_sub.md".to_owned(),
                    manager: "omnigent://s1".to_owned(),
                    text: failure.to_owned(),
                    email: Some((
                        "t_sub".to_owned(),
                        format!("{failure}\nIts manager has been told to handle it.\n")
                    )),
                },
                Nudge::Remind {
                    task: "t_work.md".to_owned(),
                    address: "omnigent://s1".to_owned(),
                    text: "You have 1 open items. See them with `amh todo list`. Continue until each is finished or cancelled. This reminder is automatic: send nobody a message because of it; if you are waiting for a subagent, keep waiting.".to_owned(),
                },
            ]
        );
        assert_eq!(now, BTreeSet::from([key.to_owned()]));
        assert_eq!(
            problems_text(&[failure.to_owned()]),
            format!(
                "Handle these agent problems; only email the human if you cannot:\n\n{failure}"
            )
        );
        // Reported and reminded: nothing more until the reminder period is over.
        reminded.insert("t_work.md".to_owned(), 5000);
        assert_eq!(
            nudges(
                &found,
                &now,
                &tasks,
                &reminded,
                5000 + REMINDER_S - 1,
                |_| REMINDER_S
            )
            .unwrap(),
            (vec![], now.clone())
        );
        let (again, _) = nudges(&found, &now, &tasks, &reminded, 5000 + REMINDER_S, |_| {
            REMINDER_S
        })
        .unwrap();
        assert!(matches!(&again[..], [Nudge::Remind { .. }]));
        // The problem goes away and is forgotten; a new one is reported, to the manager only.
        assert_eq!(
            nudges(&[], &now, &tasks, &reminded, 0, |_| REMINDER_S)
                .unwrap()
                .1,
            none
        );
        let gone = vec![(sub.clone(), Problem::Missing, "runner offline".to_owned())];
        let (second, now) = nudges(&gone, &none, &tasks, &reminded, 0, |_| REMINDER_S).unwrap();
        let [
            Nudge::Report {
                key,
                text,
                email: None,
                ..
            },
        ] = &second[..]
        else {
            panic!("{second:?}");
        };
        assert_eq!(key, "t_sub.md omnigent://s2 missing ");
        assert_eq!(
            text,
            "The agent working on task file t_sub.md is gone: nothing runs at its address.\nTool: codex, running at omnigent://s2\nIts manager: t_work.md\nOpen items on the task: 1\n"
        );
        // While Omnigent is unreachable nothing is known.
        let down = vec![(
            sub.clone(),
            Problem::Missing,
            "Omnigent is unreachable at http://x".to_owned(),
        )];
        assert_eq!(
            nudges(&down, &now, &tasks, &reminded, 0, |_| REMINDER_S),
            None
        );
        let stuck = vec![(
            sub.clone(),
            Problem::Stuck,
            "session_status=waiting waiting_on=Allow Bash: rm x".to_owned(),
        )];
        let (third, _) = nudges(&stuck, &none, &tasks, &reminded, 0, |_| REMINDER_S).unwrap();
        let [
            Nudge::Report {
                text,
                email: Some(_),
                ..
            },
        ] = &third[..]
        else {
            panic!("{third:?}");
        };
        assert!(
            text.contains("is stuck: its tool waits")
                && text.ends_with("The prompt it waits on:\nAllow Bash: rm x\n")
        );
        assert_eq!(
            approved_text(
                sub,
                &["Allow Bash: ls".to_owned(), "Allow Edit: x".to_owned()]
            ),
            "The agent of t_sub.md waited on a permission prompt; the watcher answered yes: Allow Bash: ls | Allow Edit: x"
        );
    }

    #[test]
    fn the_human_manager_is_told_to_compress_when_unread_mail_piles_up() {
        assert!(!compress_due(16, 16, 30, None, 0));
        assert!(compress_due(17, 16, 30, None, 0));
        assert!(!compress_due(20, 16, 30, Some((100, 17)), 1000));
        assert!(compress_due(20, 16, 30, Some((100, 17)), 3700));
        assert!(compress_due(30, 16, 30, Some((100, 17)), 200));
        assert!(!compress_due(35, 16, 30, Some((100, 30)), 200));
        assert!(
            countable("omnigent://a") && countable("") && !countable("amh notice omnigent://a")
        );
        let tags = ["b", "a", "b", "(no tag)"].map(str::to_owned);
        assert_eq!(
            compress_text(18, 16, &tags),
            "Mailbox compression is due: the human has 18 unread messages from agents, more than 16; pending-item notices and messages without a task tag, such as PB news, are not counted. Compress the mailbox now; under Zulip, `amh mail unread --all` lists them with their text. Unread per task tag: b 2, (no tag) 1, a 1."
        );
    }

    #[test]
    fn a_handled_human_email_is_no_longer_chased() {
        let awaiting = "10 t_a.md m-1.txt 1\n20 t_b.md m-1.txt 0\n30 t_a.md m-2.txt 0\n";
        assert_eq!(
            without_awaiting(awaiting, "t_a.md", "manager_mail/m-1.txt"),
            ("20 t_b.md m-1.txt 0\n30 t_a.md m-2.txt 0\n".to_owned(), 1)
        );
        assert_eq!(
            without_awaiting(awaiting, "t_a.md", "m-9.txt"),
            (awaiting.to_owned(), 0)
        );
    }
    #[test]
    fn jobs_run_on_their_schedule() {
        let settings: BTreeMap<String, String> = [
            ("job.a.every", "15m"),
            ("job.a.run", "x"),
            ("job.b.at", "04:00"),
            ("jobs", "no"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        assert_eq!(table_names(&settings, "job"), ["a", "b"]);
        assert_eq!(
            (
                span_s("15m"),
                span_s("2h"),
                span_s("0m"),
                span_s("m"),
                span_s("5")
            ),
            (Some(900), Some(7200), None, None, None)
        );
        assert!(job_due(Some(900), None, None, 5));
        assert!(!job_due(Some(900), None, Some(100), 999));
        assert!(job_due(Some(900), None, Some(100), 1000));
        assert!(!job_due(None, Some(500), None, 600));
        assert!(job_due(None, Some(500), Some(400), 500));
        assert!(!job_due(None, Some(500), Some(500), 600));
        assert!(!job_due(None, Some(500), Some(400), 499));
        assert_eq!(span_s("é"), None);
        assert_eq!(span_s("18446744073709551615d"), None);
        assert!(!job_due(Some(1), None, Some(u64::MAX), 0));
    }
}
