//! The daemon's only SQLite writer: a thread that group-commits short writes.

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params_from_iter};

use crate::database::Statement;

/// Most writes one batch takes.
const BATCH_MAX: usize = 256;
/// Most statements one write may hold.
pub const WRITE_MAX: usize = 64;
/// A batch slower than this is logged.
const SLOW_BATCH: Duration = Duration::from_millis(2);
/// Queue waits kept for the p99 gate.
const WAITS_KEPT: usize = 65_536;

/// A query returning one integer; the write applies only when every precondition returns nonzero.
pub struct Precondition {
    pub sql: String,
    pub params: Vec<rusqlite::types::Value>,
}

/// One caller's atomic unit: preconditions, then at most `WRITE_MAX` statements.
#[derive(Default)]
pub struct Write {
    pub preconditions: Vec<Precondition>,
    pub statements: Vec<Statement>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Committed,
    /// A precondition failed; nothing of the write was applied.
    Conflict,
}

struct Job {
    write: Write,
    enqueued: Instant,
    reply: mpsc::Sender<Result<Outcome, String>>,
}

/// The handle request threads submit writes through.
#[derive(Clone)]
pub struct Queue {
    jobs: mpsc::Sender<Job>,
    waits_us: Arc<Mutex<Vec<u32>>>,
    /// How long each batch's transaction took, from BEGIN to COMMIT: the writer's hold.
    holds_us: Arc<Mutex<Vec<u32>>>,
}

impl Queue {
    /// Open the writer connection and start the commit thread, which signals `committed` after each batch that changed something; the thread ends when every `Queue` is dropped.
    pub fn start(
        path: &Path,
        committed: mpsc::Sender<()>,
    ) -> Result<(Self, std::thread::JoinHandle<()>), String> {
        let connection = writer(path)?;
        let (jobs, receiver) = mpsc::channel();
        let waits_us = Arc::new(Mutex::new(Vec::new()));
        let holds_us = Arc::new(Mutex::new(Vec::new()));
        let (waits, holds) = (waits_us.clone(), holds_us.clone());
        let thread = std::thread::Builder::new()
            .name("amhd-commit".into())
            .spawn(move || run(connection, receiver, waits, holds, committed))
            .map_err(|error| format!("amh: cannot start the commit thread: {error}"))?;
        Ok((
            Self {
                jobs,
                waits_us,
                holds_us,
            },
            thread,
        ))
    }

    /// Block until the write is durable (fsynced) or refused; only then does the caller answer its client.
    pub fn submit(&self, write: Write) -> Result<Outcome, String> {
        WROTE.set(true);
        let (reply, answer) = mpsc::channel();
        self.jobs
            .send(Job {
                write,
                enqueued: Instant::now(),
                reply,
            })
            .map_err(|_| "amh: the commit thread has stopped".to_owned())?;
        answer
            .recv()
            .map_err(|_| "amh: the commit thread has stopped".to_owned())?
    }

    /// The 99th percentile of recent waits from enqueue to batch start, in microseconds.
    pub fn wait_p99_us(&self) -> u32 {
        p99(&self.waits_us)
    }

    /// The 99th percentile of recent batch transactions, BEGIN to COMMIT, in microseconds.
    pub fn hold_p99_us(&self) -> u32 {
        p99(&self.holds_us)
    }
}

fn p99(kept: &Mutex<Vec<u32>>) -> u32 {
    let mut values = kept.lock().map(|w| w.clone()).unwrap_or_default();
    values.sort_unstable();
    values.get(values.len() * 99 / 100).copied().unwrap_or(0)
}

fn writer(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(error)?;
    // Other writers remain: CLI processes outside the daemon and the oversized-write fallback.
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(error)?;
    connection
        // WAL with synchronous=NORMAL, as the rest of the store: a commit survives a crash of amhd (WAL is in the OS cache); only a power loss can drop the last commits. FULL would fsync every commit.
        // 🧑 "Locking should be in us not ms"
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=0;",
        )
        .map_err(error)?;
    Ok(connection)
}

