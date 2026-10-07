//! Persist old events to dated Markdown before removing their database rows.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::database::Database;
use crate::{requests, store::Store};

// 🧑 "Yes, do rolling 30 day archival. Keep everything displayed in markdown tho, so agents can grep them"
pub fn archive_events(
    database: &mut Database,
    archive_dir: &Path,
    now: DateTime<Utc>,
) -> Result<usize, String> {
    let cutoff = now - Duration::days(30);
    let mut days: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for event in database.events()? {
        let Some(time) = event["time"].as_str() else {
            continue;
        };
        let Ok(time) = DateTime::parse_from_rfc3339(time) else {
            continue;
        };
        if time < cutoff {
            days.entry(time.with_timezone(&Utc).format("%Y-%m-%d").to_string())
                .or_default()
                .push(event);
        }
    }
    let mut ids = Vec::new();
    for (day, events) in days {
        persist_batch(archive_dir, &day, "events", &events)?;
        for event in events {
            ids.push(event_id(&event)?);
        }
    }
    database.prune_events(&ids)?;
    Ok(ids.len())
}

/// Archive old resolved requests only after their dated records are durably stored.
pub fn archive_requests(store: &Store, now: DateTime<Utc>) -> Result<usize, String> {
    let mut tasks = store.all()?;
    let mut referenced = BTreeSet::new();
    for task in &tasks {
        for request in requests::read(task)? {
            referenced.extend(request.blockers);
        }
    }
    let cutoff = now - Duration::days(30);
    let mut count = 0;
    for task in &mut tasks {
        let records = requests::read(task)?;
        let mut keep = Vec::new();
        let mut days: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for request in records {
            let time = request
                .resolved_at
                .as_deref()
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok());
            if request.stage != requests::Stage::Resolved
                || referenced.contains(&request.id)
                || time.is_none_or(|time| time >= cutoff)
            {
                keep.push(request);
                continue;
            }
            let Some(time) = time else {
                continue;
            };
            days.entry(time.with_timezone(&Utc).format("%Y-%m-%d").to_string()).or_default().push(serde_json::json!({
                "id": request.id, "owner": task.name, "text": request.text,
                "creator": request.creator, "source": request.source, "stage": request.stage.text(),
                "blockers": request.blockers, "evidence": request.evidence, "resolved_at": request.resolved_at
            }));
        }
        if days.is_empty() {
            continue;
        }
        for (day, records) in days {
            persist_batch(&store.root.join("archive"), &day, "requests", &records)?;
            count += records.len();
        }
        requests::write(task, &keep);
        store.save(task)?;
    }
    Ok(count)
}

