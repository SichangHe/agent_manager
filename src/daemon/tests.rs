use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::commit::{Outcome, Precondition, Queue, Write};
use super::{edits, migrate};
use crate::database::{Database, Statement};
use crate::store::Store;

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

fn folder(label: &str) -> std::io::Result<PathBuf> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("amhd-{label}-{}-{now}", std::process::id()));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn statement(sql: &str, id: i64) -> Statement {
    Statement {
        sql: sql.into(),
        params: vec![id.into()],
    }
}

/// Two inserts that must land together.
fn pair(id: i64) -> Write {
    Write {
        preconditions: vec![],
        statements: vec![
            statement("INSERT INTO a (id) VALUES (?1)", id),
            statement("INSERT INTO b (id) VALUES (?1)", id),
        ],
    }
}

fn pairs_db(path: &Path) -> std::result::Result<(), rusqlite::Error> {
    Connection::open(path)?.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS a (id INTEGER PRIMARY KEY); CREATE TABLE IF NOT EXISTS b (id INTEGER PRIMARY KEY);",
    )
}

#[test]
fn commit_queue_batches_and_isolates_refused_writes() -> Result {
    let dir = folder("queue")?;
    let path = dir.join("db.sqlite3");
    pairs_db(&path)?;
    let (committed, commits) = mpsc::channel();
    let (queue, _thread) = Queue::start(&path, committed)?;
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let queue = queue.clone();
            std::thread::spawn(move || {
                (0..50)
                    .map(|i| queue.submit(pair(t * 1000 + i)))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for thread in threads {
        for outcome in thread.join().map_err(|_| "thread panicked")? {
            assert_eq!(outcome?, Outcome::Committed);
        }
    }
    assert!(commits.try_recv().is_ok());
    let conflict = Write {
        preconditions: vec![Precondition {
            sql: "SELECT COUNT(*) = 0 FROM a".into(),
            params: vec![],
        }],
        ..pair(1)
    };
    assert_eq!(queue.submit(conflict)?, Outcome::Conflict);
    // A duplicate id fails its second statement; the first must roll back with it.
    let failing = Write {
        preconditions: vec![],
        statements: vec![
            statement("INSERT INTO a (id) VALUES (?1)", 9999),
            statement("INSERT INTO b (id) VALUES (?1)", 0),
        ],
    };
    assert!(queue.submit(failing).is_err());
    let oversize = Write {
        preconditions: vec![],
        statements: (0..65)
            .map(|i| statement("INSERT INTO a (id) VALUES (?1)", 20_000 + i))
            .collect(),
    };
    // An over-size write is taken whole, as one job: every write goes through the one writer.
    assert_eq!(queue.submit(oversize)?, Outcome::Committed);
    let connection = Connection::open(&path)?;
    let count = |table: &str| {
        connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
            r.get::<_, i64>(0)
        })
    };
    assert_eq!((count("a")?, count("b")?), (465, 400));
    assert!(queue.wait_p99_us() < 1_000_000);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

/// Child of `commit_queue_keeps_every_acknowledged_write_across_kill_9`: submit pairs forever, printing each id once acknowledged.
#[test]
#[ignore = "run only as the child of the crash test"]
fn crash_child() -> Result {
    let path = PathBuf::from(std::env::var("AMHD_CRASH_DB")?);
    let start: i64 = std::env::var("AMHD_CRASH_START")?.parse()?;
    let (committed, _commits) = mpsc::channel();
    let (queue, _thread) = Queue::start(&path, committed)?;
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let queue = queue.clone();
            std::thread::spawn(move || {
                for i in 0.. {
                    let id = start + t * 10_000_000 + i;
                    if queue.submit(pair(id)) == Ok(Outcome::Committed) {
                        println!("acked {id}");
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        let _ = thread.join();
    }
    Ok(())
}

#[test]
fn commit_queue_keeps_every_acknowledged_write_across_kill_9() -> Result {
    let dir = folder("crash")?;
    let path = dir.join("db.sqlite3");
    pairs_db(&path)?;
    let mut acked = Vec::new();
    for round in 0..5i64 {
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "daemon::tests::crash_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("AMHD_CRASH_DB", &path)
            .env("AMHD_CRASH_START", (round * 100_000_000).to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (lines, received) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::io::Result::ok)
            {
                if let Some(id) = line
                    .strip_prefix("acked ")
                    .and_then(|id| id.parse::<i64>().ok())
                {
                    let _ = lines.send(id);
                }
            }
        });
        let kill_at = Instant::now() + Duration::from_millis(150 + 70 * round as u64);
        while Instant::now() < kill_at {
            if let Ok(id) = received.recv_timeout(Duration::from_millis(5)) {
                acked.push(id);
            }
        }
        child.kill()?;
        child.wait()?;
        acked.extend(received.try_iter());
    }
    assert!(!acked.is_empty(), "the child acknowledged nothing");
    let connection = Connection::open(&path)?;
    for id in &acked {
        let present: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM a WHERE id = ?1) AND EXISTS(SELECT 1 FROM b WHERE id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        assert!(present, "acknowledged write {id} was lost");
    }
    let torn: i64 = connection.query_row(
        "SELECT (SELECT COUNT(*) FROM a WHERE id NOT IN (SELECT id FROM b)) + (SELECT COUNT(*) FROM b WHERE id NOT IN (SELECT id FROM a))",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(torn, 0, "a write was half applied");
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

fn versions(connection: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut query = connection.prepare("SELECT version FROM schema_version ORDER BY version")?;
    query.query_map([], |r| r.get(0))?.collect()
}

#[test]
fn migration_is_atomic_idempotent_and_keeps_old_tables() -> Result {
    let dir = folder("migrate")?;
    let path = dir.join("amh.sqlite3");
    let mut database = Database::open(&path)?;
    database.commit(
        &[("t.md".into(), "text".into())],
        &[serde_json::json!({"time": "now", "actor": "x", "kind": "message queued", "subject": "t.md", "data": {"to": "w", "text": "hi", "extra": 1}})],
    )?;
    drop(database);
    // A failing step must leave nothing applied: a clashing table makes V1 fail midway.
    let mut connection = Connection::open(&path)?;
    connection.execute_batch("CREATE TABLE leases (x)")?;
    assert!(migrate::migrate(&mut connection).is_err());
    assert!(connection.prepare("SELECT version FROM tasks").is_err());
    assert!(connection.prepare("SELECT 1 FROM outbox").is_err());
    connection.execute_batch("DROP TABLE leases")?;
    migrate::migrate(&mut connection)?;
    migrate::migrate(&mut connection)?;
    assert_eq!(versions(&connection)?, vec![1, 2, 3, 4, 5]);
    // A newer binary's version is tolerated.
    connection.execute("INSERT INTO schema_version (version) VALUES (99)", [])?;
    migrate::migrate(&mut connection)?;
    let outbox: (String, String, String) =
        connection.query_row("SELECT recipient, body, state FROM outbox", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    assert_eq!(outbox, ("w".into(), "hi".into(), "dead".into()));
    drop(connection);
    // The old in-process path still works on the migrated schema.
    let mut database = Database::open(&path)?;
    database.commit(&[("t.md".into(), "newer".into())], &[])?;
    assert_eq!(database.task("t.md")?.as_deref(), Some("newer"));
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn state_files_import_once_even_when_rename_is_lost() -> Result {
    let dir = folder("state-files")?;
    let path = dir.join("amh.sqlite3");
    drop(Database::open(&path)?);
    let mut connection = Connection::open(&path)?;
    migrate::migrate(&mut connection)?;
    let queue = r#"[{"agent": "a.md", "future_field": true}, {"task": "b.md"}]"#;
    std::fs::write(dir.join("amh-starts.queue.json"), queue)?;
    std::fs::write(dir.join("amh-holds.json"), r#"{"s1": "held"}"#)?;
    let imported = migrate::import_state_files(&mut connection, &dir)?;
    assert_eq!(imported, vec!["amh-starts.queue.json", "amh-holds.json"]);
    assert!(dir.join("amh-holds.json.migrated").is_file() && !dir.join("amh-holds.json").exists());
    // A crash after the import committed but before the rename: the file is still there next start.
    std::fs::copy(
        dir.join("amh-starts.queue.json.migrated"),
        dir.join("amh-starts.queue.json"),
    )?;
    migrate::import_state_files(&mut connection, &dir)?;
    let jobs: Vec<String> = connection
        .prepare("SELECT agent FROM spawn_jobs ORDER BY agent")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(jobs, vec!["a.md", "b.md"]);
    let held: String = connection.query_row(
        "SELECT text FROM state_files WHERE name = 'amh-holds.json'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(held, r#"{"s1": "held"}"#);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

const TASK: &str = "---\nstatus: long_running\nrunat: \npending_task_items: []\n---\nfirst line\n";

/// A store with one exported task `t.md`, migrated for the daemon.
fn store_with_task(
    label: &str,
) -> std::result::Result<(Store, PathBuf), Box<dyn std::error::Error>> {
    let dir = folder(label)?;
    let store = Store {
        root: dir.join("records"),
        state_dir: dir.join("state"),
        machine: None,
    };
    std::fs::create_dir_all(&store.root)?;
    std::fs::create_dir_all(&store.state_dir)?;
    std::fs::write(store.root.join("TODO.md"), "current:\nt.md\n\nprevious:\n")?;
    std::fs::write(store.root.join("t.md"), TASK)?;
    drop(store.lock()?);
    // Exports are lazy: write the files (and their export records) once, as the watcher would.
    store.sync_files()?;
    migrate::migrate(&mut Connection::open(store.state_dir.join("amh.sqlite3"))?)?;
    Ok((store, dir))
}

#[test]
fn exporter_skips_a_task_while_its_edit_import_is_open() -> Result {
    let (store, dir) = store_with_task("export-skip")?;
    let file = store.root.join("t.md");
    let typed = format!("{TASK}typed by the human\n");
    std::fs::write(&file, &typed)?;
    let connection = Connection::open(store.state_dir.join("amh.sqlite3"))?;
    connection.execute(
        "INSERT INTO edit_import (task, base_version, chunks) VALUES ('t.md', 0, '[]')",
        [],
    )?;
    connection.execute(
        "UPDATE tasks SET text = ?1 WHERE name = 't.md'",
        [format!("{TASK}half imported\n")],
    )?;
    store.refresh_exports()?;
    assert_eq!(std::fs::read_to_string(&file)?, typed);
    connection.execute("UPDATE edit_import SET state = 'closed'", [])?;
    store.refresh_exports()?;
    // The typed line is still not in the committed text, so the exporter keeps the file for the import (revive's no-clobber rule).
    assert_eq!(std::fs::read_to_string(&file)?, typed);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn typed_edit_imports_through_the_queue_and_resumes_after_a_crash() -> Result {
    let (store, dir) = store_with_task("edit-import")?;
    let path = store.state_dir.join("amh.sqlite3");
    let (committed, _commits) = mpsc::channel();
    let (queue, _thread) = Queue::start(&path, committed)?;
    std::fs::write(store.root.join("t.md"), format!("{TASK}typed line\n"))?;
    let database = edits::initialized(&store)?;
    edits::import(&store, &database, &queue, "t.md", None)?;
    assert!(
        database
            .task("t.md")?
            .ok_or("no task")?
            .contains("typed line")
    );
    assert!(database.open_imports()?.is_empty());
    let version: i64 =
        database
            .sql()
            .query_row("SELECT version FROM tasks WHERE name = 't.md'", [], |r| {
                r.get(0)
            })?;
    assert_eq!(version, 1);
    // A crash after the row opened but before its chunk applied: the restart resumes it.
    let chunk = serde_json::json!([[{"sql": "UPDATE tasks SET text = text || ?1 WHERE name = 't.md'", "params": ["resumed\n"], "unknown": 1}]]);
    database.sql().execute(
        "INSERT INTO edit_import (task, base_version, chunks) VALUES ('t.md', 1, ?1)",
        [chunk.to_string()],
    )?;
    edits::resume_all(&database, &queue)?;
    assert!(
        database
            .task("t.md")?
            .ok_or("no task")?
            .ends_with("resumed\n")
    );
    assert!(database.open_imports()?.is_empty());
    // Resuming again applies nothing twice.
    edits::resume_all(&database, &queue)?;
    assert_eq!(
        database
            .task("t.md")?
            .ok_or("no task")?
            .matches("resumed")
            .count(),
        1
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn a_retried_request_gets_the_stored_outcome() -> Result {
    let (store, dir) = store_with_task("request-ids")?;
    assert_eq!(super::outbox::claim(&store.state_dir, "r1")?, None);
    super::outbox::record(&store.state_dir, "r1", 0, "done\n", "")?;
    assert_eq!(
        super::outbox::claim(&store.state_dir, "r1")?,
        Some((0, "done\n".to_owned(), String::new()))
    );
    assert_eq!(super::outbox::claim(&store.state_dir, "r2")?, None);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn a_request_claimed_by_an_earlier_daemon_run_is_run_again() -> Result {
    let (store, dir) = store_with_task("request-boot")?;
    let connection = Connection::open(store.state_dir.join("amh.sqlite3"))?;
    connection.execute(
        "INSERT INTO request_outcomes (id, boot) VALUES ('old', 'crashed-run')",
        [],
    )?;
    assert_eq!(super::outbox::claim(&store.state_dir, "old")?, None);
    let boot: String = connection.query_row(
        "SELECT boot FROM request_outcomes WHERE id = 'old'",
        [],
        |r| r.get(0),
    )?;
    assert_ne!(boot, "crashed-run");
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn outbox_failures_back_off_then_dead_letter() -> Result {
    let (store, dir) = store_with_task("outbox-dead")?;
    let id = super::outbox::enqueue(&store.state_dir, "w", "hi", "t.md")?;
    let connection = Connection::open(store.state_dir.join("amh.sqlite3"))?;
    let state = |c: &Connection| -> rusqlite::Result<(String, String, i64)> {
        c.query_row(
            "SELECT state, last_error, next_at FROM outbox WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    };
    super::outbox::finish(&store.state_dir, &id, "w", 1, Err("down".into()));
    let (s, e, next) = state(&connection)?;
    assert_eq!((s.as_str(), e.as_str()), ("pending", "down"));
    assert!(next > chrono::Utc::now().timestamp());
    super::outbox::finish(&store.state_dir, &id, "w", 8, Err("still down".into()));
    assert_eq!(state(&connection)?.0, "dead");
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
