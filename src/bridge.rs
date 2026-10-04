//! The parts of `amh` that are already Rust, callable from the embedded Python as the built-in module `amh_rs`.
// 🧑 "we shall gradually replace parts into Rust rewrites and see if the whole thing still works"

use std::collections::BTreeMap;
use std::path::PathBuf;

use pyo3::exceptions::{PyOSError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use crate::{omnigent, taskfile};

#[pymodule]
pub mod amh_rs {
    use super::*;

    #[pymodule_export]
    const MARKER: &str = taskfile::MARKER;
    #[pymodule_export]
    const SECTIONS: [&str; 4] = taskfile::SECTIONS;

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
}

/// The settings, or the error the Python shows as a refusal.
fn loaded() -> PyResult<BTreeMap<String, String>> {
    crate::settings::load().map_err(|message| PyRuntimeError::new_err(format!("amh: {message}")))
}
