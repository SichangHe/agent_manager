//! The agents of tasks, end to end: start one on a task, find the ones in trouble, close a task, replace a manager.
//!
//! The decisions and texts are the pure functions of `work`; this module does the input and output around them.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::store::Store;
use crate::tasks::Task;
use crate::work::{self, Problem};
use crate::{mail, omnigent, settings, taskfile};

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
    pub long_running: bool,
    pub email: Option<&'a str>,
    pub lines: Option<&'a str>,
    /// Quota priority, higher wins; unset keeps the task's own.
    pub priority: Option<i64>,
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

/// Point each task whose agent moved to a partial-compaction fork at the fork, and its workers' manager address with it.
pub fn follow_moves(settings: &Settings, store: &Store) -> Result<Vec<String>, String> {
    let mut followed = Vec::new();
    for task in store.active()? {
        let old = task.address().to_owned();
        if !old.starts_with(omnigent::PREFIX) {
            continue;
        }
        let chain = omnigent::moves(settings, &old);
        let Some(new) = chain.last().cloned() else {
            continue;
        };
        // lost_work_mgr.md asked that amh "close a task's superseded sessions itself when it follows a moved_to move"
        for left in std::iter::once(&old).chain(&chain[..chain.len() - 1]) {
            if let Err(error) = omnigent::retire(settings, left) {
                followed.push(format!(
                    "{}: closing its old session {left} failed: {error}",
                    task.name
                ));
            }
        }
        let _lock = store.lock()?;
        let mut fresh = store.load(&task.name)?;
        if fresh.address() != old {
            continue;
        }
        let session = new
            .strip_prefix(omnigent::PREFIX)
            .unwrap_or(&new)
            .to_owned();
        for (key, value) in &mut fresh.fields {
            match key.as_str() {
                "runat" => *value = new.clone(),
                "session_id" => *value = session.clone(),
                _ => {}
            }
        }
        store.save(&fresh)?;
        store.place(&fresh)?;
        for report in work::repointed(store.active()?, &old, &new) {
            store.save(&report)?;
        }
        followed.push(format!("{} moved from {old} to {new}", task.name));
    }
    Ok(followed)
}

