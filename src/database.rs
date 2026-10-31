//! SQLite task snapshots and domain events share one transaction.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

// 🧑 "I would like state consistency and state stored in SQLite"
pub struct Database(Connection);

impl Database {
    pub fn received_mail(&self, key: &str) -> Result<bool, String> {
        self.0
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = ?1)",
                [format!("mail:{key}")],
                |row| row.get(0),
            )
            .map_err(error)
    }
    pub fn open(path: &Path) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(error)?;
        connection
            .busy_timeout(Duration::from_secs(30))
            .map_err(error)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tasks (name TEXT PRIMARY KEY, text TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, time TEXT NOT NULL, actor TEXT NOT NULL,
                 kind TEXT NOT NULL, subject TEXT NOT NULL, data TEXT NOT NULL CHECK(json_valid(data))
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY, channel TEXT NOT NULL, target TEXT NOT NULL,
                 text TEXT NOT NULL, source TEXT NOT NULL,
                 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending', 'sent')),
                 attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS records (name TEXT PRIMARY KEY, text TEXT NOT NULL);"
        ).map_err(error)?;
        Ok(Self(connection))
    }

    pub fn initialized(&self) -> Result<bool, String> {
        self.0
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = 'markdown_imported')",
                [],
                |row| row.get(0),
            )
            .map_err(error)
    }

    /// Import markdown only before the first completed migration, including an empty root.
    pub fn import(
        &mut self,
        snapshots: &[(String, String)],
        events: &[Value],
    ) -> Result<(), String> {
        let transaction = self.0.transaction().map_err(error)?;
        let imported: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = 'markdown_imported')",
                [],
                |row| row.get(0),
            )
            .map_err(error)?;
        if !imported {
            for (name, text) in snapshots {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO tasks (name, text) VALUES (?1, ?2)",
                        params![name, text],
                    )
                    .map_err(error)?;
                if name != "TODO.md" {
                    let task = crate::tasks::Task::parse(name, text)?;
                    for notification in crate::requests::notifications(&task)? {
                        enqueue(&transaction, name, &notification)?;
                    }
                }
            }
            for event in events {
                insert_event(&transaction, event, false)?;
            }
            transaction
                .execute(
                    "INSERT INTO metadata VALUES ('markdown_imported', 'true')",
                    [],
                )
                .map_err(error)?;
        }
        transaction.commit().map_err(error)
    }

    pub fn task(&self, name: &str) -> Result<Option<String>, String> {
        self.0
            .query_row("SELECT text FROM tasks WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()
            .map_err(error)
    }

    pub fn tasks(&self) -> Result<Vec<(String, String)>, String> {
        let mut query = self
            .0
            .prepare("SELECT name, text FROM tasks ORDER BY name")
            .map_err(error)?;
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)
    }

    /// Commit all affected owners and their events together, including ownership transfers.
    pub fn commit(
        &mut self,
        snapshots: &[(String, String)],
        events: &[Value],
    ) -> Result<(), String> {
        let transaction = self.0.transaction().map_err(error)?;
        commit_records(&transaction, snapshots, events)?;
        transaction.commit().map_err(error)?;
        self.wake(events);
        Ok(())
    }

    /// Commit a mail receipt with its task updates and outbox exactly once per stable key.
    pub fn receive_mail(
        &mut self,
        key: &str,
        snapshots: &[(String, String)],
        events: &[Value],
    ) -> Result<bool, String> {
        let transaction = self.0.transaction().map_err(error)?;
        let inserted = transaction
            .execute(
                "INSERT OR IGNORE INTO metadata (key, value) VALUES (?1, 'received')",
                [format!("mail:{key}")],
            )
            .map_err(error)?;
        if inserted == 0 {
            return Ok(false);
        }
        commit_records(&transaction, snapshots, events)?;
        transaction.commit().map_err(error)?;
        self.wake(events);
        Ok(true)
    }

    /// Remove compatibility notifications and record successful delivery in the same transaction.
    pub fn complete_message(
        &mut self,
        id: i64,
        snapshots: &[(String, String)],
        events: &[Value],
    ) -> Result<(), String> {
        let transaction = self.0.transaction().map_err(error)?;
        let changed = transaction.execute(
            "UPDATE messages SET state = 'sent', attempts = attempts + 1, last_error = '' WHERE id = ?1 AND state = 'pending'", [id]
        ).map_err(error)?;
        if changed == 0 {
            return Err(format!("amh: no pending message {id}"));
        }
        commit_records(&transaction, snapshots, events)?;
        transaction.commit().map_err(error)?;
        self.wake(events);
        Ok(())
    }

    /// Wake the watcher only after relevant changes have committed.
    fn wake(&self, events: &[Value]) {
        if !events.is_empty()
            && let Some(path) = self.0.path()
        {
            crate::wake::notify(Path::new(path));
        }
    }

    /// The text of a small named record of the watcher; `None` when it was never written.
    pub fn record(&self, name: &str) -> Result<Option<String>, String> {
        self.0
            .query_row("SELECT text FROM records WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()
            .map_err(error)
    }

    pub fn set_record(&self, name: &str, text: &str) -> Result<(), String> {
        self.0.execute("INSERT INTO records (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = excluded.text", params![name, text]).map_err(error)?;
        Ok(())
    }

    /// Import a legacy record without replacing a concurrent import or update.
    pub fn import_record(&self, name: &str, text: &str) -> Result<(), String> {
        self.0
            .execute(
                "INSERT OR IGNORE INTO records (name, text) VALUES (?1, ?2)",
                params![name, text],
            )
            .map_err(error)?;
        Ok(())
    }

    /// Append to a record atomically so concurrent senders retain every line.
    pub fn append_record(&self, name: &str, text: &str) -> Result<(), String> {
        self.0.execute("INSERT INTO records (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = records.text || excluded.text", params![name, text]).map_err(error)?;
        Ok(())
    }

    /// Merge remembered message IDs atomically, retaining the newest `limit` IDs.
    pub fn remember_ids(
        &mut self,
        name: &str,
        ids: &[u64],
        limit: usize,
    ) -> Result<Vec<u64>, String> {
        let tx = self
            .0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(error)?;
        let known: String = tx
            .query_row("SELECT text FROM records WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()
            .map_err(error)?
            .unwrap_or_default();
        let mut known: std::collections::BTreeSet<u64> =
            known.lines().filter_map(|id| id.parse().ok()).collect();
        known.extend(ids);
        let ids: Vec<u64> = known.into_iter().rev().take(limit).collect();
        let text = ids
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        tx.execute("INSERT INTO records (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = excluded.text", params![name, text]).map_err(error)?;
        tx.commit().map_err(error)?;
        Ok(ids)
    }

    pub fn latest_event_id(&self) -> Result<i64, String> {
        self.0
            .query_row(
                "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0)",
                [],
                |row| row.get(0),
            )
            .map_err(error)
    }

    pub fn cursor(&self, key: &str) -> Result<i64, String> {
        let value: Option<String> = self
            .0
            .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(error)?;
        match value {
            Some(value) => value
                .parse()
                .map_err(|e| format!("amh: invalid event cursor {key}: {e}")),
            None => Ok(0),
        }
    }

    pub fn set_cursor(&self, key: &str, id: i64) -> Result<(), String> {
        self.0.execute("INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, id.to_string()]).map_err(error)?;
        Ok(())
    }

    pub fn prune_events(&mut self, ids: &[i64]) -> Result<(), String> {
        let transaction = self.0.transaction().map_err(error)?;
        for id in ids {
            transaction
                .execute("DELETE FROM events WHERE id = ?1", [id])
                .map_err(error)?;
        }
        transaction.commit().map_err(error)
    }

    /// Read pending delivery records of one channel in enqueue order; failed deliveries stay pending.
    ///
    /// Channel `agent`: `text` is a notification. Channel `human`: `text` names a stored message. Channel `block`: `text` contains a typed or guest-mail block. `target` names the owning task.
    pub fn pending_messages(&self, channel: &str) -> Result<Vec<Value>, String> {
        let mut query = self.0.prepare(
            "SELECT id, channel, target, text, source, state, attempts, last_error FROM messages WHERE state = 'pending' AND channel = ?1 ORDER BY id"
        ).map_err(error)?;
        query
            .query_map([channel], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, i64>(0)?, "channel": row.get::<_, String>(1)?,
                    "target": row.get::<_, String>(2)?, "text": row.get::<_, String>(3)?,
                    "source": row.get::<_, String>(4)?, "state": row.get::<_, String>(5)?,
                    "attempts": row.get::<_, i64>(6)?, "last_error": row.get::<_, String>(7)?
                }))
            })
            .map_err(error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)
    }

    /// Record a delivery result. An interrupted external send may retry the same stable ID.
    pub fn finish_message(&self, id: i64, success: bool, failure: &str) -> Result<(), String> {
        let changed = self.0.execute(
            "UPDATE messages SET state = ?1, attempts = attempts + 1, last_error = ?2 WHERE id = ?3 AND state = 'pending'",
            params![if success { "sent" } else { "pending" }, if success { "" } else { failure }, id]
        ).map_err(error)?;
        if changed == 0 {
            return Err(format!("amh: no pending message {id}"));
        }
        Ok(())
    }

    pub fn events(&self) -> Result<Vec<Value>, String> {
        let mut query = self
            .0
            .prepare("SELECT id, time, actor, kind, subject, data FROM events ORDER BY id")
            .map_err(error)?;
        let records = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(error)?;
        records.map(|record| {
            let (id, time, actor, kind, subject, data) = record.map_err(error)?;
            let data: Value = serde_json::from_str(&data).map_err(|e| format!("amh: invalid stored event: {e}"))?;
            Ok(serde_json::json!({"id": id, "time": time, "actor": actor, "kind": kind, "subject": subject, "data": data}))
        }).collect()
    }
}

