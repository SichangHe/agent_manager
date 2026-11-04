//! What the actions on the agents of tasks decide and say: starting, judging health, messaging, closing, replacing a manager, and the listings. Pure decisions and texts, no input or output.

use serde_json::Value;

use crate::taskfile;
use crate::tasks::{self, Task};

/// The `getagentsmd` guides every manager starts with.
const MANAGER_GUIDES: [&str; 3] = ["agent_work", "agent_manager", "agent_manager_core"];
// 🧑 "make the manager reporting sound more like messaging the manager, to discourage agents to dump info to manager"
const MANAGER_MESSAGE_CHARS_MAX: usize = 600;

/// Ends the request of a task that quotes no human email, so its agent does not acknowledge to the human.
const MANAGER_ONLY: &str = "Your manager gave you this task: do not acknowledge it to anyone. Report results and blockers to your manager with `amh tell manager`, not to the human, unless your task text says to report elsewhere or not at all. Message the human only about what the human asked for themselves, or when your manager asks you to.\n";

/// Mark text as coming from an agent, so the reader does not take it for the human's words.
pub fn envelope(sender: &str, text: &str) -> String {
    format!(
        "<agent_message from=\"{sender}\">\n{}\n</agent_message>",
        text.trim()
    )
}

/// Read text as Python's text mode does: every `\r\n` and lone `\r` is a line break.
pub fn unix_lines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// What is wrong with a task's agent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Problem {
    Missing,
    Error,
    Stuck,
    Idle,
}

impl Problem {
    pub fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Error => "error",
            Self::Stuck => "stuck",
            Self::Idle => "idle with open items",
        }
    }
}

/// Whether a task's record says an agent should be working at its address.
pub fn expects_agent(task: &Task) -> bool {
    task.status() != "blocked"
        && task.address() != "retired"
        && task.field("agent_stopped") != Some("true")
}

/// The problem of an active task whose agent is in `state`: gone, failed, stuck, or idle with open items.
pub fn problem_of(task: &Task, state: &str) -> Option<Problem> {
    if !expects_agent(task) {
        return None;
    }
    match state {
        "missing" => Some(Problem::Missing),
        "error" => Some(Problem::Error),
        "stuck" => Some(Problem::Stuck),
        "ready" if !crate::requests::ready_items(task).is_empty() => Some(Problem::Idle),
        _ => None,
    }
}

/// The tool a new agent runs when none is named.
// 🧑 (via the main manager) "Make Codex use Astra low by default instead of Sol ... shift management work from Claude to it"
pub fn default_tool(_as_manager: bool) -> &'static str {
    "codex"
}

/// The `getagentsmd` guides a new agent of a task starts with.
pub fn guides(as_manager: bool) -> Vec<&'static str> {
    if as_manager {
        MANAGER_GUIDES.into_iter().chain(["submanager"]).collect()
    } else {
        vec!["agent_work"]
    }
}

/// The frontmatter fields a newly started task gets.
// 🧑 “Long running simply means that the agent will not be closed if they have zero pending item.”
pub fn start_fields(manager: &str, as_manager: bool) -> Vec<(String, String)> {
    let status = if as_manager {
        "long_running"
    } else {
        "running"
    };
    [
        ("status", status),
        ("managerat", manager),
        ("is_manager", if as_manager { "true" } else { "false" }),
    ]
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .into()
}

/// Read `START-END`, both counting lines from 1.
pub fn span(lines: &str) -> Result<(usize, usize), String> {
    let number = |text: &str| {
        let plain = !text.starts_with('0') && text.chars().all(|digit| digit.is_ascii_digit());
        text.parse::<usize>().ok().filter(|_| plain)
    };
    lines
        .split_once('-')
        .and_then(|(start, end)| Some((number(start)?, number(end)?)))
        .ok_or_else(|| "amh: --lines must be START-END, counting from 1".to_owned())
}

