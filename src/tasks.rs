//! What the todo and task actions do to task records: pure changes to plain data, no input or output.
// 🧑 "make only the logic part synchronous and let's also have the logic separate from IO"

use crate::taskfile::{self, HUMAN_MARK, SECTIONS};

/// One task file: its name, frontmatter fields in file order, open items, and free-text body.
#[derive(Clone, Debug)]
pub struct Task {
    pub name: String,
    pub fields: Vec<(String, String)>,
    pub items: Vec<String>,
    pub body: String,
}

impl Task {
    pub fn parse(name: &str, text: &str) -> Result<Self, String> {
        let (fields, items, body) = taskfile::parse(name, text)?;
        Ok(Self {
            name: name.to_owned(),
            fields,
            items,
            body,
        })
    }

    pub fn render(&self) -> String {
        taskfile::render(&self.fields, &self.items, &self.body)
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, value)| value.as_str())
    }

    pub fn status(&self) -> &str {
        self.field("status").unwrap_or_default()
    }

    /// Where the task's agent runs.
    pub fn address(&self) -> &str {
        self.field("runat").unwrap_or_default()
    }

    /// The file name without `.md`: the `[tag]` of the task's email subjects.
    pub fn tag(&self) -> &str {
        self.name.strip_suffix(".md").unwrap_or(&self.name)
    }

    pub fn is_active(&self) -> bool {
        ["running", "long_running", "blocked"].contains(&self.status())
    }

    /// The task-list section the task belongs in.
    // 🧑 "None of the currently listed as human pending tasks is actually pending on the human."
    pub fn section(&self) -> &'static str {
        let waits_on_human = self.status() == "blocked"
            && self
                .field("blocked_on")
                .is_some_and(|on| on.to_lowercase() == "human");
        match self.status() {
            "done" => "previous:",
            _ if waits_on_human => "human pending:",
            _ => "current:",
        }
    }

    /// Append a one-line `(note)` to the body, after a blank line so it never joins a pending block.
    fn note(&mut self, text: &str) {
        let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        self.body = format!("{}\n\n({line})\n", self.body.trim_end_matches('\n'));
    }
}

/// Append one open item; return it, or `None` when the task already has it.
pub fn add_item(task: &mut Task, item: &str, from_human: bool) -> Result<Option<String>, String> {
    if task.status() == "done" {
        return Err(format!(
            "amh: {} is done; start a new task instead",
            task.name
        ));
    }
    let marked = taskfile::mark(item, from_human)?;
    if task.items.contains(&marked) {
        return Ok(None);
    }
    task.items.push(marked.clone());
    Ok(Some(marked))
}

/// Remove one finished or cancelled item, leaving a one-line record in the body.
pub fn remove_item(task: &mut Task, item: &str, evidence: &str) -> Result<(), String> {
    let at = task
        .items
        .iter()
        .position(|open| open == item)
        .ok_or_else(|| {
            format!(
                "amh: {} has no such open item; copy it exactly from `amh todo list`",
                task.name
            )
        })?;
    task.items.remove(at);
    task.note(&format!("removed open item: {evidence}"));
    Ok(())
}

/// Reword one open item, keeping its authorship mark.
pub fn replace_item(task: &mut Task, old: &str, new: &str) -> Result<(), String> {
    let at = task
        .items
        .iter()
        .position(|open| open == old)
        .ok_or("amh: no such open item; copy it exactly from `amh todo list`")?;
    task.items[at] = taskfile::mark(new, old.starts_with(HUMAN_MARK))?;
    Ok(())
}

/// Move one open item from `source` to `to`.
pub fn move_item(source: &mut Task, to: &mut Task, item: &str) -> Result<(), String> {
    let at = source
        .items
        .iter()
        .position(|open| open == item)
        .filter(|_| to.status() != "done" && source.name != to.name);
    let Some(at) = at else {
        return Err(format!(
            "amh: {} has no such open item, or {} is done or the same task",
            source.name, to.name
        ));
    };
    source.items.remove(at);
    if !to.items.iter().any(|open| open == item) {
        to.items.push(item.to_owned());
    }
    Ok(())
}

/// Append a manager note.
pub fn add_note(task: &mut Task, text: &str) {
    task.note(text);
}

/// Set the status and what a blocked task waits for.
pub fn set_status(task: &mut Task, status: &str, blocked_on: Option<&str>) -> Result<(), String> {
    let waits = blocked_on.is_some_and(|on| !on.is_empty());
    if (status == "blocked") != waits && status != "long_running" {
        return Err(
            "amh: `blocked` needs --on saying what it waits for; `running` takes no --on"
                .to_owned(),
        );
    }
    task.fields.retain(|(key, _)| key != "blocked_on");
    match task.fields.iter_mut().find(|(key, _)| key == "status") {
        Some(field) => field.1 = status.to_owned(),
        None => task.fields.push(("status".to_owned(), status.to_owned())),
    }
    if let Some(on) = blocked_on.filter(|_| waits) {
        task.fields.push((
            "blocked_on".to_owned(),
            on.split_whitespace().collect::<Vec<_>>().join(" "),
        ));
    }
    Ok(())
}

