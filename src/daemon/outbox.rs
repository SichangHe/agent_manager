//! The daemon's outbox: an agent message is a durable row first and is sent by one worker, so `amh tell agent` returns without waiting on Omnigent.
// The rework design: "outbox: every external send (Omnigent, Zulip, mail) is a durable row first; one delivery worker per source/channel sends with bounded exponential backoff; FIFO per recipient task; a message failing permanently goes to a dead-letter state"

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use super::commit::{Outcome, Write};

type Settings = std::collections::BTreeMap<String, String>;

fn open(state_dir: &Path) -> Result<Connection, String> {
    let connection =
        Connection::open(state_dir.join("amh.sqlite3")).map_err(|e| format!("amh: SQLite: {e}"))?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(|e| format!("amh: SQLite: {e}"))?;
    Ok(connection)
}

/// Keep `body` for the agent at `recipient`; the worker sends it.
pub fn enqueue(
    state_dir: &Path,
    recipient: &str,
    body: &str,
    source: &str,
) -> Result<String, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Within a request the id comes from the request id, so a request re-run after a crash queues nothing twice.
    let from_request = REQUEST.with_borrow_mut(|request| {
        request.as_mut().map(|(rid, n)| {
            *n += 1;
            format!("req-{rid}-{n}")
        })
    });
    let id = from_request.unwrap_or_else(|| {
        format!(
            "msg-{}-{}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    });
    // A re-run request queues the same ids; one already there must hold the same message.
    let queued = write(
        state_dir,
        Write {
            preconditions: vec![super::commit::Precondition {
                sql: "SELECT NOT EXISTS (SELECT 1 FROM outbox WHERE id = ?1 AND (recipient <> ?2 OR body <> ?3))".into(),
                params: vec![id.clone().into(), recipient.to_owned().into(), body.to_owned().into()],
            }],
            statements: vec![super::commit::statement(
                "INSERT OR IGNORE INTO outbox (id, channel, recipient, body, source) VALUES (?1, 'agent', ?2, ?3, ?4)",
                vec![id.clone().into(), recipient.to_owned().into(), body.to_owned().into(), source.to_owned().into()],
            )],
        },
    )
    .map_err(|e| format!("amh: cannot queue the message: {e}"))?;
    if queued == Outcome::Conflict {
        return Err(format!(
            "amh: message id {id} already holds a different message; nothing queued"
        ));
    }
    if let Some(wake) = WAKE.get() {
        let _ = wake.send(());
    }
    Ok(id)
}

thread_local! {
    /// The client request this thread serves, and how many messages it queued so far.
    static REQUEST: std::cell::RefCell<Option<(String, u32)>> = const { std::cell::RefCell::new(None) };
}

/// Name the client request this thread serves, for idempotent message ids, until the returned guard drops.
pub fn serving_request(id: Option<&str>) -> Serving {
    REQUEST.set(id.map(|id| (id.to_owned(), 0)));
    Serving
}

/// Clears the served request, even when the request panics.
pub struct Serving;

impl Drop for Serving {
    fn drop(&mut self) {
        REQUEST.set(None);
    }
}

static WAKE: std::sync::OnceLock<std::sync::mpsc::Sender<()>> = std::sync::OnceLock::new();

/// Attempts before a message becomes a dead letter.
const ATTEMPTS_MAX: i64 = 8;
/// Most sends in flight at once, each to a different recipient.
const PARALLEL: usize = 32;

fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Apply `write` through the daemon's writer queue, or directly when there is none (tests).
fn write(state_dir: &Path, write: Write) -> Result<Outcome, String> {
    match super::commit::global() {
        Some(queue) => queue.submit(write),
        None => super::commit::apply_now(&mut open(state_dir)?, &write),
    }
}

fn one(sql: &str, params: Vec<rusqlite::types::Value>) -> Write {
    Write {
        preconditions: vec![],
        statements: vec![super::commit::statement(sql, params)],
    }
}

/// Start the delivery worker.
///
/// Delivery is at-least-once: a row is durably marked `sending` (attempts + 1) before its send, and a send's result is retried into the store until it commits, so a commit failure never resends. Only a daemon crash between the send and that commit repeats a message: at boot, rows still `sending` go back to `pending` with `last_error` saying so. Omnigent has no idempotency key to deduplicate that one case.
/// Per recipient FIFO; each send runs on its own thread, so a slow recipient blocks only its own rows. A recipient held at send time gets the row dead-lettered, as `amh tell` refuses held agents. A row failing `ATTEMPTS_MAX` times is dead-lettered; otherwise it waits 2^attempts s (at most 10 min).
pub fn start(settings: Settings, state_dir: std::path::PathBuf) -> Result<(), String> {
    let interrupted = write(
        &state_dir,
        one(
            "UPDATE outbox SET state = 'pending', last_error = 'interrupted by a daemon restart after being marked sending; may be delivered twice' WHERE state = 'sending'",
            vec![],
        ),
    );
    if let Err(error) = interrupted {
        return Err(format!("amh: outbox recovery: {error}"));
    }
    let (wake, woken) = std::sync::mpsc::channel();
    let _ = WAKE.set(wake.clone());
    std::thread::Builder::new()
        .name("amhd-outbox".into())
        .spawn(move || {
            let settings = std::sync::Arc::new(settings);
            let flying = FLYING.get_or_init(Default::default).clone();
            loop {
                if let Err(error) = dispatch(&settings, &state_dir, &flying, &wake) {
                    eprintln!("amhd: outbox: {error}");
                }
                let _ = woken.recv_timeout(Duration::from_secs(1));
            }
        })
        .map(|_| ())
        .map_err(|e| format!("amh: cannot start the outbox worker: {e}"))
}

type Flying = std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>;

static FLYING: std::sync::OnceLock<Flying> = std::sync::OnceLock::new();

/// Sends in flight now.
pub fn in_flight() -> usize {
    FLYING
        .get()
        .and_then(|f| f.lock().ok().map(|f| f.len()))
        .unwrap_or(0)
}

/// Start a send for the oldest due row of every recipient without one in flight.
fn dispatch(
    settings: &std::sync::Arc<Settings>,
    state_dir: &Path,
    flying: &Flying,
    wake: &std::sync::mpsc::Sender<()>,
) -> Result<(), String> {
    // A stopping daemon starts no new send.
    if super::stopping() {
        return Ok(());
    }
    let connection = open(state_dir)?;
    let rows: Vec<(String, String, String, i64, i64, String)> = {
        let mut query = connection
            .prepare("SELECT id, recipient, body, attempts, next_at, state FROM outbox WHERE state IN ('pending', 'sending') AND channel = 'agent' ORDER BY created_at, id")
            .map_err(|e| format!("{e}"))?;
        query
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .map_err(|e| format!("{e}"))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{e}"))?
    };
    drop(connection);
    let now = now_s();
    let mut seen = std::collections::BTreeSet::new();
    for (id, recipient, body, attempts, next_at, state) in rows {
        // FIFO per recipient: only the oldest open row may go, and a `sending` row (in flight) holds the rest back.
        // Only this thread marks rows `sending`, so a row read as `pending` has no send in flight and its `attempts` is current.
        if !seen.insert(recipient.clone()) || state != "pending" || next_at > now {
            continue;
        }
        {
            let mut flying = flying.lock().map_err(|_| "outbox lock poisoned")?;
            if flying.len() >= PARALLEL {
                return Ok(());
            }
            if !flying.insert(recipient.clone()) {
                continue;
            }
        }
        let marked = write(
            state_dir,
            Write {
                preconditions: vec![super::commit::Precondition {
                    sql: "SELECT EXISTS (SELECT 1 FROM outbox WHERE id = ?1 AND state = 'pending' AND next_at <= ?2)"
                        .into(),
                    params: vec![id.clone().into(), now.into()],
                }],
                statements: vec![super::commit::statement(
                    "UPDATE outbox SET state = 'sending', attempts = attempts + 1 WHERE id = ?1",
                    vec![id.clone().into()],
                )],
            },
        );
        if !matches!(marked, Ok(Outcome::Committed)) {
            flying
                .lock()
                .map_err(|_| "outbox lock poisoned")?
                .remove(&recipient);
            marked?;
            continue;
        }
        let (settings, dir, flying_now, wake) = (
            settings.clone(),
            state_dir.to_owned(),
            flying.clone(),
            wake.clone(),
        );
        let (sent_id, sent_to) = (id.clone(), recipient.clone());
        let spawned = std::thread::Builder::new()
            .name("amhd-send".into())
            .spawn(move || {
                // Whatever happens below, the recipient leaves the in-flight set and the dispatcher wakes.
                let _done = Landed(flying_now, sent_to.clone(), wake);
                // Held at send time: dead-letter, as `amh tell` refuses messages to held agents. A panic anywhere here counts as a failed send.
                let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match crate::omnigent::held(&sent_to) {
                        Some(why) => Err(why),
                        // env.toml and config.toml as they are now, as the watcher reloads them each pass.
                        None => Ok(crate::omnigent::send(
                            &crate::settings::load().unwrap_or_else(|_| (*settings).clone()),
                            &sent_to,
                            &body,
                        )),
                    }
                }));
                super::crash_point("after-send");
                let result = match attempt {
                    Ok(Err(why)) => {
                        eprintln!(
                            "amhd: outbox {sent_id} to {sent_to}: held ({why}); dead-lettered"
                        );
                        store_result(
                            &dir,
                            &sent_id,
                            one(
                                "UPDATE outbox SET state = 'dead', last_error = ?2 WHERE id = ?1",
                                vec![
                                    sent_id.clone().into(),
                                    format!("held at send time: {why}").into(),
                                ],
                            ),
                        );
                        return;
                    }
                    Ok(Ok(result)) => result,
                    Err(_) => Err("send panicked".to_owned()),
                };
                finish(&dir, &sent_id, &sent_to, attempts + 1, result);
            });
        if let Err(error) = spawned {
            flying
                .lock()
                .map_err(|_| "outbox lock poisoned")?
                .remove(&recipient);
            store_result(
                state_dir,
                &id,
                one(
                    "UPDATE outbox SET state = 'pending', attempts = attempts - 1 WHERE id = ?1",
                    vec![id.clone().into()],
                ),
            );
            return Err(format!("amh: cannot start a send: {error}"));
        }
    }
    Ok(())
}

