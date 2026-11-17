//! SQLite task snapshots and domain events share one transaction.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

// 🧑 "I would like state consistency and state stored in SQLite"
pub struct Database(std::mem::ManuallyDrop<Connection>, std::path::PathBuf, u64);

/// Open connections kept for reuse: opening one (connect, map the file, probe the schema) costs ~1–2 ms, several times per command, much of it inside the store lock.
// 🧑 "Locking should be in us not ms"
static POOL: std::sync::Mutex<Vec<(std::path::PathBuf, u64, Connection)>> =
    std::sync::Mutex::new(Vec::new());
/// Most idle connections kept.
const POOL_MAX: usize = 8;

impl Drop for Database {
    fn drop(&mut self) {
        // SAFETY: the connection is taken exactly once, here, and `self` is not used after.
        let connection = unsafe { std::mem::ManuallyDrop::take(&mut self.0) };
        // A connection left inside a transaction is not reused.
        if connection.is_autocommit()
            && let Ok(mut pool) = POOL.lock()
            && pool.len() < POOL_MAX
        {
            pool.push((std::mem::take(&mut self.1), self.2, connection));
        }
    }
}

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
    /// The task a received mail was committed to, or `None` for receipts from before owners were recorded.
    pub fn mail_owner(&self, key: &str) -> Result<Option<String>, String> {
        let value: Option<String> = self
            .0
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [format!("mail:{key}")],
                |row| row.get(0),
            )
            .optional()
            .map_err(error)?;
        Ok(value.filter(|value| value != "received"))
    }
    /// The connection, for the daemon's own reads of its tables.
    pub fn sql(&self) -> &Connection {
        &self.0
    }

    pub fn open(path: &Path) -> Result<Self, String> {
        let inode =
            std::fs::metadata(path).map_or(0, |meta| std::os::unix::fs::MetadataExt::ino(&meta));
        let pooled = POOL.lock().ok().and_then(|mut pool| {
            // A connection to a file since deleted or replaced is dropped, not kept.
            pool.retain(|(p, i, _)| p != path || *i == inode);
            let at = pool
                .iter()
                .position(|(p, i, _)| p == path && *i == inode && inode != 0)?;
            Some(pool.swap_remove(at).2)
        });
        if let Some(connection) = pooled {
            return Ok(Self(
                std::mem::ManuallyDrop::new(connection),
                path.to_owned(),
                inode,
            ));
        }
        let connection = Connection::open(path).map_err(error)?;
        // A reader that closes last would otherwise copy the whole WAL back on exit (880 writes in one `agent list`); writers' automatic checkpoints keep the WAL bounded.
        connection
            .set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )
            .map_err(error)?;
        connection
            .busy_timeout(Duration::from_secs(30))
            .map_err(error)?;
        // The schema checks below run once per process and database; later opens only connect (they cost ~1 ms each, several per command).
        // Keyed by inode too: a database file deleted and made anew is checked again.
        static READY: std::sync::Mutex<Vec<(std::path::PathBuf, u64)>> =
            std::sync::Mutex::new(Vec::new());
        let inode_of = |path: &Path| {
            std::fs::metadata(path).map_or(0, |meta| std::os::unix::fs::MetadataExt::ino(&meta))
        };
        let key = (path.to_owned(), inode_of(path));
        let ready = READY
            .lock()
            .map(|known| known.contains(&key))
            .unwrap_or(false);
        // A file made anew may reuse the inode; one cheap probe confirms the schema is there.
        let has_schema = || {
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'events')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false)
        };
        if ready && has_schema() {
            connection
                .execute_batch("PRAGMA synchronous=NORMAL; PRAGMA mmap_size=1073741824;")
                .map_err(error)?;
            return Ok(Self(
                std::mem::ManuallyDrop::new(connection),
                path.to_owned(),
                inode_of(path),
            ));
        }
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL; -- WAL: no corruption on a crash; the last commits may be lost on power loss, not on a process crash
             PRAGMA mmap_size=1073741824;
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
             CREATE TABLE IF NOT EXISTS records (name TEXT PRIMARY KEY, text TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS events_by_subject ON events(subject, id);"
        ).map_err(error)?;
        // When a failed message may be tried again (Unix seconds); added after the table existed.
        let has_retry: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('messages') WHERE name = 'retry_at')",
                [],
                |row| row.get(0),
            )
            .map_err(error)?;
        // A task's status, computed from its text and indexed, so active tasks are found without reading every task.
        // 🧑 "Still unacceptably slow. Should be some ms. Why so slow"
        let has_status: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('tasks') WHERE name = 'status')",
                [],
                |row| row.get(0),
            )
            .map_err(error)?;
        if !has_status {
            connection
                .execute_batch(
                    "ALTER TABLE tasks ADD COLUMN status TEXT GENERATED ALWAYS AS (
                         substr(text, instr(text, char(10) || 'status: ') + 9,
                                instr(substr(text, instr(text, char(10) || 'status: ') + 9), char(10)) - 1)
                     ) VIRTUAL;
                     CREATE INDEX IF NOT EXISTS tasks_by_status ON tasks(status);",
                )
                .map_err(error)?;
        }
        if !has_retry {
            connection
                .execute_batch(
                    "ALTER TABLE messages ADD COLUMN retry_at INTEGER NOT NULL DEFAULT 0",
                )
                .map_err(error)?;
        }
        if let Ok(mut known) = READY.lock() {
            known.push((path.to_owned(), inode_of(path)));
        }
        Ok(Self(
            std::mem::ManuallyDrop::new(connection),
            path.to_owned(),
            inode_of(path),
        ))
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

    /// Time of the task's latest status change to `done`, if the event is still in the database.
    pub fn done_time(&self, name: &str) -> Result<Option<String>, String> {
        self.0
            .query_row(
                "SELECT time FROM events WHERE kind = 'agent record changed' AND subject = ?1 AND json_extract(data, '$.status') = 'done' ORDER BY id DESC LIMIT 1",
                [name],
                |row| row.get(0),
            )
            .optional()
            .map_err(error)
    }

    /// The active tasks whose text contains any of `words`.
    pub fn active_tasks_mentioning(
        &self,
        words: &[String],
    ) -> Result<Vec<(String, String)>, String> {
        let mut found = std::collections::BTreeMap::new();
        let mut query = self
            .0
            .prepare("SELECT name, text FROM tasks WHERE status IN ('running', 'long_running', 'blocked') AND instr(text, ?1) > 0")
            .map_err(error)?;
        for word in words {
            let rows = query
                .query_map([word], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(error)?;
            for row in rows {
                let (name, text) = row.map_err(error)?;
                found.insert(name, text);
            }
        }
        Ok(found.into_iter().collect())
    }

    /// The tasks whose status is running, long_running or blocked, through the status index.
    pub fn active_tasks(&self) -> Result<Vec<(String, String)>, String> {
        let mut query = self
            .0
            .prepare("SELECT name, text FROM tasks WHERE status IN ('running', 'long_running', 'blocked') ORDER BY name")
            .map_err(error)?;
        let rows = query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(error)?;
        rows.collect::<Result<_, _>>().map_err(error)
    }

    /// Tasks with an open daemon `edit_import` row; none before the daemon's migration made the table.
    pub fn open_imports(&self) -> Result<std::collections::BTreeSet<String>, String> {
        let migrated: bool = self
            .0
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'edit_import')",
                [],
                |row| row.get(0),
            )
            .map_err(error)?;
        if !migrated {
            return Ok(Default::default());
        }
        let mut query = self
            .0
            .prepare("SELECT DISTINCT task FROM edit_import WHERE state = 'open'")
            .map_err(error)?;
        query
            .query_map([], |row| row.get(0))
            .map_err(error)?
            .collect::<Result<_, _>>()
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

    /// Every task's name, filed month (empty at the root), and, when `compare`, whether its text equals what the last export recorded.
    ///
    /// Compares inside SQLite so a lock need not copy every task text and export record into Rust; skipping the comparison skips reading the texts.
    /// Each task's name, month folder and whether its export is current; with `hot_after`, only the task list, open tasks and tasks that are the subject of an event after that ID (every snapshot commit records one).
    // The rework design's stage 0 gate "lock hold p99 < 300 ms": every lock compared and stat-ed all 7,596 tasks (~80 ms import + ~80 ms export); a closed task's file changes only through amh, so routine passes skip it.
    pub fn export_states(
        &self,
        compare: bool,
        hot_after: Option<i64>,
    ) -> Result<Vec<(String, String, bool)>, String> {
        let mut query = self
            .0
            .prepare(
                // The hot set is built from indexes first; an OR in the outer WHERE made SQLite compute every task's status (~100 ms).
                "WITH hot(name) AS (
                     SELECT name FROM tasks WHERE status IN ('running', 'long_running', 'blocked')
                     UNION SELECT 'TODO.md'
                     UNION SELECT subject FROM events WHERE id > ?2)
                 SELECT t.name, COALESCE(m.text, ''), ?1 AND e.text IS t.text FROM tasks t
                 LEFT JOIN records e ON e.name = 'export:' || t.name
                 LEFT JOIN records m ON m.name = 'month:' || t.name
                 WHERE ?2 IS NULL OR t.name IN (SELECT name FROM hot)
                 ORDER BY t.name",
            )
            .map_err(error)?;
        query
            .query_map(params![compare, hot_after], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
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
        if let Ok(statements) = record_statements(snapshots, events)
            && let Some(done) = crate::daemon::commit::route(|| crate::daemon::commit::Write {
                preconditions: vec![],
                statements,
            })
        {
            done?;
            self.wake(events);
            return Ok(());
        }
        let transaction = self.0.transaction().map_err(error)?;
        commit_records(&transaction, snapshots, events)?;
        transaction.commit().map_err(error)?;
        self.wake(events);
        Ok(())
    }

    /// Commit a mail receipt with its task updates and outbox exactly once per stable key; the first snapshot's task owns the mail.
    pub fn receive_mail(
        &mut self,
        key: &str,
        snapshots: &[(String, String)],
        events: &[Value],
    ) -> Result<bool, String> {
        let owner = snapshots
            .first()
            .map_or("received", |(name, _)| name.as_str());
        let key_name = format!("mail:{key}");
        if let Ok(records) = record_statements(snapshots, events)
            && let Some(done) = crate::daemon::commit::route(|| {
                let mut statements = vec![crate::daemon::commit::statement(
                    "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
                    vec![key_name.clone().into(), owner.to_owned().into()],
                )];
                statements.extend(records);
                crate::daemon::commit::Write {
                    preconditions: vec![crate::daemon::commit::Precondition {
                        sql: "SELECT NOT EXISTS (SELECT 1 FROM metadata WHERE key = ?1)".into(),
                        params: vec![key_name.clone().into()],
                    }],
                    statements,
                }
            })
        {
            return match done? {
                crate::daemon::commit::Outcome::Committed => {
                    self.wake(events);
                    Ok(true)
                }
                crate::daemon::commit::Outcome::Conflict => Ok(false),
            };
        }
        let transaction = self.0.transaction().map_err(error)?;
        let inserted = transaction
            .execute(
                "INSERT OR IGNORE INTO metadata (key, value) VALUES (?1, ?2)",
                [format!("mail:{key}"), owner.to_owned()],
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
        if let Ok(records) = record_statements(snapshots, events)
            && let Some(done) = crate::daemon::commit::route(|| {
                let mut statements = vec![crate::daemon::commit::statement(
                    "UPDATE messages SET state = 'sent', attempts = attempts + 1, last_error = '' WHERE id = ?1",
                    vec![id.into()],
                )];
                statements.extend(records);
                crate::daemon::commit::Write {
                    preconditions: vec![pending_message(id)],
                    statements,
                }
            })
        {
            return match done? {
                crate::daemon::commit::Outcome::Committed => {
                    self.wake(events);
                    Ok(())
                }
                crate::daemon::commit::Outcome::Conflict => {
                    Err(format!("amh: no pending message {id}"))
                }
            };
        }
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
        if let Some(done) = one(
            "INSERT INTO records (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = excluded.text",
            vec![name.to_owned().into(), text.to_owned().into()],
        ) {
            return done;
        }
        self.0.execute("INSERT INTO records (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = excluded.text", params![name, text]).map_err(error)?;
        Ok(())
    }

    /// Import a legacy record without replacing a concurrent import or update.
    pub fn import_record(&self, name: &str, text: &str) -> Result<(), String> {
        if let Some(done) = one(
            "INSERT OR IGNORE INTO records (name, text) VALUES (?1, ?2)",
            vec![name.to_owned().into(), text.to_owned().into()],
        ) {
            return done;
        }
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
        if let Some(done) = one(
            "INSERT INTO records (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = records.text || excluded.text",
            vec![name.to_owned().into(), text.to_owned().into()],
        ) {
            return done;
        }
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
        if crate::daemon::commit::global().is_some() {
            // Compare-and-set through the writer queue: retry when another writer changed the record in between.
            loop {
                let old = self.record(name)?;
                let mut known: std::collections::BTreeSet<u64> = old
                    .as_deref()
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|id| id.parse().ok())
                    .collect();
                known.extend(ids);
                let merged: Vec<u64> = known.into_iter().rev().take(limit).collect();
                let text = merged
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                let unchanged = crate::daemon::commit::Precondition {
                    sql: "SELECT (SELECT text FROM records WHERE name = ?1) IS ?2".into(),
                    params: vec![
                        name.to_owned().into(),
                        old.map_or(rusqlite::types::Value::Null, Into::into),
                    ],
                };
                let done = crate::daemon::commit::route(|| crate::daemon::commit::Write {
                    preconditions: vec![unchanged],
                    statements: vec![crate::daemon::commit::statement(
                        "INSERT INTO records (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = excluded.text",
                        vec![name.to_owned().into(), text.to_owned().into()],
                    )],
                });
                match done {
                    Some(Ok(crate::daemon::commit::Outcome::Committed)) => return Ok(merged),
                    Some(Ok(crate::daemon::commit::Outcome::Conflict)) => continue,
                    Some(Err(error)) => return Err(error),
                    None => break,
                }
            }
        }
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

    /// When `subject` first had an event of `kind`, if the log still holds one.
    pub fn first_event_time(&self, kind: &str, subject: &str) -> Result<Option<String>, String> {
        self.0
            .query_row(
                "SELECT MIN(time) FROM events WHERE kind = ?1 AND subject = ?2",
                [kind, subject],
                |row| row.get(0),
            )
            .map_err(error)
    }

    /// The subjects of events of `kind` after event `after`.
    pub fn subjects_since(&self, after: i64, kind: &str) -> Result<Vec<String>, String> {
        let mut query = self
            .0
            .prepare("SELECT DISTINCT subject FROM events WHERE id > ?1 AND kind = ?2")
            .map_err(error)?;
        let rows = query
            .query_map(params![after, kind], |row| row.get::<_, String>(0))
            .map_err(error)?;
        rows.collect::<Result<_, _>>().map_err(error)
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

    /// Make `owner` the task that handles a received mail, as when its owner forwards it.
    pub fn set_mail_owner(&self, key: &str, owner: &str) -> Result<(), String> {
        if let Some(done) = one(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            vec![format!("mail:{key}").into(), owner.to_owned().into()],
        ) {
            return done;
        }
        self.0.execute("INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![format!("mail:{key}"), owner]).map_err(error)?;
        Ok(())
    }

    pub fn set_cursor(&self, key: &str, id: i64) -> Result<(), String> {
        if let Some(done) = one(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            vec![key.to_owned().into(), id.to_string().into()],
        ) {
            return done;
        }
        self.0.execute("INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, id.to_string()]).map_err(error)?;
        Ok(())
    }

    pub fn prune_events(&mut self, ids: &[i64]) -> Result<(), String> {
        if let Some(queue) = crate::daemon::commit::global() {
            for chunk in ids.chunks(crate::daemon::commit::WRITE_MAX) {
                queue.submit(crate::daemon::commit::Write {
                    preconditions: vec![],
                    statements: chunk
                        .iter()
                        .map(|id| {
                            crate::daemon::commit::statement(
                                "DELETE FROM events WHERE id = ?1",
                                vec![(*id).into()],
                            )
                        })
                        .collect(),
                })?;
            }
            return Ok(());
        }
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
            "SELECT id, channel, target, text, source, state, attempts, last_error, retry_at FROM messages WHERE state = 'pending' AND channel = ?1 ORDER BY id"
        ).map_err(error)?;
        query
            .query_map([channel], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, i64>(0)?, "channel": row.get::<_, String>(1)?,
                    "target": row.get::<_, String>(2)?, "text": row.get::<_, String>(3)?,
                    "source": row.get::<_, String>(4)?, "state": row.get::<_, String>(5)?,
                    "attempts": row.get::<_, i64>(6)?, "last_error": row.get::<_, String>(7)?,
                    "retry_at": row.get::<_, i64>(8)?
                }))
            })
            .map_err(error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)
    }

    /// Record a failed delivery and when to try again: after 1 min, doubling each attempt, at most 6 h.
    // The main manager: notices to retired tasks were "retried about every 1.2 s with no end, each storing a 'message failed' event: 1.93 M events today"
    pub fn defer_message(&self, id: i64, failure: &str, now_s: i64) -> Result<(), String> {
        if let Some(done) = one(
            "UPDATE messages SET attempts = attempts + 1, last_error = ?1,
                 retry_at = ?2 + MIN(60 * (1 << MIN(attempts, 9)), 21600)
                 WHERE id = ?3 AND state = 'pending'",
            vec![failure.to_owned().into(), now_s.into(), id.into()],
        ) {
            return done;
        }
        self.0
            .execute(
                "UPDATE messages SET attempts = attempts + 1, last_error = ?1,
                 retry_at = ?2 + MIN(60 * (1 << MIN(attempts, 9)), 21600)
                 WHERE id = ?3 AND state = 'pending'",
                params![failure, now_s, id],
            )
            .map_err(error)?;
        Ok(())
    }

    /// Record a delivery result. An interrupted external send may retry the same stable ID.
    pub fn finish_message(&self, id: i64, success: bool, failure: &str) -> Result<(), String> {
        if let Some(done) = crate::daemon::commit::route(|| crate::daemon::commit::Write {
            preconditions: vec![pending_message(id)],
            statements: vec![crate::daemon::commit::statement(
                "UPDATE messages SET state = ?1, attempts = attempts + 1, last_error = ?2 WHERE id = ?3",
                vec![
                    (if success { "sent" } else { "pending" }).to_owned().into(),
                    (if success { "" } else { failure }).to_owned().into(),
                    id.into(),
                ],
            )],
        }) {
            return match done? {
                crate::daemon::commit::Outcome::Committed => Ok(()),
                crate::daemon::commit::Outcome::Conflict => {
                    Err(format!("amh: no pending message {id}"))
                }
            };
        }
        let changed = self.0.execute(
            "UPDATE messages SET state = ?1, attempts = attempts + 1, last_error = ?2 WHERE id = ?3 AND state = 'pending'",
            params![if success { "sent" } else { "pending" }, if success { "" } else { failure }, id]
        ).map_err(error)?;
        if changed == 0 {
            return Err(format!("amh: no pending message {id}"));
        }
        Ok(())
    }

    /// Render the event view directly from stored JSON without rebuilding each event as a Rust value.
    /// The JSON lines of events with an id above `after`, and the highest id seen (`after` when none).
    pub fn event_lines_after(&self, after: i64) -> Result<(String, i64), String> {
        let mut query = self
            .0
            .prepare(
                "SELECT id, json_object('actor', actor, 'data', json(data), 'id', id,
                'kind', kind, 'subject', subject, 'time', time) FROM events WHERE id > ?1 ORDER BY id",
            )
            .map_err(error)?;
        let rows = query
            .query_map([after], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(error)?;
        let (mut text, mut last) = (String::new(), after);
        for row in rows {
            let (id, line) = row.map_err(error)?;
            text.push_str(&line);
            text.push('\n');
            last = id;
        }
        Ok((text, last))
    }

    /// The newest `n` events about task `name` or its requests (`NAME#N`), oldest first, through the subject index.
    // 🧑 "Still unacceptably slow. Should be some ms. Why so slow"
    pub fn task_events(&self, name: &str, n: usize) -> Result<Vec<String>, String> {
        let mut query = self
            .0
            .prepare(
                "SELECT json_object('id', id, 'time', time, 'actor', actor, 'kind', kind, 'subject', subject, 'data', json(data))
                 FROM (SELECT * FROM events WHERE subject = ?1
                       UNION ALL SELECT * FROM events WHERE subject >= ?2 AND subject < ?3
                       ORDER BY id DESC LIMIT ?4)
                 ORDER BY id",
            )
            .map_err(error)?;
        let rows = query
            .query_map(
                params![name, format!("{name}#"), format!("{name}$"), n as i64],
                |row| row.get::<_, String>(0),
            )
            .map_err(error)?;
        rows.collect::<Result<_, _>>().map_err(error)
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

/// One SQL statement with its parameters, so the daemon's commit thread can apply writes built elsewhere.
pub struct Statement {
    pub sql: String,
    pub params: Vec<rusqlite::types::Value>,
}

/// The writes that commit task snapshots and their events, queuing a message for each `message queued` event.
pub fn record_statements(
    snapshots: &[(String, String)],
    events: &[Value],
) -> Result<Vec<Statement>, String> {
    let mut statements: Vec<Statement> = snapshots
        .iter()
        .map(|(name, text)| Statement {
            sql: "INSERT INTO tasks (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = excluded.text".into(),
            params: vec![name.clone().into(), text.clone().into()],
        })
        .collect();
    for event in events {
        statements.extend(event_statements(event, true)?);
    }
    Ok(statements)
}

fn event_statements(event: &Value, queue: bool) -> Result<Vec<Statement>, String> {
    let field = |name: &str| {
        event
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("amh: event has no string {name}"))
    };
    let data = event
        .get("data")
        .map_or_else(|| "null".to_owned(), Value::to_string);
    let mut statements = vec![Statement {
        sql: "INSERT INTO events (time, actor, kind, subject, data) VALUES (?1, ?2, ?3, ?4, ?5)"
            .into(),
        params: vec![
            field("time")?.to_owned().into(),
            field("actor")?.to_owned().into(),
            field("kind")?.to_owned().into(),
            field("subject")?.to_owned().into(),
            data.into(),
        ],
    }];
    if queue && field("kind")? == "message queued" {
        statements.push(enqueue_statement(field("subject")?, &event["data"])?);
    }
    Ok(statements)
}

