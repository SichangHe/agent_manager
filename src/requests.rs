//! Request records stored alongside the compatible open-item list in task frontmatter.

use crate::taskfile::HUMAN_MARK;
use crate::tasks::Task;
use serde_json::{Value, json};

const KEY: &str = "request_records";

// 🧑 "there should only be three stages ... Let's use resolution requested and resolved."
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Added,
    Requested,
    Resolved,
}

impl Stage {
    pub fn text(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Requested => "resolution requested",
            Self::Resolved => "resolved",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub id: String,
    pub text: String,
    pub creator: String,
    pub source: String,
    pub stage: Stage,
    pub blockers: Vec<String>,
    pub evidence: String,
    pub resolved_at: Option<String>,
    /// When it was added (RFC 3339); legacy records get it from the event log or the first maintenance pass.
    pub added_at: Option<String>,
}

/// Open items that can progress; legacy items without request records remain ready.
// 🧑 "change amh s.t. \"blocking\" status is per-task instead of per-agent"
pub fn ready_items(task: &Task) -> Vec<&str> {
    let records = read(task).unwrap_or_default();
    task.items
        .iter()
        .filter(|item| {
            !records.iter().any(|r| {
                r.text == **item
                    && r.stage != Stage::Resolved
                    && (r.stage == Stage::Requested || !r.blockers.is_empty())
            })
        })
        .map(String::as_str)
        .collect()
}

/// Read records without silently discarding corrupt metadata.
pub fn read(task: &Task) -> Result<Vec<Request>, String> {
    let Some(raw) = task.field(KEY) else {
        return Ok(Vec::new());
    };
    let records: Value = serde_json::from_str(raw)
        .map_err(|e| format!("amh: {} has invalid {KEY}: {e}", task.name))?;
    let records = records
        .as_array()
        .ok_or_else(|| format!("amh: {} {KEY} is not an array", task.name))?;
    records
        .iter()
        .map(|record| {
            let text = |key: &str| {
                record[key]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("amh: {} request lacks {key}", task.name))
            };
            let stage = match text("stage")?.as_str() {
                "added" => Stage::Added,
                "resolution requested" => Stage::Requested,
                "resolved" => Stage::Resolved,
                other => return Err(format!("amh: invalid request stage {other}")),
            };
            let blockers = record["blockers"]
                .as_array()
                .ok_or("amh: request blockers is not an array")?
                .iter()
                .map(|id| {
                    id.as_str()
                        .map(str::to_owned)
                        .ok_or("amh: invalid blocker ID".to_owned())
                })
                .collect::<Result<_, _>>()?;
            Ok(Request {
                id: text("id")?,
                text: text("text")?,
                creator: text("creator")?,
                source: text("source")?,
                stage,
                blockers,
                evidence: text("evidence")?,
                resolved_at: record["resolved_at"].as_str().map(str::to_owned),
                added_at: record["added_at"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

pub fn write(task: &mut Task, requests: &[Request]) {
    let records: Vec<Value> = requests.iter().map(|r| json!({"id": r.id, "text": r.text, "creator": r.creator, "source": r.source, "stage": r.stage.text(), "blockers": r.blockers, "evidence": r.evidence, "resolved_at":r.resolved_at, "added_at": r.added_at})).collect();
    task.fields.retain(|(key, _)| key != KEY);
    task.fields
        .push((KEY.to_owned(), json!(records).to_string()));
}

/// Adopt legacy open items without changing their text or inferring external provenance.
pub fn sync(task: &mut Task) -> Result<Vec<Request>, String> {
    let mut records = read(task)?;
    for text in &task.items {
        if records
            .iter()
            .any(|r| r.stage != Stage::Resolved && r.text == *text)
        {
            continue;
        }
        let n = records
            .iter()
            .filter_map(|r| {
                r.id.strip_prefix(&format!("{}#", task.name))?
                    .parse::<u64>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
            .max(
                task.field("request_next")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            )
            + 1;
        task.fields.retain(|(key, _)| key != "request_next");
        task.fields.push(("request_next".to_owned(), n.to_string()));
        records.push(Request {
            id: format!("{}#{n}", task.name),
            text: text.clone(),
            creator: if text.starts_with(HUMAN_MARK) {
                "human".to_owned()
            } else {
                task.name.clone()
            },
            source: "legacy pending_task_items; original source unknown".to_owned(),
            stage: Stage::Added,
            blockers: Vec::new(),
            evidence: String::new(),
            resolved_at: None,
            added_at: Some(chrono::Utc::now().to_rfc3339()),
        });
    }
    write(task, &records);
    Ok(records)
}

pub fn register(task: &mut Task, text: &str, creator: &str, source: &str) -> Result<(), String> {
    let mut records = sync(task)?;
    let request = records
        .iter_mut()
        .find(|r| r.text == text && r.stage != Stage::Resolved)
        .ok_or("amh: added item has no request")?;
    request.creator = creator.to_owned();
    request.source = source.to_owned();
    write(task, &records);
    Ok(())
}

/// Resolve human requests directly; agent requests wait for their creator's decision.
// 🧑 "Yes, just email" ... "agents do not report to humans and do not need human approval, but need task creator approval"
pub fn finish(task: &mut Task, given: &str, evidence: &str) -> Result<Request, String> {
    let mut records = sync(task)?;
    let bare = |s: &str| s.strip_prefix(HUMAN_MARK).unwrap_or(s).trim().to_owned();
    let request = records
        .iter_mut()
        .find(|r| r.stage != Stage::Resolved && (r.id == given || bare(&r.text) == bare(given)))
        .ok_or("amh: no such open request; copy its ID or text from `amh todo list`")?;
    if request.stage != Stage::Added {
        return Err("amh: resolution already requested; wait for the creator".to_owned());
    }
    if !request.blockers.is_empty() {
        return Err("amh: request still has unresolved dependencies".to_owned());
    }
    request.evidence = evidence.to_owned();
    // Items on the human's own task are the human's to resolve, whoever asked.
    request.stage = if request.creator == "human" || task.name == crate::tasks::HUMAN_TASK {
        Stage::Resolved
    } else {
        Stage::Requested
    };
    request.resolved_at =
        (request.stage == Stage::Resolved).then(|| chrono::Utc::now().to_rfc3339());
    let result = request.clone();
    if result.stage == Stage::Resolved {
        crate::tasks::remove_item(task, &result.text, evidence)?;
    }
    write(task, &records);
    Ok(result)
}

pub fn decide(
    task: &mut Task,
    id: &str,
    creator: &str,
    approve: bool,
    reason: &str,
) -> Result<Request, String> {
    let mut records = sync(task)?;
    let request = records
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or("amh: no such request")?;
    if request.creator != creator {
        return Err(format!(
            "amh: only {} may decide this request",
            request.creator
        ));
    }
    // The creator may also approve a request its owner never finished, which withdraws it; else a creator whose worker is gone could never close.
    if request.stage == Stage::Resolved || (!approve && request.stage != Stage::Requested) {
        return Err("amh: request is not awaiting a resolution decision".to_owned());
    }
    if request.stage == Stage::Added {
        request.evidence = "withdrawn by its creator".to_owned();
    }
    if !approve && reason.trim().is_empty() {
        return Err("amh: rejection needs --reason".to_owned());
    }
    request.stage = if approve {
        Stage::Resolved
    } else {
        Stage::Added
    };
    request.resolved_at = approve.then(|| chrono::Utc::now().to_rfc3339());
    if !approve {
        request.evidence = reason.to_owned();
    }
    let result = request.clone();
    if approve {
        crate::tasks::remove_item(task, &result.text, &result.evidence)?;
    }
    write(task, &records);
    Ok(result)
}

pub fn listed(task: &mut Task) -> Result<String, String> {
    Ok(sync(task)?
        .iter()
        .filter(|r| r.stage != Stage::Resolved)
        .map(|r| {
            format!(
                "{} [{}] {}{}\n",
                r.id,
                r.stage.text(),
                r.text,
                if r.blockers.is_empty() {
                    String::new()
                } else {
                    format!(" (waits for {})", r.blockers.join(", "))
                }
            )
        })
        .collect())
}

// 🧑 "the target agent needs to explicitly transfer task ownership to themselves"
pub fn transfer(source: &mut Task, to: &mut Task, id: &str) -> Result<(), String> {
    if source.name == to.name || to.status() == "done" {
        return Err("amh: destination is done or already owns the request".to_owned());
    }
    let mut from = sync(source)?;
    let at = from
        .iter()
        .position(|r| r.id == id)
        .ok_or("amh: no such request")?;
    if from[at].stage != Stage::Added {
        return Err("amh: only added requests may change owner".to_owned());
    }
    if to.items.contains(&from[at].text) {
        return Err("amh: destination already has the same open item".to_owned());
    }
    let request = from.remove(at);
    let mut target = sync(to)?;
    source.items.retain(|item| item != &request.text);
    to.items.push(request.text.clone());
    target.push(request);
    write(source, &from);
    write(to, &target);
    Ok(())
}

/// Whether the human gave this task a request themselves; only then do automatic notices about the task go to the human.
// The human manager: "automatic 'Closed omnigent://...' notices, worker acknowledgements, and 'agent has failed' notices sent to the human (dozens tonight ...) Could those go to the worker's manager only, not the human?"
pub fn from_human(task: &Task) -> bool {
    read(task).is_ok_and(|records| records.iter().any(|request| request.creator == "human"))
}

pub fn can_close(task: &Task, all: &[Task]) -> Result<(), String> {
    for owner in all {
        let mut owner = owner.clone();
        for request in sync(&mut owner)? {
            if request.stage != Stage::Resolved
                && (owner.name == task.name || request.creator == task.name)
            {
                return Err(format!(
                    "amh: {} must stay alive for unresolved request {}; {} resolves it with `amh todo done`, then {} with `amh todo approve`, which also works alone to withdraw it",
                    task.name, request.id, owner.name, request.creator
                ));
            }
        }
    }
    Ok(())
}

/// Reject missing blockers and cycles before recording any edge.
pub fn wait(task: &mut Task, id: &str, blockers: &[String], all: &[Task]) -> Result<(), String> {
    let mut graph = Vec::new();
    for owner in all {
        let mut owner = owner.clone();
        graph.extend(sync(&mut owner)?);
    }
    let mut records = sync(task)?;
    let request = records
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or("amh: no such request")?;
    if request.stage != Stage::Added {
        return Err("amh: only added requests may change dependencies".to_owned());
    }
    let mut pending = Vec::new();
    for blocker in blockers {
        let found = graph
            .iter()
            .find(|r| r.id == *blocker)
            .ok_or_else(|| format!("amh: no blocker request {blocker}"))?;
        let mut frontier = vec![blocker.clone()];
        let mut seen = Vec::new();
        while let Some(next) = frontier.pop() {
            if next == id {
                return Err("amh: dependency would create a cycle".to_owned());
            }
            if seen.contains(&next) {
                continue;
            }
            if let Some(r) = graph.iter().find(|r| r.id == next) {
                frontier.extend(r.blockers.clone());
            }
            seen.push(next);
        }
        if found.stage != Stage::Resolved && !pending.contains(blocker) {
            pending.push(blocker.clone());
        }
    }
    pending.extend(
        request
            .blockers
            .iter()
            .filter(|b| b.starts_with(EXTERNAL) || b.starts_with(DATE))
            .cloned(),
    );
    request.blockers = pending;
    write(task, &records);
    Ok(())
}

/// Record messages in markdown before delivery, so an interrupted action can retry them.
pub fn queue(task: &mut Task, to: &str, message: &str) -> Result<(), String> {
    let mut queued = notifications(task)?;
    queued.push(json!({"to": to, "text": message}));
    set_notifications(task, queued);
    Ok(())
}

pub fn notifications(task: &Task) -> Result<Vec<Value>, String> {
    match task.field("request_notifications") {
        None => Ok(Vec::new()),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| format!("amh: invalid request notifications: {e}")),
    }
}

pub fn validate(task: &Task) -> Result<(), String> {
    let records = read(task)?;
    for (at, request) in records.iter().enumerate() {
        if records[..at].iter().any(|r| r.id == request.id) {
            return Err(format!(
                "amh: {} repeats request ID {}",
                task.name, request.id
            ));
        }
        if request.stage != Stage::Resolved && !task.items.contains(&request.text) {
            return Err(format!(
                "amh: {} request {} has no matching pending_task_items entry; resolve it with amh",
                task.name, request.id
            ));
        }
    }
    Ok(())
}

pub fn set_notifications(task: &mut Task, queued: Vec<Value>) {
    task.fields
        .retain(|(key, _)| key != "request_notifications");
    if !queued.is_empty() {
        task.fields.push((
            "request_notifications".to_owned(),
            json!(queued).to_string(),
        ));
    }
}

/// How an external reason is stored among a request's blockers; it never matches a request ID, so only its owner clears it.
pub const EXTERNAL: &str = "on: ";

/// How a wait for a date is stored among a request's blockers: `until: RFC3339`; the watcher drops it once the date passes.
pub const DATE: &str = "until: ";

/// A date or time: `2026-11-20`, `2026-11-20 14:00` (local time), or RFC 3339.
// 🧑 "I think tasks can block on other tasks, a date, or else should cause reminders to the responsible agent. Let's disallow blocking on someone."
pub fn parse_date(text: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    use chrono::TimeZone;
    let text = text.trim();
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(at.with_timezone(&chrono::Utc));
    }
    let local = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M")
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(|day| day.and_hms_opt(0, 0, 0).unwrap_or_default())
        })
        .map_err(|_| {
            format!("amh: say a date like 2026-11-20 or 2026-11-20 14:00, not `{text}`")
        })?;
    chrono::Local
        .from_local_datetime(&local)
        .earliest()
        .map(|at| at.with_timezone(&chrono::Utc))
        .ok_or_else(|| format!("amh: `{text}` is no valid local time"))
}

/// Make one added request wait until `at`, replacing an earlier date wait.
pub fn wait_until(
    task: &mut Task,
    id: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    let mut records = sync(task)?;
    let request = records
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or("amh: no such request")?;
    if request.stage != Stage::Added {
        return Err("amh: only added requests may change dependencies".to_owned());
    }
    request.blockers.retain(|b| !b.starts_with(DATE));
    request.blockers.push(format!(
        "{DATE}{}",
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ));
    write(task, &records);
    Ok(())
}

/// Replace the outside wait of request `id` with a dependency on `human_item`, the human's item made from it.
pub fn wait_to_item(task: &mut Task, id: &str, human_item: &str) -> Result<(), String> {
    let mut records = sync(task)?;
    let request = records
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or("amh: no such request")?;
    request.blockers.retain(|b| !b.starts_with(EXTERNAL));
    request.blockers.push(human_item.to_owned());
    write(task, &records);
    Ok(())
}

/// How the next reminder of an outside wait is written after its reason: `[remind RFC3339 #N]`, N reminders so far.
const REMIND: &str = " [remind ";

// 🧑 "Blocked tasks that are not blocked on other tasks should have long reminders with exponential backoff. Maybe 1hr, base 1.5?"
/// The first reminder of a wait comes this long after it starts.
pub const REMIND_FIRST_S: f64 = 3600.0;
/// Each later reminder waits this many times longer than the one before.
pub const REMIND_BASE: f64 = 1.5;

/// When the reminder after `n` earlier ones is due, counted from `from`.
pub fn next_reminder(from: chrono::DateTime<chrono::Utc>, n: i32) -> chrono::DateTime<chrono::Utc> {
    from + chrono::Duration::seconds((REMIND_FIRST_S * REMIND_BASE.powi(n)) as i64)
}

/// An outside wait's reason, its next reminder time and how many reminders came before; `None` for a blocker without a schedule.
pub fn wait_schedule(blocker: &str) -> (&str, Option<(chrono::DateTime<chrono::Utc>, i32)>) {
    let reason = blocker.strip_prefix(EXTERNAL).unwrap_or(blocker);
    let parsed = reason
        .rsplit_once(REMIND)
        .and_then(|(reason, rest)| Some((reason, parse_schedule(rest.strip_suffix(']')?)?)));
    match parsed {
        Some((reason, schedule)) => (reason, Some(schedule)),
        None => (reason, None),
    }
}

/// A reminder schedule as text: `RFC3339 #N`.
pub fn schedule_text(at: chrono::DateTime<chrono::Utc>, n: i32) -> String {
    format!(
        "{} #{n}",
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

pub fn parse_schedule(text: &str) -> Option<(chrono::DateTime<chrono::Utc>, i32)> {
    let (at, n) = text.split_once(" #")?;
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    Some((at.with_timezone(&chrono::Utc), n.parse().ok()?))
}

fn scheduled(reason: &str, at: chrono::DateTime<chrono::Utc>, n: i32) -> String {
    format!("{EXTERNAL}{reason}{REMIND}{}]", schedule_text(at, n))
}

/// Make one added request wait for something outside amh, `reason`, starting at `now`, or clear that wait with `None`; its request-ID dependencies stay.
// 🧑 "change amh s.t. \"blocking\" status is per-task instead of per-agent"
pub fn wait_on(
    task: &mut Task,
    id: &str,
    reason: Option<(&str, chrono::DateTime<chrono::Utc>)>,
) -> Result<(), String> {
    let mut records = sync(task)?;
    let request = records
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or("amh: no such request")?;
    if request.stage != Stage::Added {
        return Err("amh: only added requests may change dependencies".to_owned());
    }
    request.blockers.retain(|b| !b.starts_with(EXTERNAL));
    if let Some((reason, now)) = reason {
        let reason = reason.trim();
        if reason.is_empty() || reason.contains(REMIND.trim()) {
            return Err("amh: say what the request waits for, without `[remind`".to_owned());
        }
        request
            .blockers
            .push(scheduled(reason, next_reminder(now, 0), 0));
    }
    write(task, &records);
    Ok(())
}

/// Remind about outside waits that are due, with backoff, and date records that lack `added_at` with `added(id)` or `now`; true when anything changed.
///
/// A wait without a schedule (made before reminders) gets its first reminder `REMIND_FIRST_S` from `now`.
pub fn remind_waits(
    task: &mut Task,
    now: chrono::DateTime<chrono::Utc>,
    added: impl Fn(&str) -> Option<String>,
) -> Result<bool, String> {
    let known = read(task)?.len();
    let mut records = sync(task)?;
    let mut changed = records.len() != known;
    let mut due = Vec::new();
    let mut ended = Vec::new();
    for request in records.iter_mut().filter(|r| r.stage != Stage::Resolved) {
        if request.added_at.is_none() {
            request.added_at = Some(added(&request.id).unwrap_or_else(|| now.to_rfc3339()));
            changed = true;
        }
        let before = request.blockers.len();
        request.blockers.retain(|blocker| {
            let passed = blocker
                .strip_prefix(DATE)
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .is_some_and(|at| at <= now);
            if passed {
                ended.push(format!("{}: {}", request.id, request.text));
            }
            !passed
        });
        changed |= request.blockers.len() != before;
        for blocker in request
            .blockers
            .iter_mut()
            .filter(|b| b.starts_with(EXTERNAL))
        {
            let (reason, schedule) = wait_schedule(blocker);
            let next = match schedule {
                None => scheduled(reason, next_reminder(now, 0), 0),
                Some((at, n)) if at <= now => {
                    due.push(format!(
                        "{}: {} (waits on: {reason})",
                        request.id, request.text
                    ));
                    scheduled(reason, next_reminder(now, n + 1), n + 1)
                }
                Some(_) => continue,
            };
            *blocker = next;
            changed = true;
        }
    }
    write(task, &records);
    if !ended.is_empty() {
        let text = format!(
            "The date these items waited for has come, so they are ready again:\n- {}",
            ended.join("\n- ")
        );
        queue(task, &task.name.clone(), &text)?;
    }
    if !due.is_empty() {
        queue(
            task,
            &task.name.clone(),
            &format!(
                "These items still wait:\n- {}\nIf a wait is over, clear it (`amh todo wait ID --clear-on`) and do the item; if it waits on the human, make it the human's item (`amh todo wait ID --ask-human`); if the item is finished or no longer wanted, resolve it (`amh todo done`). A standing rule is no item: write it into the suitable instructions and resolve the item. Otherwise nothing is needed; the next reminder comes later.",
                due.join("\n- ")
            ),
        )?;
    }
    Ok(changed)
}

/// Ready open requests older than `OLD_S`, oldest first, with their age in days; and the number of open items.
pub fn stale(
    task: &Task,
    now: chrono::DateTime<chrono::Utc>,
) -> (usize, Vec<(String, String, i64)>) {
    let ready = ready_items(task);
    let mut old: Vec<(String, String, i64)> = read(task)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.stage == Stage::Added && ready.contains(&r.text.as_str()))
        .filter_map(|r| {
            let added = chrono::DateTime::parse_from_rfc3339(r.added_at.as_deref()?).ok()?;
            let age_s = (now - added.with_timezone(&chrono::Utc)).num_seconds();
            (age_s >= OLD_S).then_some((r.id, r.text, age_s / 86400))
        })
        .collect();
    old.sort_by_key(|(_, _, days)| std::cmp::Reverse(*days));
    (task.items.len(), old)
}

/// An open item this old is flagged and reminded about with the choice to finish, wait with an end, or make it a rule.
pub const OLD_S: i64 = 7 * 86400;
/// A list with more open items than this is flagged.
pub const LONG_LIST: usize = 15;

pub fn unblock(task: &mut Task, resolved: &[String]) -> Result<bool, String> {
    let mut records = sync(task)?;
    let mut changed = false;
    for request in &mut records {
        let was_blocked = !request.blockers.is_empty();
        let n = request.blockers.len();
        request.blockers.retain(|id| !resolved.contains(id));
        if was_blocked && request.blockers.is_empty() {
            queue(
                task,
                &task.name.clone(),
                &format!(
                    "All dependencies resolved for {}: {}",
                    request.id, request.text
                ),
            )?;
            changed = true;
        } else if n != request.blockers.len() {
            changed = true;
        }
    }
    write(task, &records);
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(name: &str, human: bool) -> Task {
        let mut task = Task {
            name: name.to_owned(),
            fields: vec![
                ("status".to_owned(), "running".to_owned()),
                ("runat".to_owned(), format!("omnigent://{name}")),
            ],
            items: Vec::new(),
            body: String::new(),
        };
        assert!(crate::tasks::add_item(&mut task, "deliver the requested change", human).is_ok());
        task
    }

    #[test]
    fn reminders_follow_each_request_through_wait_and_release() -> Result<(), String> {
        let mut owner = task("owner.md", false);
        let mut dependency = task("dependency.md", true);
        let waiting = sync(&mut owner)?[0].id.clone();
        let blocker = sync(&mut dependency)?[0].id.clone();
        crate::tasks::add_item(&mut owner, "independent work", false)?;
        let all = [owner.clone(), dependency.clone()];
        wait(&mut owner, &waiting, std::slice::from_ref(&blocker), &all)?;
        crate::tasks::set_status(&mut owner, "long_running", Some("legacy blocker"))?;
        let mut owner = Task::parse(&owner.name, &owner.render())?;
        assert_eq!(owner.status(), "long_running");
        assert_eq!(owner.field("blocked_on"), Some("legacy blocker"));
        assert!(crate::work::expects_agent(&owner));
        assert_eq!(ready_items(&owner), ["independent work"]);
        assert_eq!(
            crate::work::problem_of(&owner, "ready"),
            Some(crate::work::Problem::Idle)
        );
        let found = [(owner.clone(), crate::work::Problem::Idle, String::new())];
        let (nudges, _) = crate::decide::nudges(
            &found,
            &Default::default(),
            &[owner.clone()],
            &Default::default(),
            0,
            |_| 1,
        )
        .ok_or("unexpected unavailable runtime")?;
        assert!(
            matches!(&nudges[..], [crate::decide::Nudge::Remind { text, .. }]
            if text.contains("independent work") && !text.contains("deliver the requested change"))
        );
        let reminded = std::collections::BTreeMap::from([(owner.name.clone(), 0)]);
        let (nudges, _) = crate::decide::nudges(
            &found,
            &Default::default(),
            &[owner.clone()],
            &reminded,
            9,
            |_| 10,
        )
        .ok_or("unexpected unavailable runtime")?;
        assert!(nudges.is_empty());
        let (nudges, _) = crate::decide::nudges(
            &found,
            &Default::default(),
            &[owner.clone()],
            &reminded,
            10,
            |_| 10,
        )
        .ok_or("unexpected unavailable runtime")?;
        assert!(matches!(&nudges[..], [crate::decide::Nudge::Remind { .. }]));
        finish(&mut owner, "independent work", "delivered")?;
        assert!(ready_items(&owner).is_empty());
        assert_eq!(crate::work::problem_of(&owner, "ready"), None);
        let found = [(owner.clone(), crate::work::Problem::Idle, String::new())];
        let (nudges, _) = crate::decide::nudges(
            &found,
            &Default::default(),
            &[owner.clone()],
            &Default::default(),
            0,
            |_| 1,
        )
        .ok_or("unexpected unavailable runtime")?;
        assert!(nudges.is_empty());
        finish(&mut dependency, &blocker, "dependency delivered")?;
        unblock(&mut owner, std::slice::from_ref(&blocker))?;
        assert_eq!(ready_items(&owner), ["deliver the requested change"]);
        assert_eq!(
            crate::work::problem_of(&owner, "ready"),
            Some(crate::work::Problem::Idle)
        );
        Ok(())
    }

    #[test]
    fn legacy_and_repeated_items_remain_ready() -> Result<(), String> {
        let mut owner = task("owner.md", true);
        assert_eq!(ready_items(&owner), owner.items);
        let text = owner.items[0].clone();
        finish(&mut owner, &text, "finished")?;
        crate::tasks::add_item(&mut owner, &text, false)?;
        sync(&mut owner)?;
        assert_eq!(ready_items(&owner), owner.items);
        owner.fields.push((KEY.to_owned(), "invalid".to_owned()));
        owner
            .fields
            .retain(|(key, value)| key != KEY || value == "invalid");
        assert_eq!(ready_items(&owner), owner.items);
        let found = [(owner.clone(), crate::work::Problem::Idle, String::new())];
        let (nudges, _) = crate::decide::nudges(
            &found,
            &Default::default(),
            &[owner.clone()],
            &Default::default(),
            0,
            |_| 1,
        )
        .ok_or("unexpected unavailable runtime")?;
        assert!(matches!(&nudges[..], [crate::decide::Nudge::Remind { .. }]));
        Ok(())
    }

    #[test]
    fn legacy_whole_task_pause_still_suppresses_reminders_and_reports() -> Result<(), String> {
        let owner = Task::parse(
            "legacy.md",
            "---\nstatus: blocked\nblocked_on: human review\nrunat: omnigent://legacy\npending_task_items:\n  - legacy work\n---\n",
        )?;
        assert!(!crate::work::expects_agent(&owner));
        for (state, problem) in [
            ("ready", crate::work::Problem::Idle),
            ("missing", crate::work::Problem::Missing),
            ("error", crate::work::Problem::Error),
            ("stuck", crate::work::Problem::Stuck),
        ] {
            assert_eq!(crate::work::problem_of(&owner, state), None);
            let (nudges, _) = crate::decide::nudges(
                &[(owner.clone(), problem, String::new())],
                &Default::default(),
                std::slice::from_ref(&owner),
                &Default::default(),
                0,
                |_| 1,
            )
            .ok_or("unexpected unavailable runtime")?;
            assert!(nudges.is_empty());
        }
        let mut owner = owner;
        crate::tasks::set_status(&mut owner, "long_running", Some("legacy wait"))?;
        assert_eq!(ready_items(&owner), ["legacy work"]);
        assert_eq!(
            crate::work::problem_of(&owner, "ready"),
            Some(crate::work::Problem::Idle)
        );
        Ok(())
    }

    #[test]
    fn human_resolution_and_creator_rejection_preserve_the_request() -> Result<(), String> {
        let mut human = task("human.md", true);
        let resolved = finish(
            &mut human,
            "deliver the requested change",
            "real verification",
        )?;
        assert_eq!(resolved.stage, Stage::Resolved);
        assert!(human.items.is_empty());
        let mut worker = task("worker.md", false);
        let text = worker.items[0].clone();
        register(
            &mut worker,
            &text,
            "creator.md",
            "original email: lines 3-8",
        )?;
        let request = finish(&mut worker, &text, "implementation delivered")?;
        assert_eq!(request.stage, Stage::Requested);
        assert_eq!(worker.items, vec![text.clone()]);
        assert!(decide(&mut worker, &request.id, "other.md", true, "").is_err());
        decide(
            &mut worker,
            &request.id,
            "creator.md",
            false,
            "missing requested behavior",
        )?;
        assert_eq!(read(&worker)?[0].stage, Stage::Added);
        finish(&mut worker, &request.id, "missing behavior delivered")?;
        decide(&mut worker, &request.id, "creator.md", true, "")?;
        assert!(worker.items.is_empty());
        assert_eq!(
            read(&Task::parse(&worker.name, &worker.render())?)?[0].stage,
            Stage::Resolved
        );
        Ok(())
    }

    #[test]
    fn external_waits_hold_only_their_item_survive_resolution_and_clear() -> Result<(), String> {
        let mut owner = task("owner.md", false);
        let mut dependency = task("dependency.md", true);
        let waiting = sync(&mut owner)?[0].id.clone();
        let blocker = sync(&mut dependency)?[0].id.clone();
        crate::tasks::add_item(&mut owner, "independent work", false)?;
        let end = chrono::Utc::now();
        wait_on(
            &mut owner,
            &waiting,
            Some(("human_mgr calling amh mail done", end)),
        )?;
        let item = owner.items[0].clone();
        assert_eq!(ready_items(&owner), ["independent work"]);
        let all = [owner.clone(), dependency.clone()];
        wait(&mut owner, &waiting, std::slice::from_ref(&blocker), &all)?;
        let blockers = &sync(&mut owner)?[0].blockers;
        assert!(blockers.contains(&blocker) && blockers.iter().any(|b| b.starts_with(EXTERNAL)));
        // A resolved request ID never releases an external wait.
        assert!(unblock(&mut owner, std::slice::from_ref(&blocker))?);
        assert!(!ready_items(&owner).contains(&item.as_str()));
        assert!(wait_on(&mut owner, &waiting, Some(("  ", end))).is_err());
        wait_on(&mut owner, &waiting, None)?;
        assert!(ready_items(&owner).contains(&item.as_str()));
        let owner = Task::parse(&owner.name, &owner.render())?;
        assert_eq!(ready_items(&owner).len(), 2);
        Ok(())
    }

    #[test]
    fn outside_waits_are_reminded_with_backoff() -> Result<(), String> {
        let mut owner = task("owner.md", false);
        let id = sync(&mut owner)?[0].id.clone();
        let now =
            chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).ok_or("clock")?;
        let hours = |h: f64| chrono::Duration::seconds((h * 3600.0) as i64);
        wait_on(&mut owner, &id, Some(("someone", now)))?;
        remind_waits(&mut owner, now, |_| None)?;
        assert!(read(&owner)?[0].added_at.is_some());
        assert!(notifications(&owner)?.is_empty());
        // Due after 1 h, then 1.5 h after that, then 2.25 h.
        for (n, after_h) in [(1, 1.0), (2, 2.5), (3, 4.75)] {
            assert!(remind_waits(&mut owner, now + hours(after_h), |_| None)?);
            assert_eq!(notifications(&owner)?.len(), n);
            assert!(!remind_waits(
                &mut owner,
                now + hours(after_h + 0.1),
                |_| None
            )?);
        }
        assert!(ready_items(&owner).is_empty());
        assert!(
            notifications(&owner)?[0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("waits on: someone")
        );
        // A wait made before reminders gets its first one `REMIND_FIRST_S` from now.
        let mut records = read(&owner)?;
        records[0].blockers = vec![format!("{EXTERNAL}human")];
        write(&mut owner, &records);
        assert!(remind_waits(&mut owner, now, |_| None)?);
        let blocker = read(&owner)?[0].blockers[0].clone();
        assert_eq!(
            wait_schedule(&blocker),
            (
                "human",
                Some((next_reminder(now, 0).with_timezone(&chrono::Utc), 0))
            )
        );
        Ok(())
    }

    #[test]
    fn dependencies_release_only_on_final_resolution_and_notify_once() -> Result<(), String> {
        let mut dependent = task("dependent.md", false);
        let mut blocker = task("blocker.md", false);
        let dependent_id = sync(&mut dependent)?[0].id.clone();
        let blocker_id = sync(&mut blocker)?[0].id.clone();
        let all = [dependent.clone(), blocker.clone()];
        wait(
            &mut dependent,
            &dependent_id,
            std::slice::from_ref(&blocker_id),
            &all,
        )?;
        let all = [dependent.clone(), blocker.clone()];
        assert!(
            wait(
                &mut blocker,
                &blocker_id,
                std::slice::from_ref(&dependent_id),
                &all
            )
            .is_err()
        );
        assert!(finish(&mut dependent, &dependent_id, "finished").is_err());
        finish(&mut blocker, &blocker_id, "delivered")?;
        assert!(!unblock(&mut dependent, &[])?);
        decide(&mut blocker, &blocker_id, "blocker.md", true, "")?;
        assert!(unblock(&mut dependent, std::slice::from_ref(&blocker_id))?);
        assert_eq!(notifications(&dependent)?.len(), 1);
        assert!(!unblock(&mut dependent, std::slice::from_ref(&blocker_id))?);
        assert_eq!(notifications(&dependent)?.len(), 1);
        Ok(())
    }

    #[test]
    fn transferring_keeps_id_creator_and_prevents_id_reuse() -> Result<(), String> {
        let mut source = task("source.md", false);
        let mut to = task("target.md", false);
        to.items.clear();
        let id = sync(&mut source)?[0].id.clone();
        transfer(&mut source, &mut to, &id)?;
        assert_eq!(read(&to)?[0].creator, "source.md");
        assert!(can_close(&source, &[source.clone(), to.clone()]).is_err());
        crate::tasks::add_item(&mut source, "another request", false)?;
        assert_ne!(sync(&mut source)?[0].id, id);
        assert_eq!(read(&to)?[0].id, id);
        Ok(())
    }
}