/// Removes a recipient from the in-flight set when its send thread ends, even by panic.
struct Landed(Flying, String, std::sync::mpsc::Sender<()>);

impl Drop for Landed {
    fn drop(&mut self) {
        if let Ok(mut flying) = self.0.lock() {
            flying.remove(&self.1);
        }
        let _ = self.2.send(());
    }
}

/// Store a send's result, retrying until it commits so a sent message is never sent again.
pub(super) fn finish(
    state_dir: &Path,
    id: &str,
    recipient: &str,
    attempts: i64,
    result: Result<(), String>,
) {
    let update = match &result {
        Ok(()) => one(
            "UPDATE outbox SET state = 'delivered', last_error = '', first_sent_at = strftime('%Y-%m-%dT%H:%M:%fZ') WHERE id = ?1",
            vec![id.to_owned().into()],
        ),
        Err(error) if attempts >= ATTEMPTS_MAX && !transient(error) => one(
            "UPDATE outbox SET state = 'dead', last_error = ?2 WHERE id = ?1",
            vec![id.to_owned().into(), error.clone().into()],
        ),
        Err(error) => one(
            "UPDATE outbox SET state = 'pending', last_error = ?2, next_at = ?3 WHERE id = ?1",
            vec![
                id.to_owned().into(),
                error.clone().into(),
                (now_s() + (1i64 << attempts.clamp(0, 9)).min(600)).into(),
            ],
        ),
    };
    if let Err(error) = &result {
        eprintln!("amhd: outbox {id} to {recipient} failed (attempt {attempts}): {error}");
    }
    store_result(state_dir, id, update);
}

