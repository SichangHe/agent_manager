//! Settings: the committed `config.toml` and the machine-local `env.toml`, exposed to the Python as module `amh_rs`.
// 🧑 "Separate config file, which should live in .config or something and get committed, and env, which should be in local state or something and include things like where the work_logs is"
// 🧑 "The config file can use `$AMH_CFG_DIR` for paths" ... "Also respect XDG etc."

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{env, fs};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// Resolve one of the program's folders: the explicit variable, else `amh` under the XDG base, else under the home default.
fn folder(explicit: &str, xdg: &str, home_default: &str) -> PathBuf {
    let set = |name: &str| env::var_os(name).filter(|value| !value.is_empty());
    if let Some(folder) = set(explicit) {
        return folder.into();
    }
    let base = set(xdg)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(set("HOME").unwrap_or_default()).join(home_default));
    base.join("amh")
}

/// The folder of `config.toml`, which is committed and holds no machine-specific value.
fn config_dir() -> PathBuf {
    folder("AMH_CFG_DIR", "XDG_CONFIG_HOME", ".config")
}

/// The folder of `env.toml` and of everything the program records while running.
fn state_dir() -> PathBuf {
    folder("AMH_STATE_DIR", "XDG_STATE_HOME", ".local/state")
}

/// Add every leaf of a TOML value under dotted keys, e.g. `mail.agent_address`.
fn flatten(key: String, value: &toml::Value, values: &mut BTreeMap<String, String>) {
    match value {
        toml::Value::Table(table) => {
            for (name, inner) in table {
                let dotted = if key.is_empty() {
                    name.clone()
                } else {
                    format!("{key}.{name}")
                };
                flatten(dotted, inner, values);
            }
        }
        toml::Value::String(text) => {
            values.insert(key, text.clone());
        }
        other => {
            values.insert(key, other.to_string());
        }
    }
}

/// Read both files; `env.toml` must exist, and its values win over `config.toml`.
fn load() -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for (path, required) in [
        (config_dir().join("config.toml"), false),
        (state_dir().join("env.toml"), true),
    ] {
        match fs::read_to_string(&path) {
            Ok(text) => {
                let table: toml::Table = text
                    .parse()
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                flatten(String::new(), &toml::Value::Table(table), &mut values);
            }
            Err(error) if required => {
                return Err(format!("cannot read {}: {error}", path.display()));
            }
            Err(_) => {}
        }
    }
    Ok(values)
}

/// The parts of `amh` that are already Rust, callable from the embedded Python.
#[pymodule]
pub mod amh_rs {
    use super::*;

    /// Every setting under its dotted key.
    #[pyfunction]
    fn settings() -> PyResult<BTreeMap<String, String>> {
        load().map_err(|message| PyRuntimeError::new_err(format!("amh: {message}")))
    }

    /// The folder holding `env.toml` and the program's records.
    #[pyfunction]
    fn state_dir() -> PathBuf {
        super::state_dir()
    }
}
