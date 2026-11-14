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
// 🧑 "I think amh should say to add TODO and attach, if any, additional ack" ... "let agents answer quick questions directly immediately instead of adding to TODO"
pub const ACCEPT: &str = "from the human: add each new request as an open item at once with `amh tell human --add 'ITEM'`; answer a quick question directly; a standing rule goes into the suitable instructions, not an item";
pub const ADD_REMINDER: &str =
    "\nAdd every still-open request to your open items; answer a quick question directly instead.";

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
    }) || first.starts_with("replace this agent")
}

/// Whether the human's own text starts by asking to replace the agent.
pub fn replace_order(body: &str) -> bool {
    starts_with_order(body, "replace this agent")
}

/// Whether the human's own text starts by asking to terminate the agent.
// 🧑 "Things like \"replace this agent\" and \"terminate this agent\" should immediately cause the system to do them, no?"
pub fn terminate_order(body: &str) -> bool {
    starts_with_order(body, "terminate this agent")
}

/// Whether the human's first own (unquoted) line starts with `order`.
fn starts_with_order(body: &str, order: &str) -> bool {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('>'))
        .is_some_and(|line| {
            line.trim_start_matches(['*', '_'])
                .to_lowercase()
                .starts_with(order)
        })
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
        // 🧑 "\"replace this agent\" ... should trigger automatic replacement": amh replaces the agent when delivering to its own task.
        Some(tagged)
            if staffed(tagged)
                && (replace_order(body) || terminate_order(body))
                && tagged.address().starts_with(crate::omnigent::PREFIX) =>
        {
            Some(tagged)
        }
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

