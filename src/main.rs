//! `amh`, the agent manager helper.
//!
//! Rust parses the command line. The embedded Python in `python/amh` does the work until each part is ported.
// 🧑 "at the beginning use py03 to call everything and only have the Rust part do command line parsing"

mod actions;
mod bridge;
mod cli;
mod log;
mod omnigent;
mod settings;
mod taskfile;
mod tasks;
mod view;

use std::ffi::CString;
use std::process::ExitCode;

use clap::{ArgAction, ArgMatches, Command};
use pyo3::exceptions::PySystemExit;
use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyDict};
use serde_json::{Map, Value};

use bridge::amh_rs;

/// The Python package, compiled into the binary so that no source path is needed at run time.
const SOURCES: &[(&str, &str)] = &[
    ("amh", ""),
    ("amh.config", include_str!("../python/amh/config.py")),
    ("amh.taskfile", include_str!("../python/amh/taskfile.py")),
    ("amh.agents", include_str!("../python/amh/agents.py")),
    ("amh.mail", include_str!("../python/amh/mail.py")),
    ("amh.guest", include_str!("../python/amh/guest.py")),
    ("amh.work", include_str!("../python/amh/work.py")),
    ("amh.watch", include_str!("../python/amh/watch.py")),
    ("amh.cli", include_str!("../python/amh/cli.py")),
];
/// Makes `import amh.*` load from `SOURCES`.
const BOOT: &str = r#"
import importlib.abc, importlib.util, sys

def install(sources):
    class Embedded(importlib.abc.MetaPathFinder, importlib.abc.Loader):
        def find_spec(self, name, path=None, target=None):
            if name in sources:
                return importlib.util.spec_from_loader(name, self, is_package=name == "amh")

        def exec_module(self, module):
            exec(compile(sources[module.__name__], f"<{module.__name__}>", "exec"), module.__dict__)

    sys.meta_path.insert(0, Embedded())
"#;
/// Turn the chosen action and its values into the JSON object the Python entry point takes.
fn parsed(command: &Command, matches: &ArgMatches) -> Value {
    let (group, group_matches) = matches.subcommand().expect("clap requires a group");
    let (action, values) = group_matches.subcommand().expect("clap requires an action");
    let leaf = command
        .find_subcommand(group)
        .and_then(|found| found.find_subcommand(action))
        .expect("clap matched this action");
    let mut object = Map::from_iter([
        ("group".to_owned(), Value::from(group)),
        ("action".to_owned(), Value::from(action)),
    ]);
    for arg in leaf.get_arguments() {
        let id = arg.get_id().as_str();
        let value = match arg.get_action() {
            ArgAction::SetTrue => Value::from(values.get_flag(id)),
            ArgAction::Append => values
                .get_many::<String>(id)
                .map(|many| many.cloned().collect::<Vec<_>>())
                .unwrap_or_default()
                .into(),
            ArgAction::Set => values.get_one::<String>(id).cloned().into(),
            _ => continue,
        };
        object.insert(id.to_owned(), value);
    }
    Value::Object(object)
}

/// Run the Python entry point and return the process exit status.
fn run_python(py: Python<'_>, parsed: &str) -> PyResult<u8> {
    let sources = PyDict::new(py);
    for (name, source) in SOURCES {
        sources.set_item(name, source)?;
    }
    let boot = CString::new(BOOT).expect("no NUL in the boot code");
    PyModule::from_code(py, &boot, c"amh_boot.py", c"amh_boot")?
        .getattr("install")?
        .call1((sources,))?;
    let status = match std::env::var("AMH_RUN_SCRIPT") {
        // The test script runs inside the binary, so it exercises the Rust parts too.
        Ok(script) => py.import("runpy")?.getattr("run_path")?.call(
            (script,),
            Some(&[("run_name", "__main__")].into_py_dict(py)?),
        ),
        Err(_) => py.import("amh.cli")?.getattr("run")?.call1((parsed,)),
    };
    let code = match status {
        Ok(status) => status.extract::<u8>().unwrap_or(0),
        Err(error) if error.is_instance_of::<PySystemExit>(py) => {
            let code = error.value(py).getattr("code")?;
            if code.is_none() {
                0
            } else if let Ok(number) = code.extract::<u8>() {
                number
            } else {
                eprintln!("{}", code.str()?);
                1
            }
        }
        Err(error) => {
            error.print(py);
            1
        }
    };
    let sys = py.import("sys")?;
    sys.getattr("stdout")?.call_method0("flush")?;
    sys.getattr("stderr")?.call_method0("flush")?;
    Ok(code)
}

/// Make the embedded Python use UTF-8 for all text, whatever locale the caller has; task files and mail hold non-ASCII text.
fn force_utf8() {
    // SAFETY: Python is not initialized yet and no other thread exists; the config struct is filled by Python before use.
    unsafe {
        let mut config = std::mem::MaybeUninit::<pyo3::ffi::PyPreConfig>::uninit();
        pyo3::ffi::PyPreConfig_InitPythonConfig(config.as_mut_ptr());
        let mut config = config.assume_init();
        config.utf8_mode = 1;
        pyo3::ffi::Py_PreInitialize(&config);
    }
}

fn main() -> ExitCode {
    let command = cli::command();
    let parsed = if std::env::var_os("AMH_RUN_SCRIPT").is_some() {
        String::new()
    } else {
        parsed(&command, &command.clone().get_matches()).to_string()
    };
    // Tests take the parsed command line and run the Python with fake agents and mail.
    if std::env::var_os("AMH_PARSE_ONLY").is_some() {
        println!("{parsed}");
        return ExitCode::SUCCESS;
    }
    let values: Value = serde_json::from_str(&parsed).unwrap_or_default();
    if (values["group"].as_str(), values["action"].as_str()) == (Some("agent"), Some("view")) {
        log::record(&parsed, "started");
        let read_only = values["read_only"].as_bool().unwrap_or(false);
        eprintln!(
            "{}",
            view::open(values["who"].as_str().unwrap_or_default(), read_only)
        );
        return ExitCode::FAILURE;
    }
    let long_running = values["action"] == "watch";
    if long_running {
        log::record(&parsed, "started");
    }
    pyo3::append_to_inittab!(amh_rs);
    force_utf8();
    let code = match Python::attach(|py| run_python(py, &parsed)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("amh: the embedded Python failed: {error}");
            1
        }
    };
    if !parsed.is_empty() {
        log::record(&parsed, &format!("exit={code}"));
    }
    ExitCode::from(code)
}