/// Whether a task's record names an agent and something still runs there.
pub fn has_live_agent(settings: &Settings, task: &Task) -> bool {
    // 🧑 "Make restarting a failed or stopped task on another tool work, keeping the task file and its open items"
    if task.status() == "done" || task.address() == "retired" {
        return false;
    }
    match omnigent::status(settings, task.address()).0 {
        "running" | "stuck" | "paused" => true,
        "ready" => task.field("agent_stopped") != Some("true"),
        _ => false,
    }
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
    if store.contains(start.name)? {
        let existing = store.load(start.name)?;
        if omnigent::status(settings, existing.address()).0 == "paused"
            && existing.field("tool") == Some(start.tool)
        {
            return Err(format!(
                "amh: {} is paused for its provider session quota; keep its goals until reset",
                start.name
            ));
        }
    }
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
        let mut task = if store.contains(start.name)? {
            store.load(start.name)?
        } else {
            work::new_task(start.name)
        };
        let old = task.address().to_owned();
        work::record_start(&mut task, start.fields, &address, start.tool, start.record);
        store.save(&task)?;
        store.place(&task)?;
        if !old.is_empty() && old != address {
            for report in work::repointed(store.active()?, &old, &address) {
                store.save(&report)?;
            }
            if old == settings::get(settings, "main_manager") {
                let env = store.state_dir.join("env.toml");
                let failed =
                    |error: io::Error| format!("amh: cannot rewrite {}: {error}", env.display());
                let text = fs::read_to_string(&env).map_err(failed)?;
                taskfile::write(&env, &text.replace(&old, &address)).map_err(failed)?;
            }
        }
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

/// Where the agent of `who` runs: the address a task file records; an address is passed through.
pub fn address_of(store: &Store, who: &str) -> Result<String, String> {
    if who.ends_with(".md") {
        Ok(store.load(who)?.address().to_owned())
    } else {
        Ok(who.to_owned())
    }
}

/// Give `text` to the manager at `manager`: wake it, or, when `report_sink.TAG` names a file for the manager's task and the report is not `urgent` (a blocker or a final result), append there. Returns the file used.
// vl_catchup.md: "each Codex reader that vl_lit_dig.md's dispatcher script starts sends it an acknowledgement and a done message ... each waking an Opus agent for nothing ... let a task name a sink (a file or script) in place of its manager for such reports"
pub fn report(
    settings: &Settings,
    store: &Store,
    manager: &str,
    text: &str,
    urgent: bool,
) -> Result<Option<String>, String> {
    let tasks = store.all()?;
    let owner = tasks.iter().find(|task| task.address() == manager);
    let sink = owner.map_or("", |task| {
        settings::get(settings, &format!("report_sink.{}", task.tag()))
    });
    // The main manager: "make sure a blocker or a final result still wakes the manager"
    if sink.is_empty() || urgent {
        omnigent::send(settings, manager, text)?;
        return Ok(None);
    }
    let entry = format!(
        "{}\n{text}\n\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sink)
        .and_then(|mut file| file.write_all(entry.as_bytes()))
        .map_err(|error| format!("cannot write {sink}: {error}"))?;
    Ok(Some(sink.to_owned()))
}

/// Mark a task done, move it to `previous`, tell the human, stop its agent last, and return the agent's address.
///
/// `own` means the agent closes itself: its manager is told, and stopping the session ends the caller.
pub fn close_task(
    settings: &Settings,
    store: &Store,
    name: &str,
    agent_gone: bool,
    email: bool,
    own: bool,
) -> Result<String, String> {
    let task = {
        let _lock = store.lock()?;
        let mut task = store.load(name)?;
        crate::requests::can_close(&task, &store.all()?)?;
        if !store.queued_mail(name)?.is_empty() {
            return Err(format!(
                "amh: {name} still has an undelivered message; finish or cancel delivery before closing"
            ));
        }
        work::close(&mut task)?;
        store.save(&task)?;
        store.place(&task)?;
        task
    };
    let at = task.address();
    // 🧑 "When closing an agent, use the last email chain the agent used to send an automatic email ‘Closed xx:n’ with the agent’s window."
    if email && crate::requests::from_human(&task) {
        let body = format!("Closed {at}\n");
        mail::send(settings, &store.state_dir, task.tag(), "", &body, at, &[])
            .map_err(|error| format!("amh: {error}"))?;
    }
    // 🧑 "Agents should be able to close themselves. Have a command in amh for that"
    if own {
        let _ = writeln!(io::stdout(), "closed {name}; stopping your own session now");
        let manager = task.field("managerat").unwrap_or_default();
        let _ = report(settings, store, manager, &work::self_closed(&task), true);
    }
    match omnigent::stop(settings, at) {
        Err(error) if !agent_gone => Err(format!(
            "amh: {name} is marked done, but its agent was not stopped: {error}"
        )),
        _ => Ok(at.to_owned()),
    }
}

/// Replace a manager with a fresh agent that takes over its task file and its reports; returns the new agent's address.
// 🧑 "Make it s.t. rotation works for Omnigent overall"
pub fn rotate(
    settings: &Settings,
    store: &Store,
    name: &str,
    tool: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<String, String> {
    let agent = |error: String| format!("amh: {error}");
    let old = store.load(name)?.address().to_owned();
    let Some(id) = old.strip_prefix(omnigent::PREFIX) else {
        return Err("amh: only Omnigent managers can be rotated".to_owned());
    };
    let path = format!("/v1/sessions/{id}?include_items=false");
    let session = omnigent::api(settings, "GET", &path, None).map_err(agent)?;
    let is_main = old == settings::get(settings, "main_manager");
    // 🧑 (via the main manager) "make main-manager rotation start the main manager on Codex Astra, so I (Claude) can rotate out"
    let (tool, model, effort) = if is_main && tool.is_none() && model.is_none() {
        (
            Some("codex"),
            Some(settings::get(settings, "launch.codex.manager_model")).filter(|m| !m.is_empty()),
            effort.or(Some(settings::get(settings, "launch.codex.manager_effort"))
                .filter(|e| !e.is_empty())),
        )
    } else {
        (tool, model, effort)
    };
    let [tool, model, effort, workdir] = work::relaunch(&session, tool, model, effort)?;
    let root = store.root.display().to_string();
    let (guides, request, record) = work::handover(name, &old, &root, is_main);
    let start = Start {
        name,
        fields: &[],
        tool: &tool,
        model: &model,
        effort: &effort,
        workdir: &workdir,
        guides: &guides,
        request: &request,
        record: &record,
    };
    let new = start_agent(settings, store, &start)?;
    omnigent::stop(settings, &old).map_err(agent)?;
    Ok(new)
}

/// Replace the agent of a task for `why` (e.g. on the human's order): start a fresh agent on the same tool, model, effort, and folder, or on the given `tool`, `model` and `effort`, with the original request, a replacement note, the human's `message`, and the open items; then stop the old agent and tell the manager. Returns the new address.
// 🧑 "I see \"replace this agent\" does not trigger automatic replacement. It should"
pub fn replace(
    settings: &Settings,
    store: &Store,
    name: &str,
    message: &str,
    to: Option<(&str, &str, &str)>,
    why: &str,
) -> Result<String, String> {
    let agent = |error: String| format!("amh: {error}");
    let mut task = store.load(name)?;
    let old = task.address().to_owned();
    let Some(id) = old.strip_prefix(omnigent::PREFIX) else {
        return Err(format!(
            "amh: {name} does not run on Omnigent, so it cannot be replaced"
        ));
    };
    let session = omnigent::api(
        settings,
        "GET",
        &format!("/v1/sessions/{id}?include_items=false"),
        None,
    )
    .map_err(agent)?;
    let [tool, model, effort, workdir] =
        work::relaunch(&session, to.map(|t| t.0), to.map(|t| t.1), to.map(|t| t.2))?;
    let open = crate::requests::listed(&mut task)?;
    let root = store.root.display().to_string();
    let request = work::replacement(&task, &old, &root, message, &open);
    let is_manager = task.field("is_manager") == Some("true");
    let mut guides = work::guides(is_manager);
    if old == settings::get(settings, "main_manager") {
        guides = work::handover(name, &old, &root, true).0;
    }
    let record = format!("(agent replaced {why}: {old} handed over to a fresh agent)\n");
    let started = start_agent(
        settings,
        store,
        &Start {
            name,
            fields: &[],
            tool: &tool,
            model: &model,
            effort: &effort,
            workdir: &workdir,
            guides: &guides,
            request: &request,
            record: &record,
        },
    );
    // An agent that launched but did not show its first prompt in time is still the replacement: stop the old one regardless.
    let new = match started {
        Ok(new) => new,
        Err(error) => match store.load(name).map(|fresh| fresh.address().to_owned()) {
            Ok(new) if new != old => {
                eprintln!("{error}");
                new
            }
            _ => return Err(error),
        },
    };
    if let Err(error) = omnigent::stop(settings, &old) {
        eprintln!("amh: {name}: the replaced agent {old} was not stopped: {error}");
    }
    if let Some(manager) = task
        .field("managerat")
        .filter(|manager| *manager != old && !manager.is_empty())
    {
        let text = work::envelope(
            &new,
            &format!(
                "{name}: amh replaced its agent {old} with {new} {why}; it got the original request, the message and the open items; nothing to do."
            ),
        );
        let _ = report(settings, store, manager, &text, false);
    }
    Ok(new)
}

/// A value the caller gave; an empty one counts as not given.
fn given(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.is_empty())
}

/// Create or reuse a task file, launch its agent with the goal as its manager's request, and return the agent's address.
pub fn start_task(settings: &Settings, store: &Store, new: &NewTask) -> Result<String, String> {
    let mut fields = work::start_fields(new.manager, new.as_manager);
    if new.long_running
        && let Some((_, status)) = fields.iter_mut().find(|(key, _)| key == "status")
    {
        *status = "long_running".to_owned();
    }
    let pinned = given(new.model).is_some() || given(new.effort).is_some();
    fields.push(("model_pinned".to_owned(), pinned.to_string()));
    if let Some(priority) = new.priority {
        fields.push(("priority".to_owned(), priority.to_string()));
    }
    if store.contains(new.name)? {
        let old = store.load(new.name)?;
        let migrating = given(new.tool).is_some_and(|tool| old.field("tool") != Some(tool))
            && omnigent::status(settings, old.address()).0 == "paused";
        if !migrating && has_live_agent(settings, &old) {
            return Err(format!(
                "amh: {} already has a live agent at {}; message it, or stop it first",
                new.name,
                old.address()
            ));
        }
        if old.status() == "long_running" {
            for (key, value) in &mut fields {
                if key == "status" {
                    *value = "long_running".to_owned();
                }
            }
        }
    }
    let tool = given(new.tool).unwrap_or_else(|| {
        let tool = work::default_tool(new.as_manager);
        if new.as_manager || pinned {
            tool
        } else {
            crate::fallback::default_tool(settings, tool)
        }
    });
    if new.as_manager && !settings::allows(settings, "manager.tools", tool) {
        return Err(format!(
            "amh: a manager must run on {}, not {tool}: the human wants only those to make top-level decisions",
            settings::get(settings, "manager.tools")
        ));
    }
    let own_priority = store
        .contains(new.name)?
        .then(|| store.load(new.name))
        .transpose()?
        .and_then(|old| old.field("priority").and_then(|p| p.parse().ok()));
    // Managers are never held back, and a tool the caller named or a pinned model cannot be swapped.
    let tool = &if new.as_manager {
        tool.to_owned()
    } else {
        crate::quota_pause::admit(
            settings,
            store,
            tool,
            given(new.tool).is_some() || pinned,
            new.priority.or(own_priority).unwrap_or_default(),
        )?
    };
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
            fields: &fields,
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
