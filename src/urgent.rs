//! Urgent messages to the human: the sending task sorts first in `amh task priorities` until the human has read that message.
// 🧑 "Emergency messages should. Carry high priority. In our priority system, so our clients list them at the top."

use std::collections::BTreeMap;

use serde_json::{Value, json};

type Settings = BTreeMap<String, String>;
use crate::store::Store;

/// SQLite record: `{TAG: {"message": ID, "at": EPOCH_S}}`, one entry per task with an unread urgent message.
const RECORD: &str = "amh-urgent.json";
/// A message whose reading cannot be checked (email transport) stops counting as urgent after this long.
const UNCHECKED_S: i64 = 24 * 3600;
/// No message keeps its task on top longer than this.
const MAX_S: i64 = 7 * 24 * 3600;

fn load(store: &Store) -> BTreeMap<String, Value> {
    store
        .record(RECORD)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|map| map.into_iter().collect())
        .unwrap_or_default()
}

fn save(store: &Store, marks: &BTreeMap<String, Value>) -> Result<(), String> {
    store.set_record(RECORD, &json!(marks).to_string())
}

/// Whether a `tell human` is urgent: `--urgent`, the PB urgent report stream, or a sign-in / CAPTCHA notice.
pub fn is_urgent(flag: bool, subject: &str, report_label: Option<&str>) -> bool {
    let subject = subject.trim().to_lowercase();
    flag || report_label.is_some_and(|label| label.trim() == "pb urgent")
        || subject.contains("pb urgent")
        || subject.starts_with("sign-in needed")
        || subject.contains("captcha")
}

/// Mark `tag`'s task urgent until the human reads message `message` (the Message-ID `tell human` printed).
pub fn mark(store: &Store, tag: &str, message: &str) -> Result<(), String> {
    let mut marks = load(store);
    marks.insert(
        tag.to_owned(),
        json!({"message": message, "at": chrono::Utc::now().timestamp()}),
    );
    save(store, &marks)
}

/// The tasks with an unread urgent message, oldest mark first.
pub fn tags(store: &Store) -> Vec<String> {
    let mut marks: Vec<(i64, String)> = load(store)
        .into_iter()
        .map(|(tag, mark)| (mark["at"].as_i64().unwrap_or_default(), tag))
        .collect();
    marks.sort();
    marks.into_iter().map(|(_, tag)| tag).collect()
}

/// Drop the marks whose message the human has read (Zulip read receipts), or that can no longer count; the watcher calls it each pass.
pub fn expire(settings: &Settings, store: &Store) -> Result<(), String> {
    let marks = load(store);
    if marks.is_empty() {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    let mut kept = BTreeMap::new();
    for (tag, mark) in &marks {
        let age = now - mark["at"].as_i64().unwrap_or_default();
        let id = mark["message"]
            .as_str()
            .and_then(|id| id.trim().trim_start_matches("zulip:").parse::<u64>().ok());
        let unread = match id {
            _ if age > MAX_S => false,
            Some(id) if crate::zulip::enabled(settings) => {
                // A failed check keeps the mark for the next pass.
                crate::zulip::unread_by_human(settings, id).unwrap_or(true)
            }
            _ => age <= UNCHECKED_S,
        };
        if unread {
            kept.insert(tag.clone(), mark.clone());
        }
    }
    if kept.len() != marks.len() {
        save(store, &kept)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tasks::{Task, by_priority};

    fn task(name: &str, priority: Option<i64>) -> Task {
        let field = priority.map_or(String::new(), |p| format!("priority: {p}\n"));
        Task::parse(
            name,
            &format!("---\nstatus: running\nrunat: omnigent://{name}\n{field}---\n"),
        )
        .unwrap()
    }

    #[test]
    fn urgent_tasks_come_first_below_every_priority_then_the_usual_order() {
        let tasks = vec![
            task("a.md", Some(1000)),
            task("b.md", Some(-5)),
            task("c.md", None),
            task("d.md", None),
        ];
        let ranked = by_priority(tasks.clone(), &["d".into(), "a".into()]);
        let tags: Vec<&str> = ranked.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(tags.len(), 4);
        assert_eq!(&tags[..2], [tasks[3].tag(), tasks[0].tag()]);
        assert_eq!(ranked[0].1, Some(-6));
        assert_eq!(ranked[2], (tasks[1].tag().to_owned(), Some(-5)));
        assert_eq!(by_priority(tasks, &[])[0].1, Some(-5));
    }

    #[test]
    fn urgent_paths() {
        assert!(super::is_urgent(true, "", None));
        assert!(super::is_urgent(false, "Re: pb urgent", None));
        assert!(super::is_urgent(false, "", Some("pb urgent")));
        assert!(super::is_urgent(false, "Sign-in needed: gmail", None));
        assert!(!super::is_urgent(false, "pb news", Some("pb news")));
    }
}
