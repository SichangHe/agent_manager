//! SQLite records with recoverable markdown exports, under the one lock every writer takes.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use crate::tasks::Task;
use crate::{database::Database, log, omnigent, requests, settings, taskfile};

/// The work-log root with the lock every writer of it takes.
pub struct Store {
    pub root: PathBuf,
    pub state_dir: PathBuf,
}

/// A wait for the lock at least this long is logged with the holder's name.
const LONG_LOCK_WAIT_S: u64 = 5;

impl Store {
    pub fn new(values: &BTreeMap<String, String>) -> Self {
        Self {
            root: settings::work_logs(values),
            state_dir: settings::state_dir(),
        }
    }

    /// Serialize every writer of the root through one lock file, held until the returned file is dropped.
    pub fn lock(&self) -> Result<File, String> {
        let failed = |error: std::io::Error| {
            format!("amh: cannot lock {}: {error}", self.state_dir.display())
        };
        fs::create_dir_all(&self.state_dir).map_err(failed)?;
        let path = self.state_dir.join("amh.lock");
        let mut lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(failed)?;
        if lock.try_lock().is_err() {
            // The lock file names its holder, so a long wait can be traced to the run that caused it.
            let holder = fs::read_to_string(&path).unwrap_or_default();
            let started = std::time::Instant::now();
            lock.lock().map_err(failed)?;
            let waited_s = started.elapsed().as_secs();
            if waited_s >= LONG_LOCK_WAIT_S {
                let line = format!(
                    "{} waited {waited_s} s for the amh lock held by: {}\n",
                    chrono::Local::now().format("%F %T"),
                    holder.trim()
                );
                eprint!("amh: {line}");
                if let Ok(mut waits) = fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(self.state_dir.join("amh-lock-waits.log"))
                {
                    let _ = waits.write_all(line.as_bytes());
                }
            }
        }
        let me: Vec<String> = std::env::args().take(4).collect();
        lock.set_len(0).map_err(failed)?;
        lock.write_all(format!("pid {} {}", std::process::id(), me.join(" ")).as_bytes())
            .map_err(failed)?;
        let mut database = Database::open(&self.state_dir.join("amh.sqlite3"))?;
        if !database.initialized()? {
            self.recover_transfer()?;
            let mut tasks = self.markdown_tasks()?;
            for task in &mut tasks {
                let mut records = requests::read(task)?;
                let mut changed = false;
                for request in &mut records {
                    if request.stage == requests::Stage::Resolved && request.resolved_at.is_none() {
                        request.resolved_at = Some(chrono::Utc::now().to_rfc3339());
                        changed = true;
                    }
                }
                if changed {
                    requests::write(task, &records);
                }
            }
            let mut snapshots: Vec<_> = tasks
                .iter()
                .map(|task| (task.name.clone(), task.render()))
                .collect();
            match fs::read_to_string(self.root.join("TODO.md")) {
                Ok(text) => snapshots.push(("TODO.md".to_owned(), text)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("amh: cannot import TODO.md: {error}")),
            }
            let mut events = Vec::new();
            events.push(log::event_value(
                "markdown imported",
                "work logs",
                serde_json::json!({"legacy_resolution_dates":"retention starts at migration"}),
            ));
            match fs::read_to_string(self.state_dir.join("amh-events.jsonl")) {
                Ok(text) => {
                    for line in text.lines().filter(|line| !line.trim().is_empty()) {
                        events.push(
                            serde_json::from_str(line)
                                .map_err(|error| format!("amh: invalid legacy event: {error}"))?,
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("amh: cannot import events: {error}")),
            }
            for task in tasks {
                for notification in requests::notifications(&task)? {
                    if !events.iter().any(|event: &serde_json::Value| {
                        event["kind"] == "message queued"
                            && event["subject"] == task.name
                            && event["data"] == notification
                    }) {
                        events.push(log::event_value("message queued", &task.name, notification));
                    }
                }
            }
            database.import(&snapshots, &events)?;
        }
        self.take_in_edits(&database)?;
        if database.record("pending-block-queue-v1")?.is_none() {
            let mut snapshots = Vec::new();
            let mut events = Vec::new();
            for (name, text) in database.tasks()? {
                if name == "TODO.md" {
                    continue;
                }
                let task = Task::parse(&name, &text)?;
                if !taskfile::pending_blocks(&task.body).is_empty() {
                    let (task, queued) = self.prepare(&task)?;
                    snapshots.push((name, task.render()));
                    events.extend(queued);
                }
            }
            database.commit(&snapshots, &events)?;
            database.set_record("pending-block-queue-v1", "done")?;
        }
        self.export(&database)?;
        Ok(lock)
    }

    /// Keep body lines someone typed into an exported task file, which the next export would overwrite.
    ///
    /// A typed line is one in the file's body that is not in the text amh last exported there; it is appended to the task's body in the database.
    // lost_work_mgr.md: "amh task start rewrote vd_lit_venues.md's body and dropped the added opc line"
    fn take_in_edits(&self, database: &Database) -> Result<(), String> {
        for (name, text) in database.tasks()? {
            let Ok(file) = fs::read_to_string(self.root.join(&name)) else {
                continue;
            };
            let Some(last) = database.record(&format!("export:{name}"))? else {
                continue;
            };
            if file == text || file == last || name == "TODO.md" {
                continue;
            }
            let (Ok(typed), Ok(exported)) = (Task::parse(&name, &file), Task::parse(&name, &last))
            else {
                continue;
            };
            let mut known: Vec<&str> = exported.body.lines().collect();
            let added: Vec<&str> = typed
                .body
                .lines()
                .filter(|line| match known.iter().position(|seen| seen == line) {
                    Some(at) => {
                        known.remove(at);
                        false
                    }
                    None => true,
                })
                .collect();
            if added.iter().all(|line| line.trim().is_empty()) {
                continue;
            }
            let mut task = Task::parse(&name, &text)?;
            task.body = format!(
                "{}\n{}\n",
                task.body.trim_end_matches('\n'),
                added.join("\n")
            );
            self.save(&task)?;
        }
        Ok(())
    }

    fn database(&self) -> Result<Option<Database>, String> {
        let path = self.state_dir.join("amh.sqlite3");
        if !path.exists() {
            return Ok(None);
        }
        let database = Database::open(&path)?;
        Ok(database.initialized()?.then_some(database))
    }

    /// Regenerate exports on every lock, repairing an interrupted export from committed state.
    fn export(&self, database: &Database) -> Result<(), String> {
        for (name, text) in database.tasks()? {
            if fs::read_to_string(self.root.join(&name)).ok().as_deref() != Some(&text) {
                taskfile::write(&self.root.join(&name), &text)
                    .map_err(|error| format!("amh: cannot export {name}: {error}"))?;
            }
            let key = format!("export:{name}");
            if database.record(&key)?.as_deref() != Some(&text) {
                database.set_record(&key, &text)?;
            }
        }
        let mut text =
            "# AMH events\n\n(authored by agents unless marked 🧑)\n\n```json\n".to_owned();
        for event in database.events()? {
            text.push_str(&event.to_string());
            text.push('\n');
        }
        text.push_str("```\n");
        let folder = self.root.join("amh_views");
        fs::create_dir_all(&folder)
            .map_err(|error| format!("amh: cannot create view directory: {error}"))?;
        let path = folder.join("events.md");
        if fs::read_to_string(&path).ok().as_deref() != Some(&text) {
            taskfile::write(&path, &text)
                .map_err(|error| format!("amh: cannot export events: {error}"))?;
        }
        Ok(())
    }

    pub fn refresh_exports(&self) -> Result<(), String> {
        if let Some(database) = self.database()? {
            self.export(&database)?;
        }
        Ok(())
    }

    pub fn contains(&self, name: &str) -> Result<bool, String> {
        match self.database()? {
            Some(database) => Ok(database.task(name)?.is_some()),
            None => Ok(self.root.join(name).is_file()),
        }
    }

    /// Finish an interrupted two-file ownership transfer before allowing another writer.
    fn recover_transfer(&self) -> Result<(), String> {
        let path = self.state_dir.join("amh-transfer.json");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("amh: cannot read transfer recovery: {e}")),
        };
        let records: Vec<(String, String)> = serde_json::from_str(&text)
            .map_err(|e| format!("amh: invalid transfer recovery: {e}"))?;
        for (name, text) in records {
            let task = Task::parse(&name, &text)?;
            self.save(&task)?;
        }
        fs::remove_file(path).map_err(|e| format!("amh: cannot finish transfer recovery: {e}"))
    }

