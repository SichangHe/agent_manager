//! The parts of `amh` that are already Rust, callable from the embedded Python as the built-in module `amh_rs`.
// 🧑 "we shall gradually replace parts into Rust rewrites and see if the whole thing still works"

use std::collections::BTreeMap;
use std::path::PathBuf;

use pyo3::exceptions::{PyOSError, PyRuntimeError, PySystemExit, PyValueError};
use pyo3::prelude::*;

use crate::{mail, omnigent, taskfile};

/// A found email as the Python takes it: uid, subject, Message-ID, references, date.
type HeaderRow = (String, String, String, String, String, String);
/// A file carried by an email as the Python takes it: name, MIME type, content.
type FileRow = (String, String, Vec<u8>);
/// A received email as the Python takes it: sender address, `Authentication-Results`, Message-ID, In-Reply-To, References, subject, body, images.
type MailRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Vec<FileRow>,
);

fn header_rows(headers: Vec<mail::Header>) -> Vec<HeaderRow> {
    let row = |h: mail::Header| {
        (
            h.uid,
            h.subject,
            h.message_id,
            h.references,
            h.date,
            h.sender,
        )
    };
    headers.into_iter().map(row).collect()
}

/// One logged-in connection to the agent account's mailbox, which the watcher keeps across polls; every failure is an `OSError`.
#[pyclass(unsendable)]
pub struct Mailbox(mail::Mailbox);

#[pymethods]
impl Mailbox {
    /// The headers of matching mail in `folder`, oldest first.
    fn search(&mut self, folder: &str, readonly: bool, query: &str) -> PyResult<Vec<HeaderRow>> {
        let found = self.0.search(folder, readonly, query);
        found.map(header_rows).map_err(PyOSError::new_err)
    }

    /// Each unread inbox email from `sender` as its uid and raw bytes, without marking it read.
    fn unseen_from(&mut self, sender: &str) -> PyResult<Vec<(String, Vec<u8>)>> {
        self.0.unseen_from(sender).map_err(PyOSError::new_err)
    }

    fn mark_read(&mut self, uid: &str) -> PyResult<()> {
        self.0.mark_read(uid).map_err(PyOSError::new_err)
    }
}

#[pymodule]
pub mod amh_rs {
    use super::*;

    #[pymodule_export]
    const MARKER: &str = taskfile::MARKER;
    #[pymodule_export]
    const SECTIONS: [&str; 4] = taskfile::SECTIONS;
    #[pymodule_export]
    const MAIL_WATCHER: &str = mail::WATCHER;
    #[pymodule_export]
    const MAIL_NOTICE_SENDER: &str = mail::NOTICE_SENDER;
    #[pymodule_export]
    const MAIL_SENT_LOG: &str = mail::SENT_LOG;
    #[pymodule_export]
    use super::Mailbox;

    /// Every setting under its dotted key.
    #[pyfunction]
    fn settings() -> PyResult<BTreeMap<String, String>> {
        crate::settings::load()
            .map_err(|message| PyRuntimeError::new_err(format!("amh: {message}")))
    }

    /// The folder holding `env.toml` and the program's records.
    #[pyfunction]
    fn state_dir() -> PathBuf {
        crate::settings::state_dir()
    }

    /// Split a task file into (fields in order, open items, body); `ValueError` when it is not a task file.
    #[pyfunction]
    fn parse_task(name: &str, text: &str) -> PyResult<taskfile::Parts> {
        taskfile::parse(name, text).map_err(PyValueError::new_err)
    }

    /// Write a task file from its parts.
    #[pyfunction]
    fn render_task(fields: Vec<(String, String)>, items: Vec<String>, body: &str) -> String {
        taskfile::render(&fields, &items, body)
    }

    /// Each `(pending)` line of a body with the block after it.
    #[pyfunction]
    fn pending_blocks(body: &str) -> Vec<(usize, Vec<String>)> {
        taskfile::pending_blocks(body)
    }

