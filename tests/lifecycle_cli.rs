//! Exercise the installed command paths against isolated real markdown records, without connectors.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Workspace(PathBuf);

#[test]
fn task_start_resolves_parent_before_launch_without_native_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let workspace =
        Workspace(std::env::temp_dir().join(format!("amh-parent-{}-{now}", std::process::id())));
    let root = workspace.0.join("records");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(workspace.0.join("state"))?;
    fs::write(
        workspace.0.join("state/env.toml"),
        format!(
            "work_logs = {}\n",
            serde_json::to_string(&root.to_string_lossy())?
        ),
    )?;
    fs::write(
        root.join("TODO.md"),
        "current:\nparent.md\nempty.md\n\nprevious:\n",
    )?;
    fs::write(
        root.join("parent.md"),
        "---\nstatus: long_running\nrunat: omnigent://parent\nis_manager: true\npending_task_items: []\n---\n",
    )?;
    fs::write(
        root.join("empty.md"),
        "---\nstatus: long_running\nrunat: \npending_task_items: []\n---\n",
    )?;
    let prompt = workspace.0.join("prompt.md");
    fs::write(&prompt, "Preserve the existing human goal.\n")?;
    let args = [
        "task",
        "start",
        "child.md",
        "--tool",
        "codex",
        "--dir",
        root.to_str().ok_or("invalid path")?,
        "--prompt",
        prompt.to_str().ok_or("invalid path")?,
    ];
    for (flags, native, launch_env) in [
        (vec!["--task-file", "parent.md"], None, None),
        (vec![], None, Some("parent.md")),
        (
            vec!["--task-file", "missing.md"],
            Some("native-parent"),
            None,
        ),
        (
            vec![
                "--task-file",
                "missing.md",
                "--manager",
                "omnigent://explicit",
            ],
            None,
            None,
        ),
    ] {
        let mut command = workspace.command(&args);
        command.args(flags).env("PATH", "/usr/bin:/bin");
        if let Some(native) = native {
            command.env("OMNIGENT_RUNNER_PRIMARY_SESSION_ID", native);
        }
        if let Some(task) = launch_env {
            command.env("OMO_AGENT_TASK_FILE", task);
        }
        let output = command.output()?;
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("getagentsmd"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.join("child.md").exists());
    }
    let output = workspace
        .command(&args)
        .args(["--task-file", "empty.md"])
        .output()?;
    assert!(String::from_utf8_lossy(&output.stderr).contains("has no runat address"));
    Ok(())
}

#[test]
fn pb_labels_reject_wrong_owner_before_item_changes() -> Result<(), Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let workspace =
        Workspace(std::env::temp_dir().join(format!("amh-pb-{}-{now}", std::process::id())));
    let root = workspace.0.join("records");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(workspace.0.join("state"))?;
    fs::create_dir_all(workspace.0.join("config"))?;
    fs::write(
        workspace.0.join("state/env.toml"),
        format!(
            "work_logs = {}\n",
            serde_json::to_string(&root.to_string_lossy())?
        ),
    )?;
    fs::write(
        root.join("TODO.md"),
        "current:\nother.md\nstock_news_watch.md\n\nprevious:\n",
    )?;
    for task in ["other", "stock_news_watch"] {
        fs::write(
            root.join(format!("{task}.md")),
            "---\nstatus: running\nrunat: omnigent://pb-test\npending_task_items: []\n---\n",
        )?;
    }
    for (task, transport) in [("other.md", "zulip"), ("other.md", "email")] {
        fs::write(
            workspace.0.join("config/config.toml"),
            format!("[human]\ntransport = \"{transport}\"\n"),
        )?;
        let before = workspace.ok(&["task", "show", task])?;
        let output = workspace.run(&[
            "tell",
            "human",
            "--task-file",
            task,
            "--report-label",
            "pb urgent",
            "--subject",
            "Report",
            "--add",
            "must not persist",
            "body",
        ])?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires stock_news_watch"));
        assert_eq!(workspace.ok(&["task", "show", task])?, before);
    }
    Ok(())
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
impl Workspace {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_amh"));
        command
            .args(args)
            .env("AMH_STATE_DIR", self.0.join("state"))
            .env("AMH_CFG_DIR", self.0.join("config"))
            .env_remove("OMO_AGENT_TASK_FILE")
            .env_remove("OMNIGENT_RUNNER_PRIMARY_SESSION_ID");
        command
    }
    fn run(&self, args: &[&str]) -> Result<Output, std::io::Error> {
        self.command(args).output()
    }
    fn ok(&self, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
        let output = self.run(args)?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }
}