    /// Preserve both markdown after-images before replacing either file, under the store lock.
    pub fn save_transfer(&self, source: &Task, to: &Task) -> Result<(), String> {
        if let Some(mut database) = self.database()? {
            let (to, mut events) = self.prepare(to)?;
            let (source, source_events) = self.prepare(source)?;
            events.extend(source_events);
            let mut old = self.load(&source.name)?;
            let remaining = requests::read(&source)?;
            for request in requests::sync(&mut old)? {
                if !remaining.iter().any(|record| record.id == request.id) {
                    events.push(log::event_value(
                        "request moved",
                        &request.id,
                        serde_json::json!({"from":source.name,"to":to.name}),
                    ));
                }
            }
            database.commit(
                &[
                    (to.name.clone(), to.render()),
                    (source.name.clone(), source.render()),
                ],
                &events,
            )?;
            return self.export(&database);
        }
        let data = serde_json::to_string(&[
            (to.name.clone(), to.render()),
            (source.name.clone(), source.render()),
        ])
        .map_err(|e| format!("amh: cannot record transfer: {e}"))?;
        taskfile::write(&self.state_dir.join("amh-transfer.json"), &data)
            .map_err(|e| format!("amh: cannot record transfer recovery: {e}"))?;
        self.recover_transfer()
    }

