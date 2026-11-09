//! Scheduled jobs and always-there agents named in the config, run by the watcher.
// 🧑 "allow some sort of scripts or commands that should be run like a cron job, but is tied to AMH server ... some sort of method to define long running agents that are always there"
//!
//! `[job.NAME]`: `every = "15m"` or `at = "04:00"` (local time, daily), then `run = "COMMAND"` (with `dir` optional) or `tell = "TASK"` with `text`.
//! `[keep.TASK]`: `dir` and `prompt` (a file holding the goal), optionally `tool`; the agent of `TASK.md` is started when it is missing.
//! When a job is due is decided by `decide::job_due`; this module does the clock, the processes, and the record.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Local, NaiveTime};

use crate::agents::{self, NewTask};
use crate::store::Store;
use crate::{decide, omnigent, settings, tasks, work};

type Settings = BTreeMap<String, String>;

/// The SQLite record of when each job last ran: `NAME EPOCH` per line.
const RECORD: &str = "amh-jobs.txt";
const KEEP_EVERY: Duration = Duration::from_secs(60);

/// What the watcher remembers about jobs between polls.
#[derive(Default)]
pub struct Jobs {
    running: BTreeMap<String, Child>,
    last: Option<BTreeMap<String, u64>>,
    last_keep: Option<Instant>,
    /// Agents found missing at the last check; one is restarted only when it is missing twice in a row.
    missing: Vec<String>,
}

/// Today's local `HH:MM` in seconds since the epoch.
fn at_today_s(at: &str) -> Option<u64> {
    let time = NaiveTime::parse_from_str(at, "%H:%M").ok()?;
    let moment = Local::now().date_naive().and_time(time);
    u64::try_from(moment.and_local_timezone(Local).earliest()?.timestamp()).ok()
}

impl Jobs {
    /// Reap finished jobs, start the due ones, and keep the always-there agents; returns lines for the watcher's log.
    pub fn tick(&mut self, settings: &Settings, store: &Store) -> Vec<String> {
        let mut lines = Vec::new();
        self.running.retain(|name, child| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                lines.push(format!("job {name} ended: {status}"));
                false
            }
            Err(error) => {
                lines.push(format!("job {name} lost: {error}"));
                false
            }
        });
        let mut last = match self.last.take() {
            Some(last) => last,
            None => match store.record(RECORD) {
                Ok(text) => text
                    .lines()
                    .filter_map(|line| line.split_once(' '))
                    .filter_map(|(name, at)| Some((name.to_owned(), at.parse().ok()?)))
                    .collect(),
                Err(error) => {
                    lines.push(format!("cannot read {RECORD}: {error}"));
                    return lines;
                }
            },
        };
        let now_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let mut changed = false;
        for name in decide::table_names(settings, "job") {
            let key = |field: &str| settings::get(settings, &format!("job.{name}.{field}"));
            let (every, at) = (decide::span_s(key("every")), at_today_s(key("at")));
            if every.is_none() && at.is_none() {
                continue;
            }
            let seen = last.get(&name).copied();
            let due = decide::job_due(every, at, seen, now_s);
            if due && self.running.contains_key(&name) {
                continue;
            }
            if due {
                lines.push(match self.start(settings, store, &name) {
                    Ok(()) => format!("job {name} started"),
                    Err(error) => format!("job {name} failed to start: {error}"),
                });
            }
            if due || seen.is_none() {
                last.insert(name, now_s);
                changed = true;
            }
        }
        if changed {
            let text: String = last
                .iter()
                .map(|(name, at)| format!("{name} {at}\n"))
                .collect();
            if let Err(error) = store.set_record(RECORD, &text) {
                lines.push(format!("cannot write {RECORD}: {error}"));
            }
        }
        self.last = Some(last);
        if self
            .last_keep
            .is_none_or(|last| last.elapsed() > KEEP_EVERY)
        {
            self.last_keep = Some(Instant::now());
            self.keep(settings, store, &mut lines);
        }
        lines
    }

    /// Run one job: its command in the background with its output in `job-NAME.log`, or its message to an agent.
    fn start(&mut self, settings: &Settings, store: &Store, name: &str) -> Result<(), String> {
        let key = |field: &str| settings::get(settings, &format!("job.{name}.{field}"));
        if !key("tell").is_empty() {
            let to = agents::address_of(store, &work::task_name(key("tell")))?;
            let message = work::envelope(&format!("job {name}"), key("text"));
            return omnigent::send(settings, &to, &message);
        }
        if key("run").is_empty() {
            return Err("it has neither `run` nor `tell`".to_owned());
        }
        let failed = |error: std::io::Error| error.to_string();
        let output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(store.state_dir.join(format!("job-{name}.log")))
            .map_err(failed)?;
        let mut command = Command::new("sh");
        command
            .args(["-c", key("run")])
            .env("AMH_JOB", name)
            .stdin(Stdio::null())
            .stdout(output.try_clone().map_err(failed)?)
            .stderr(output);
        if !key("dir").is_empty() {
            command.current_dir(key("dir"));
        }
        self.running
            .insert(name.to_owned(), command.spawn().map_err(failed)?);
        Ok(())
    }

    /// Start the agent of every `[keep.TASK]` that was missing at two checks in a row.
    fn keep(&mut self, settings: &Settings, store: &Store, lines: &mut Vec<String>) {
        let mut missing = Vec::new();
        for task in decide::table_names(settings, "keep") {
            let name = work::task_name(&task);
            let alive = store
                .load(&name)
                .is_ok_and(|task| agents::has_live_agent(settings, &task));
            if alive {
                continue;
            }
            if !self.missing.contains(&name) {
                missing.push(name);
                continue;
            }
            lines.push(match revive(settings, store, &task, &name) {
                Ok(address) => format!("always-there agent {name} started at {address}"),
                Err(error) => format!("always-there agent {name} could not start: {error}"),
            });
        }
        self.missing = missing;
    }
}