#[test]
fn tell_agent_resolves_task_sender_without_native_identity()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, Read, Write};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let workspace =
        Workspace(std::env::temp_dir().join(format!("amh-sender-{}-{now}", std::process::id())));
    let root = workspace.0.join("records");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(workspace.0.join("state"))?;
    fs::write(
        workspace.0.join("state/env.toml"),
        format!(
            "work_logs = {}\n",
            serde_json::to_string(&root.to_string_lossy())?
        ),
    )?;
    fs::write(
        root.join("TODO.md"),
        "current:\ncaller.md omnigent://caller\nempty.md\n\nhuman pending:\n\nlow priority:\n\nprevious:\n",
    )?;
    fs::write(
        root.join("caller.md"),
        "---\nstatus: running\nrunat: omnigent://caller\npending_task_items: []\n---\n",
    )?;
    fs::write(
        root.join("empty.md"),
        "---\nstatus: running\nrunat: \npending_task_items: []\n---\n",
    )?;
    let args = ["tell", "agent", "omnigent://recipient", "hello"];
    for (task_args, native, script, expected) in [
        (
            vec!["--task-file", "caller.md"],
            None,
            None,
            "omnigent://caller",
        ),
        (vec![], None, None, "omnigent://caller"),
        (
            vec!["--task-file", "missing.md"],
            None,
            Some("cron"),
            "script cron",
        ),
        (
            vec!["--task-file", "missing.md"],
            Some("native"),
            Some("cron"),
            "omnigent://native",
        ),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        fs::write(
            workspace.0.join("state/env.toml"),
            format!(
                "work_logs = {}\n[omnigent]\nurl = {}\n",
                serde_json::to_string(&root.to_string_lossy())?,
                serde_json::to_string(&url)?
            ),
        )?;
        let server = std::thread::spawn(move || -> Result<String, std::io::Error> {
            let mut message = String::new();
            // The first answer is the move check, which finds no move.
            for response in [
                "{\"status\":\"idle\"}",
                "{\"status\":\"idle\"}",
                "{\"queued\":true}",
            ] {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
                let mut reader = std::io::BufReader::new(stream.try_clone()?);
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        size = value.trim().parse().map_err(std::io::Error::other)?;
                    }
                }
                let mut body = vec![0; size];
                reader.read_exact(&mut body)?;
                if !body.is_empty() {
                    message = String::from_utf8(body).map_err(std::io::Error::other)?;
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )?;
            }
            Ok(message)
        });
        let mut command = workspace.command(&args);
        if task_args.is_empty() {
            command.env("OMO_AGENT_TASK_FILE", "caller.md");
        } else {
            command.args(task_args);
        }
        if let Some(native) = native {
            command.env("OMNIGENT_RUNNER_PRIMARY_SESSION_ID", native);
        }
        if let Some(script) = script {
            command.args(["--from", script]);
        }
        let output = command.output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let message = server.join().map_err(|_| "test server panicked")??;
        assert!(message.contains(expected), "{message}");
        assert!(message.contains("hello"), "{message}");
    }
    let empty = workspace.run(&[
        "tell",
        "agent",
        "omnigent://recipient",
        "hello",
        "--task-file",
        "empty.md",
    ])?;
    assert!(!empty.status.success());
    let error = String::from_utf8_lossy(&empty.stderr);
    assert!(
        error.contains("caller task empty") && error.contains("has no runat address"),
        "{error}"
    );
    let missing = workspace.run(&args)?;
    assert!(!missing.status.success());
    let error = String::from_utf8_lossy(&missing.stderr);
    assert!(error.contains("--task-file NAME.md"));
    assert!(error.contains("--from NAME"));
    Ok(())
}