/// Store a send's result, retrying until it commits (so a sent message is never sent again) or the writer has stopped.
fn store_result(state_dir: &Path, id: &str, update: Write) {
    let mut wait = Duration::from_millis(50);
    loop {
        let statements = update
            .statements
            .iter()
            .map(|s| super::commit::statement(&s.sql, s.params.clone()))
            .collect();
        match write(
            state_dir,
            Write {
                preconditions: vec![],
                statements,
            },
        ) {
            Ok(_) => return,
            Err(error) if error.contains("commit thread has stopped") => {
                eprintln!(
                    "amhd: outbox {id}: result lost, the writer has stopped; it is resent after restart"
                );
                return;
            }
            Err(error) => {
                eprintln!("amhd: outbox {id}: cannot store the result, retrying: {error}");
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_secs(5));
            }
        }
    }
}

/// A failure that says nothing about the message: the agent is paused or held for quota, no start slot, its runner is offline, or Omnigent is unreachable. Such rows keep retrying (backoff at most 10 min) and never become dead letters, as in the watcher's queue.
fn transient(error: &str) -> bool {
    error.contains("keep requests pending")
        || error == crate::omnigent::NO_SLOT
        || error.contains("runner")
        || error.contains("onnect")
        || error.contains("timed out")
        || error.contains("Omnigent answered 5")
}

