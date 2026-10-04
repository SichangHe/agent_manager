//! The parts of `amh` that are already Rust, callable from the embedded Python as the built-in module `amh_rs`.
// 🧑 "we shall gradually replace parts into Rust rewrites and see if the whole thing still works"

use std::collections::BTreeMap;
use std::path::PathBuf;

use pyo3::exceptions::{PyOSError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use crate::taskfile;

#[pymodule]
pub mod amh_rs {
    use super::*;

    #[pymodule_export]
    const HUMAN_MARK: &str = taskfile::HUMAN_MARK;
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

    /// One open item as a single line with the right authorship mark; `ValueError` when empty.
    #[pyfunction]
    fn mark_item(item: &str, from_human: bool) -> PyResult<String> {
        taskfile::mark(item, from_human).map_err(PyValueError::new_err)
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
}