    pub fn load(&self, name: &str) -> Result<Task, String> {
        let path = self.root.join(name);
        if name.contains('/') || !name.ends_with(".md") {
            return Err(format!("amh: no task file {name} in the work-log root"));
        }
        if let Some(database) = self.database()? {
            let text = database
                .task(name)?
                .ok_or_else(|| format!("amh: no task file {name} in the work-log root"))?;
            return Task::parse(name, &text).map_err(|message| format!("amh: {message}"));
        }
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("amh: cannot read {name}: {error}"))?;
        Task::parse(name, &text).map_err(|message| format!("amh: {message}"))
    }

    pub fn save(&self, task: &Task) -> Result<(), String> {
        let (task, events) = self.prepare(task)?;
        if let Some(mut database) = self.database()? {
            database.commit(&[(task.name.clone(), task.render())], &events)?;
            return self.export(&database);
        }
        self.write(&task.name, &task.render())?;
        for event in events {
            log::event(
                &self.state_dir,
                event["kind"].as_str().unwrap_or_default(),
                event["subject"].as_str().unwrap_or_default(),
                event["data"].clone(),
            )?;
        }
        Ok(())
    }

    /// Record mail disposition and queued notifications together before external mail deletion.
    pub fn receive_mail(&self, key: &str, task: &Task) -> Result<bool, String> {
        let mut database = self
            .database()?
            .ok_or_else(|| "amh: mail receipts require an initialized SQLite store".to_owned())?;
        let (task, events) = self.prepare(task)?;
        let received =
            database.receive_mail(key, &[(task.name.clone(), task.render())], &events)?;
        self.export(&database)?;
        Ok(received)
    }

    /// Commit a task together with one stored message of the human queued for its agent; with a receipt `key`, at most once.
    pub fn queue_mail(&self, key: Option<&str>, task: &Task, mail: &str) -> Result<bool, String> {
        let mut database = self
            .database()?
            .ok_or_else(|| "amh: queued messages require an initialized SQLite store".to_owned())?;
        let (task, mut events) = self.prepare(task)?;
        events.push(log::event_value(
            "message queued",
            &task.name,
            serde_json::json!({"channel": "human", "to": task.name, "text": mail}),
        ));
        let snapshots = [(task.name.clone(), task.render())];
        let queued = match key {
            Some(key) => database.receive_mail(key, &snapshots, &events)?,
            None => database.commit(&snapshots, &events).map(|()| true)?,
        };
        self.export(&database)?;
        Ok(queued)
    }

    /// The text of a small named record of the watcher, kept in the `records` table; empty when there is none.
    ///
    /// A record file of that name left in the state folder by an older version is taken over on first use.
    pub fn record(&self, name: &str) -> Result<String, String> {
        let database = Database::open(&self.state_dir.join("amh.sqlite3"))?;
        if let Some(text) = database.record(name)? {
            return Ok(text);
        }
        let legacy = self.state_dir.join(name);
        let text = match fs::read_to_string(&legacy) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("cannot import {name}: {error}")),
        };
        database.import_record(name, &text)?;
        let _ = fs::remove_file(&legacy);
        Ok(database.record(name)?.unwrap_or_default())
    }

    pub fn set_record(&self, name: &str, text: &str) -> Result<(), String> {
        Database::open(&self.state_dir.join("amh.sqlite3"))?.set_record(name, text)?;
        let _ = fs::remove_file(self.state_dir.join(name));
        Ok(())
    }

    /// Import a legacy file first, then atomically append without losing other writers' lines.
    pub fn append_record(&self, name: &str, text: &str) -> Result<(), String> {
        self.record(name)?;
        Database::open(&self.state_dir.join("amh.sqlite3"))?.append_record(name, text)
    }

    /// The IDs of stored human messages and typed/guest blocks queued for a task.
    pub fn queued_mail(&self, name: &str) -> Result<Vec<i64>, String> {
        let Some(database) = self.database()? else {
            return Ok(Vec::new());
        };
        let mut queued = database.pending_messages("human")?;
        queued.extend(database.pending_messages("block")?);
        Ok(queued
            .iter()
            .filter(|message| message["target"] == name)
            .filter_map(|message| message["id"].as_i64())
            .collect())
    }

    /// Give up delivering stored human messages and typed/guest blocks queued for a task.
    pub fn drop_queued_mail(&self, name: &str) -> Result<(), String> {
        let queued = self.queued_mail(name)?;
        if let Some(database) = self.database()? {
            for id in queued {
                database.finish_message(id, true, "")?;
                log::event(
                    &self.state_dir,
                    "message dropped",
                    name,
                    serde_json::json!({"id": id}),
                )?;
            }
        }
        Ok(())
    }

    fn prepare(&self, task: &Task) -> Result<(Task, Vec<serde_json::Value>), String> {
        let mut task = task.clone();
        let current = requests::sync(&mut task)?;
        requests::validate(&task)?;
        let old = match self.database()? {
            Some(database) => database
                .task(&task.name)?
                .map(|text| Task::parse(&task.name, &text))
                .transpose()?,
            None => self.load(&task.name).ok(),
        };
        let previous = match &old {
            Some(task) => requests::read(task)?,
            None => Vec::new(),
        };
        let mut events = Vec::new();
        for (_, block) in taskfile::pending_blocks(&task.body) {
            if let Some(first) = block.first() {
                task.body = crate::decide::without_marker(&task.body, first)
                    .ok_or("pending block disappeared while queuing")?;
                events.push(log::event_value("message queued", &task.name,
                    serde_json::json!({"channel": "block", "to": task.name, "text": block.join("\n")})));
            }
        }
        if old
            .as_ref()
            .is_none_or(|before| before.render() != task.render())
        {
            events.push(log::event_value(
                "task changed",
                &task.name,
                serde_json::json!({}),
            ));
        }
        for request in &current {
            let before = previous.iter().find(|r| r.id == request.id);
            let kind = match before {
                None => Some("request added"),
                Some(r) if r.stage != request.stage => Some(match request.stage {
                    requests::Stage::Added => "request rejected",
                    requests::Stage::Requested => "resolution requested",
                    requests::Stage::Resolved => "request resolved",
                }),
                Some(r) if r.blockers != request.blockers => Some(if request.blockers.is_empty() {
                    "request unblocked"
                } else {
                    "request blocked"
                }),
                Some(r) if r.text != request.text => Some("request reworded"),
                _ => None,
            };
            if let Some(kind) = kind {
                events.push(log::event_value(
                    kind,
                    &request.id,
                    serde_json::json!({"owner": task.name, "creator": request.creator, "source": request.source, "text": request.text, "blockers": request.blockers, "evidence": request.evidence}),
                ));
            }
            if before.is_none() && request.stage != requests::Stage::Added {
                events.push(log::event_value(
                    if request.stage == requests::Stage::Resolved {
                        "request resolved"
                    } else {
                        "resolution requested"
                    },
                    &request.id,
                    serde_json::json!({"owner": task.name, "creator": request.creator, "evidence": request.evidence}),
                ));
            }
        }
        let mut previous_notifications = match &old {
            Some(task) => requests::notifications(task)?,
            None => Vec::new(),
        };
        for notification in requests::notifications(&task)? {
            if let Some(at) = previous_notifications
                .iter()
                .position(|before| before == &notification)
            {
                previous_notifications.remove(at);
            } else {
                events.push(log::event_value("message queued", &task.name, notification));
            }
        }
        if old
            .as_ref()
            .is_some_and(|before| before.field("agent_stopped") != Some("true"))
            && task.field("agent_stopped") == Some("true")
        {
            events.push(log::event_value(
                "agent stopped",
                &task.name,
                serde_json::json!({"address": task.address()}),
            ));
        }
        if old.as_ref().is_none_or(|before| {
            before.status() != task.status() || before.address() != task.address()
        }) {
            events.push(log::event_value(
                "agent record changed",
                &task.name,
                serde_json::json!({"status": task.status(), "address": task.address()}),
            ));
        }
        Ok((task, events))
    }

    pub fn write(&self, name: &str, text: &str) -> Result<(), String> {
        if name.contains('/') || !name.ends_with(".md") {
            return Err(format!("amh: invalid work-log filename {name}"));
        }
        if let Some(mut database) = self.database()? {
            if name != "TODO.md" {
                let task = Task::parse(name, text)?;
                return self.save(&task);
            }
            database.commit(&[(name.to_owned(), text.to_owned())], &[])?;
            return self.export(&database);
        }
        taskfile::write(&self.root.join(name), text)
            .map_err(|error| format!("amh: cannot write {name}: {error}"))
    }

    pub fn list(&self) -> Result<String, String> {
        if let Some(database) = self.database()? {
            return database
                .task("TODO.md")?
                .ok_or_else(|| "amh: no TODO.md in database".to_owned());
        }
        fs::read_to_string(self.root.join("TODO.md"))
            .map_err(|error| format!("amh: cannot read TODO.md: {error}"))
    }

    /// Every task file in the root, by file name; a file that is not a task file is left out.
    pub fn all(&self) -> Result<Vec<Task>, String> {
        if let Some(database) = self.database()? {
            return database
                .tasks()?
                .into_iter()
                .filter(|(name, _)| name != "TODO.md")
                .map(|(name, text)| Task::parse(&name, &text))
                .collect();
        }
        self.markdown_tasks()
    }

    fn markdown_tasks(&self) -> Result<Vec<Task>, String> {
        let entries = fs::read_dir(&self.root)
            .map_err(|error| format!("amh: cannot read {}: {error}", self.root.display()))?;
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(".md") && !name.starts_with('.'))
            .collect();
        names.sort();
        let mut parsed = Vec::new();
        for name in names {
            let text = fs::read_to_string(self.root.join(&name))
                .map_err(|e| format!("amh: cannot read {name}: {e}"))?;
            match Task::parse(&name, &text) {
                Ok(task) => parsed.push(task),
                Err(error)
                    if text.starts_with("---\n")
                        && (text.contains("\nrequest_records:")
                            || (text.contains("\nstatus:") && text.contains("\nrunat:"))) =>
                {
                    return Err(format!("amh: {error}"));
                }
                Err(_) => {}
            }
        }
        Ok(parsed)
    }

    /// Every task file in the root that is not done, by file name.
    pub fn active(&self) -> Result<Vec<Task>, String> {
        Ok(self.all()?.into_iter().filter(Task::is_active).collect())
    }

    /// The caller's task: the named file, else the one active task whose agent runs at the caller's address.
    // 🧑 "We need to pass in the task file name at launch and let the agent pass in that when they use any helper command."
    pub fn own(&self, named: Option<&str>) -> Result<Task, String> {
        let from_launch = std::env::var("OMO_AGENT_TASK_FILE")
            .ok()
            .filter(|name| !name.is_empty());
        if let Some(name) = named
            .filter(|name| !name.is_empty())
            .or(from_launch.as_deref())
        {
            return self.load(name);
        }
        let address = omnigent::own_address()?;
        let mut matches: Vec<Task> = self
            .active()?
            .into_iter()
            .filter(|task| task.address() == address)
            .collect();
        match matches.len() {
            1 => Ok(matches.remove(0)),
            n => Err(format!(
                "amh: {n} active tasks run at {address}; pass --task-file NAME.md"
            )),
        }
    }

    /// Save a bullet entry without a task record, moving an existing identical bullet.
    // 🧑 "There is no task file for it. ... add a way and tell me; I added nothing."
    pub fn list_add(&self, entry: &str, section: &str) -> Result<(), String> {
        let entry = entry.trim();
        if entry.is_empty() || entry.contains(['\n', '\r']) {
            return Err("amh: a list entry must be one nonempty line".to_owned());
        }
        let section = format!("{section}:");
        if !taskfile::SECTIONS.contains(&section.as_str()) {
            return Err(format!("amh: invalid task-list section `{section}`"));
        }
        let list = self.list()?;
        let row = format!("- {entry}");
        let mut lines: Vec<_> = list.lines().filter(|line| line.trim() != row).collect();
        let at = lines
            .iter()
            .position(|line| line.trim().eq_ignore_ascii_case(&section))
            .ok_or_else(|| format!("amh: TODO.md has no `{section}` section"))?;
        lines.insert(at + 1, &row);
        self.write("TODO.md", &(lines.join("\n") + "\n"))
    }

    /// The plain bullets of one section of the task list, one per line.
    pub fn list_entries(&self, section: &str) -> Result<String, String> {
        let list = self.list()?;
        let rows = list
            .lines()
            .skip_while(|line| !line.trim().eq_ignore_ascii_case(&format!("{section}:")))
            .skip(1)
            .take_while(|line| !taskfile::SECTIONS.contains(&line.trim().to_lowercase().as_str()));
        Ok(rows
            .filter(|line| line.starts_with("- "))
            .map(|line| format!("{line}\n"))
            .collect())
    }

    /// Remove a plain bullet that `list_add` saved.
    // 🧑 "How does the human add human pending items for themselves?"
    pub fn list_remove(&self, entry: &str) -> Result<(), String> {
        let (list, row) = (self.list()?, format!("- {}", entry.trim()));
        let lines: Vec<_> = list.lines().filter(|line| line.trim() != row).collect();
        if lines.len() == list.lines().count() {
            return Err(format!("amh: the task list has no entry `{row}`"));
        }
        self.write("TODO.md", &(lines.join("\n") + "\n"))
    }

    /// Put the task's row at the top of its section of the task list.
    pub fn place(&self, task: &Task) -> Result<(), String> {
        match taskfile::placed(&self.list()?, &task.name, task.address(), task.section())? {
            Some(list) => self.write("TODO.md", &list),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn pending_blocks_migrate_once_and_stop_close_follow_the_queue()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!(
            "amh-blocks-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let store = Store {
            root: root.clone(),
            state_dir: root.join("state"),
        };
        taskfile::write(&root.join("TODO.md"), "current:\nt.md\n\nprevious:\n")?;
        taskfile::write(
            &root.join("t.md"),
            "---\nstatus: running\nrunat: omnigent://worker\n---\n\n(pending)\nfirst\nsecond\n\n(pending)\nfirst\nsecond\n\n```\n(pending)\nexample\n```\n",
        )?;
        drop(store.lock()?);
        let db = Database::open(&store.state_dir.join("amh.sqlite3"))?;
        let initial = db.pending_messages("block")?;
        assert_eq!(initial.len(), 2);
        assert_eq!(initial[0]["text"], "first\nsecond");
        assert_eq!(initial[1]["text"], "first\nsecond");
        assert!(taskfile::pending_blocks(&store.load("t.md")?.body).is_empty());
        assert!(store.load("t.md")?.body.contains("```\n(pending)\nexample"));
        drop(store.lock()?);
        assert_eq!(db.pending_messages("block")?, initial);
        let failure =
            crate::agents::close_task(&BTreeMap::new(), &store, "t.md", false, false, false);
        assert!(
            failure
                .err()
                .ok_or("closed despite queued blocks")?
                .contains("undelivered message")
        );
        let path = root.join("t.md");
        fs::write(
            &path,
            fs::read_to_string(&path)? + "\n(pending)\nnew typed request\n",
        )?;
        drop(store.lock()?);
        assert_eq!(db.pending_messages("block")?.len(), 3);
        {
            let _lock = store.lock()?;
            let mut task = store.load("t.md")?;
            crate::guest::queue(&mut task, "guest_mail", "guest-1.txt");
            assert!(store.receive_mail("guest:guest_mail/guest-1.txt", &task)?);
            assert!(!store.receive_mail("guest:guest_mail/guest-1.txt", &task)?);
        }
        assert_eq!(store.queued_mail("t.md")?.len(), 4);
        assert!(
            db.pending_messages("block")?[3]["text"]
                .as_str()
                .ok_or("missing guest source")?
                .contains("guest_mail/guest-1.txt")
        );
        store.drop_queued_mail("t.md")?;
        assert!(store.queued_mail("t.md")?.is_empty());
        drop(store.lock()?);
        assert!(db.pending_messages("block")?.is_empty());
        assert!(store.load("t.md")?.body.contains("new typed request"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn concurrent_record_migration_keeps_legacy_and_appended_lines()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!(
            "amh-records-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let store = Store {
            root: root.clone(),
            state_dir: root.clone(),
        };
        Database::open(&root.join("amh.sqlite3"))?;
        fs::write(root.join("sent"), "legacy\n")?;
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| -> Result<(), String> {
            let mut writers = Vec::new();
            for writer in 0..4 {
                let store = &store;
                let barrier = &barrier;
                writers.push(scope.spawn(move || -> Result<(), String> {
                    barrier.wait();
                    for line in 0..20 {
                        store.append_record("sent", &format!("{writer}:{line}\n"))?;
                        Database::open(&store.state_dir.join("amh.sqlite3"))?.remember_ids(
                            "read",
                            &[writer * 20 + line],
                            150,
                        )?;
                    }
                    Ok(())
                }));
            }
            for writer in writers {
                writer.join().map_err(|_| "record writer panicked")??;
            }
            Ok(())
        })?;
        let text = store.record("sent")?;
        assert_eq!(text.lines().count(), 81);
        assert_eq!(
            text.lines()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            81
        );
        assert!(text.starts_with("legacy\n"));
        let mut database = Database::open(&root.join("amh.sqlite3"))?;
        assert_eq!(database.remember_ids("read", &[], 150)?.len(), 80);
        assert_eq!(database.remember_ids("read", &[100], 3)?, vec![100, 79, 78]);
        assert!(!root.join("sent").exists());
        fs::write(root.join("sent"), "stale\n")?;
        assert_eq!(store.record("sent")?, text);
        fs::create_dir(root.join("unreadable"))?;
        assert!(store.record("unreadable").is_err());
        assert_eq!(
            Database::open(&root.join("amh.sqlite3"))?.record("unreadable")?,
            None
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn a_line_typed_into_a_task_file_survives_the_next_export()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!(
            "amh-typed-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let store = Store {
            root: root.clone(),
            state_dir: root.join("state"),
        };
        taskfile::write(
            &root.join("t.md"),
            "---\nstatus: running\nrunat: omnigent://worker\n---\n\nbody\n",
        )?;
        taskfile::write(&root.join("TODO.md"), "current:\n\nprevious:\n")?;
        drop(store.lock()?);
        let path = root.join("t.md");
        fs::write(&path, fs::read_to_string(&path)? + "typed by a manager\n")?;
        let lock = store.lock()?;
        let mut task = store.load("t.md")?;
        task.fields.push(("tool".to_owned(), "codex".to_owned()));
        store.save(&task)?;
        drop(lock);
        let text = fs::read_to_string(&path)?;
        assert!(text.contains("typed by a manager") && text.contains("tool: codex"));
        assert_eq!(text.matches("typed by a manager").count(), 1);
        drop(store.lock()?);
        assert_eq!(fs::read_to_string(&path)?, text);
        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn a_human_message_is_queued_once_on_its_own_channel() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = std::env::temp_dir().join(format!(
            "amh-queue-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let store = Store {
            root: root.clone(),
            state_dir: root.join("state"),
        };
        taskfile::write(
            &root.join("t.md"),
            "---\nstatus: running\nrunat: omnigent://worker\n---\n\nbody\n",
        )?;
        taskfile::write(&root.join("TODO.md"), "current:\n\nprevious:\n")?;
        let _lock = store.lock()?;
        let mut task = store.load("t.md")?;
        task.body = crate::decide::queued(&task.body, "m-1.txt");
        assert!(store.queue_mail(Some("k"), &task, "m-1.txt")?);
        assert!(!store.queue_mail(Some("k"), &task, "m-1.txt")?);
        assert!(store.queue_mail(None, &task, "m-2.txt")?);
        let database = Database::open(&store.state_dir.join("amh.sqlite3"))?;
        assert!(database.pending_messages("agent")?.is_empty());
        let queued = database.pending_messages("human")?;
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0]["target"], "t.md");
        assert_eq!(queued[0]["text"], "m-1.txt");
        assert!(
            store
                .load("t.md")?
                .body
                .ends_with("(record and delegate manager_mail/m-1.txt)\n")
        );
        store.drop_queued_mail("t.md")?;
        assert!(store.queued_mail("t.md")?.is_empty());
        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn migration_transfer_and_export_repair_preserve_database_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!(
            "amh-store-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let store = Store {
            root: root.clone(),
            state_dir: root.join("state"),
        };
        let text = "---\nstatus: running\nrunat: omnigent://worker\npending_task_items:\n  - implement SQLite\n---\n\noriginal body\n";
        taskfile::write(&root.join("source.md"), text)?;
        taskfile::write(
            &root.join("target.md"),
            &text
                .replace("worker", "target")
                .replace("implement SQLite", "verify migration"),
        )?;
        taskfile::write(&root.join("TODO.md"), "current:\n\nprevious:\n")?;
        let lock = store.lock()?;
        let mut source = store.load("source.md")?;
        let mut target = store.load("target.md")?;
        let request_id = requests::sync(&mut source)?[0].id.clone();
        requests::transfer(&mut source, &mut target, &request_id)?;
        source.body = "source after transfer\n".to_owned();
        target.body = "target after transfer\n".to_owned();
        store.save_transfer(&source, &target)?;
        store.write("TODO.md", "current:\n- target\n\nprevious:\n")?;
        let expected_source = store.load("source.md")?.render();
        let expected_target = store.load("target.md")?.render();
        drop(lock);
        fs::write(root.join("source.md"), "interrupted export")?;
        fs::remove_file(root.join("target.md"))?;
        fs::write(root.join("TODO.md"), "stale task list")?;
        assert_eq!(store.load("source.md")?.render(), expected_source);
        assert_eq!(store.load("target.md")?.render(), expected_target);
        assert_eq!(store.all()?.len(), 2);
        assert!(requests::read(&store.load("source.md")?)?.is_empty());
        assert!(
            requests::read(&store.load("target.md")?)?
                .iter()
                .any(|request| request.id == request_id)
        );
        let lock = store.lock()?;
        assert_eq!(fs::read_to_string(root.join("source.md"))?, expected_source);
        assert_eq!(fs::read_to_string(root.join("target.md"))?, expected_target);
        assert_eq!(fs::read_to_string(root.join("TODO.md"))?, store.list()?);
        let database = store.database()?.ok_or("database was not initialized")?;
        assert!(!database.events()?.is_empty());
        let baseline = database.pending_messages("agent")?.len();
        let mut source = store.load("source.md")?;
        requests::queue(&mut source, "target.md", "repeat")?;
        store.save(&source)?;
        requests::queue(&mut source, "target.md", "repeat")?;
        store.save(&source)?;
        assert_eq!(database.pending_messages("agent")?.len(), baseline + 2);
        assert!(fs::read_to_string(root.join("amh_views/events.md"))?.contains("message queued"));
        drop(database);
        drop(lock);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