/// The request a new agent gets from its manager: the goal, then the stored human email lines that caused the task.
///
/// `quoted` is the stored email's file name, its `START-END` lines, and its text.
pub fn request(
    manager: &str,
    goal: &str,
    quoted: Option<(&str, &str, &str)>,
) -> Result<String, String> {
    let mut request = format!(
        "<manager_delegation from=\"{manager}\">\n{}\n</manager_delegation>\n",
        goal.trim()
    );
    if let Some((email, lines, stored)) = quoted {
        // 🧑 "human requests should be sent verbatim"
        let (start, end) = span(lines)?;
        let stored = unix_lines(stored);
        let excerpt: Vec<&str> = stored
            .lines()
            .skip(start - 1)
            .take(end.saturating_sub(start - 1))
            .collect();
        request += &format!(
            "<human_instruction authoritative=\"true\" source=\"manager_mail/{email}:{lines}\">\n{}\n</human_instruction>\n",
            excerpt.join("\n")
        );
    } else {
        // The human manager: "All are Codex workers under tt_impl_mgr with no request from the human, sent with amh tell human."
        request += MANAGER_ONLY;
    }
    Ok(request)
}

/// A first prompt: the task's name, the output of each `getagentsmd` command in `shown`, then the request.
// 🧑 "put task file, without the ‘.md’ in all starter prompts and use that as tags"
// 🧑 “makes its content available in the manager's initial prompt before the manager starts ... make it clear that it is the output of that command”
pub fn first_prompt(name: &str, shown: &[(String, String)], request: &str) -> String {
    let tag = name.strip_suffix(".md").unwrap_or(name);
    let mut parts = vec![format!(
        "Task tag: {tag}\nTask file: {name}\nUse --task-file {name} with `amh` when it cannot tell who you are."
    )];
    parts.extend(
        shown
            .iter()
            .map(|(command, output)| format!("$ {command}\n{}", output.trim())),
    );
    format!("{}\n{request}", parts.join("\n"))
}

/// A task file that does not exist yet.
pub fn new_task(name: &str) -> Task {
    Task {
        name: name.to_owned(),
        fields: vec![("version".to_owned(), "v1.0.0".to_owned())],
        items: Vec::new(),
        body: String::new(),
    }
}

/// Record in a task that a fresh agent of `tool` runs at `address`: set `fields`, and append `record` to the body.
pub fn record_start(
    task: &mut Task,
    fields: &[(String, String)],
    address: &str,
    tool: &str,
    record: &str,
) {
    let persistent = task.status() == "long_running";
    let session = address
        .strip_prefix(crate::omnigent::PREFIX)
        .unwrap_or(address);
    task.fields
        .retain(|(key, _)| key != "session_id" && key != "agent_stopped");
    let placed = [("runat", address), ("tool", tool), ("session_id", session)];
    let fields = fields
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()));
    for (key, value) in fields.chain(placed) {
        match task.fields.iter_mut().find(|(known, _)| known == key) {
            Some(field) => field.1 = value.to_owned(),
            None => task.fields.push((key.to_owned(), value.to_owned())),
        }
    }
    if task.status() != "blocked" && !(persistent && task.status() == "long_running") {
        task.fields.retain(|(key, _)| key != "blocked_on");
    }
    let before = if task.body.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n\n", task.body.trim_end_matches('\n'))
    };
    task.body = before + record;
}

/// The message `tell manager` posts for a task whose agent is `state` (`blocked`, `done`, or nothing to say): the manager's address and the text.
pub fn manager_message<'a>(
    task: &'a Task,
    text: &str,
    state: &str,
) -> Result<(&'a str, String), String> {
    if text.chars().count() > MANAGER_MESSAGE_CHARS_MAX {
        return Err(format!(
            "amh: keep it under {MANAGER_MESSAGE_CHARS_MAX} characters; write details to a file and name its path"
        ));
    }
    let manager = task
        .field("managerat")
        .ok_or_else(|| format!("amh: {} records no manager", task.name))?;
    let label = if state.is_empty() {
        format!("{}: ", task.name)
    } else {
        format!("{} is {state}: ", task.name)
    };
    Ok((manager, envelope(task.address(), &(label + text))))
}

/// Mark a task done; refused while it has open work.
pub fn close(task: &mut Task) -> Result<(), String> {
    if !task.items.is_empty() || !taskfile::pending_blocks(&task.body).is_empty() {
        return Err(format!(
            "amh: {} still has open items or an undelivered `(pending)` block; finish, cancel, or move them first",
            task.name
        ));
    }
    tasks::set_status(task, "done", None)
}