/// What the human wrote since `since_unix`, across all tasks, oldest first: one line per stored message with its local time, task tag, file and first own line.
///
/// `entries` are the stored mail files as name, modification time (Unix seconds) and text. Only the human's Zulip messages (`zulip-*.txt`) and emails (`HASH-N.txt`) count; routed notifications and other files do not. Quoted lines (`>`) are skipped, so a reply shows what the human added.
// 🧑 "maybe we need to strengthen tooling + prompts for human manager so they always get all the recent stuffs the human has sent for context before compiling the list?"
/// `mail_dir`: the absolute manager_mail directory, so agents started outside the work logs can open the files.
pub fn recent_human(entries: &[(String, i64, String)], since_unix: i64, mail_dir: &str) -> String {
    let human = |name: &str| {
        let stem = name.strip_suffix(".txt").unwrap_or("");
        let email = stem.split_once('-').is_some_and(|(hash, n)| {
            hash.len() == 12
                && hash.chars().all(|c| c.is_ascii_hexdigit())
                && n.chars().all(|c| c.is_ascii_digit())
                && !n.is_empty()
        });
        stem.starts_with("zulip-") || email
    };
    let mut kept: Vec<&(String, i64, String)> = entries
        .iter()
        .filter(|(name, at, _)| *at >= since_unix && human(name))
        .collect();
    kept.sort_by_key(|(name, at, _)| (*at, name.clone()));
    kept.iter()
        .map(|(name, at, text)| {
            let subject = text
                .lines()
                .find_map(|line| line.strip_prefix("Subject: "))
                .unwrap_or("");
            let tag = crate::mail::tag_of(subject).unwrap_or("-");
            let first = text
                .split_once("\n\n")
                .map_or("", |(_, body)| body)
                .lines()
                .map(str::trim)
                .find(|line| {
                    !line.is_empty()
                        && !line.starts_with('>')
                        && !(line.starts_with("On ") && line.ends_with("wrote:"))
                })
                .unwrap_or("");
            let first: String = first.chars().take(160).collect();
            let time = chrono::DateTime::from_timestamp(*at, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            format!("{time} [{tag}] {mail_dir}/{name}: {first}\n")
        })
        .collect()
}

/// A routed notification file `routed-KIND-GENERATION-UID.txt` and its text as mailbox kind, UID generation, UID, and receipt key.
/// Names of routed notification files `routed-KIND-GENERATION-UID.txt` mentioned in `text`.
pub fn routed_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (at, _) in text.match_indices("routed-") {
        let name: String = text[at..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
            .collect();
        let name = name.trim_end_matches('.');
        let name = match name.find(".txt") {
            Some(end) => &name[..end + 4],
            None => continue,
        };
        if routed_mail(name, "").is_ok() && !names.iter().any(|n: &String| n == name) {
            names.push(name.to_owned());
        }
    }
    names
}

#[test]
fn finds_routed_names() {
    assert_eq!(
        routed_names("see manager_mail/routed-human-1-26695.txt, and routed-agent-2-7.txt."),
        vec!["routed-human-1-26695.txt", "routed-agent-2-7.txt"]
    );
    assert!(routed_names("routed-x.txt routed-").is_empty());
}

pub fn routed_mail(name: &str, text: &str) -> Result<(String, u32, String, String), String> {
    let bad = || {
        format!("amh: {name} is not a routed notification file `routed-KIND-GENERATION-UID.txt`")
    };
    let rest = name
        .strip_prefix("routed-")
        .and_then(|rest| rest.strip_suffix(".txt"));
    let mut parts = rest.ok_or_else(bad)?.splitn(3, '-');
    let (Some(kind), Some(generation), Some(uid)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let generation: u32 = generation.parse().map_err(|_| bad())?;
    if uid.parse::<u32>().is_err() {
        return Err(bad());
    }
    let message_id = text
        .lines()
        .find_map(|line| line.strip_prefix("Message-ID: "))
        .unwrap_or("")
        .trim();
    let key = match message_id {
        "" => format!("{kind}:{generation}:{uid}"),
        id => format!("{kind}:{id}"),
    };
    Ok((kind.to_owned(), generation, uid.to_owned(), key))
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

/// The message for an agent from a stored human email: the human's text bare, with at most one note after it.
///
/// The subject is kept only for an email that starts a thread; Zulip topics, `amh say` labels, and `Re:` subjects carry no words of the human's.
/// With `remind` (one message in eight, at random, whatever the open items), it ends with the one-line `ACCEPT` note.
/// A message typed in another task's thread than `to_tag` names that thread first.
// 🧑 "human requests should be sent verbatim"
// 🧑 "Normal human messages should arrive without things around them, only sometimes with reminders of adding pending items etc."
pub fn mail_text(name: &str, stored: &str, to_tag: &str, remind: bool) -> String {
    let stored = work::unix_lines(stored);
    let (subject, body) = match stored
        .strip_prefix("Subject: ")
        .and_then(|rest| rest.split_once("\n\n"))
    {
        Some((subject, body)) if !subject.contains('\n') => (subject.trim(), body),
        _ => ("", stored.as_str()),
    };
    let generated = name.starts_with("zulip-") || name.contains("-edit-");
    let reply = ["re:", "fwd:", "fw:"]
        .iter()
        .any(|prefix| subject.to_lowercase().starts_with(prefix));
    // 🧑 "amh is not telling you which thread that was? That is wrong"
    let topic = subject
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix("] Zulip message"));
    let thread = topic
        .or_else(|| crate::mail::tag_of(subject))
        .filter(|tag| *tag != to_tag)
        .map_or(String::new(), |tag| format!("In the {tag} thread:\n"));
    let mut text = match subject.trim() {
        "" => String::new(),
        _ if generated || reply => thread,
        subject => format!("Subject: {subject}\n\n"),
    };
    text += body.trim();
    // 🧑 "Don't add the reminder every message. Added about one eighth of the times randomly."
    match remind {
        true => text + &format!("\n\n({ACCEPT})"),
        false => text,
    }
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
    mail_dir: &str,
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
        // The task's own send, or a send by a task it manages (the one it delegated the email to), answers it.
        let answers = |tag: &str| {
            tag == task.tag()
                || active.iter().any(|other| {
                    other.tag() == tag
                        && !task.address().is_empty()
                        && other.field("managerat") == Some(task.address())
                })
        };
        if sent.iter().any(|(when, tag)| *when >= at_s && answers(tag)) {
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
                    "The human's message {mail_dir}/{mail} was given to you at {given} and you have not messaged the human since. Message the human now with `amh tell human`, even if only to say you got it and what you are doing. If the human wrote that no reply is needed, run `amh mail handled {mail} --because 'THE HUMAN'S WORDS'` instead."
                ),
                row: format!("{at} {name} {mail} 1"),
            }
        } else {
            Chase::Escalate {
                task: name.to_owned(),
                manager: task.field("managerat").unwrap_or_default().to_owned(),
                text: format!(
                    "The agent of {name} was given the human's message {mail_dir}/{mail} at {given}, was reminded, and still has not messaged the human. Find out why and make sure the human gets an answer."
                ),
                human: (stage == 1).then(|| {
                    (
                        format!("unanswered {mail}"),
                        format!(
                            "The agent of {name} was given your message at {given} and was reminded once, but has not messaged you since. Its manager has been told to find out why."
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

/// The reminder for an idle agent: its first `REMIND_LISTED` ready items and how many more there are.
// 🧑 "Normal human messages should arrive without things around them"; the parent: "drop ... the long open-item reminder lists (80+ items)"
fn remind_text(ready: &[&str], open: usize, old: &[(String, String, i64)]) -> String {
    let listed = &ready[..ready.len().min(REMIND_LISTED)];
    let more = match ready.len() - listed.len() {
        0 => String::new(),
        n => format!("\n- and {n} more"),
    };
    let mut text = format!(
        "Continue your ready items:\n- {}{more}\nSee all, with request IDs and dependencies, with `amh todo list`. This reminder is automatic: send nobody a message because of it.",
        listed.join("\n- ")
    );
    // 🧑 "I previously noticed a widespread problem of agents keeping lots of TODOs around. Let the relevant agent study this and how to prevent this"
    if open > crate::requests::LONG_LIST || !old.is_empty() {
        let oldest: Vec<String> = old
            .iter()
            .take(REMIND_LISTED)
            .map(|(id, item, days)| format!("{id} ({days} d): {item}"))
            .collect();
        text += &format!(
            "\nYou have {open} open items, {} of them ready for over {} days. For each, oldest first: finish it (`amh todo done`), make it wait on what it waits for (`amh todo wait ID --on WHAT` or `--for ID`), or, if it is a standing rule, write it into the suitable instructions and resolve it.",
            old.len(),
            crate::requests::OLD_S / 86400
        );
        if !oldest.is_empty() {
            text += &format!("\n- {}", oldest.join("\n- "));
        }
    }
    text
}

const REMIND_LISTED: usize = 3;

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
        if !crate::work::expects_agent(task) {
            continue;
        }
        if *problem == Problem::Idle {
            let ready = crate::requests::ready_items(task);
            if ready.is_empty() {
                continue;
            }
            let due = reminded_s
                .get(&task.name)
                .is_none_or(|at| now_s - at >= remind_every_s(task));
            if due {
                nudges.push(Nudge::Remind {
                    task: task.name.clone(),
                    address: task.address().to_owned(),
                    text: {
                        let (open, old) = crate::requests::stale(task, chrono::Utc::now());
                        remind_text(&ready, open, &old)
                    },
                });
            }
            continue;
        }
        let error = error_of(*problem, evidence);
        let key = problem_key(task, *problem, &error);
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

/// The key under which one agent problem is reported once.
fn problem_key(task: &Task, problem: Problem, error: &str) -> String {
    format!(
        "{} {} {} {}",
        task.name,
        task.address(),
        problem.name(),
        error.chars().take(200).collect::<String>()
    )
}

/// When each recipient was last told of a reported problem (Unix seconds) and how often it was told again, by `recipient problem-key`.
pub type Repeats = BTreeMap<String, (u64, u32)>;

/// How many times an unresolved agent problem is reported again to one recipient, at most.
pub const REPEAT_REPORTS: u32 = 3;

/// Where a notice about a task goes.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// To this Omnigent address; with a routing-defect note when it replaces the task's unreachable manager.
    To(String, Option<String>),
    /// The task is the main manager, which answers only to the human: no agent above it to tell.
    Root,
    /// Neither the recorded manager nor the main manager is an Omnigent session.
    Unroutable,
}

/// Route a notice about the task at `task_address` whose recorded manager is `manager`: to that manager when it is an Omnigent session, otherwise to the main manager with a note about the broken route; the main manager itself is the only exception.
pub fn notice_route(manager: &str, task_address: &str, main_manager: &str) -> Route {
    const OMNIGENT: &str = "omnigent://";
    if manager.starts_with(OMNIGENT) {
        Route::To(manager.to_owned(), None)
    } else if task_address == main_manager {
        Route::Root
    } else if main_manager.starts_with(OMNIGENT) {
        Route::To(
            main_manager.to_owned(),
            Some(format!(
                "Routing defect: this task's recorded manager {manager} is not an Omnigent session, so you get this instead. "
            )),
        )
    } else {
        Route::Unroutable
    }
}

/// One repeated problem notice; `entry` is recorded under `state_key` only once it was delivered.
#[derive(Debug, PartialEq)]
pub struct Repeat {
    pub to: String,
    pub text: String,
    pub state_key: String,
    pub entry: (u64, u32),
}

/// The repeated notices due for agent problems already reported and still unresolved after `every_s`, at most `REPEAT_REPORTS` per recipient; the main manager is told once its manager was told again, or after two periods if the manager could not be told.
///
/// Returns the notices and the records to keep: records of problems no longer found are dropped, and a problem seen for the first time starts its clock now. The caller records each notice's `entry` only after delivering it, so an undelivered notice is due again on the next scan.
// 🧑 "One problem you had was you would start a bunch of stuff and they would all stop at some point. You need mechanisms to repeatedly check."
pub fn repeat_reports(
    found: &[(Task, Problem, String)],
    reported: &BTreeSet<String>,
    active: &[Task],
    repeats: &Repeats,
    now_unix: u64,
    every_s: u64,
    main_manager: &str,
) -> (Vec<Repeat>, Repeats) {
    let mut notices = Vec::new();
    let mut kept = Repeats::new();
    for (task, problem, evidence) in found {
        if *problem == Problem::Idle {
            continue;
        }
        let error = error_of(*problem, evidence);
        let key = problem_key(task, *problem, &error);
        if !reported.contains(&key) {
            continue;
        }
        let recorded = task.field("managerat").unwrap_or_default();
        let (manager, note) = match notice_route(recorded, task.address(), main_manager) {
            Route::To(to, note) => (to, note.unwrap_or_default()),
            Route::Root | Route::Unroutable => continue,
        };
        let mut recipients = vec![manager.clone()];
        let (manager_at, manager_count) = repeats
            .get(&format!("{manager} {key}"))
            .copied()
            .unwrap_or((now_unix, 0));
        // Escalate once the manager was told again, or after two periods even if it could not be told.
        let overdue = manager_count >= 1 || now_unix.saturating_sub(manager_at) >= 2 * every_s;
        if overdue && !main_manager.is_empty() && main_manager != manager {
            recipients.push(main_manager.to_owned());
        }
        for to in recipients {
            let state_key = format!("{to} {key}");
            // The main manager is due at once: its manager was already told again.
            let start = if to == manager {
                now_unix
            } else {
                now_unix.saturating_sub(every_s)
            };
            let (at, count) = repeats.get(&state_key).copied().unwrap_or((start, 0));
            kept.insert(state_key.clone(), (at, count));
            if count >= REPEAT_REPORTS || now_unix.saturating_sub(at) < every_s {
                continue;
            }
            let mut text = format!(
                "Still unresolved (repeat {} of {REPEAT_REPORTS}): {}",
                count + 1,
                failure_text(task, *problem, &error, active)
            );
            if to != manager {
                text = format!("{} (its manager has not resolved it) {text}", task.name);
            } else {
                text = format!("{note}{text}");
            }
            notices.push(Repeat {
                to,
                text,
                state_key,
                entry: (now_unix, count + 1),
            });
        }
    }
    (notices, kept)
}

/// The repeat records as saved between watcher restarts: one `seconds count key` line each.
pub fn repeats_text(repeats: &BTreeMap<String, (u64, u32)>) -> String {
    repeats
        .iter()
        .map(|(key, (at, count))| format!("{at} {count} {key}\n"))
        .collect()
}

/// Read saved repeat records, skipping malformed lines.
pub fn parse_repeats(text: &str) -> BTreeMap<String, (u64, u32)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let at = parts.next()?.parse().ok()?;
            let count = parts.next()?.parse().ok()?;
            Some((parts.next()?.to_owned(), (at, count)))
        })
        .collect()
}

/// The message telling a manager about the problems of its agents.
pub fn problems_text(texts: &[String]) -> String {
    format!(
        "Handle these agent problems; only message the human if you cannot:\n\n{}",
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

/// The periodic nudge to the human manager.
// 🧑 "Email compression instructions no longer apply / What really is needed is the human manager should consolidate agents status once in a while and tell human what to handle next and prune any trifle that does not really need the human"
// 🧑 "Coordinate with human manager to actually use the priority, when the human manager. Reprioritizes. Which should happen once in a while."
pub const CONSOLIDATE_TEXT: &str = "Time to consolidate agent status: new agent messages reached the human since the last time. The human: \"What really is needed is the human manager should consolidate agents status once in a while and tell human what to handle next and prune any trifle that does not really need the human\". First rerank with `amh task priority` (see your instructions), then send the what-next list in that order. This reminder is automatic.";

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

/// Reminders an idle agent may get without any change to its task before its manager is told it stalled.
pub const STALL_REMINDERS: u32 = 3;

/// One idle task's reminders since its task last changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stall {
    pub task_text: String,
    pub reminders: u32,
    pub reported: bool,
}

/// What a delivered reminder means for an idle task: the updated stall record, and whether its manager must now be told it stalled.\n///\n/// The caller sets `reported` only after the manager was told, so a failed notice is retried on the next reminder.
///
/// Any change to the task file counts as progress and starts the count again.
// 🧑 "One problem you had was you would start a bunch of stuff and they would all stop at some point. You need mechanisms to repeatedly check."
pub fn stall_step(previous: Option<&Stall>, task_text: &str, limit: u32) -> (Stall, bool) {
    let mut stall = match previous {
        Some(stall) if stall.task_text == task_text => stall.clone(),
        _ => Stall {
            task_text: task_text.to_owned(),
            ..Stall::default()
        },
    };
    stall.reminders += 1;
    let escalate = !stall.reported && stall.reminders > limit;
    (stall, escalate)
}

/// What counts as progress of a task: its open items and request records, as a stable hash; notes, reminders and watcher metadata do not count.
pub fn progress_mark(items: &[String], request_records: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in items.join("\n").bytes().chain(request_records.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The stall records as saved between watcher restarts: one `task reminders reported mark` line each.
pub fn stalls_text(stalls: &BTreeMap<String, Stall>) -> String {
    stalls
        .iter()
        .map(|(task, stall)| {
            format!(
                "{task} {} {} {}\n",
                stall.reminders, stall.reported, stall.task_text
            )
        })
        .collect()
}

/// Read saved stall records, skipping malformed lines.
pub fn parse_stalls(text: &str) -> BTreeMap<String, Stall> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split(' ');
            let task = parts.next()?.to_owned();
            let reminders = parts.next()?.parse().ok()?;
            let reported = parts.next()?.parse().ok()?;
            let task_text = parts.next()?.to_owned();
            Some((
                task,
                Stall {
                    task_text,
                    reminders,
                    reported,
                },
            ))
        })
        .collect()
}

/// The message telling a manager that one of its agents keeps idling without progress.
pub fn stall_text(task: &str, reminders: u32, n_items: usize) -> String {
    format!(
        "{task} stalled: it received {reminders} reminders of its {n_items} open items and its open items and request stages have not changed since. This is advisory: long work can look like this. Check its actual output before acting; if it is stuck, answer what it waits for, unblock it, or replace it with `amh agent replace`."
    )
}

#[cfg(test)]
mod tests {

    #[test]
    fn notices_for_unreachable_managers_go_to_the_main_manager_except_for_the_main_manager() {
        let m = "omnigent://main";
        assert_eq!(
            notice_route("omnigent://boss", "omnigent://t", m),
            Route::To("omnigent://boss".into(), None)
        );
        let Route::To(to, Some(note)) = notice_route("wl:16", "omnigent://t", m) else {
            panic!()
        };
        assert!(to == m && note.contains("wl:16 is not an Omnigent session"));
        assert_eq!(notice_route("wl:1", m, m), Route::Root);
        assert_eq!(
            notice_route("wl:1", "omnigent://t", "wl:0"),
            Route::Unroutable
        );
        // A broken route escalates to the main manager with success-only counting; a repaired route goes to the new manager.
        let text = |managerat: &str| {
            format!(
                "---\nstatus: running\nrunat: omnigent://t\nmanagerat: {managerat}\npending_task_items:\n  - x\n---\n"
            )
        };
        let broken = Task::parse("t.md", &text("wl:16")).unwrap();
        let found = vec![(broken.clone(), Problem::Error, "error=boom".to_owned())];
        let key = problem_key(
            &broken,
            Problem::Error,
            &error_of(Problem::Error, "error=boom"),
        );
        let reported = BTreeSet::from([key.clone()]);
        let (_, start) = repeat_reports(
            &found,
            &reported,
            std::slice::from_ref(&broken),
            &Repeats::new(),
            0,
            50,
            m,
        );
        let (due, kept) = repeat_reports(
            &found,
            &reported,
            std::slice::from_ref(&broken),
            &start,
            60,
            50,
            m,
        );
        assert_eq!(due.len(), 1);
        assert!(due[0].to == m && due[0].text.starts_with("Routing defect"));
        assert_eq!(
            kept[&format!("{m} {key}")],
            (0, 0),
            "nothing counted before delivery"
        );
        let fixed = Task::parse("t.md", &text("omnigent://boss")).unwrap();
        let found = vec![(fixed.clone(), Problem::Error, "error=boom".to_owned())];
        let key = problem_key(
            &fixed,
            Problem::Error,
            &error_of(Problem::Error, "error=boom"),
        );
        let (_, start) = repeat_reports(
            &found,
            &BTreeSet::from([key.clone()]),
            std::slice::from_ref(&fixed),
            &Repeats::new(),
            0,
            50,
            m,
        );
        let (due, _) = repeat_reports(&found, &BTreeSet::from([key]), &[fixed], &start, 60, 50, m);
        assert_eq!(due[0].to, "omnigent://boss");
        let root = Task::parse("main.md", "---\nstatus: long_running\nrunat: omnigent://main\nmanagerat: wl:1\npending_task_items:\n  - x\n---\n").unwrap();
        let found = vec![(root.clone(), Problem::Error, "error=boom".to_owned())];
        let key = problem_key(
            &root,
            Problem::Error,
            &error_of(Problem::Error, "error=boom"),
        );
        let (none, _) = repeat_reports(
            &found,
            &BTreeSet::from([key]),
            &[root],
            &Repeats::new(),
            999,
            50,
            m,
        );
        assert!(none.is_empty());
    }

    #[test]
    fn unresolved_problems_are_repeated_boundedly_and_only_delivered_ones_count() {
        let task = Task::parse(
            "t.md",
            "---\nstatus: running\nrunat: omnigent://t\nmanagerat: omnigent://m\npending_task_items:\n  - x\n---\n",
        )
        .unwrap();
        let found = vec![(task.clone(), Problem::Error, "error=boom".to_owned())];
        let key = problem_key(
            &task,
            Problem::Error,
            &error_of(Problem::Error, "error=boom"),
        );
        let (mk, gk) = (format!("omnigent://m {key}"), format!("omnigent://g {key}"));
        let reported = BTreeSet::from([key.clone()]);
        let active = [task];
        let run = |state: &Repeats, now: u64| {
            repeat_reports(&found, &reported, &active, state, now, 50, "omnigent://g")
        };
        let deliver = |notices: Vec<Repeat>, mut state: Repeats, ok: &[&str]| {
            for notice in notices.into_iter().filter(|n| ok.contains(&n.to.as_str())) {
                state.insert(notice.state_key, notice.entry);
            }
            state
        };
        let (none, state) = run(&Repeats::new(), 100);
        assert!(none.is_empty() && state[&mk] == (100, 0));
        // Manager unreachable: nothing is counted, so it is due again.
        let (first, kept) = run(&state, 150);
        assert_eq!(first.len(), 1);
        let state = deliver(first, kept, &[]);
        assert_eq!(state[&mk], (100, 0));
        // Survives a restart through its text form, then is delivered.
        let state = parse_repeats(&repeats_text(&state));
        let (again, kept) = run(&state, 160);
        assert!(
            again[0]
                .text
                .starts_with("Still unresolved (repeat 1 of 3)")
        );
        let state = deliver(again, kept, &["omnigent://m"]);
        // Now the main manager is told too; its failure does not block the manager's own count.
        let (both, kept) = run(&state, 220);
        assert_eq!(
            both.iter().map(|n| n.to.as_str()).collect::<Vec<_>>(),
            ["omnigent://m", "omnigent://g"]
        );
        let state = deliver(both, kept, &["omnigent://m"]);
        assert_eq!((state[&mk].1, state[&gk].1), (2, 0));
        let (main_again, _) = run(&state, 230);
        assert_eq!(
            main_again.iter().map(|n| n.to.as_str()).collect::<Vec<_>>(),
            ["omnigent://g"]
        );
        // Bounded: after three delivered repeats the manager is not told again.
        let (third, kept) = run(&state, 300);
        let state = deliver(third, kept, &["omnigent://m", "omnigent://g"]);
        assert_eq!(state[&mk].1, 3);
        let (later, _) = run(&state, 900);
        assert!(later.iter().all(|n| n.to == "omnigent://g"));
        // A manager that can never be told: the main manager is still told after two periods.
        let (_, start) = run(&Repeats::new(), 1000);
        let (early, kept) = run(&start, 1060);
        let start = deliver(early, kept, &[]);
        let (late, _) = run(&start, 1100);
        assert_eq!(
            late.iter().map(|n| n.to.as_str()).collect::<Vec<_>>(),
            ["omnigent://m", "omnigent://g"]
        );
        let (gone, kept) = repeat_reports(&[], &reported, &active, &state, 900, 50, "omnigent://g");
        assert!(gone.is_empty() && kept.is_empty());
    }

    #[test]
    fn stall_is_reported_once_after_the_limit_and_reset_by_progress() {
        let (one, e1) = stall_step(None, "a", 2);
        let (two, e2) = stall_step(Some(&one), "a", 2);
        let (three, e3) = stall_step(Some(&two), "a", 2);
        let (unsent, e4) = stall_step(Some(&three), "a", 2);
        assert!(e4, "an undelivered stall notice is retried");
        let mut told = unsent.clone();
        told.reported = true;
        let (four, e5) = stall_step(Some(&told), "a", 2);
        assert_eq!((e1, e2, e3, e5), (false, false, true, false));
        assert_eq!(four.reminders, 5);
        let (moved, e6) = stall_step(Some(&four), "b", 2);
        assert!(!e6 && moved.reminders == 1 && !moved.reported);
        assert!(
            stall_text("t.md", 4, 2)
                .starts_with("t.md stalled: it received 4 reminders of its 2 open items")
        );
    }
    use super::*;
    use crate::work::tests::task;

    const MAIN: &str = "omnigent://main";

    #[test]
    fn recent_human_lists_only_the_human_s_own_messages_since_the_cutoff() {
        let e = |name: &str, at: i64, text: &str| (name.to_owned(), at, text.to_owned());
        let entries = [
            e(
                "zulip-2.txt",
                200,
                "Subject: [t_one] Zulip message\n\nOn Monday please do X",
            ),
            e(
                "zulip-1.txt",
                100,
                "Subject: [t_one] Zulip message\n\nToo old",
            ),
            e(
                "85c5dff58359-26.txt",
                150,
                "Subject: Re: [t_two] pb news\n\nRegister for this\n\nOn Tue someone wrote:\n> quoted",
            ),
            e(
                "routed-human-1-9.txt",
                160,
                "From: x\nSubject: CI\n\nnot the human",
            ),
            e("vl_x_body.txt", 170, "Subject: [t] x\n\nagent file"),
            e(
                "85c5dff58359-edit-3.txt",
                180,
                "Subject: [t] x\n\nedit file",
            ),
        ];
        let out = recent_human(&entries, 120, "manager_mail");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(
            lines[0].ends_with("[t_two] manager_mail/85c5dff58359-26.txt: Register for this"),
            "{out}"
        );
        assert!(
            lines[1].ends_with("[t_one] manager_mail/zulip-2.txt: On Monday please do X"),
            "{out}"
        );
    }

    #[test]
    fn routed_mail_names_its_receipt() {
        let text = "From: a@b\nMessage-ID: <x@y>\nSubject: s\n\nbody";
        let parsed = routed_mail("routed-human-1-26558.txt", text).unwrap();
        assert_eq!(
            parsed,
            ("human".into(), 1, "26558".into(), "human:<x@y>".into())
        );
        let parsed = routed_mail("routed-agent-7-9.txt", "From: a@b\nMessage-ID: \n").unwrap();
        assert_eq!(parsed.3, "agent:7:9");
        assert!(routed_mail("zulip-1.txt", text).is_err());
        assert!(routed_mail("routed-human-x-1.txt", text).is_err());
    }

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
            ("[t_sub] hello", "Terminate this agent now", "t_sub.md"),
            (
                "[t_sub] hello",
                "replace this agent, it is stuck",
                "t_sub.md",
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
        assert!(replace_order("> quoted\n\n**Replace this agent** please"));
        assert!(!replace_order("please\nreplace this agent"));
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
    fn an_idle_reminder_lists_few_ready_items() {
        let ready: Vec<String> = (1..=80).map(|n| format!("item {n}")).collect();
        let ready: Vec<&str> = ready.iter().map(String::as_str).collect();
        let text = remind_text(&ready, ready.len(), &[]);
        assert!(text.starts_with(
            "Continue your ready items:\n- item 1\n- item 2\n- item 3\n- and 77 more\n"
        ));
        assert!(!text.contains("item 4"));
        assert!(!remind_text(&["a", "b"], 2, &[]).contains("more"));
        let old = [("t.md#1".to_owned(), "x".to_owned(), 9)];
        assert!(remind_text(&["x"], 1, &old).contains("- t.md#1 (9 d): x"));
        assert!(remind_text(&["x"], 16, &[]).contains("You have 16 open items, 0 of them"));
    }

    #[test]
    fn a_block_becomes_the_message_for_its_agent() {
        let body = "note\n\n(pending)\n(record and delegate manager_mail/m-11.txt)\n\n(pending)\nplain block line one\nline two\n";
        let blocks = taskfile::pending_blocks(body);
        assert_eq!(source_of(&blocks[0].1[0]), Source::Mail("m-11.txt"));
        assert_eq!(source_of(&blocks[1].1[0]), Source::Plain);
        assert_eq!(source_of("(guest mail d/guest-3.txt)"), Source::Guest);
        let reply = "Subject: Re: [t_sub] do more\n\nPlease do Z.\r\n> old\r\n";
        assert_eq!(
            mail_text("m-11.txt", reply, "t_sub", true),
            format!("Please do Z.\n> old\n\n({ACCEPT})")
        );
        assert_eq!(
            mail_text(
                "zulip-7.txt",
                "Subject: [t_sub] Zulip message\n\nStop it\n",
                "t_sub",
                true,
            ),
            format!("Stop it\n\n({ACCEPT})")
        );
        assert_eq!(
            mail_text(
                "zulip-7.txt",
                "Subject: [toms_river] Zulip message\n\nTerminate this agent. Done\n",
                "human_mgr",
                true,
            ),
            format!("In the toms_river thread:\nTerminate this agent. Done\n\n({ACCEPT})")
        );
        assert_eq!(
            mail_text(
                "zulip-7.txt",
                "Subject: [toms river] Zulip message\n\nhi\n",
                "m",
                true,
            ),
            format!("In the toms river thread:\nhi\n\n({ACCEPT})")
        );
        assert_eq!(
            mail_text(
                "h-edit-1.txt",
                "Subject: [t_sub] said with amh say\n\nGo on.\n",
                "t_sub",
                true,
            ),
            format!("Go on.\n\n({ACCEPT})")
        );
        assert_eq!(
            mail_text(
                "m-12.txt",
                "Subject: [t_sub] build X\n\nUse Rust.\n",
                "t_sub",
                true,
            ),
            format!("Subject: [t_sub] build X\n\nUse Rust.\n\n({ACCEPT})")
        );
        assert_eq!(
            mail_text("zulip-9.txt", "first para\n\nsecond\n", "t_sub", true),
            format!("first para\n\nsecond\n\n({ACCEPT})")
        );
        assert_eq!(
            mail_text(
                "zulip-8.txt",
                "Subject: [t] Zulip message\n\nok\n",
                "t",
                true
            ),
            format!("ok\n\n({ACCEPT})")
        );
        assert_eq!(
            mail_text(
                "zulip-8.txt",
                "Subject: [t] Zulip message\n\nok\n",
                "t",
                false
            ),
            "ok"
        );
        assert_eq!(
            plain_text(&blocks[1].1, false),
            "plain block line one\nline two"
        );
        assert_eq!(
            plain_text(&["do it".to_owned()], true),
            format!("do it{ADD_REMINDER}")
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
        let chased =
            |awaiting: &str, sent: &str| chase(awaiting, sent, &tasks, now, given, "manager_mail");
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
            "The human's message manager_mail/m-11.txt was given to you at T99095 and you have not messaged the human since. Message the human now with `amh tell human`, even if only to say you got it and what you are doing. If the human wrote that no reply is needed, run `amh mail handled m-11.txt --because 'THE HUMAN'S WORDS'` instead."
        );
        // A send by a task the reminded one manages answers it; one before the email does not.
        assert_eq!(chased("99095 t_main.md m-11.txt 0", "99500 t_work\n"), []);
        assert_eq!(
            chased("99095 t_main.md m-11.txt 0", "99000 t_work\n").len(),
            1
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
        assert!(text.contains("at T98195, was reminded, and still has not messaged the human"));
        let (key, text) = human.clone().unwrap();
        assert_eq!(key, "unanswered m-11.txt");
        assert!(text.contains("has not messaged you since"));
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
                    text: "Continue your ready items:\n- x\nSee all, with request IDs and dependencies, with `amh todo list`. This reminder is automatic: send nobody a message because of it.".to_owned(),
                },
            ]
        );
        assert_eq!(now, BTreeSet::from([key.to_owned()]));
        assert_eq!(
            problems_text(&[failure.to_owned()]),
            format!(
                "Handle these agent problems; only message the human if you cannot:\n\n{failure}"
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
