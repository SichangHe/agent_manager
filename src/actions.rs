//! The todo and task actions end to end: find the task files, change them under the lock, tell the human.
//!
//! The changes themselves are the pure functions of `tasks`; this module does the input and output around them.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use pyo3::exceptions::PySystemExit;
use pyo3::prelude::*;
use serde_json::Value;

use crate::mail::{self, Attached, Draft};
use crate::store::Store;
use crate::tasks::{self, Task};
use crate::{guest, settings};

/// An expected refusal: the message is printed and the command fails.
fn refuse(message: impl Into<String>) -> PyErr {
    PySystemExit::new_err(message.into())
}

/// Email the human about their items on a task's list, through the Python wrappers of the mail part, which the tests replace.
///
/// A task's earlier notice that the human has not read yet is folded into the new one and trashed, so at most one notice per task waits unread.
// 🧑 "At all time, try to keep 1 unread email for the human from yourself."
fn notify(py: Python<'_>, task: &Task, change: &str, items: &[String]) -> PyResult<()> {
    let Some(mut body) = tasks::human_notice(change, items) else {
        return Ok(());
    };
    let record = settings::state_dir().join("amh-notices.json");
    let mut notices: BTreeMap<String, (String, String)> = fs::read_to_string(&record)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let config = py.import("amh.config")?.call_method0("load")?;
    let wrappers = py.import("amh.mail")?;
    let unread_ids = |trash: Vec<String>| -> PyResult<Vec<String>> {
        let unread = wrappers.call_method1("unread", (&config, task.tag(), trash))?;
        unread
            .try_iter()?
            .map(|header| header?.getattr("message_id")?.extract())
            .collect()
    };
    let earlier = notices
        .get(task.tag())
        .filter(|(id, _)| unread_ids(Vec::new()).is_ok_and(|ids| ids.contains(id)))
        .cloned();
    if let Some((_, text)) = &earlier {
        body = format!("{text}{body}");
    }
    let sent: String = wrappers
        .call_method1(
            "send",
            (
                &config,
                task.tag(),
                "",
                &body,
                format!("{} {}", mail::NOTICE_SENDER, task.address()),
            ),
        )?
        .extract()?;
    if let Some((id, _)) = earlier {
        // The new notice is sent; failing to trash the old one only leaves one more unread email.
        let _ = unread_ids(vec![id]);
    }
    notices.insert(task.tag().to_owned(), (sent, body));
    let _ = fs::write(&record, serde_json::to_string(&notices).unwrap_or_default());
    Ok(())
}

/// Email an answer to the guest alone, on the thread of the stored guest email `mail_name`; returns what to print.
fn tell_guest(
    settings: &BTreeMap<String, String>,
    store: &Store,
    mail_name: &str,
    file: &str,
    images: &[&str],
) -> Result<String, String> {
    let read = |path: &Path| {
        fs::read_to_string(path)
            .map_err(|error| format!("amh: cannot read {}: {error}", path.display()))
    };
    let body = read(Path::new(file))?;
    let name = Path::new(mail_name).file_name().unwrap_or_default();
    let stored = read(&store.root.join(guest::mail_dir(settings)).join(name))?;
    let plan = guest::reply_plan(&stored, &body)?;
    let mut attachments = Vec::new();
    for image in images {
        let path = Path::new(image);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        attachments.push(Attached {
            name: name.into_owned(),
            kind: guest::image_kind(image)?.to_owned(),
            data: fs::read(path).map_err(|error| format!("amh: cannot read {image}: {error}"))?,
        });
    }
    let to = settings::get(settings, "guest.address");
    let draft = Draft {
        from: settings::get(settings, "mail.agent_address"),
        to,
        subject: &plan.subject,
        body: &body,
        reply_to: &plan.reply_to,
        references: &plan.references,
        sender: "",
        attachments: &attachments,
    };
    let message_id = mail::new_id();
    let raw = mail::compose(&draft, &message_id).map_err(|error| format!("amh: {error}"))?;
    mail::deliver(settings, to, &raw).map_err(|error| format!("amh: {error}"))?;
    Ok(format!(
        "Email sent to the guest.\nMessage-ID: {message_id}\n"
    ))
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
            | ("tell", "guest")
            | (
                "task",
                "show" | "note" | "give" | "move" | "status" | "check"
            )
    ) {
        return Ok(None);
    }
    let settings = settings::load().map_err(|message| refuse(format!("amh: {message}")))?;
    let store = Store::new(&settings);
    let output = match action {
        ("todo", "list") => {
            store
                .own(text("task_file"))
                .map_err(refuse)?
                .items
                .join("\n")
                + "\n"
        }
        ("todo", "add") | ("task", "give") => {
            let lock = store.lock().map_err(refuse)?;
            let mut task = match action.0 {
                "task" => store.load(need("task")).map_err(refuse)?,
                _ => store.own(text("task_file")).map_err(refuse)?,
            };
            let new: Vec<String> = tasks::add_item(&mut task, need("item"), flag("from_human"))
                .map_err(refuse)?
                .into_iter()
                .collect();
            if !new.is_empty() {
                store.save(&task).map_err(refuse)?;
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
            let lock = store.lock().map_err(refuse)?;
            let mut task = store.own(text("task_file")).map_err(refuse)?;
            tasks::remove_item(&mut task, need("item"), need("evidence")).map_err(refuse)?;
            store.save(&task).map_err(refuse)?;
            drop(lock);
            notify(py, &task, "deleted", &[need("item").to_owned()])?;
            "removed 1 open item\n".to_owned()
        }
        ("todo", "replace") => {
            let _lock = store.lock().map_err(refuse)?;
            let mut task = store.own(text("task_file")).map_err(refuse)?;
            tasks::replace_item(&mut task, need("old"), need("new")).map_err(refuse)?;
            store.save(&task).map_err(refuse)?;
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
                .map(|name| Ok(tasks::show(&store.load(name).map_err(refuse)?)))
                .collect();
            shown?.concat()
        }
        ("task", "note") => {
            let _lock = store.lock().map_err(refuse)?;
            let mut task = store.load(need("task")).map_err(refuse)?;
            tasks::add_note(&mut task, need("text"));
            store.save(&task).map_err(refuse)?;
            String::new()
        }
        ("task", "move") => {
            let _lock = store.lock().map_err(refuse)?;
            let (mut source, mut to) = (
                store.load(need("source")).map_err(refuse)?,
                store.load(need("to")).map_err(refuse)?,
            );
            tasks::move_item(&mut source, &mut to, need("item")).map_err(refuse)?;
            store.save(&to).map_err(refuse)?;
            store.save(&source).map_err(refuse)?;
            String::new()
        }
        ("task", "status") => {
            let _lock = store.lock().map_err(refuse)?;
            let mut task = store.load(need("task")).map_err(refuse)?;
            tasks::set_status(&mut task, need("status"), text("on")).map_err(refuse)?;
            store.save(&task).map_err(refuse)?;
            store.place(&task).map_err(refuse)?;
            String::new()
        }
        ("tell", "guest") => {
            let images = values["image"].as_array().into_iter().flatten();
            let images: Vec<&str> = images.filter_map(Value::as_str).collect();
            tell_guest(&settings, &store, need("mail"), need("file"), &images).map_err(refuse)?
        }
        ("task", "check") => {
            let findings = tasks::check(
                &store.list().map_err(refuse)?,
                &store.active().map_err(refuse)?,
            );
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