/// Start the agent of `[keep.TASK]` under the main manager and mark its task long-running.
fn revive(settings: &Settings, store: &Store, table: &str, name: &str) -> Result<String, String> {
    let key = |field: &str| settings::get(settings, &format!("keep.{table}.{field}"));
    let old = if store.contains(name)? {
        Some(store.load(name)?)
    } else {
        None
    };
    let as_manager = old
        .as_ref()
        .is_some_and(|task| task.field("is_manager") == Some("true"));
    let manager = old
        .as_ref()
        .and_then(|task| task.field("managerat"))
        .unwrap_or(settings::get(settings, "main_manager"));
    let goal = fs::read_to_string(key("prompt"))
        .map_err(|error| format!("cannot read its prompt {}: {error}", key("prompt")))?;
    let new = NewTask {
        name,
        workdir: key("dir"),
        goal: &goal,
        manager,
        tool: Some(key("tool")),
        model: None,
        effort: None,
        as_manager,
        long_running: true,
        email: None,
        lines: None,
        priority: None,
    };
    let address = agents::start_task(settings, store, &new)?;
    let _lock = store.lock()?;
    let mut task = store.load(name)?;
    let blocked_on = task.field("blocked_on").map(str::to_owned);
    tasks::set_status(&mut task, "long_running", blocked_on.as_deref())?;
    store.save(&task)?;
    store.place(&task)?;
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_runs_and_records_attempt_across_watcher_restart()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "amh-jobs-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&dir)?;
        let store = Store {
            root: dir.clone(),
            state_dir: dir.clone(),
        };
        let settings = [
            ("job.check.every".to_owned(), "1h".to_owned()),
            (
                "job.check.run".to_owned(),
                "printf '%s' \"$AMH_JOB\"".to_owned(),
            ),
        ]
        .into_iter()
        .collect();
        let mut jobs = Jobs::default();
        assert_eq!(jobs.tick(&settings, &store), ["job check started"]);
        let mut child = jobs
            .running
            .remove("check")
            .ok_or("command was not started")?;
        assert!(child.wait()?.success());
        assert_eq!(fs::read_to_string(dir.join("job-check.log"))?, "check");
        assert!(store.record(RECORD)?.starts_with("check "));
        assert!(!dir.join(RECORD).exists());
        assert!(Jobs::default().tick(&settings, &store).is_empty());
        let legacy = dir.join("legacy");
        fs::create_dir(&legacy)?;
        fs::write(legacy.join(RECORD), store.record(RECORD)?)?;
        let migrated = Store {
            root: legacy.clone(),
            state_dir: legacy.clone(),
        };
        assert!(Jobs::default().tick(&settings, &migrated).is_empty());
        assert!(!legacy.join(RECORD).exists());
        assert_eq!(store.record(RECORD)?, migrated.record(RECORD)?);
        assert!(!legacy.join("job-check.log").exists());
        let failed = dir.join("failed");
        fs::create_dir(&failed)?;
        fs::create_dir(failed.join("amh.sqlite3"))?;
        let unavailable = Store {
            root: failed.clone(),
            state_dir: failed,
        };
        assert!(Jobs::default().tick(&settings, &unavailable)[0].starts_with("cannot read"));
        assert!(!unavailable.state_dir.join("job-check.log").exists());
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
