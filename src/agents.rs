//! The agents of tasks, end to end: start one on a task, and find the ones in trouble.
//!
//! The decisions and texts are the pure functions of `work`; this module does the input and output around them.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::store::Store;
use crate::tasks::Task;
use crate::work::{self, Problem};
use crate::{omnigent, settings};

const GUIDE_WAIT: Duration = Duration::from_secs(30);
const PROMPT_WAIT: Duration = Duration::from_secs(90);

type Settings = BTreeMap<String, String>;

/// An agent to start on the task file `name`: `fields` and `record` go into the task file, `request` ends its first prompt.
pub struct Start<'a> {
    pub name: &'a str,
    pub fields: &'a [(String, String)],
    pub tool: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub workdir: &'a str,
    pub guides: &'a [&'a str],
    pub request: &'a str,
    pub record: &'a str,
}

/// A task to create or reuse, with the agent to start on it; `None` takes the default.
///
/// `email` and `lines` name the stored human email lines that caused the task; they are quoted to the agent.
pub struct NewTask<'a> {
    pub name: &'a str,
    pub workdir: &'a str,
    pub goal: &'a str,
    pub manager: &'a str,
    pub tool: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub as_manager: bool,
    pub email: Option<&'a str>,
    pub lines: Option<&'a str>,
}

/// The active tasks whose agent is gone, failed, stuck, or idle with open items, each with its evidence.
pub fn problems(
    settings: &Settings,
    store: &Store,
) -> Result<Vec<(Task, Problem, String)>, String> {
    let mut found = Vec::new();
    for task in store.active()? {
        if !work::expects_agent(&task) {
            continue;
        }
        let (state, evidence) = omnigent::status(settings, task.address());
        if let Some(problem) = work::problem_of(&task, state) {
            found.push((task, problem, evidence));
        }
    }
    Ok(found)
}

/// Whether a task's record names an agent and something still runs there.
pub fn has_live_agent(settings: &Settings, task: &Task) -> bool {
    task.status() != "done"
        && task.address() != "retired"
        && omnigent::status(settings, task.address()).0 != "missing"
}

/// Read a pipe to its end on its own thread, so a long output cannot block the command while its end is awaited.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Run one `getagentsmd` command and return what it printed.
fn shown(args: &[&str]) -> Result<(String, String), String> {
    let command = ["getagentsmd"]
        .iter()
        .chain(args)
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let failed = |why: &dyn std::fmt::Display| format!("amh: `{command}` failed: {why}");
    let mut child = Command::new("getagentsmd")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failed(&error))?;
    let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let deadline = Instant::now() + GUIDE_WAIT;
    let status = loop {
        match child.try_wait().map_err(|error| failed(&error))? {
            Some(status) => break status,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(failed(&"it did not finish in time"));
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    let (out, err) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    if !status.success() || out.trim().is_empty() {
        return Err(failed(&err.trim()));
    }
    Ok((command, out))
}

/// Launch an agent for a task, record where it runs in the task file, give it its first prompt, and return its address.
pub fn start_agent(settings: &Settings, store: &Store, start: &Start) -> Result<String, String> {
    let agent = |error: String| format!("amh: {error}");
    let commands = [vec![]]
        .into_iter()
        .chain(start.guides.iter().map(|guide| vec!["get", guide]));
    let guides: Vec<(String, String)> = commands
        .map(|args| shown(&args))
        .collect::<Result<_, _>>()?;
    let prompt = work::first_prompt(start.name, &guides, start.request);
    let title = start.name.strip_suffix(".md").unwrap_or(start.name);
    let address = omnigent::launch(
        settings,
        start.tool,
        start.model,
        start.effort,
        start.workdir,
        title,
    )
    .map_err(agent)?;
    {
        let _lock = store.lock()?;
        let mut task = if store.root.join(start.name).exists() {
            store.load(start.name)?
        } else {
            work::new_task(start.name)
        };
        work::record_start(&mut task, start.fields, &address, start.tool, start.record);
        store.save(&task)?;
        store.place(&task)?;
    }
    // 🧑 "Make the agent spawning script start the session and then inject the prompt, in 2 separate steps, and verify that it works"
    omnigent::send(settings, &address, &prompt).map_err(agent)?;
    let deadline = Instant::now() + PROMPT_WAIT;
    while !omnigent::saw_message(settings, &address, &prompt).map_err(agent)? {
        if Instant::now() > deadline {
            return Err(format!(
                "amh: {address} runs and {} records it, but the first prompt did not show up in its history; send it again with `amh tell agent`",
                start.name
            ));
        }
        thread::sleep(Duration::from_secs(1));
    }
    Ok(address)
}

/// A value the caller gave; an empty one counts as not given.
fn given(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.is_empty())
}

/// Create or reuse a task file, launch its agent with the goal as its manager's request, and return the agent's address.
pub fn start_task(settings: &Settings, store: &Store, new: &NewTask) -> Result<String, String> {
    if store.root.join(new.name).exists() {
        let old = store.load(new.name)?;
        if has_live_agent(settings, &old) {
            return Err(format!(
                "amh: {} already has a live agent at {}; message it, or stop it first",
                new.name,
                old.address()
            ));
        }
    }
    let tool = given(new.tool).unwrap_or(work::default_tool(new.as_manager));
    let (model, effort) = omnigent::launch_default(settings, tool, new.as_manager)
        .map_err(|error| format!("amh: {error}"))?;
    let quoted = match given(new.email) {
        Some(email) => {
            work::span(new.lines.unwrap_or_default())?;
            let name = Path::new(email)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let path = settings::work_logs(settings)
                .join("manager_mail")
                .join(&name);
            let stored = fs::read_to_string(&path)
                .map_err(|error| format!("amh: cannot read {}: {error}", path.display()))?;
            Some((name, stored))
        }
        None => None,
    };
    let quoted = quoted.as_ref().map(|(name, stored)| {
        (
            name.as_str(),
            new.lines.unwrap_or_default(),
            stored.as_str(),
        )
    });
    let request = work::request(new.manager, new.goal, quoted)?;
    start_agent(
        settings,
        store,
        &Start {
            name: new.name,
            fields: &work::start_fields(new.manager, new.as_manager),
            tool,
            model: given(new.model).unwrap_or(&model),
            effort: given(new.effort).unwrap_or(&effort),
            workdir: new.workdir,
            guides: &work::guides(new.as_manager),
            request: &request,
            record: &request,
        },
    )
}