fn run(
    mut connection: Connection,
    receiver: mpsc::Receiver<Job>,
    waits_us: Arc<Mutex<Vec<u32>>>,
    holds_us: Arc<Mutex<Vec<u32>>>,
    committed: mpsc::Sender<()>,
) {
    ON_WRITER.set(true);
    while let Ok(first) = receiver.recv() {
        // Group commit without waiting: a batch is whatever queued while the last one committed.
        let mut batch = vec![first];
        while batch.len() < BATCH_MAX {
            match receiver.try_recv() {
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }
        let started = Instant::now();
        if let Ok(mut waits) = waits_us.lock() {
            if waits.len() + batch.len() > WAITS_KEPT {
                waits.clear();
            }
            waits.extend(batch.iter().map(|job| {
                started
                    .saturating_duration_since(job.enqueued)
                    .as_micros()
                    .min(u32::MAX as u128) as u32
            }));
        }
        let results = commit(&mut connection, &batch);
        let took = started.elapsed();
        if let Ok(mut holds) = holds_us.lock() {
            if holds.len() >= WAITS_KEPT {
                holds.clear();
            }
            holds.push(took.as_micros().min(u32::MAX as u128) as u32);
        }
        if took > SLOW_BATCH {
            // What was in it: each statement's start and the bytes of its parameters, largest first.
            // The main manager: "record, for every writer batch over 2 ms, the statements and row sizes in it"
            let mut parts: Vec<(usize, String)> = batch
                .iter()
                .flat_map(|job| job.write.statements.iter())
                .map(|statement| {
                    let bytes: usize = statement
                        .params
                        .iter()
                        .map(|param| match param {
                            rusqlite::types::Value::Text(text) => text.len(),
                            rusqlite::types::Value::Blob(blob) => blob.len(),
                            _ => 8,
                        })
                        .sum();
                    let head: String = statement
                        .sql
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(70)
                        .collect();
                    let first = statement
                        .params
                        .first()
                        .and_then(|param| match param {
                            rusqlite::types::Value::Text(text) => {
                                Some(text.chars().take(40).collect::<String>())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    (bytes, format!("{bytes}B {head} [{first}]"))
                })
                .collect();
            parts.sort_by(|a, b| b.0.cmp(&a.0));
            eprintln!(
                "amhd: commit batch of {} writes, {} statements, {} bytes took {} us: {}",
                batch.len(),
                parts.len(),
                parts.iter().map(|p| p.0).sum::<usize>(),
                took.as_micros(),
                parts
                    .iter()
                    .take(6)
                    .map(|p| p.1.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        if results
            .iter()
            .any(|result| result == &Ok(Outcome::Committed))
        {
            let _ = committed.send(());
        }
        for (job, result) in batch.into_iter().zip(results) {
            let _ = job.reply.send(result);
        }
    }
}

/// Apply every write in one transaction, each under its own savepoint so a refused write leaves the rest; on a failed commit every write fails.
fn commit(connection: &mut Connection, batch: &[Job]) -> Vec<Result<Outcome, String>> {
    let transaction =
        match connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
            Ok(transaction) => transaction,
            Err(e) => {
                let message = error(e);
                return batch.iter().map(|_| Err(message.clone())).collect();
            }
        };
    let results: Vec<_> = batch
        .iter()
        .map(|job| {
            transaction
                .execute_batch("SAVEPOINT write")
                .map_err(error)?;
            let result = apply(&transaction, &job.write);
            let end = match result {
                Ok(Outcome::Committed) => "RELEASE write",
                _ => "ROLLBACK TO write; RELEASE write",
            };
            transaction.execute_batch(end).map_err(error)?;
            result
        })
        .collect();
    super::crash_point("before-commit");
    let committed = transaction.commit();
    super::crash_point("after-commit");
    match committed {
        Ok(()) => results,
        Err(e) => {
            let message = error(e);
            batch.iter().map(|_| Err(message.clone())).collect()
        }
    }
}

fn apply(transaction: &rusqlite::Transaction<'_>, write: &Write) -> Result<Outcome, String> {
    if write.statements.len() > WRITE_MAX {
        return Err(format!(
            "amh: a write holds {} statements; at most {WRITE_MAX} are allowed",
            write.statements.len()
        ));
    }
    for precondition in &write.preconditions {
        let holds: i64 = transaction
            .prepare_cached(&precondition.sql)
            .and_then(|mut query| {
                query.query_row(params_from_iter(&precondition.params), |row| row.get(0))
            })
            .map_err(error)?;
        if holds == 0 {
            return Ok(Outcome::Conflict);
        }
    }
    for statement in &write.statements {
        transaction
            .prepare_cached(&statement.sql)
            .and_then(|mut query| query.execute(params_from_iter(&statement.params)))
            .map_err(error)?;
    }
    Ok(Outcome::Committed)
}

fn error(error: rusqlite::Error) -> String {
    format!("amh: SQLite: {error}")
}

static GLOBAL: std::sync::OnceLock<Queue> = std::sync::OnceLock::new();

thread_local! {
    static WROTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this thread submitted a write since the last call.
pub fn take_wrote() -> bool {
    WROTE.replace(false)
}

thread_local! {
    static ON_WRITER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Make `queue` the daemon's writer for every store write; set once by `serve`.
pub fn install(queue: Queue) {
    let _ = GLOBAL.set(queue);
}

/// The daemon's writer queue, when this process is the daemon and the caller is not the writer itself.
pub fn global() -> Option<&'static Queue> {
    if ON_WRITER.get() {
        return None;
    }
    GLOBAL.get()
}

/// One statement as a write.
pub fn statement(sql: &str, params: Vec<rusqlite::types::Value>) -> Statement {
    Statement {
        sql: sql.into(),
        params,
    }
}

/// Run `write` through the daemon's queue when there is one; `None` tells the caller to write directly.
pub fn route(write: impl FnOnce() -> Write) -> Option<Result<Outcome, String>> {
    let queue = global()?;
    let write = write();
    if write.statements.len() > WRITE_MAX {
        return None;
    }
    Some(queue.submit(write))
}

/// Apply `write` on `connection` in its own transaction, for callers outside the daemon (tests, tools).
pub fn apply_now(connection: &mut Connection, write: &Write) -> Result<Outcome, String> {
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(error)?;
    let outcome = apply(&transaction, write)?;
    if outcome == Outcome::Committed {
        transaction.commit().map_err(error)?;
    }
    Ok(outcome)
}