fn persist_batch(
    archive_dir: &Path,
    day: &str,
    kind: &str,
    incoming: &[Value],
) -> Result<(), String> {
    let directory = archive_dir.join(day);
    fs::create_dir_all(&directory).map_err(error)?;
    File::open(archive_dir)
        .and_then(|file| file.sync_all())
        .map_err(error)?;
    if let Some(parent) = archive_dir.parent() {
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(error)?;
    }
    let path = directory.join(format!("{kind}.md"));
    let key = |record: &Value| -> Result<String, String> {
        match &record["id"] {
            Value::String(id) => Ok(id.clone()),
            Value::Number(id) => id
                .as_i64()
                .map(|id| format!("{id:020}"))
                .ok_or_else(|| "amh: invalid archive ID".to_owned()),
            _ => Err("amh: archive record lacks ID".to_owned()),
        }
    };
    let mut records = BTreeMap::new();
    match fs::read_to_string(&path) {
        Ok(text) => {
            for line in text.lines().filter(|line| line.starts_with('{')) {
                let record: Value = serde_json::from_str(line)
                    .map_err(|e| format!("amh: invalid archived record: {e}"))?;
                records.insert(key(&record)?, record);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(error(e)),
    }
    for record in incoming {
        let id = key(record)?;
        if records.get(&id).is_some_and(|old| old != record) {
            return Err(format!(
                "amh: archived ID {id} conflicts with current record"
            ));
        }
        records.insert(id, record.clone());
    }
    let mut text =
        format!("# Archived AMH {kind}\n\n(authored by agents unless marked 🧑)\n\n```json\n");
    for record in records.values() {
        text.push_str(&record.to_string());
        text.push('\n');
    }
    text.push_str("```\n");
    let temporary = directory.join(format!(".{kind}.md.amh-tmp"));
    let mut file = File::create(&temporary).map_err(error)?;
    file.write_all(text.as_bytes()).map_err(error)?;
    file.sync_all().map_err(error)?;
    fs::rename(&temporary, &path).map_err(error)?;
    File::open(&directory)
        .and_then(|file| file.sync_all())
        .map_err(error)
}

fn event_id(event: &Value) -> Result<i64, String> {
    event["id"]
        .as_i64()
        .ok_or_else(|| "amh: archived event has no integer ID".to_owned())
}

// 🧑 "Keep the md files in a git repo like we do now with work_logs, and automatically periodically commit all changes."
/// Commit Markdown only, retaining staged changes to other files. The caller schedules this helper.
pub fn commit_markdown(root: &Path) -> Result<bool, String> {
    let output = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.md",
        ],
        None,
    )?;
    if !output.status.success() {
        return Err(command_error(&output));
    }
    let paths = output.stdout;
    if paths.is_empty() {
        return Ok(false);
    }
    let untracked = git(
        root,
        &[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--",
            "*.md",
        ],
        None,
    )?;
    if !untracked.status.success() {
        return Err(command_error(&untracked));
    }
    let diff = git(root, &["diff", "--quiet", "HEAD", "--", "*.md"], None)?;
    match diff.status.code() {
        Some(0) if untracked.stdout.is_empty() => return Ok(false),
        Some(0 | 1) => {}
        _ => return Err(command_error(&diff)),
    }
    if !untracked.stdout.is_empty() {
        let added = git(
            root,
            &[
                "add",
                "--intent-to-add",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            Some(&untracked.stdout),
        )?;
        if !added.status.success() {
            return Err(command_error(&added));
        }
    }
    let committed = git(
        root,
        &[
            "commit",
            "--only",
            "-m",
            "docs: record work logs",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
        Some(&paths),
    )?;
    if !committed.status.success() {
        return Err(command_error(&committed));
    }
    Ok(true)
}

fn git(root: &Path, args: &[&str], input: Option<&[u8]>) -> Result<std::process::Output, String> {
    let mut child = Command::new("timeout")
        .args(["30s", "git", "-C"])
        .arg(root)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(error)?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .ok_or("amh: git stdin unavailable")?
            .write_all(input)
            .map_err(error)?;
    }
    child.wait_with_output().map_err(error)
}

fn command_error(output: &std::process::Output) -> String {
    format!(
        "amh: work-log commit failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn error(error: std::io::Error) -> String {
    format!("amh: archival: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_keep_dependencies_and_legacy_resolution() -> Result<(), String> {
        let root =
            std::env::temp_dir().join(format!("amh-archive-requests-{}", std::process::id()));
        fs::create_dir_all(&root).map_err(error)?;
        let store = Store {
            root: root.clone(),
            state_dir: root.join("state"),
        };
        let _lock = store.lock()?;
        let now = Utc::now();
        let mut task = crate::tasks::Task {
            name: "a.md".to_owned(),
            fields: vec![
                ("status".to_owned(), "running".to_owned()),
                ("runat".to_owned(), "omnigent:test".to_owned()),
                ("request_next".to_owned(), "5".to_owned()),
            ],
            items: vec!["a:4".to_owned()],
            body: "history\n".to_owned(),
        };
        let record = |id: &str, stage, blockers: Vec<String>, resolved_at| requests::Request {
            id: id.to_owned(),
            text: id.to_owned(),
            creator: "human".to_owned(),
            source: "test".to_owned(),
            stage,
            blockers,
            evidence: String::new(),
            resolved_at,
        };
        let old = Some((now - Duration::days(31)).to_rfc3339());
        requests::write(
            &mut task,
            &[
                record("a:1", requests::Stage::Resolved, Vec::new(), old.clone()),
                record("a:2", requests::Stage::Resolved, Vec::new(), old),
                record("a:3", requests::Stage::Resolved, Vec::new(), None),
                record("a:4", requests::Stage::Added, vec!["a:2".to_owned()], None),
            ],
        );
        store.save(&task)?;
        assert_eq!(archive_requests(&store, now)?, 1);
        let stored = store.load("a.md")?;
        assert_eq!(
            requests::read(&stored)?
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a:2", "a:3", "a:4"]
        );
        assert_eq!(stored.body, "history\n");
        assert_eq!(stored.field("request_next"), Some("5"));
        assert_eq!(archive_requests(&store, now)?, 0);
        fs::remove_dir_all(root).map_err(error)
    }

    #[test]
    fn archive_preserves_recent_events_and_retries() -> Result<(), String> {
        let root = std::env::temp_dir().join(format!("amh-archive-{}", std::process::id()));
        fs::create_dir_all(&root).map_err(error)?;
        let mut db = Database::open(&root.join("state.db"))?;
        let now = Utc::now();
        let event = |time: DateTime<Utc>| serde_json::json!({"time": time.to_rfc3339(), "actor": "human", "kind": "resolved", "subject": "r1", "data": {"text": "complete"}});
        db.commit(&[], &[event(now - Duration::days(31)), event(now)])?;
        assert_eq!(archive_events(&mut db, &root.join("archive"), now)?, 1);
        assert_eq!(db.events()?.len(), 1);
        assert_eq!(archive_events(&mut db, &root.join("archive"), now)?, 0);
        let path = root
            .join("archive")
            .join((now - Duration::days(31)).format("%Y-%m-%d").to_string())
            .join("events.md");
        assert!(
            fs::read_to_string(path)
                .map_err(error)?
                .contains("complete")
        );
        fs::remove_dir_all(root).map_err(error)
    }

    #[test]
    fn git_commit_preserves_unrelated_staging() -> Result<(), String> {
        let root = std::env::temp_dir().join(format!("amh-archive-git-{}", std::process::id()));
        fs::create_dir_all(&root).map_err(error)?;
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Archive Test"],
            vec!["config", "user.email", "archive@example.invalid"],
        ] {
            let output = git(&root, &args, None)?;
            assert!(output.status.success(), "{}", command_error(&output));
        }
        fs::write(root.join("notes.md"), "old\n").map_err(error)?;
        fs::write(root.join("code.txt"), "old\n").map_err(error)?;
        for args in [vec!["add", "."], vec!["commit", "-qm", "initial"]] {
            let output = git(&root, &args, None)?;
            assert!(output.status.success(), "{}", command_error(&output));
        }
        fs::write(root.join("notes.md"), "new\n").map_err(error)?;
        fs::write(root.join("new.md"), "new file\n").map_err(error)?;
        fs::write(root.join("code.txt"), "staged\n").map_err(error)?;
        assert!(git(&root, &["add", "code.txt"], None)?.status.success());
        assert!(commit_markdown(&root)?);
        let staged = git(&root, &["diff", "--cached", "--name-only"], None)?;
        assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "code.txt");
        assert!(!commit_markdown(&root)?);
        fs::remove_dir_all(root).map_err(error)
    }
}
