//! The work-log root on disk: loading and saving task files and the task list, under the one lock every writer takes.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::PathBuf;

use crate::tasks::Task;
use crate::{omnigent, settings, taskfile};

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
        Ok(lock)
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
        self.write(&task.name, &task.render())
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
        let parsed = names.iter().filter_map(|name| {
            Task::parse(name, &fs::read_to_string(self.root.join(name)).ok()?).ok()
        });
        Ok(parsed.collect())
    }

    /// Every task file in the root that is not done, by file name.
    pub fn active(&self) -> Result<Vec<Task>, String> {
        Ok(self.all()?.into_iter().filter(Task::is_active).collect())
    }

    /// Every task file the task list must show: the active ones and the pinned ones.
    pub fn listed(&self) -> Result<Vec<Task>, String> {
        let shown = |task: &Task| task.is_active() || task.pinned().is_some();
        Ok(self.all()?.into_iter().filter(shown).collect())
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