#[test]
fn task_give_uses_explicit_caller_and_preserves_parked_task()
-> Result<(), Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let workspace =
        Workspace(std::env::temp_dir().join(format!("amh-give-{}-{now}", std::process::id())));
    let root = workspace.0.join("records");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(workspace.0.join("state"))?;
    fs::write(
        workspace.0.join("state/env.toml"),
        format!(
            "work_logs = {}\n",
            serde_json::to_string(&root.to_string_lossy())?
        ),
    )?;
    fs::write(
        root.join("TODO.md"),
        "current:\nmanager.md omnigent://manager\n\nhuman pending:\n\nlow priority:\nparked.md omnigent://parked\n\nprevious:\n",
    )?;
    fs::write(
        root.join("manager.md"),
        "---\nstatus: long_running\nrunat: omnigent://manager\nis_manager: true\npending_task_items: []\n---\n",
    )?;
    fs::write(
        root.join("parked.md"),
        "---\nstatus: blocked\nrunat: omnigent://parked\nblocked_on: spare quota\nlisted_under: low priority\nagent_stopped: true\npending_task_items: []\n---\n",
    )?;
    workspace.ok(&[
        "task",
        "give",
        "parked.md",
        "first requirement",
        "--task-file",
        "manager.md",
    ])?;
    workspace.ok(&[
        "--task-file",
        "manager.md",
        "task",
        "give",
        "parked.md",
        "second requirement",
    ])?;
    let saved = fs::read_to_string(root.join("parked.md"))?;
    for expected in [
        "status: blocked",
        "blocked_on: spare quota",
        "listed_under: low priority",
        "agent_stopped: true",
        "first requirement",
        "second requirement",
        "\"creator\":\"manager.md\"",
    ] {
        assert!(saved.contains(expected), "missing {expected}: {saved}");
    }
    assert!(
        fs::read_to_string(root.join("TODO.md"))?
            .contains("low priority:\nparked.md omnigent://parked")
    );
    Ok(())
}

#[test]
fn plain_list_entries_survive_exports_and_task_placement() -> Result<(), Box<dyn std::error::Error>>
{
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let workspace =
        Workspace(std::env::temp_dir().join(format!("amh-list-{}-{now}", std::process::id())));
    let root = workspace.0.join("records");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(workspace.0.join("state"))?;
    fs::write(
        workspace.0.join("state/env.toml"),
        format!(
            "work_logs = {}\n",
            serde_json::to_string(&root.to_string_lossy())?
        ),
    )?;
    fs::write(
        root.join("TODO.md"),
        "current:\na.md omnigent://a\n\nhuman pending:\n\nlow priority:\n\nprevious:\n",
    )?;
    fs::write(
        root.join("a.md"),
        "---\nstatus: running\nrunat: omnigent://a\npending_task_items: []\n---\n",
    )?;
    let entry = "Exxact server access below the OS";
    let add = ["task", "list-add", entry, "--under", "low priority"];
    workspace.ok(&add)?;
    let expected = fs::read_to_string(root.join("TODO.md"))?;
    assert!(expected.contains(&format!("low priority:\n- {entry}\n")));
    assert_eq!(
        fs::read_dir(&root)?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "md"))
            .count(),
        2
    );
    fs::write(root.join("TODO.md"), "stale export")?;
    workspace.ok(&add)?;
    assert_eq!(fs::read_to_string(root.join("TODO.md"))?, expected);
    workspace.ok(&["task", "role", "a", "manager"])?;
    let promoted = fs::read_to_string(root.join("a.md"))?;
    assert!(
        promoted.contains("is_manager: 'true'")
            || promoted.contains("is_manager: \"true\"")
            || promoted.contains("is_manager: true"),
        "{promoted}"
    );
    assert!(promoted.contains("status: running") && promoted.contains("runat: omnigent://a"));
    workspace.ok(&["task", "role", "a", "worker"])?;
    assert!(fs::read_to_string(root.join("a.md"))?.contains("false"));
    workspace.ok(&["task", "place", "a", "--under", "low priority"])?;
    let placed = fs::read_to_string(root.join("TODO.md"))?;
    assert!(placed.contains(&format!("- {entry}")));
    workspace.ok(&["task", "list-add", entry, "--under", "human pending"])?;
    let moved = fs::read_to_string(root.join("TODO.md"))?;
    assert_eq!(moved.matches(entry).count(), 1);
    assert!(moved.contains(&format!("human pending:\n- {entry}\n")));
    workspace.ok(&["task", "check"])?;
    for invalid in ["", "one\ntwo", "one\rtwo"] {
        assert!(
            !workspace
                .run(&["task", "list-add", invalid, "--under", "low priority"])?
                .status
                .success()
        );
        assert_eq!(fs::read_to_string(root.join("TODO.md"))?, moved);
    }
    Ok(())
}

