//! The todo and task actions end to end: find the task files, change them under the lock, tell the human.
//!
//! The changes themselves are the pure functions of `tasks`; this module does the input and output around them.

use std::fs::{self, File};
use std::path::PathBuf;

use pyo3::exceptions::PySystemExit;
use pyo3::prelude::*;
use serde_json::Value;

use crate::tasks::{self, Task};
use crate::{omnigent, settings, taskfile};

/// An expected refusal: the message is printed and the command fails.
fn refuse(message: impl Into<String>) -> PyErr {
    PySystemExit::new_err(message.into())
}

/// The work-log root with the lock every writer of it takes.
struct Store {
    root: PathBuf,
    state_dir: PathBuf,
}

impl Store {
    fn open() -> PyResult<Self> {
        let values = settings::load().map_err(|message| refuse(format!("amh: {message}")))?;
        let root = settings::work_logs(&values);
        Ok(Self {
            root,
            state_dir: settings::state_dir(),
        })
    }

    /// Serialize every writer of the root through one lock file, held until the returned file is dropped.
    fn lock(&self) -> PyResult<File> {
        let failed = |error: std::io::Error| {
            refuse(format!(
                "amh: cannot lock {}: {error}",
                self.state_dir.display()
            ))
        };
        fs::create_dir_all(&self.state_dir).map_err(failed)?;
        let lock = File::create(self.state_dir.join("amh.lock")).map_err(failed)?;
        lock.lock().map_err(failed)?;
        Ok(lock)
    }

    fn load(&self, name: &str) -> PyResult<Task> {
        let path = self.root.join(name);
        if name.contains('/') || !name.ends_with(".md") || !path.is_file() {
            return Err(refuse(format!(
                "amh: no task file {name} in the work-log root"
            )));
        }
        let text = fs::read_to_string(&path)
            .map_err(|error| refuse(format!("amh: cannot read {name}: {error}")))?;
        Task::parse(name, &text).map_err(|message| refuse(format!("amh: {message}")))
    }

    fn save(&self, task: &Task) -> PyResult<()> {
        self.write(&task.name, &task.render())
    }

    fn write(&self, name: &str, text: &str) -> PyResult<()> {
        taskfile::write(&self.root.join(name), text)
            .map_err(|error| refuse(format!("amh: cannot write {name}: {error}")))
    }

    fn list(&self) -> PyResult<String> {
        fs::read_to_string(self.root.join("TODO.md"))
            .map_err(|error| refuse(format!("amh: cannot read TODO.md: {error}")))
    }

    /// Every task file in the root that is not done, by file name.
    fn active(&self) -> PyResult<Vec<Task>> {
        let entries = fs::read_dir(&self.root).map_err(|error| {
            refuse(format!("amh: cannot read {}: {error}", self.root.display()))
        })?;
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(".md") && !name.starts_with('.'))
            .collect();
        names.sort();
        let parsed = names.iter().filter_map(|name| {
            Task::parse(name, &fs::read_to_string(self.root.join(name)).ok()?).ok()
        });
        Ok(parsed.filter(Task::is_active).collect())
    }

    /// The caller's task: the named file, else the one active task whose agent runs at the caller's address.
    // 🧑 "We need to pass in the task file name at launch and let the agent pass in that when they use any helper command."
    fn own(&self, named: Option<&str>) -> PyResult<Task> {
        let from_launch = std::env::var("OMO_AGENT_TASK_FILE")
            .ok()
            .filter(|name| !name.is_empty());
        if let Some(name) = named
            .filter(|name| !name.is_empty())
            .or(from_launch.as_deref())
        {
            return self.load(name);
        }
        let address = omnigent::own_address().map_err(refuse)?;
        let mut matches: Vec<Task> = self
            .active()?
            .into_iter()
            .filter(|task| task.address() == address)
            .collect();
        match matches.len() {
            1 => Ok(matches.remove(0)),
            n => Err(refuse(format!(
                "amh: {n} active tasks run at {address}; pass --task-file NAME.md"
            ))),
        }
    }

    /// Put the task's row at the top of its section of the task list.
    fn place(&self, task: &Task) -> PyResult<()> {
        match taskfile::placed(&self.list()?, &task.name, task.address(), task.section())
            .map_err(refuse)?
        {
            Some(list) => self.write("TODO.md", &list),
            None => Ok(()),
        }
    }
}