/// A task as `amh task show` prints it.
pub fn show(task: &Task) -> String {
    let fields: String = task
        .fields
        .iter()
        .map(|(key, value)| format!("  {key}: {value}\n"))
        .collect();
    let items: String = task
        .items
        .iter()
        .map(|item| format!("  - {item}\n"))
        .collect();
    format!("{}\n{fields}{items}", task.name)
}

/// The email telling the human that their requests were created or deleted on a task's list, when any item is theirs.
// 🧑 “Pending items originated from the human need emails, ones from agents do not.”
pub fn human_notice(change: &str, items: &[String]) -> Option<String> {
    let theirs: String = items
        .iter()
        .filter_map(|item| item.strip_prefix(HUMAN_MARK))
        .map(|item| format!("- {item}\n"))
        .collect();
    (!theirs.is_empty()).then(|| format!("pending item {change}:\n{theirs}"))
}

/// Every disagreement between the active task files and the task list.
// 🧑 "Maybe we should have a validation script that agents should run after editing task file/TODO file?"
pub fn check(list: &str, active: &[Task]) -> Vec<String> {
    let mut rows: Vec<(&str, Vec<(String, &str)>)> = Vec::new();
    let mut section = String::new();
    for row in list.lines() {
        let heading = row.trim().to_lowercase();
        if SECTIONS.contains(&heading.as_str()) {
            section = heading;
            continue;
        }
        let (name, rest) = row.split_once(' ').unwrap_or((row, ""));
        let is_row = name.len() > 3 && name.ends_with(".md") && !name.contains(char::is_whitespace);
        if !section.is_empty() && is_row {
            let placed = (section.clone(), rest.split(' ').next().unwrap_or_default());
            match rows.iter_mut().find(|(known, _)| *known == name) {
                Some(found) => found.1.push(placed),
                None => rows.push((name, vec![placed])),
            }
        }
    }
    let mut findings = Vec::new();
    for task in active {
        let placed = rows
            .iter()
            .find(|(name, _)| *name == task.name)
            .map(|(_, placed)| placed.as_slice())
            .unwrap_or_default();
        match placed {
            [] if task.status() == "blocked" => {}
            [(section, address)] if section == task.section() && *address == task.address() => {}
            [(section, address)] => findings.push(format!(
                "{}: TODO.md has it under `{section}` at {address}, the task file says `{}` at {}",
                task.name,
                task.section(),
                task.address()
            )),
            _ => findings.push(format!(
                "{}: {} rows in TODO.md, want 1",
                task.name,
                placed.len()
            )),
        }
    }
    // 🧑 "You have current tasks in \"previous\" in TODO.md ... Act to prevent that in the future"
    for (name, placed) in &rows {
        let open = placed.iter().any(|(section, _)| section != "previous:");
        if open && !active.iter().any(|task| task.name == *name) {
            findings.push(format!(
                "{name}: listed as open work in TODO.md but it is done or not a task file"
            ));
        }
    }
    let mut shared: Vec<(&str, Vec<&str>)> = Vec::new();
    for task in active {
        match shared
            .iter_mut()
            .find(|(address, _)| *address == task.address())
        {
            Some(found) => found.1.push(&task.name),
            None => shared.push((task.address(), vec![&task.name])),
        }
    }
    for (address, names) in shared {
        if names.len() > 1 && address != "retired" {
            findings.push(format!(
                "{address}: several active tasks run there: {}",
                names.join(", ")
            ));
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::tests::task;

    const NO_ITEM: &str = "no such open item";

    fn worker() -> Task {
        let fields = [
            ("version", "v1.0.0"),
            ("status", "running"),
            ("managerat", "omnigent://main"),
            ("runat", "omnigent://s1"),
        ];
        task("t_work.md", &fields, &[], "request\n")
    }

    #[test]
    fn items_are_added_reworded_and_removed() {
        let mut work = worker();
        let added = add_item(&mut work, "write   the\n code", false);
        assert_eq!(added, Ok(Some("write the code".to_owned())));
        assert_eq!(add_item(&mut work, "write the code", false), Ok(None));
        let theirs = add_item(&mut work, "🧑 ship it", true).unwrap();
        assert_eq!(theirs.as_deref(), Some("🧑 ship it"));
        assert_eq!(work.items, ["write the code", "🧑 ship it"]);
        assert!(
            add_item(&mut work, " 🧑 ", true)
                .unwrap_err()
                .contains("empty")
        );
        assert_eq!(
            replace_item(&mut work, "🧑 ship it", "ship it today"),
            Ok(())
        );
        assert_eq!(work.items, ["write the code", "🧑 ship it today"]);
        assert!(
            replace_item(&mut work, "absent", "x")
                .unwrap_err()
                .contains(NO_ITEM)
        );
        assert!(
            remove_item(&mut work, "absent", "e")
                .unwrap_err()
                .contains(NO_ITEM)
        );
        assert_eq!(
            remove_item(&mut work, "write the code", "tests\npass"),
            Ok(())
        );
        assert_eq!(work.items, ["🧑 ship it today"]);
        assert_eq!(work.body, "request\n\n(removed open item: tests pass)\n");
        let mut done = task("t_done.md", &[("status", "done"), ("runat", "x")], &[], "");
        assert_eq!(
            add_item(&mut done, "x", false),
            Err("amh: t_done.md is done; start a new task instead".to_owned())
        );
    }

    #[test]
    fn only_the_humans_items_are_announced() {
        let items = ["🧑 ship it".to_owned(), "write the code".to_owned()];
        assert_eq!(
            human_notice("created", &items).as_deref(),
            Some("pending item created:\n- ship it\n")
        );
        assert_eq!(
            human_notice("deleted", &items[..1]).as_deref(),
            Some("pending item deleted:\n- ship it\n")
        );
        assert_eq!(human_notice("deleted", &items[1..]), None);
    }

    #[test]
    fn a_task_is_shown_moved_from_and_noted() {
        let mut work = worker();
        work.items = vec!["do x".to_owned(), "🧑 do y".to_owned()];
        assert_eq!(
            show(&work),
            "t_work.md\n  version: v1.0.0\n  status: running\n  managerat: omnigent://main\n  runat: omnigent://s1\n  - do x\n  - 🧑 do y\n"
        );
        let mut sub = task(
            "t_sub.md",
            &[("status", "long_running"), ("runat", "s2")],
            &["do x"],
            "",
        );
        assert_eq!(move_item(&mut work, &mut sub, "do x"), Ok(()));
        assert_eq!(
            (work.items.as_slice(), sub.items.as_slice()),
            (&["🧑 do y".to_owned()][..], &["do x".to_owned()][..])
        );
        assert!(
            move_item(&mut work, &mut sub, "absent")
                .unwrap_err()
                .contains(NO_ITEM)
        );
        let mut same = sub.clone();
        assert!(
            move_item(&mut sub, &mut same, "do x")
                .unwrap_err()
                .contains("same task")
        );
        add_note(&mut sub, "checked\n twice");
        assert_eq!(sub.body, "\n\n(checked twice)\n");
    }

    #[test]
    fn status_decides_the_section() {
        let mut work = worker();
        assert_eq!(set_status(&mut work, "blocked", Some("human")), Ok(()));
        assert_eq!(
            (work.field("blocked_on"), work.section()),
            (Some("human"), "human pending:")
        );
        assert_eq!(set_status(&mut work, "blocked", Some("t_sub.md")), Ok(()));
        assert_eq!(work.section(), "current:");
        assert!(
            set_status(&mut work, "blocked", None)
                .unwrap_err()
                .contains("needs --on")
        );
        assert!(
            set_status(&mut work, "running", Some("x"))
                .unwrap_err()
                .contains("needs --on")
        );
        assert_eq!(set_status(&mut work, "running", None), Ok(()));
        assert_eq!((work.field("blocked_on"), work.status()), (None, "running"));
        assert_eq!(set_status(&mut work, "long_running", Some("human")), Ok(()));
        assert_eq!(
            (work.field("blocked_on"), work.section()),
            (Some("human"), "current:")
        );
    }

    #[test]
    fn check_reports_each_disagreement() {
        let sub = task(
            "t_sub.md",
            &[("status", "long_running"), ("runat", "omnigent://s2")],
            &[],
            "",
        );
        let twin = task(
            "t_twin.md",
            &[("status", "running"), ("runat", "omnigent://s2")],
            &[],
            "",
        );
        let waiting = task(
            "t_wait.md",
            &[("status", "blocked"), ("runat", "retired")],
            &[],
            "",
        );
        let active = [sub, twin, waiting, worker()];
        let list = "current:\nt_work.md omnigent://s1\nt_sub.md omnigent://s2\nt_twin.md omnigent://s2\n\nprevious:\nt_gone.md x\n";
        assert_eq!(
            check(list, &active),
            ["omnigent://s2: several active tasks run there: t_sub.md, t_twin.md"]
        );
        let list = "current:\nt_work.md omnigent://old\nt_sub.md omnigent://s2\nt_gone.md x\n\nprevious:\nt_sub.md x\n";
        assert_eq!(
            check(
                list,
                &active[..1]
                    .iter()
                    .chain(&active[3..])
                    .cloned()
                    .collect::<Vec<_>>()
            ),
            [
                "t_sub.md: 2 rows in TODO.md, want 1",
                "t_work.md: TODO.md has it under `current:` at omnigent://old, the task file says `current:` at omnigent://s1",
                "t_gone.md: listed as open work in TODO.md but it is done or not a task file",
            ]
        );
    }
}