fn commit_records(
    transaction: &rusqlite::Transaction<'_>,
    snapshots: &[(String, String)],
    events: &[Value],
) -> Result<(), String> {
    for (name, text) in snapshots {
        transaction.execute(
            "INSERT INTO tasks (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = excluded.text",
            params![name, text]
        ).map_err(error)?;
    }
    for event in events {
        insert_event(transaction, event, true)?;
    }
    Ok(())
}

fn insert_event(
    transaction: &rusqlite::Transaction<'_>,
    event: &Value,
    queue: bool,
) -> Result<(), String> {
    let field = |name: &str| {
        event
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("amh: event has no string {name}"))
    };
    transaction
        .execute(
            "INSERT INTO events (time, actor, kind, subject, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                field("time")?,
                field("actor")?,
                field("kind")?,
                field("subject")?,
                event
                    .get("data")
                    .map_or_else(|| "null".to_owned(), Value::to_string)
            ],
        )
        .map_err(error)?;
    if queue && field("kind")? == "message queued" {
        enqueue(transaction, field("subject")?, &event["data"])?;
    }
    Ok(())
}

fn enqueue(
    transaction: &rusqlite::Transaction<'_>,
    source: &str,
    data: &Value,
) -> Result<(), String> {
    let target = data
        .get("to")
        .and_then(Value::as_str)
        .ok_or_else(|| "amh: queued message has no string to".to_owned())?;
    let text = data
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "amh: queued message has no string text".to_owned())?;
    let channel = data
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or("agent");
    transaction
        .execute(
            "INSERT INTO messages (channel, target, text, source) VALUES (?1, ?2, ?3, ?4)",
            params![channel, target, text, source],
        )
        .map_err(error)?;
    Ok(())
}

