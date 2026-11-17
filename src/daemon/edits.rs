//! Typed edits of task files, imported through the commit queue in chunks recorded in one `edit_import` row.

use rusqlite::types::Value as Sql;
use serde_json::{Value, json};

use super::commit::{Outcome, Precondition, Queue, WRITE_MAX, Write};
use crate::database::{Database, Statement, record_statements};
use crate::store::Store;

/// Statements of the edit per chunk, leaving room for the version bump and the progress update.
const CHUNK: usize = WRITE_MAX - 2;

/// Import the typed edit of every task whose file changed since `since` (all with `None`).
pub fn import_all(store: &Store, queue: &Queue, since: Option<i128>) -> Result<(), String> {
    let database = initialized(store)?;
    for (name, _) in database.tasks()? {
        import(store, &database, queue, &name, since)?;
    }
    Ok(())
}

/// Import the typed edit of task `name`, if its file has one; a task with an open row is left to `resume_all`.
pub fn import(
    store: &Store,
    database: &Database,
    queue: &Queue,
    name: &str,
    since: Option<i128>,
) -> Result<(), String> {
    if database.open_imports()?.contains(name) {
        return Ok(());
    }
    let Some(text) = database.task(name)? else {
        return Ok(());
    };
    let Some(task) = store.typed_edit(database, name, &text, since)? else {
        return Ok(());
    };
    let (task, events) = store.prepare(&task)?;
    let statements = record_statements(&[(name.to_owned(), task.render())], &events)?;
    let chunks: Vec<Value> = statements
        .chunks(CHUNK)
        .map(|chunk| Value::Array(chunk.iter().map(encode).collect()))
        .collect();
    let version: i64 = database
        .sql()
        .query_row("SELECT version FROM tasks WHERE name = ?1", [name], |row| {
            row.get(0)
        })
        .map_err(|e| format!("amh: SQLite: {e}"))?;
    let opened = queue.submit(Write {
        preconditions: vec![
            Precondition {
                sql: "SELECT NOT EXISTS(SELECT 1 FROM edit_import WHERE task = ?1 AND state = 'open')".into(),
                params: vec![name.to_owned().into()],
            },
            // Writers outside the daemon do not bump `version` yet, so the opening also checks the text it diffed against.
            Precondition {
                sql: "SELECT EXISTS(SELECT 1 FROM tasks WHERE name = ?1 AND version = ?2 AND text = ?3)".into(),
                params: vec![name.to_owned().into(), version.into(), text.into()],
            },
        ],
        statements: vec![Statement {
            sql: "INSERT INTO edit_import (task, base_version, chunks) VALUES (?1, ?2, ?3)".into(),
            params: vec![name.to_owned().into(), version.into(), Value::Array(chunks).to_string().into()],
        }],
    })?;
    if opened == Outcome::Committed {
        resume_all(database, queue)?;
    }
    Ok(())
}

/// Apply the remaining chunks of every open `edit_import` row, as after a crash mid-import.
///
/// Chunk `i` applies only while the task's version is `base_version + i` and the row's `next_chunk` is `i`; it bumps both. A version moved by another writer closes the row unfinished, and the next scan diffs again.
pub fn resume_all(database: &Database, queue: &Queue) -> Result<(), String> {
    let rows: Vec<(i64, String, i64, String, i64)> = {
        let mut query = database
            .sql()
            .prepare("SELECT id, task, base_version, chunks, next_chunk FROM edit_import WHERE state = 'open' ORDER BY id")
            .map_err(|e| format!("amh: SQLite: {e}"))?;
        query
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .and_then(|rows| rows.collect())
            .map_err(|e| format!("amh: SQLite: {e}"))?
    };
    for (id, task, base_version, chunks, next_chunk) in rows {
        let chunks: Vec<Value> = serde_json::from_str(&chunks)
            .map_err(|e| format!("amh: invalid edit_import chunks: {e}"))?;
        for (at, chunk) in chunks.iter().enumerate().skip(next_chunk as usize) {
            let at = at as i64;
            let last = at + 1 == chunks.len() as i64;
            let mut statements = chunk
                .as_array()
                .ok_or("amh: invalid edit_import chunk")?
                .iter()
                .map(decode)
                .collect::<Result<Vec<_>, _>>()?;
            statements.push(Statement {
                sql: "UPDATE tasks SET version = version + 1 WHERE name = ?1".into(),
                params: vec![task.clone().into()],
            });
            statements.push(progress(id, at + 1, last));
            let outcome = queue.submit(Write {
                preconditions: vec![Precondition {
                    sql: "SELECT EXISTS(SELECT 1 FROM edit_import e JOIN tasks t ON t.name = e.task WHERE e.id = ?1 AND e.next_chunk = ?2 AND t.version = ?3)".into(),
                    params: vec![id.into(), at.into(), (base_version + at).into()],
                }],
                statements,
            })?;
            if outcome == Outcome::Conflict {
                queue.submit(Write {
                    preconditions: vec![],
                    statements: vec![progress(id, at, true)],
                })?;
                break;
            }
        }
    }
    Ok(())
}

fn progress(id: i64, next_chunk: i64, close: bool) -> Statement {
    Statement {
        sql: "UPDATE edit_import SET next_chunk = ?2, state = ?3 WHERE id = ?1".into(),
        params: vec![
            id.into(),
            next_chunk.into(),
            if close { "closed" } else { "open" }.to_owned().into(),
        ],
    }
}

pub fn initialized(store: &Store) -> Result<Database, String> {
    store
        .database()?
        .ok_or_else(|| "amh: the SQLite store is not initialized".to_owned())
}

fn encode(statement: &Statement) -> Value {
    let params: Vec<Value> = statement
        .params
        .iter()
        .map(|param| match param {
            Sql::Null | Sql::Blob(_) => Value::Null,
            Sql::Integer(n) => json!(n),
            Sql::Real(x) => json!(x),
            Sql::Text(s) => json!(s),
        })
        .collect();
    json!({"sql": statement.sql, "params": params})
}

fn decode(value: &Value) -> Result<Statement, String> {
    let sql = value["sql"]
        .as_str()
        .ok_or("amh: invalid edit_import statement")?;
    let params = value["params"]
        .as_array()
        .ok_or("amh: invalid edit_import statement")?
        .iter()
        .map(|param| match param {
            Value::Null => Sql::Null,
            Value::String(s) => Sql::Text(s.clone()),
            Value::Number(n) => n
                .as_i64()
                .map_or(Sql::Real(n.as_f64().unwrap_or_default()), Sql::Integer),
            other => Sql::Text(other.to_string()),
        })
        .collect();
    Ok(Statement {
        sql: sql.to_owned(),
        params,
    })
}
