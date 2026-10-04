//! What starting an agent and judging its health mean for a task record: pure decisions and texts, no input or output.

use crate::tasks::Task;

/// The `getagentsmd` guides every manager starts with.
pub const MANAGER_GUIDES: [&str; 3] = ["agent_work", "agent_manager", "agent_manager_core"];

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
    task.status() != "blocked" && task.address() != "retired"
}

/// The problem of an active task whose agent is in `state`: gone, failed, stuck, or idle with open items.
pub fn problem_of(task: &Task, state: &str) -> Option<Problem> {
    let waiting = task.status() == "long_running" && task.field("blocked_on").is_some();
    match state {
        "missing" => Some(Problem::Missing),
        "error" => Some(Problem::Error),
        "stuck" => Some(Problem::Stuck),
        "ready" if !task.items.is_empty() && !waiting => Some(Problem::Idle),
        _ => None,
    }
}

/// The tool a new agent runs when none is named.
pub fn default_tool(as_manager: bool) -> &'static str {
    if as_manager { "claude" } else { "codex" }
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
    let session = address
        .strip_prefix(crate::omnigent::PREFIX)
        .unwrap_or(address);
    task.fields.retain(|(key, _)| key != "session_id");
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
    if task.status() != "blocked" {
        task.fields.retain(|(key, _)| key != "blocked_on");
    }
    let before = if task.body.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n\n", task.body.trim_end_matches('\n'))
    };
    task.body = before + record;
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
        assert_eq!(problem_of(&waiting, "ready"), None);
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
            "<manager_delegation from=\"m\">\ngoal\n</manager_delegation>\n"
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
            ("claude", "codex")
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
    }
}