/// This daemon run; a request claimed by an earlier run that never stored its outcome is run again.
fn boot() -> &'static str {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| {
        format!(
            "{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
    })
}

/// The stored outcome of request `id`, or `None` when this call should run it; a request running in this daemon run is waited for, and one an earlier run claimed but never finished is taken over and run again.
pub fn claim(state_dir: &Path, id: &str) -> Result<Option<(u8, String, String)>, String> {
    let inserted = write(
        state_dir,
        Write {
            preconditions: vec![super::commit::Precondition {
                sql: "SELECT NOT EXISTS (SELECT 1 FROM request_outcomes WHERE id = ?1)".into(),
                params: vec![id.to_owned().into()],
            }],
            statements: {
                let mut statements = vec![super::commit::statement(
                    "INSERT INTO request_outcomes (id, boot) VALUES (?1, ?2)",
                    vec![id.to_owned().into(), boot().to_owned().into()],
                )];
                // Day-old outcomes go at most once a minute: the delete scanned ~18 MB of stored replies in every request's write (17 ms in the writer).
                static PRUNED: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
                let now_s = chrono::Utc::now().timestamp();
                if now_s - PRUNED.load(std::sync::atomic::Ordering::Relaxed) >= 60 {
                    PRUNED.store(now_s, std::sync::atomic::Ordering::Relaxed);
                    statements.push(super::commit::statement(
                        "DELETE FROM request_outcomes WHERE at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 day')",
                        vec![],
                    ));
                }
                statements
            },
        },
    )?;
    if inserted == Outcome::Committed {
        return Ok(None);
    }
    let connection = open(state_dir)?;
    for _ in 0..600 {
        let stored: Option<(Option<i64>, String, String, String)> = connection
            .query_row(
                "SELECT code, out, err, boot FROM request_outcomes WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| format!("amh: {e}"))?;
        match stored {
            Some((Some(code), out, err, _)) => return Ok(Some((code as u8, out, err))),
            Some((None, _, _, claimed)) if claimed != boot() => {
                let taken = write(state_dir, Write {
                    preconditions: vec![super::commit::Precondition {
                        sql: "SELECT EXISTS (SELECT 1 FROM request_outcomes WHERE id = ?1 AND code IS NULL AND boot = ?2)".into(),
                        params: vec![id.to_owned().into(), claimed.into()],
                    }],
                    statements: vec![super::commit::statement("UPDATE request_outcomes SET boot = ?2, at = strftime('%Y-%m-%dT%H:%M:%fZ') WHERE id = ?1", vec![id.to_owned().into(), boot().to_owned().into()])],
                })?;
                if taken == Outcome::Committed {
                    return Ok(None);
                }
            }
            // Pruned while waiting: claim it afresh.
            None => {
                let fresh = write(
                    state_dir,
                    Write {
                        preconditions: vec![super::commit::Precondition {
                            sql: "SELECT NOT EXISTS (SELECT 1 FROM request_outcomes WHERE id = ?1)"
                                .into(),
                            params: vec![id.to_owned().into()],
                        }],
                        statements: vec![super::commit::statement(
                            "INSERT INTO request_outcomes (id, boot) VALUES (?1, ?2)",
                            vec![id.to_owned().into(), boot().to_owned().into()],
                        )],
                    },
                )?;
                if fresh == Outcome::Committed {
                    return Ok(None);
                }
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("amh: request {id} is still running"))
}

/// Keep the outcome of request `id` for its retries.
pub fn record(state_dir: &Path, id: &str, code: u8, out: &str, err: &str) -> Result<(), String> {
    write(
        state_dir,
        one(
            "UPDATE request_outcomes SET code = ?2, out = ?3, err = ?4 WHERE id = ?1",
            vec![
                id.to_owned().into(),
                i64::from(code).into(),
                out.to_owned().into(),
                err.to_owned().into(),
            ],
        ),
    )
    .map(|_| ())
}
