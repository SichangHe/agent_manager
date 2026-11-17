//! Daemon schema migrations and the one-time import of today's state files.

use std::fs;
use std::path::Path;

use rusqlite::{Connection, params};

/// Today's state files the daemon takes over; each is renamed `<name>.migrated` once imported.
pub const STATE_FILES: [&str; 4] = [
    "amh-starts.queue.json",
    "amh-notices.json",
    "amh-holds.json",
    "amh-quota-pauses.json",
];

/// Additive schema steps, by version: only new tables, new nullable or defaulted columns, and copies; never a drop, rename, or type change, so binaries of older versions keep working on a newer schema.
const MIGRATIONS: &[(i64, &str)] = &[(1, V1), (2, V2), (3, V3), (4, V4)];

/// The watcher keeps delivering its own `messages` queue, so V1's copies of its pending notices would be sent twice (and all pending ones at cut-over were notices to held agents): dead-letter them.
const V4: &str = "
UPDATE outbox SET state = 'dead', last_error = 'legacy notice: the watcher''s messages queue keeps it'
    WHERE id LIKE 'legacy-%' AND state IN ('pending', 'sending');";

/// Outbox dead letters, failure text and retry time; the daemon boot that claimed each request.
const V3: &str = "
CREATE TABLE outbox_v3 (
    id TEXT PRIMARY KEY, channel TEXT NOT NULL, recipient TEXT NOT NULL,
    body TEXT NOT NULL, source TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending', 'sending', 'delivered', 'dead')),
    attempts INTEGER NOT NULL DEFAULT 0, first_sent_at TEXT, item_id TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ')),
    last_error TEXT NOT NULL DEFAULT '', next_at INTEGER NOT NULL DEFAULT 0
);
INSERT INTO outbox_v3 (id, channel, recipient, body, source, state, attempts, first_sent_at, item_id, created_at)
    SELECT id, channel, recipient, body, source, state, attempts, first_sent_at, item_id, created_at FROM outbox;
DROP TABLE outbox;
ALTER TABLE outbox_v3 RENAME TO outbox;
CREATE INDEX outbox_open ON outbox (state, created_at) WHERE state IN ('pending', 'sending');
ALTER TABLE request_outcomes ADD COLUMN boot TEXT NOT NULL DEFAULT '';";

/// Apply every step not yet in `schema_version`, all in one transaction; a crash leaves none applied, so the next start redoes them.
/// Versions in the table that this binary does not know come from a newer binary and are left alone.
pub fn migrate(connection: &mut Connection) -> Result<(), String> {
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(error)?;
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                 version INTEGER PRIMARY KEY,
                 applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ'))
             )",
        )
        .map_err(error)?;
    for (version, sql) in MIGRATIONS {
        let applied: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_version WHERE version = ?1)",
                [version],
                |row| row.get(0),
            )
            .map_err(error)?;
        if !applied {
            transaction.execute_batch(sql).map_err(error)?;
            transaction
                .execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    [version],
                )
                .map_err(error)?;
        }
    }
    transaction.commit().map_err(error)
}

/// Outcomes of requests by their client-made id, so a retry gets the stored answer instead of running again.
const V2: &str = "
CREATE TABLE request_outcomes (
    id TEXT PRIMARY KEY, code INTEGER, out TEXT NOT NULL DEFAULT '', err TEXT NOT NULL DEFAULT '',
    at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ'))
);
CREATE INDEX outbox_open ON outbox (state, created_at) WHERE state IN ('pending', 'sending');";

const V1: &str = "
ALTER TABLE tasks ADD COLUMN version INTEGER NOT NULL DEFAULT 0;
CREATE TABLE outbox (
    id TEXT PRIMARY KEY, channel TEXT NOT NULL, recipient TEXT NOT NULL,
    body TEXT NOT NULL, source TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending', 'sending', 'delivered')),
    attempts INTEGER NOT NULL DEFAULT 0, first_sent_at TEXT, item_id TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ'))
);
CREATE TABLE inbox (
    source TEXT NOT NULL, source_id TEXT NOT NULL, body TEXT NOT NULL,
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ')),
    PRIMARY KEY (source, source_id)
);
CREATE TABLE spawn_jobs (
    id TEXT PRIMARY KEY, agent TEXT NOT NULL, request TEXT NOT NULL CHECK(json_valid(request)),
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued', 'running', 'done', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0, error TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ')), caller TEXT NOT NULL DEFAULT ''
);
CREATE TABLE leases (
    session TEXT PRIMARY KEY, owner TEXT NOT NULL, pool TEXT NOT NULL DEFAULT '',
    deadline TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ'))
);
CREATE TABLE edit_import (
    id INTEGER PRIMARY KEY, task TEXT NOT NULL, base_version INTEGER NOT NULL, chunks TEXT NOT NULL CHECK(json_valid(chunks)),
    next_chunk INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'open' CHECK(state IN ('open', 'closed'))
);
CREATE INDEX edit_import_open ON edit_import (task) WHERE state = 'open';
CREATE TABLE state_files (name TEXT PRIMARY KEY, text TEXT NOT NULL);
INSERT OR IGNORE INTO outbox (id, channel, recipient, body, source)
    SELECT 'legacy-' || id, channel, target, text, source FROM messages WHERE state = 'pending';";

/// Import each of `STATE_FILES` present in `state_dir`, then rename it `.migrated`.
///
/// Import and rename are separate steps; a crash between them re-imports the same text on the next start, which is harmless: `state_files` is keyed by name and queued starts by a content-derived id.
/// A file changed after it was read is left in place for the next start.
pub fn import_state_files(
    connection: &mut Connection,
    state_dir: &Path,
) -> Result<Vec<String>, String> {
    let mut imported = Vec::new();
    for name in STATE_FILES {
        let path = state_dir.join(name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("amh: cannot read {}: {e}", path.display())),
        };
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(error)?;
        transaction
            .execute(
                "INSERT INTO state_files (name, text) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET text = excluded.text",
                params![name, text],
            )
            .map_err(error)?;
        if name == "amh-starts.queue.json" {
            queue_starts(&transaction, &text)?;
        }
        transaction.commit().map_err(error)?;
        if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            continue;
        }
        let renamed = state_dir.join(format!("{name}.migrated"));
        fs::rename(&path, &renamed)
            .and_then(|()| fs::File::open(state_dir)?.sync_all())
            .map_err(|e| format!("amh: cannot rename {}: {e}", path.display()))?;
        imported.push(name.to_owned());
    }
    Ok(imported)
}

/// Queue each start of the legacy queue file (a JSON array of start requests) as a `spawn_jobs` row.
fn queue_starts(transaction: &rusqlite::Transaction<'_>, text: &str) -> Result<(), String> {
    let requests: Vec<serde_json::Value> = serde_json::from_str(text)
        .map_err(|e| format!("amh: invalid amh-starts.queue.json: {e}"))?;
    for request in requests {
        let body = request.to_string();
        let agent = ["agent", "task", "name"]
            .iter()
            .find_map(|key| request.get(*key).and_then(|v| v.as_str()))
            .unwrap_or_default();
        transaction
            .execute(
                "INSERT OR IGNORE INTO spawn_jobs (id, agent, request, caller) VALUES (?1, ?2, ?3, 'migration')",
                params![format!("legacy-{:016x}", fnv1a(body.as_bytes())), agent, body],
            )
            .map_err(error)?;
    }
    Ok(())
}

/// A stable 64-bit FNV-1a hash, so a re-import makes the same id.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn error(error: rusqlite::Error) -> String {
    format!("amh: SQLite: {error}")
}