/// What the manager of an agent that closed itself is told.
pub fn self_closed(task: &Task) -> String {
    let text = format!("{} had no open work left and closed itself.", task.name);
    envelope(task.address(), &text)
}

/// One row of `amh agent list`: what the agent of a task is doing.
pub fn listed(task: &Task, state: &str, evidence: &str) -> String {
    format!(
        "{state}: {} [{}] {} {evidence}",
        task.name,
        task.status(),
        task.address()
    )
}

/// One row of `amh agent problems`.
pub fn problem_line(task: &Task, problem: Problem, evidence: &str) -> String {
    let (name, at) = (&task.name, task.address());
    format!("{name} at {at}: {} ({evidence})", problem.name())
}

/// Add a task and, under it, everyone who reports to its agent; a task is drawn once.
fn draw<'a>(
    task: &'a Task,
    depth: usize,
    tasks: &'a [Task],
    drawn: &mut Vec<&'a str>,
    lines: &mut Vec<String>,
) {
    if drawn.contains(&task.name.as_str()) {
        return;
    }
    drawn.push(&task.name);
    let indent = "  ".repeat(depth);
    let role = if task.field("is_manager") == Some("true") {
        "manager"
    } else {
        "worker"
    };
    lines.push(format!(
        "{indent}{} [{role}, {}] {}",
        task.name,
        task.status(),
        task.address()
    ));
    lines.extend(task.items.iter().map(|item| format!("{indent}  - {item}")));
    let reports = |other: &&Task| other.field("managerat").unwrap_or_default() == task.address();
    for report in tasks.iter().filter(reports) {
        draw(report, depth + 1, tasks, drawn, lines);
    }
}

/// Draw who reports to whom among active tasks, starting from the main manager.
pub fn tree(tasks: &[Task], main_manager: &str) -> String {
    let (mut drawn, mut lines) = (Vec::new(), Vec::new());
    for task in tasks.iter().filter(|task| task.address() == main_manager) {
        draw(task, 0, tasks, &mut drawn, &mut lines);
    }
    let orphans: Vec<&Task> = tasks
        .iter()
        .filter(|task| !drawn.contains(&task.name.as_str()))
        .collect();
    if !orphans.is_empty() {
        lines.push("not under the main manager:".to_owned());
    }
    for task in orphans {
        draw(task, 1, tasks, &mut drawn, &mut lines);
    }
    lines.join("\n")
}

/// What the replacement of the manager at `old` starts with: its `getagentsmd` guides, its request, and the record for the task file `name` in the work-log root `root`.
pub fn handover(
    name: &str,
    old: &str,
    root: &str,
    is_main: bool,
) -> (Vec<&'static str>, String, String) {
    let role = if is_main {
        "main_manager"
    } else {
        "submanager"
    };
    (
        MANAGER_GUIDES.into_iter().chain([role]).collect(),
        format!(
            "You replace the manager that ran at {old}. Its task file {name} in {root} holds its open work and history; take over from there.\n"
        ),
        format!("(manager replaced: {old} handed over to a fresh agent)\n"),
    )
}

/// The request a task's first agent started with, as recorded in its task file: the first manager delegation and the human message or note that followed it.
pub fn original_request(body: &str) -> Option<String> {
    let start = body.find("<manager_delegation")?;
    let rest = &body[start..];
    let close = "</manager_delegation>\n";
    let mut end = rest.find(close)? + close.len();
    let after = &rest[end..];
    if after.starts_with("<human_instruction") {
        let done = "</human_instruction>\n";
        end += after.find(done).map_or(0, |at| at + done.len());
    } else if after.starts_with(MANAGER_ONLY) {
        end += MANAGER_ONLY.len();
    }
    Some(rest[..end].to_owned())
}

