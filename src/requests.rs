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
            })
        })
        .collect()
}

pub fn write(task: &mut Task, requests: &[Request]) {
    let records: Vec<Value> = requests.iter().map(|r| json!({"id": r.id, "text": r.text, "creator": r.creator, "source": r.source, "stage": r.stage.text(), "blockers": r.blockers, "evidence": r.evidence, "resolved_at":r.resolved_at})).collect();
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
    request.stage = if request.creator == "human" {
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