    /// The task list with the task's row moved to the top of `section`, or `None` when already right; `ValueError` when the section is missing.
    #[pyfunction]
    fn placed_in_list(
        list: &str,
        name: &str,
        address: &str,
        section: &str,
    ) -> PyResult<Option<String>> {
        taskfile::placed(list, name, address, section).map_err(PyValueError::new_err)
    }

    /// Replace a file atomically, keeping its permissions.
    #[pyfunction]
    fn write_file(path: PathBuf, text: &str) -> PyResult<()> {
        taskfile::write(&path, text)
            .map_err(|error| PyOSError::new_err(format!("{}: {error}", path.display())))
    }

    /// Run one action that is already ported, from the JSON object of the parsed command line; `None` for the others. Returns the exit status and what to print.
    #[pyfunction]
    fn run_action(py: Python<'_>, parsed: &str) -> PyResult<Option<(u8, String)>> {
        let values = serde_json::from_str(parsed)
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        crate::actions::run(py, &values)
    }

    /// Deliver `text` to an agent as its next user message; `ValueError` when Omnigent refuses or is unreachable.
    #[pyfunction]
    fn agent_send(address: &str, text: &str) -> PyResult<()> {
        omnigent::send(&loaded()?, address, text).map_err(PyValueError::new_err)
    }

    /// An agent's state (`running`, `ready`, `error`, `missing`) and one line of evidence.
    #[pyfunction]
    fn agent_status(address: &str) -> PyResult<(&'static str, String)> {
        Ok(omnigent::status(&loaded()?, address))
    }

    /// Answer yes to every permission prompt an agent waits on; returns what was approved.
    #[pyfunction]
    fn agent_approve(address: &str) -> PyResult<Vec<String>> {
        omnigent::approve_prompts(&loaded()?, address).map_err(PyValueError::new_err)
    }

    /// Stop an agent by ending its session.
    #[pyfunction]
    fn agent_stop(address: &str) -> PyResult<()> {
        omnigent::stop(&loaded()?, address).map_err(PyValueError::new_err)
    }

    /// Start an agent with no prompt and return its address.
    #[pyfunction]
    fn agent_launch(
        tool: &str,
        model: &str,
        effort: &str,
        workdir: &str,
        title: &str,
    ) -> PyResult<String> {
        omnigent::launch(&loaded()?, tool, model, effort, workdir, title)
            .map_err(PyValueError::new_err)
    }

    /// Whether an agent's recent history holds the last line of `text`.
    #[pyfunction]
    fn agent_saw(address: &str, text: &str) -> PyResult<bool> {
        omnigent::saw_message(&loaded()?, address, text).map_err(PyValueError::new_err)
    }

    /// Where the calling agent runs.
    #[pyfunction]
    fn own_address() -> PyResult<String> {
        omnigent::own_address().map_err(PyValueError::new_err)
    }

    /// The model and effort a tool starts with.
    #[pyfunction]
    fn launch_default(tool: &str, as_manager: bool) -> PyResult<(String, String)> {
        omnigent::launch_default(&loaded()?, tool, as_manager).map_err(PyValueError::new_err)
    }

    /// One read from the Omnigent server as JSON text.
    #[pyfunction]
    fn omnigent_get(path: &str) -> PyResult<String> {
        omnigent::api(&loaded()?, "GET", path, None)
            .map(|answer| answer.to_string())
            .map_err(PyValueError::new_err)
    }

    /// A subject without its reply prefixes and `[tag]`s at the front.
    #[pyfunction]
    fn mail_bare(subject: &str) -> &str {
        mail::bare(subject)
    }

    /// The first `[tag]` at the front of a subject, ignoring reply prefixes.
    #[pyfunction]
    fn mail_tag_of(subject: &str) -> Option<&str> {
        mail::tag_of(subject)
    }

