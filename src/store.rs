//! SQLite records with recoverable markdown exports, under the one lock every writer takes.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::PathBuf;

use crate::tasks::Task;
use crate::{database::Database, log, omnigent, requests, settings, taskfile};

/// The work-log root with the lock every writer of it takes.
pub struct Store {
    pub root: PathBuf,
    pub state_dir: PathBuf,
}

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
        let lock = File::create(self.state_dir.join("amh.lock")).map_err(failed)?;
        lock.lock().map_err(failed)?;
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
        self.export(&database)?;
        Ok(lock)
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
            let old = self.load(&source.name)?;
            let remaining = requests::read(&source)?;
            for request in requests::read(&old)? {
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
        let baseline = database.pending_messages()?.len();
        let mut source = store.load("source.md")?;
        requests::queue(&mut source, "target.md", "repeat")?;
        store.save(&source)?;
        requests::queue(&mut source, "target.md", "repeat")?;
        store.save(&source)?;
        assert_eq!(database.pending_messages()?.len(), baseline + 2);
        assert!(fs::read_to_string(root.join("amh_views/events.md"))?.contains("message queued"));
        drop(database);
        drop(lock);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