#[test]
fn commands_preserve_items_ids_creator_and_cycle_checks() -> Result<(), Box<dyn std::error::Error>>
{
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let workspace =
        Workspace(std::env::temp_dir().join(format!("amh-lifecycle-{}-{now}", std::process::id())));
    fs::create_dir_all(workspace.0.join("state"))?;
    let root = workspace.0.join("records");
    fs::create_dir_all(&root)?;
    fs::write(
        workspace.0.join("state/env.toml"),
        format!(
            "work_logs = {}\n",
            serde_json::to_string(&root.to_string_lossy())?
        ),
    )?;
    fs::write(
        root.join("TODO.md"),
        "current:\na.md omnigent://a\nb.md omnigent://b\nh.md omnigent://h\n\nhuman pending:\n\nlow priority:\n\nprevious:\n",
    )?;
    for name in ["a", "b", "h"] {
        fs::write(
            root.join(format!("{name}.md")),
            format!(
                "---\nstatus: running\nrunat: omnigent://{name}\nmanagerat: omnigent://manager\nis_manager: false\npending_task_items: []\n---\n"
            ),
        )?;
    }
    fs::write(
        root.join("h.md"),
        "---\nstatus: running\nrunat: omnigent://h\npending_task_items:\n  - 🧑 human request\n---\n",
    )?;
    workspace.ok(&["todo", "add", "change A", "--task-file", "a"])?;
    workspace.ok(&["todo", "add", "change B", "--task-file", "b"])?;
    assert!(
        workspace
            .ok(&["todo", "list", "--task-file", "a"])?
            .contains("a.md#1 [added] change A")
    );
    workspace.ok(&[
        "todo",
        "wait",
        "a.md#1",
        "--for",
        "b.md#1",
        "--task-file",
        "a",
    ])?;
    let cycle = workspace.run(&[
        "todo",
        "wait",
        "b.md#1",
        "--for",
        "a.md#1",
        "--task-file",
        "b",
    ])?;
    assert!(!cycle.status.success());
    assert!(String::from_utf8_lossy(&cycle.stderr).contains("cycle"));
    workspace.ok(&["todo", "take", "a.md#1", "--task-file", "b"])?;
    let b = fs::read_to_string(root.join("b.md"))?;
    assert!(b.contains("a.md#1"));
    assert!(b.contains("change A"));
    assert!(b.contains("creator\":\"a.md"));
    workspace.ok(&["todo", "add", "next A", "--task-file", "a"])?;
    assert!(
        workspace
            .ok(&["todo", "list", "--task-file", "a"])?
            .contains("a.md#2")
    );
    let close = workspace.run(&["task", "close", "a"])?;
    assert!(!close.status.success());
    assert!(String::from_utf8_lossy(&close.stderr).contains("stay alive"));
    workspace.ok(&[
        "todo",
        "done",
        "human request",
        "--task-file",
        "h",
        "--evidence",
        "requested behavior delivered",
        "--quiet-because",
        "do not email this completion",
    ])?;
    let h = fs::read_to_string(root.join("h.md"))?;
    assert!(h.contains("pending_task_items: []"));
    assert!(h.contains("stage\":\"resolved"));
    let database = rusqlite::Connection::open(workspace.0.join("state/amh.sqlite3"))?;
    let mut query = database.prepare("SELECT kind || ' ' || actor FROM events ORDER BY id")?;
    let events = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    assert!(events.contains("request blocked"));
    assert!(events.contains("request moved"));
    assert!(events.contains("request resolved"));
    assert!(events.contains("omnigent://h"));
    Ok(())
}

