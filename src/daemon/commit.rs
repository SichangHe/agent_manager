//! The daemon's only SQLite writer: a thread that group-commits short writes.

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params_from_iter};

use crate::database::Statement;

/// Most writes one batch takes.
const BATCH_MAX: usize = 256;
/// Longest a batch waits for more writes after its first.
const BATCH_WINDOW: Duration = Duration::from_millis(2);
/// Most statements one write may hold.
pub const WRITE_MAX: usize = 64;
/// A batch slower than this is logged.
const SLOW_BATCH: Duration = Duration::from_millis(5);
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
        let waits = waits_us.clone();
        let thread = std::thread::Builder::new()
            .name("amhd-commit".into())
            .spawn(move || run(connection, receiver, waits, committed))
            .map_err(|error| format!("amh: cannot start the commit thread: {error}"))?;
        Ok((Self { jobs, waits_us }, thread))
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
        let mut waits = self.waits_us.lock().map(|w| w.clone()).unwrap_or_default();
        waits.sort_unstable();
        waits.get(waits.len() * 99 / 100).copied().unwrap_or(0)
    }
}

fn writer(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(error)?;
    // Other writers remain: CLI processes outside the daemon and the oversized-write fallback.
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(error)?;
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .map_err(error)?;
    Ok(connection)
}

fn run(
    mut connection: Connection,
    receiver: mpsc::Receiver<Job>,
    waits_us: Arc<Mutex<Vec<u32>>>,
    committed: mpsc::Sender<()>,
) {
    ON_WRITER.set(true);
    while let Ok(first) = receiver.recv() {
        let began = Instant::now();
        let mut batch = vec![first];
        while batch.len() < BATCH_MAX {
            let left = BATCH_WINDOW.saturating_sub(began.elapsed());
            match receiver.recv_timeout(left) {
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
        if took > SLOW_BATCH {
            eprintln!(
                "amhd: commit batch of {} writes took {} ms",
                batch.len(),
                took.as_millis()
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
            .query_row(
                &precondition.sql,
                params_from_iter(&precondition.params),
                |row| row.get(0),
            )
            .map_err(error)?;
        if holds == 0 {
            return Ok(Outcome::Conflict);
        }
    }
    for statement in &write.statements {
        transaction
            .execute(&statement.sql, params_from_iter(&statement.params))
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
