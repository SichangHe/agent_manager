//! Exercise the installed command paths against isolated real markdown records, without connectors.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Workspace(PathBuf);
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
impl Workspace {
    fn run(&self, args: &[&str]) -> Result<Output, std::io::Error> {
        Command::new(env!("CARGO_BIN_EXE_amh"))
            .args(args)
            .env("AMH_STATE_DIR", self.0.join("state"))
            .env("AMH_CFG_DIR", self.0.join("config"))
            .env_remove("OMO_AGENT_TASK_FILE")
            .env_remove("OMNIGENT_RUNNER_PRIMARY_SESSION_ID")
            .output()
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
    fs::write(
        root.join("h.md"),
        "---\nstatus: running\nrunat: omnigent://h\npending_task_items:\n  - 🧑 human request\n---\n",
    )?;
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
    let events = fs::read_to_string(workspace.0.join("state/amh-events.jsonl"))?;
    assert!(events.contains("request blocked"));
    assert!(events.contains("request moved"));
    assert!(events.contains("request resolved"));
    assert!(events.contains("\"actor\":\"omnigent://h\""));
    Ok(())
}