fn enqueue_statement(source: &str, data: &Value) -> Result<Statement, String> {
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
    Ok(Statement {
        sql: "INSERT INTO messages (channel, target, text, source) VALUES (?1, ?2, ?3, ?4)".into(),
        params: vec![
            channel.to_owned().into(),
            target.to_owned().into(),
            text.to_owned().into(),
            source.to_owned().into(),
        ],
    })
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
        assert_eq!(db.mail_owner("inbox:message-id")?, Some("task.md".into()));
        assert_eq!(db.mail_owner("inbox:unknown")?, None);
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
    fn event_view_preserves_json_values_and_order() -> Result<(), Box<dyn std::error::Error>> {
        let mut db = Database::open(Path::new(":memory:"))?;
        let events = [
            serde_json::json!({"time":"now", "actor":"agent\n🧑", "kind":"test", "subject":"quotes\"\\", "data":{"array":[null,true,42,1.25],"text":"line\n\t中文","nested":{"a":1}}}),
            serde_json::json!({"time":"later", "actor":"agent", "kind":"test", "subject":"second", "data":null}),
        ];
        db.commit(&[], &events)?;
        let rendered = db.event_lines_after(0)?.0;
        let values = rendered
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(values, db.events()?);
        assert!(rendered.ends_with('\n'));
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

/// One statement through the daemon's writer queue; `None` when there is no queue here.
fn one(sql: &str, params: Vec<rusqlite::types::Value>) -> Option<Result<(), String>> {
    crate::daemon::commit::route(|| crate::daemon::commit::Write {
        preconditions: vec![],
        statements: vec![crate::daemon::commit::statement(sql, params)],
    })
    .map(|done| done.map(|_| ()))
}

fn pending_message(id: i64) -> crate::daemon::commit::Precondition {
    crate::daemon::commit::Precondition {
        sql: "SELECT EXISTS (SELECT 1 FROM messages WHERE id = ?1 AND state = 'pending')".into(),
        params: vec![id.into()],
    }
}