fn error(error: rusqlite::Error) -> String {
    format!("amh: SQLite: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_receipt_rollback_retry_and_reopen() -> Result<(), Box<dyn std::error::Error>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let folder =
            std::env::temp_dir().join(format!("amh-mail-receipt-{}-{now}", std::process::id()));
        std::fs::create_dir(&folder)?;
        let path = folder.join("state.sqlite3");
        let mut db = Database::open(&path)?;
        let snapshots = [("task.md".into(), "received notification".into())];
        let event = serde_json::json!({"time": "now", "actor": "router", "kind": "message queued", "subject": "task.md", "data": {"to": "worker", "text": "GitHub notification"}});
        assert!(
            db.receive_mail(
                "inbox:message-id",
                &snapshots,
                &[event.clone(), serde_json::json!({})]
            )
            .is_err()
        );
        assert_eq!(db.task("task.md")?, None);
        assert!(db.events()?.is_empty());
        assert!(db.pending_messages("agent")?.is_empty());
        assert!(db.receive_mail("inbox:message-id", &snapshots, std::slice::from_ref(&event))?);
        drop(db);
        let mut db = Database::open(&path)?;
        let baseline = db.events()?;
        assert!(!db.receive_mail(
            "inbox:message-id",
            &[("task.md".into(), "stale retry".into())],
            &[serde_json::json!({})]
        )?);
        assert_eq!(db.task("task.md")?, Some("received notification".into()));
        assert_eq!(db.events()?, baseline);
        assert_eq!(db.pending_messages("agent")?.len(), 1);
        assert!(db.receive_mail("other-inbox:message-id", &snapshots, &[event])?);
        assert_eq!(db.pending_messages("agent")?.len(), 2);
        drop(db);
        std::fs::remove_dir_all(folder)?;
        Ok(())
    }

    #[test]
    fn import_commit_rollback_and_reopen() -> Result<(), Box<dyn std::error::Error>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let folder = std::env::temp_dir().join(format!("amh-sqlite-{}-{now}", std::process::id()));
        std::fs::create_dir(&folder)?;
        let path = folder.join("state.sqlite3");
        let mut db = Database::open(&path)?;
        let before = "---\nstatus: running\nrunat: omnigent://a\n---\nbefore";
        db.import(&[("a.md".into(), before.into())], &[])?;
        db.import(&[("a.md".into(), "stale export".into())], &[])?;
        assert_eq!(db.task("a.md")?, Some(before.into()));
        let event = serde_json::json!({"time": "now", "actor": "agent", "kind": "transferred", "subject": "request", "data": {"to": "b.md"}});
        let snapshots = [
            ("a.md".into(), "after".into()),
            ("b.md".into(), "owned".into()),
        ];
        assert!(
            db.commit(&snapshots, &[event.clone(), serde_json::json!({})])
                .is_err()
        );
        assert_eq!(db.task("a.md")?, Some(before.into()));
        assert_eq!(db.task("b.md")?, None);
        assert!(db.events()?.is_empty());
        db.commit(&snapshots, &[event])?;
        drop(db);
        let db = Database::open(&path)?;
        assert_eq!(db.tasks()?, snapshots);
        assert_eq!(db.events()?.len(), 1);
        drop(db);
        std::fs::remove_dir_all(folder)?;
        Ok(())
    }

    #[test]
    fn migration_preserves_history_without_resending_delivered_messages()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut db = Database::open(Path::new(":memory:"))?;
        let snapshots = [
            (
                "a.md".into(),
                "---\nstatus: running\nrunat: omnigent://a\nrequest_notifications: [{\"to\":\"b.md\",\"text\":\"pending\"}]\n---\n"
                    .into(),
            ),
            ("TODO.md".into(), "task list".into()),
        ];
        let event = serde_json::json!({"time": "now", "actor": "agent", "kind": "message queued", "subject": "a.md", "data": {"to": "b.md", "text": "already delivered"}});
        db.import(&snapshots, std::slice::from_ref(&event))?;
        assert_eq!(db.events()?.len(), 1);
        assert_eq!(db.pending_messages("agent")?.len(), 1);
        assert_eq!(db.pending_messages("agent")?[0]["text"], "pending");
        db.import(&snapshots, std::slice::from_ref(&event))?;
        assert_eq!(db.pending_messages("agent")?.len(), 1);
        let first = db.latest_event_id()?;
        db.set_cursor("watch", first)?;
        db.prune_events(&[first])?;
        assert!(db.events()?.is_empty());
        assert_eq!(db.latest_event_id()?, first);
        assert_eq!(db.cursor("watch")?, first);
        db.commit(&[], &[event])?;
        assert!(db.latest_event_id()? > first);
        Ok(())
    }

    #[test]
    fn queue_is_atomic_and_retries_survive_reopen() -> Result<(), Box<dyn std::error::Error>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let folder = std::env::temp_dir().join(format!("amh-queue-{}-{now}", std::process::id()));
        std::fs::create_dir(&folder)?;
        let path = folder.join("state.sqlite3");
        let mut db = Database::open(&path)?;
        let event = serde_json::json!({"time": "now", "actor": "agent", "kind": "message queued", "subject": "a.md", "data": {"to": "b.md", "text": "ready"}});
        let snapshots = [("a.md".into(), "queued".into())];
        assert!(
            db.commit(&snapshots, &[event.clone(), serde_json::json!({})])
                .is_err()
        );
        assert!(db.pending_messages("agent")?.is_empty());
        assert_eq!(db.task("a.md")?, None);
        db.commit(&snapshots, &[event])?;
        let id = db.pending_messages("agent")?[0]["id"]
            .as_i64()
            .ok_or("missing message id")?;
        db.finish_message(id, false, "offline")?;
        drop(db);
        let db = Database::open(&path)?;
        let pending = db.pending_messages("agent")?;
        assert_eq!(pending[0]["id"], id);
        assert_eq!(pending[0]["attempts"], 1);
        assert_eq!(pending[0]["last_error"], "offline");
        let mut db = db;
        let snapshots = [("a.md".into(), "delivered".into())];
        assert!(
            db.complete_message(id, &snapshots, &[serde_json::json!({})])
                .is_err()
        );
        assert_eq!(db.task("a.md")?, Some("queued".into()));
        assert_eq!(db.pending_messages("agent")?[0]["attempts"], 1);
        let delivered = serde_json::json!({"time": "now", "actor": "agent", "kind": "message delivered", "subject": "a.md", "data": {"id": id}});
        db.complete_message(id, &snapshots, &[delivered])?;
        assert_eq!(db.task("a.md")?, Some("delivered".into()));
        assert!(db.pending_messages("agent")?.is_empty());
        assert!(db.finish_message(id, true, "").is_err());
        drop(db);
        std::fs::remove_dir_all(folder)?;
        Ok(())
    }
}