/// The request of an agent that replaces the one at `old` on the human's order: the original starter request, a note that it replaces an agent, the human's message, and the open items.
// 🧑 "should give the new agent the original starter prompt the replaced agent was given, plus a short explanation that they replace a previous agent, the human message, the pending item list; instead of the manager prompting them"
pub fn replacement(task: &Task, old: &str, root: &str, message: &str, open: &str) -> String {
    let original = original_request(&task.body).unwrap_or_else(|| {
        "(The original request is not recorded in the task file; read the task file for it.)\n"
            .to_owned()
    });
    let open = if open.trim().is_empty() {
        "(none)\n"
    } else {
        open
    };
    format!(
        "{original}\nYou replace the agent that ran this task at {old}; the human asked for its replacement in the message below. Its task file {} in {root} holds its history; continue from there. The previous agent is stopped.\n\n{}\n\nThe task's pending items (`amh todo list`):\n{open}",
        task.name,
        message.trim_end()
    )
}

/// The tool, model, effort, and folder a manager's replacement runs with: what was asked for, else what the old Omnigent session has.
pub fn relaunch(
    session: &Value,
    tool: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<[String; 4], String> {
    let kept = |asked: Option<&str>, key: &str| {
        match asked.filter(|asked| !asked.is_empty()) {
        Some(asked) => Ok(asked.to_owned()),
        None => session[key].as_str().map(str::to_owned).ok_or_else(|| {
            format!("amh: the manager's session has no `{key}`; name it with an option of `amh manager rotate`")
        }),
    }
    };
    let harness = kept(tool, "harness")?;
    Ok([
        harness
            .strip_suffix("-native")
            .unwrap_or(&harness)
            .to_owned(),
        kept(model, "model_override")?,
        kept(effort, "reasoning_effort")?,
        kept(None, "workspace")?,
    ])
}

/// The tasks that reported to the agent at `old`, changed to report to `new`.
pub fn repointed(tasks: Vec<Task>, old: &str, new: &str) -> Vec<Task> {
    let mut moved: Vec<Task> = tasks
        .into_iter()
        .filter(|task| task.field("managerat") == Some(old))
        .collect();
    for field in moved.iter_mut().flat_map(|task| &mut task.fields) {
        if field.0 == "managerat" {
            field.1 = new.to_owned();
        }
    }
    moved
}

/// What `amh manager worktree` says of one repository, from its `git status --porcelain`.
pub fn worktree(repo: &str, dirty: &str) -> String {
    if dirty.is_empty() {
        format!("{repo}: clean\n")
    } else {
        format!("{repo}:\n{dirty}\n")
    }
}

/// What the human added at the end of a task file in the editor: `None` when nothing changed; refused when anything else was changed.
// 🧑 "some sort of shortcut that opens the editor with a copy of the task file and pushes the appended stuff on close, just like what things like cron do"
// 🧑 "Should be append-only. If the edit changed other things, offer to reopen the file and ask for fix"
pub fn appended(before: &str, after: &str) -> Result<Option<String>, String> {
    let (kept, edited) = (before.trim_end(), after.trim_end());
    match edited.strip_prefix(kept) {
        Some("") => Ok(None),
        Some(added) if added.starts_with('\n') || kept.is_empty() => {
            Ok(Some(format!("{}\n", added.trim_matches('\n'))))
        }
        _ => Err(
            "the text that was already there has changed; only adding at the end is taken"
                .to_owned(),
        ),
    }
}

/// The task file name meant by what was typed: the name with or without `.md`, or a path to the file. An agent address stays as it is.
// 🧑 "Make it lenient and allow both having and not having .md and allow full paths."
pub fn task_name(given: &str) -> String {
    if given.starts_with(crate::omnigent::PREFIX) || given.is_empty() {
        return given.to_owned();
    }
    let name = given
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(given);
    if name.ends_with(".md") {
        name.to_owned()
    } else {
        format!("{name}.md")
    }
}

/// The newest `n` lines about one task from the watcher's log and the action log, in time order; long lines are cut.
pub fn task_log(watch: &str, actions: &str, name: &str, address: &str, n: usize) -> String {
    let named = format!("\"{name}\"");
    let about_watch = watch
        .lines()
        .filter(|line| line.contains(name) || line.contains(address));
    let about_action = actions
        .lines()
        .filter(|line| line.contains(address) || line.contains(&named));
    let mut lines: Vec<&str> = about_watch.chain(about_action).collect();
    lines.sort_by_key(|line| line.get(..19).unwrap_or_default());
    let newest = &lines[lines.len().saturating_sub(n)..];
    newest
        .iter()
        .map(|line| format!("{}\n", line.chars().take(300).collect::<String>()))
        .collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn task(name: &str, fields: &[(&str, &str)], items: &[&str], body: &str) -> Task {
        Task {
            name: name.to_owned(),
            fields: fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            items: items.iter().map(|item| (*item).to_owned()).collect(),
            body: body.to_owned(),
        }
    }

    #[test]
    fn envelope_wraps_trimmed_text() {
        assert_eq!(
            envelope("omnigent://m", " do\nthis \n"),
            "<agent_message from=\"omnigent://m\">\ndo\nthis\n</agent_message>"
        );
    }

    #[test]
    fn problems_by_state() {
        let idle = task(
            "a.md",
            &[("status", "running"), ("runat", "omnigent://a")],
            &["x"],
            "",
        );
        let found = ["missing", "error", "stuck", "ready", "running"].map(|s| problem_of(&idle, s));
        assert_eq!(
            found.map(|problem| problem.map(Problem::name)),
            [
                Some("missing"),
                Some("error"),
                Some("stuck"),
                Some("idle with open items"),
                None
            ]
        );
        let no_items = task("a.md", &[("status", "running"), ("runat", "x")], &[], "");
        assert_eq!(problem_of(&no_items, "ready"), None);
        let waiting = task(
            "a.md",
            &[
                ("status", "long_running"),
                ("runat", "x"),
                ("blocked_on", "human"),
            ],
            &["x"],
            "",
        );
        assert_eq!(problem_of(&waiting, "ready"), Some(Problem::Idle));
        assert_eq!(problem_of(&waiting, "error"), Some(Problem::Error));
        let blocked = task("a.md", &[("status", "blocked"), ("runat", "x")], &[], "");
        let retired = task(
            "a.md",
            &[("status", "running"), ("runat", "retired")],
            &[],
            "",
        );
        assert!(expects_agent(&idle) && !expects_agent(&blocked) && !expects_agent(&retired));
    }

    #[test]
    fn request_quotes_the_named_email_lines() {
        let stored = "Subject: [t] Please\r\n\r\nline three\nline four\nline five\n";
        let got = request(
            "omnigent://m",
            " Build the thing.\n- part one\n",
            Some(("m-7.txt", "3-4", stored)),
        );
        assert_eq!(
            got.unwrap(),
            "<manager_delegation from=\"omnigent://m\">\nBuild the thing.\n- part one\n</manager_delegation>\n<human_instruction authoritative=\"true\" source=\"manager_mail/m-7.txt:3-4\">\nline three\nline four\n</human_instruction>\n"
        );
        assert_eq!(
            request("m", "goal", None).unwrap(),
            format!("<manager_delegation from=\"m\">\ngoal\n</manager_delegation>\n{MANAGER_ONLY}")
        );
        for bad in ["0-2", "2", "a-b", "1-", "-3", "01-2", "1-2-3"] {
            assert!(span(bad).is_err(), "{bad}");
        }
        assert_eq!(span("12-340"), Ok((12, 340)));
        assert!(
            request("m", "g", Some(("m.txt", "5-2", stored)))
                .unwrap()
                .contains(":5-2\">\n\n</human_instruction>")
        );
    }

    #[test]
    fn first_prompt_names_the_task_and_shows_each_guide() {
        let shown = [
            ("getagentsmd".to_owned(), "\nbase rules\n".to_owned()),
            ("getagentsmd get agent_work".to_owned(), "work".to_owned()),
        ];
        assert_eq!(
            first_prompt("t_work.md", &shown, "REQUEST\n"),
            "Task tag: t_work\nTask file: t_work.md\nUse --task-file t_work.md with `amh` when it cannot tell who you are.\n$ getagentsmd\nbase rules\n$ getagentsmd get agent_work\nwork\nREQUEST\n"
        );
        assert_eq!(guides(false), ["agent_work"]);
        assert_eq!(
            guides(true),
            [
                "agent_work",
                "agent_manager",
                "agent_manager_core",
                "submanager"
            ]
        );
        assert_eq!(
            (default_tool(true), default_tool(false)),
            ("codex", "codex")
        );
    }

    #[test]
    fn start_is_recorded_in_a_new_and_in_a_done_task() {
        let mut new = new_task("t_work.md");
        record_start(
            &mut new,
            &start_fields("omnigent://main", false),
            "omnigent://s1",
            "codex",
            "REQUEST\n",
        );
        let want = [
            ("version", "v1.0.0"),
            ("status", "running"),
            ("managerat", "omnigent://main"),
            ("is_manager", "false"),
            ("runat", "omnigent://s1"),
            ("tool", "codex"),
            ("session_id", "s1"),
        ];
        assert_eq!(new.fields, task("", &want, &[], "").fields);
        assert_eq!(new.body, "REQUEST\n");
        let mut done = task(
            "t.md",
            &[
                ("status", "done"),
                ("session_id", "old"),
                ("runat", "omnigent://old"),
                ("blocked_on", "human"),
                ("tool", "codex"),
            ],
            &[],
            "history\n\n",
        );
        record_start(
            &mut done,
            &start_fields("omnigent://m", true),
            "omnigent://s2",
            "claude",
            "AGAIN\n",
        );
        let want = [
            ("status", "long_running"),
            ("runat", "omnigent://s2"),
            ("tool", "claude"),
            ("managerat", "omnigent://m"),
            ("is_manager", "true"),
            ("session_id", "s2"),
        ];
        assert_eq!(done.fields, task("", &want, &[], "").fields);
        assert_eq!(done.body, "history\n\nAGAIN\n");
        // A manager replaced by `rotate` keeps its fields; a blocked task keeps what it waits for.
        let mut blocked = task(
            "t.md",
            &[
                ("status", "blocked"),
                ("runat", "a"),
                ("blocked_on", "human"),
            ],
            &[],
            "",
        );
        record_start(&mut blocked, &[], "omnigent://s3", "codex", "(replaced)\n");
        assert_eq!(blocked.field("blocked_on"), Some("human"));
        assert_eq!(blocked.status(), "blocked");
        let mut standing = task(
            "standing.md",
            &[
                ("status", "long_running"),
                ("blocked_on", "standing role: waits for requests"),
            ],
            &["human request"],
            "history\n",
        );
        let mut fields = start_fields("omnigent://m", false);
        if let Some((_, status)) = fields.iter_mut().find(|(key, _)| key == "status") {
            *status = "long_running".to_owned();
        }
        record_start(
            &mut standing,
            &fields,
            "omnigent://s4",
            "codex",
            "handoff\n",
        );
        assert_eq!(
            standing.field("blocked_on"),
            Some("standing role: waits for requests")
        );
        assert_eq!(standing.items, ["human request"]);
        assert_eq!(standing.body, "history\n\nhandoff\n");
    }

    const MAIN: &str = "omnigent://main";

    fn made(name: &str, status: &str, at: &str, manager: &str, items: &[&str]) -> Task {
        let fields = [("status", status), ("runat", at), ("managerat", manager)];
        task(name, &fields, items, "")
    }

    #[test]
    fn manager_message_is_labelled_and_limited() {
        let worker = made("t_work.md", "running", "omnigent://s1", MAIN, &[]);
        let sent = |text: &str, state: &str| manager_message(&worker, text, state);
        let wrapped = |text: &str| {
            let text = format!("<agent_message from=\"omnigent://s1\">\n{text}\n</agent_message>");
            Ok((MAIN, text))
        };
        assert_eq!(
            sent(" need a decision ", "blocked"),
            wrapped("t_work.md is blocked:  need a decision")
        );
        assert_eq!(
            sent("all good", "done"),
            wrapped("t_work.md is done: all good")
        );
        assert_eq!(sent("hi", ""), wrapped("t_work.md: hi"));
        assert!(sent(&"é".repeat(600), "").is_ok());
        assert_eq!(
            sent(&"x".repeat(601), ""),
            Err(
                "amh: keep it under 600 characters; write details to a file and name its path"
                    .to_owned()
            )
        );
        let main = task(
            "t_main.md",
            &[("status", "running"), ("runat", MAIN)],
            &[],
            "",
        );
        assert_eq!(
            manager_message(&main, "x", ""),
            Err("amh: t_main.md records no manager".to_owned())
        );
    }

    #[test]
    fn close_marks_done_and_refuses_open_work() {
        let refused = "amh: t_sub.md still has open items or an undelivered `(pending)` block; finish, cancel, or move them first";
        let mut sub = made(
            "t_sub.md",
            "long_running",
            "omnigent://s2",
            "omnigent://s1",
            &["do x"],
        );
        sub.fields
            .push(("blocked_on".to_owned(), "human".to_owned()));
        assert_eq!(close(&mut sub), Err(refused.to_owned()));
        sub.items.clear();
        sub.body = "history\n(pending)\nundelivered\n".to_owned();
        assert_eq!(close(&mut sub), Err(refused.to_owned()));
        assert_eq!(sub.status(), "long_running");
        sub.body = "history\nundelivered\n".to_owned();
        assert_eq!(close(&mut sub), Ok(()));
        let want = made("", "done", "omnigent://s2", "omnigent://s1", &[]);
        assert_eq!((&sub.fields, sub.section()), (&want.fields, "previous:"));
        assert_eq!(
            self_closed(&sub),
            "<agent_message from=\"omnigent://s2\">\nt_sub.md had no open work left and closed itself.\n</agent_message>"
        );
    }

    #[test]
    fn agents_are_listed_and_drawn_as_a_tree() {
        let mut main = task(
            "t_main.md",
            &[("status", "long_running"), ("runat", MAIN)],
            &[],
            "",
        );
        main.fields
            .push(("is_manager".to_owned(), "true".to_owned()));
        let mut sub = made(
            "t_sub.md",
            "long_running",
            "omnigent://s2",
            "omnigent://s1",
            &["do x"],
        );
        sub.fields
            .push(("is_manager".to_owned(), "true".to_owned()));
        let tasks = [
            made(
                "t_loop.md",
                "running",
                "omnigent://l",
                "omnigent://l",
                &["spin"],
            ),
            main,
            made("t_orphan.md", "blocked", "retired", "omnigent://gone", &[]),
            sub,
            made("t_tmux.md", "blocked", "pb:3", MAIN, &[]),
            made("t_under.md", "running", "omnigent://u", "retired", &[]),
            made(
                "t_work.md",
                "running",
                "omnigent://s1",
                MAIN,
                &["🧑 do y", "z"],
            ),
        ];
        assert_eq!(
            tree(&tasks, MAIN),
            "t_main.md [manager, long_running] omnigent://main\n  t_tmux.md [worker, blocked] pb:3\n  t_work.md [worker, running] omnigent://s1\n    - 🧑 do y\n    - z\n    t_sub.md [manager, long_running] omnigent://s2\n      - do x\nnot under the main manager:\n  t_loop.md [worker, running] omnigent://l\n    - spin\n  t_orphan.md [worker, blocked] retired\n    t_under.md [worker, running] omnigent://u"
        );
        assert_eq!(tree(&[], MAIN), "");
        assert_eq!(
            listed(&tasks[6], "ready", "idle evidence"),
            "ready: t_work.md [running] omnigent://s1 idle evidence"
        );
        assert_eq!(
            listed(&tasks[2], "retired", ""),
            "retired: t_orphan.md [blocked] retired "
        );
        assert_eq!(
            problem_line(
                &tasks[4],
                Problem::Missing,
                "pb:3 is not an Omnigent session"
            ),
            "t_tmux.md at pb:3: missing (pb:3 is not an Omnigent session)"
        );
    }

    #[test]
    fn a_replaced_manager_hands_over_its_tool_and_its_reports() {
        let mut session = serde_json::json!({"harness": "claude-native", "model_override": "claude-opus-5-5", "reasoning_effort": "low", "workspace": "/w"});
        assert_eq!(
            relaunch(&session, None, None, Some("high")).unwrap(),
            ["claude", "claude-opus-5-5", "high", "/w"]
        );
        assert_eq!(
            relaunch(&session, Some("codex"), Some("m"), Some("")).unwrap(),
            ["codex", "m", "low", "/w"]
        );
        session["model_override"] = Value::Null;
        assert!(
            relaunch(&session, None, None, None)
                .unwrap_err()
                .contains("`model_override`")
        );
        assert!(relaunch(&session, None, Some("m"), None).is_ok());
        let (guides, request, record) = handover("t_main.md", MAIN, "/root", true);
        assert_eq!(
            guides,
            [
                "agent_work",
                "agent_manager",
                "agent_manager_core",
                "main_manager"
            ]
        );
        assert_eq!(
            request,
            "You replace the manager that ran at omnigent://main. Its task file t_main.md in /root holds its open work and history; take over from there.\n"
        );
        assert_eq!(
            record,
            "(manager replaced: omnigent://main handed over to a fresh agent)\n"
        );
        assert_eq!(
            handover("t_sub.md", "omnigent://s2", "/root", false).0[3],
            "submanager"
        );
        let tasks = vec![
            made("t_report.md", "running", "omnigent://s3", MAIN, &[]),
            made(
                "t_sub.md",
                "long_running",
                "omnigent://s2",
                "omnigent://s1",
                &[],
            ),
            task(
                "t_main.md",
                &[("status", "long_running"), ("runat", "omnigent://new1")],
                &[],
                "",
            ),
        ];
        let moved = repointed(tasks, MAIN, "omnigent://new1");
        assert_eq!(moved.len(), 1);
        assert_eq!(
            (moved[0].name.as_str(), moved[0].field("managerat")),
            ("t_report.md", Some("omnigent://new1"))
        );
    }

    #[test]
    fn worktree_names_each_repository() {
        assert_eq!(worktree("/r", ""), "/r: clean\n");
        assert_eq!(worktree("/r", " M a.md\n?? b\n"), "/r:\n M a.md\n?? b\n\n");
    }

    #[test]
    fn only_text_added_at_the_end_of_a_task_file_is_taken() {
        let before = "---\nstatus: running\n---\nbody\n";
        assert_eq!(appended(before, before), Ok(None));
        assert_eq!(
            appended(before, "---\nstatus: running\n---\nbody\n\n\n"),
            Ok(None)
        );
        assert_eq!(
            appended(before, &format!("{before}\nPlease also do X.\n\nAnd Y.\n")),
            Ok(Some("Please also do X.\n\nAnd Y.\n".to_owned()))
        );
        assert!(appended(before, "---\nstatus: done\n---\nbody\nnew\n").is_err());
        assert!(appended(before, "---\nstatus: running\n---\nbody and more\n").is_err());
    }

    #[test]
    fn a_task_is_named_with_or_without_md_or_by_its_path() {
        for given in [
            "t_work",
            "t_work.md",
            "/root/logs/t_work.md",
            "./t_work",
            "logs/t_work",
        ] {
            assert_eq!(task_name(given), "t_work.md");
        }
        assert_eq!(task_name("omnigent://abc"), "omnigent://abc");
        assert_eq!(task_name(""), "");
    }

    #[test]
    fn a_task_log_merges_both_logs_in_time_order() {
        let watch = "2026-10-06 10:00:05 mail m-1.txt 'x' -> t_a.md\n2026-10-06 10:00:09 delivered in t_b.md to omnigent://b\n";
        let actions = "2026-10-06 10:00:07 omnigent://a exit=0 {\"action\":\"human\"}\n2026-10-06 10:00:01 user:x exit=0 {\"task\":\"t_a.md\"}\n";
        assert_eq!(
            task_log(watch, actions, "t_a.md", "omnigent://a", 2),
            "2026-10-06 10:00:05 mail m-1.txt 'x' -> t_a.md\n2026-10-06 10:00:07 omnigent://a exit=0 {\"action\":\"human\"}\n"
        );
    }

    #[test]
    fn a_replacement_gets_the_original_request_the_message_and_open_items() {
        let mut task = new_task("t.md");
        let first = request("omnigent://m", "Do X.", None).unwrap();
        task.body = format!(
            "intro\n\n{first}\nlater\n<manager_delegation from=\"omnigent://m\">\nDo Y.\n</manager_delegation>\n"
        );
        assert_eq!(
            original_request(&task.body).as_deref(),
            Some(first.as_str())
        );
        let text = replacement(
            &task,
            "omnigent://old",
            "/r",
            "<human_instruction>replace this agent</human_instruction>",
            "",
        );
        assert!(
            text.starts_with(&first)
                && text.contains("You replace the agent that ran this task at omnigent://old")
        );
        assert!(
            text.contains("replace this agent</human_instruction>")
                && text.ends_with("(none)\n")
                && !text.contains("Do Y.")
        );
    }
}