    /// Read a raw email into plain data.
    #[pyfunction]
    fn mail_parse(raw: &[u8]) -> MailRow {
        let mail = mail::parse(raw);
        let image = |image: mail::Attached| (image.name, image.kind, image.data);
        (
            mail.sender,
            mail.authentication,
            mail.message_id,
            mail.in_reply_to,
            mail.references,
            mail.subject,
            mail.body,
            mail.images.into_iter().map(image).collect(),
        )
    }

    /// The subject, the Message-ID to answer, and the references of an email on a tag, given the emails found carrying the tag, oldest first; `ValueError` when a new subject is too long.
    #[pyfunction]
    fn mail_plan(
        tag: &str,
        subject: &str,
        found: Vec<HeaderRow>,
    ) -> PyResult<(String, String, String)> {
        let header = |(uid, subject, message_id, references, date, sender)| mail::Header {
            uid,
            subject,
            message_id,
            references,
            date,
            sender,
        };
        let found: Vec<mail::Header> = found.into_iter().map(header).collect();
        mail::plan(tag, subject, &found)
            .map(|plan| (plan.subject, plan.reply_to, plan.references))
            .map_err(PyValueError::new_err)
    }

    /// Build an email from the agent account; returns its new Message-ID and its bytes. An empty `reply_to` starts a thread, an empty `sender` adds no `X-AMH-From`.
    #[pyfunction]
    fn mail_compose(
        to: &str,
        subject: &str,
        body: &str,
        reply_to: &str,
        references: &str,
        sender: &str,
        attachments: Vec<FileRow>,
    ) -> PyResult<(String, Vec<u8>)> {
        let file = |(name, kind, data)| mail::Attached { name, kind, data };
        let settings = loaded()?;
        let draft = mail::Draft {
            from: settings.get("mail.agent_address").map_or("", |from| from),
            to,
            subject,
            body,
            reply_to,
            references,
            sender,
            attachments: &attachments.into_iter().map(file).collect::<Vec<_>>(),
        };
        let message_id = mail::new_id();
        let raw = mail::compose(&draft, &message_id).map_err(PyValueError::new_err)?;
        Ok((message_id, raw))
    }

    /// Send built email bytes to one address through the agent account; `OSError` when it fails.
    #[pyfunction]
    fn mail_deliver(to: &str, raw: &[u8]) -> PyResult<()> {
        mail::deliver(&loaded()?, to, raw).map_err(PyOSError::new_err)
    }

    /// Email the human on the tag's thread and return the new Message-ID; `SystemExit` when the request is refused, `OSError` when the network or a file fails.
    #[pyfunction]
    fn mail_send(
        tag: &str,
        subject: &str,
        body: &str,
        sender: &str,
        attachments: Vec<PathBuf>,
    ) -> PyResult<String> {
        let state_dir = crate::settings::state_dir();
        mail::send(
            &loaded()?,
            &state_dir,
            tag,
            subject,
            body,
            sender,
            &attachments,
        )
        .map_err(|error| match error {
            mail::SendError::Refused(message) => PySystemExit::new_err(format!("amh: {message}")),
            mail::SendError::Failed(message) => PyOSError::new_err(message),
        })
    }

    /// The tag's emails the human has not opened, after moving those named in `trash_ids` to the trash.
    #[pyfunction]
    fn mail_unread(tag: &str, trash_ids: Vec<String>) -> PyResult<Vec<HeaderRow>> {
        let found = mail::unread(&loaded()?, tag, &trash_ids);
        found.map(header_rows).map_err(PyOSError::new_err)
    }

    /// Log in to the agent account's mailbox.
    #[pyfunction]
    fn mail_connect() -> PyResult<Mailbox> {
        let mailbox = mail::Mailbox::agent(&loaded()?);
        mailbox.map(Mailbox).map_err(PyOSError::new_err)
    }
}

/// The settings, or the error the Python shows as a refusal.
fn loaded() -> PyResult<BTreeMap<String, String>> {
    crate::settings::load().map_err(|message| PyRuntimeError::new_err(format!("amh: {message}")))
}
