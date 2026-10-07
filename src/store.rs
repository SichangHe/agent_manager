//! The work-log root on disk: loading and saving task files and the task list, under the one lock every writer takes.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::PathBuf;

use crate::tasks::Task;
use crate::{log, omnigent, requests, settings, taskfile};

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
        self.recover_transfer()?;
        Ok(lock)
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
        if name.contains('/') || !name.ends_with(".md") || !path.is_file() {
            return Err(format!("amh: no task file {name} in the work-log root"));
        }
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("amh: cannot read {name}: {error}"))?;
        Task::parse(name, &text).map_err(|message| format!("amh: {message}"))
    }

    pub fn save(&self, task: &Task) -> Result<(), String> {
        let mut task = task.clone();
        let current = requests::sync(&mut task)?;
        requests::validate(&task)?;
        let old = self.load(&task.name).ok();
        let previous = match &old {
            Some(task) => requests::read(task)?,
            None => Vec::new(),
        };
        self.write(&task.name, &task.render())?;
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
                log::event(
                    &self.state_dir,
                    kind,
                    &request.id,
                    serde_json::json!({"owner": task.name, "creator": request.creator, "source": request.source, "text": request.text, "blockers": request.blockers, "evidence": request.evidence}),
                )?;
            }
            if before.is_none() && request.stage != requests::Stage::Added {
                log::event(
                    &self.state_dir,
                    if request.stage == requests::Stage::Resolved {
                        "request resolved"
                    } else {
                        "resolution requested"
                    },
                    &request.id,
                    serde_json::json!({"owner": task.name, "creator": request.creator, "evidence": request.evidence}),
                )?;
            }
        }
        let previous_notifications = match &old {
            Some(task) => requests::notifications(task)?,
            None => Vec::new(),
        };
        for notification in requests::notifications(&task)? {
            if !previous_notifications.contains(&notification) {
                log::event(&self.state_dir, "message queued", &task.name, notification)?;
            }
        }
        if old
            .as_ref()
            .is_some_and(|before| before.field("agent_stopped") != Some("true"))
            && task.field("agent_stopped") == Some("true")
        {
            log::event(
                &self.state_dir,
                "agent stopped",
                &task.name,
                serde_json::json!({"address": task.address()}),
            )?;
        }
        if old.as_ref().is_none_or(|before| {
            before.status() != task.status() || before.address() != task.address()
        }) {
            log::event(
                &self.state_dir,
                "agent record changed",
                &task.name,
                serde_json::json!({"status": task.status(), "address": task.address()}),
            )?;
        }
        Ok(())
    }

    pub fn write(&self, name: &str, text: &str) -> Result<(), String> {
        taskfile::write(&self.root.join(name), text)
            .map_err(|error| format!("amh: cannot write {name}: {error}"))
    }

    pub fn list(&self) -> Result<String, String> {
        fs::read_to_string(self.root.join("TODO.md"))
            .map_err(|error| format!("amh: cannot read TODO.md: {error}"))
    }

    /// Every task file in the root, by file name; a file that is not a task file is left out.
    pub fn all(&self) -> Result<Vec<Task>, String> {
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