#[test]
fn waits_need_an_item_or_date_and_the_human_owns_items() -> Result<(), Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let workspace =
        Workspace(std::env::temp_dir().join(format!("amh-waits-{}-{now}", std::process::id())));
    fs::create_dir_all(workspace.0.join("state"))?;
    let root = workspace.0.join("records");
    fs::create_dir_all(&root)?;
    fs::write(
        workspace.0.join("state/env.toml"),
        format!(
            "work_logs = {}\n",
            serde_json::to_string(&root.to_string_lossy())?
        ),
    )?;
    fs::write(
        root.join("TODO.md"),
        "current:\na.md omnigent://a\n\nhuman pending:\n\nlow priority:\n\nprevious:\n",
    )?;
    let old_wait = r#"[{"added_at":null,"blockers":["on: human deciding on the draft"],"creator":"human","evidence":"","id":"a.md#2","resolved_at":null,"source":"test","stage":"added","text":"🧑 Post the draft"}]"#;
    fs::write(
        root.join("a.md"),
        format!(
            "---\nstatus: running\nrunat: omnigent://a\nmanagerat: omnigent://manager\nis_manager: false\npending_task_items:\n  - 🧑 Minimize diffs\n  - 🧑 Post the draft\nrequest_records: '{old_wait}'\nrequest_next: 2\n---\n"
        ),
    )?;
    // Exact duplicates, up to quotes, are skipped.
    let duplicate = workspace.run(&[
        "todo",
        "add",
        "“Minimize diffs”",
        "--from-human",
        "--task-file",
        "a",
    ])?;
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already open"));
    assert_eq!(
        fs::read_to_string(root.join("a.md"))?
            .matches("Minimize diffs")
            .count(),
        1
    );
    let listed = workspace.ok(&["todo", "list", "--task-file", "a"])?;
    let id = listed
        .lines()
        .find(|line| line.contains("Minimize"))
        .and_then(|line| line.split_whitespace().next())
        .ok_or("no item")?
        .to_owned();
    // Waiting on someone in words is refused.
    let refused = workspace.run(&[
        "todo",
        "wait",
        &id,
        "--on",
        "human review",
        "--task-file",
        "a",
    ])?;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("amh todo ask-human"));
    let refused = workspace.run(&["task", "status", "a", "blocked", "--on", "human review"])?;
    assert!(!refused.status.success());
    // The human owns an item; the agent waits for it; the human resolves it.
    let asked = workspace.ok(&["todo", "ask-human", "Review the plan", "--task-file", "a"])?;
    let human_item = asked.lines().next().ok_or("no ID")?.to_owned();
    assert_eq!(human_item, "human.md#1");
    workspace.ok(&[
        "todo",
        "wait",
        &id,
        "--for",
        &human_item,
        "--task-file",
        "a",
    ])?;
    assert!(
        workspace
            .ok(&["human"])?
            .contains("human.md#1 [added] Review the plan (asked by a.md)")
    );
    workspace.ok(&["task", "status", "a", "blocked", "--on", &human_item])?;
    workspace.ok(&["human", "--done", &human_item])?;
    assert!(
        !workspace
            .ok(&["todo", "list", "--task-file", "a"])?
            .contains("waits for human.md#1")
    );
    // An old wait in words on the human becomes the human's item and a dependency.
    let converted = workspace.ok(&["todo", "wait", "a.md#2", "--ask-human", "--task-file", "a"])?;
    assert!(converted.contains("human.md#2: human deciding on the draft"));
    assert!(
        workspace
            .ok(&["todo", "list", "--task-file", "a"])?
            .contains("🧑 Post the draft (waits for human.md#2)")
    );
    // Dates.
    workspace.ok(&[
        "todo",
        "wait",
        "a.md#2",
        "--until",
        "2099-01-01",
        "--task-file",
        "a",
    ])?;
    assert!(
        workspace
            .ok(&["todo", "list", "--task-file", "a"])?
            .contains("until: 2099-01-0")
    );
    workspace.ok(&[
        "task",
        "status",
        "a",
        "blocked",
        "--until",
        "2099-01-01 09:00",
    ])?;
    assert!(fs::read_to_string(root.join("a.md"))?.contains("blocked_on: until 2099-01-01"));
    Ok(())
}

#[test]
fn priorities_order_tasks_for_clients() -> Result<(), Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let workspace =
        Workspace(std::env::temp_dir().join(format!("amh-priority-{}-{now}", std::process::id())));
    fs::create_dir_all(workspace.0.join("state"))?;
    let root = workspace.0.join("records");
    fs::create_dir_all(&root)?;
    fs::write(
        workspace.0.join("state/env.toml"),
        format!(
            "work_logs = {}\n",
            serde_json::to_string(&root.to_string_lossy())?
        ),
    )?;
    fs::write(
        root.join("TODO.md"),
        "current:\n\nhuman pending:\n\nlow priority:\n\nprevious:\n",
    )?;
    for name in ["a", "b", "c", "d"] {
        fs::write(
            root.join(format!("{name}.md")),
            format!(
                "---\nstatus: running\nrunat: omnigent://{name}\npending_task_items: []\n---\n"
            ),
        )?;
    }
    workspace.ok(&["task", "priority", "c", "1000"])?;
    workspace.ok(&["task", "priority", "a", "2000"])?;
    workspace.ok(&["task", "priority", "d", "1500"])?;
    workspace.ok(&["task", "priority", "d", "none"])?;
    workspace.ok(&["task", "priority", "d", "1500"])?;
    assert!(
        workspace
            .run(&["task", "priority", "d", "high"])?
            .status
            .code()
            != Some(0)
    );
    assert_eq!(
        workspace.ok(&["task", "priorities", "--json"])?,
        "{\"c\":1000,\"d\":1500,\"a\":2000,\"b\":null}\n"
    );
    assert_eq!(
        workspace.ok(&["task", "priorities"])?,
        "1000 c\n1500 d\n2000 a\n- b\n"
    );
    Ok(())
}