/// Email the human about their items on a task's list; the mail part is still Python.
fn notify(py: Python<'_>, task: &Task, change: &str, items: &[String]) -> PyResult<()> {
    let Some(body) = tasks::human_notice(change, items) else {
        return Ok(());
    };
    let config = py.import("amh.config")?.call_method0("load")?;
    py.import("amh.mail")?
        .call_method1("send", (config, task.tag(), "", body, task.address()))?;
    Ok(())
}

/// Run one todo or task action; `None` when the action is not ported yet. Returns the exit status and what to print.
pub fn run(py: Python<'_>, values: &Value) -> PyResult<Option<(u8, String)>> {
    let text = |key: &str| values[key].as_str();
    let need = |key: &str| text(key).unwrap_or_default();
    let flag = |key: &str| values[key].as_bool().unwrap_or(false);
    let action = (need("group"), need("action"));
    if !matches!(
        action,
        ("todo", _)
            | (
                "task",
                "show" | "note" | "give" | "move" | "status" | "check"
            )
    ) {
        return Ok(None);
    }
    let store = Store::open()?;
    let output = match action {
        ("todo", "list") => store.own(text("task_file"))?.items.join("\n") + "\n",
        ("todo", "add") | ("task", "give") => {
            let lock = store.lock()?;
            let mut task = match action.0 {
                "task" => store.load(need("task"))?,
                _ => store.own(text("task_file"))?,
            };
            let new: Vec<String> = tasks::add_item(&mut task, need("item"), flag("from_human"))
                .map_err(refuse)?
                .into_iter()
                .collect();
            if !new.is_empty() {
                store.save(&task)?;
            }
            drop(lock);
            notify(py, &task, "created", &new)?;
            let to = if action.0 == "task" {
                format!(" to {}", task.name)
            } else {
                String::new()
            };
            format!("added {} open item(s){to}\n", new.len())
        }
        ("todo", "done") => {
            let lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            tasks::remove_item(&mut task, need("item"), need("evidence")).map_err(refuse)?;
            store.save(&task)?;
            drop(lock);
            notify(py, &task, "deleted", &[need("item").to_owned()])?;
            "removed 1 open item\n".to_owned()
        }
        ("todo", "replace") => {
            let _lock = store.lock()?;
            let mut task = store.own(text("task_file"))?;
            tasks::replace_item(&mut task, need("old"), need("new")).map_err(refuse)?;
            store.save(&task)?;
            "replaced 1 open item\n".to_owned()
        }
        ("task", "show") => {
            let names = values["task"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let shown: PyResult<Vec<String>> = names
                .iter()
                .filter_map(Value::as_str)
                .map(|name| store.load(name).map(|task| tasks::show(&task)))
                .collect();
            shown?.concat()
        }
        ("task", "note") => {
            let _lock = store.lock()?;
            let mut task = store.load(need("task"))?;
            tasks::add_note(&mut task, need("text"));
            store.save(&task)?;
            String::new()
        }
        ("task", "move") => {
            let _lock = store.lock()?;
            let (mut source, mut to) = (store.load(need("source"))?, store.load(need("to"))?);
            tasks::move_item(&mut source, &mut to, need("item")).map_err(refuse)?;
            store.save(&to)?;
            store.save(&source)?;
            String::new()
        }
        ("task", "status") => {
            let _lock = store.lock()?;
            let mut task = store.load(need("task"))?;
            tasks::set_status(&mut task, need("status"), text("on")).map_err(refuse)?;
            store.save(&task)?;
            store.place(&task)?;
            String::new()
        }
        ("task", "check") => {
            let findings = tasks::check(&store.list()?, &store.active()?);
            let failed = u8::from(!findings.is_empty());
            return Ok(Some((
                failed,
                findings
                    .iter()
                    .map(|finding| format!("{finding}\n"))
                    .collect(),
            )));
        }
        _ => return Ok(None),
    };
    Ok(Some((0, output)))
}
