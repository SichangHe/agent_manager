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
        let task = Self {
            name: name.to_owned(),
            fields,
            items,
            body,
        };
        crate::requests::validate(&task)?;
        Ok(task)
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

    /// The task-list section the task is pinned under, whatever its status.
    // 🧑 "put Teamtype in low priority" and "restore the HANDBOOK.md paper review to human pending", both for done tasks.
    pub fn pinned(&self) -> Option<&'static str> {
        let under = self.field(PIN)?;
        SECTIONS
            .into_iter()
            .find(|section| section.trim_end_matches(':') == under)
    }

    /// The task-list section the task belongs in.
    // 🧑 "None of the currently listed as human pending tasks is actually pending on the human."
    // 🧑 "Should be something they are blocked on, not someone, e.g. human reviewing a file."
    // So a blocker that starts with `human` and then says what the human is to do counts as waiting on the human.
    pub fn section(&self) -> &'static str {
        if let Some(pinned) = self.pinned() {
            return pinned;
        }
        let on = self.field("blocked_on").unwrap_or_default();
        let waits_on_human = self.status() == "blocked"
            && match block_of(on) {
                Some(Block::Task(name)) => name == HUMAN_TASK,
                Some(Block::Item(id)) => id.starts_with(&format!("{HUMAN_TASK}#")),
                Some(Block::Date(_)) => false,
                None => on.to_lowercase().starts_with("human"),
            };
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
    // The study: near-duplicates like `🧑 Minimize diffs` and `🧑 “Minimize diffs”` sat open side by side.
    if let Some(open) = open_match(task, &marked) {
        eprintln!("amh: not added: already open as `{open}`");
        return Ok(None);
    }
    task.items.push(marked.clone());
    Ok(Some(marked))
}

/// The open item that `marked` repeats: same kind (the human's or not), same words ignoring quotes and spacing.
pub fn open_match(task: &Task, marked: &str) -> Option<String> {
    let plain = |text: &str| -> String {
        text.chars()
            .filter(|c| !matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’' | '`'))
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    task.items
        .iter()
        .find(|open| {
            open.starts_with(HUMAN_MARK) == marked.starts_with(HUMAN_MARK)
                && plain(open) == plain(marked)
        })
        .cloned()
}

/// The record left in a task file when an open item is removed: the item itself, then how it is known to be finished when that was said.
pub fn removal_note(item: &str, evidence: &str) -> String {
    match evidence.trim() {
        "" => format!("removed open item: {item}"),
        evidence => format!("removed open item: {item}; evidence: {evidence}"),
    }
}

/// Where the open item `given` is: its exact text, or the same text with or without the leading human mark.
fn position(task: &Task, given: &str) -> Option<usize> {
    let bare = |item: &str| {
        item.strip_prefix(HUMAN_MARK)
            .unwrap_or(item)
            .trim()
            .to_owned()
    };
    let exact = task.items.iter().position(|open| open == given);
    exact.or_else(|| task.items.iter().position(|open| bare(open) == bare(given)))
}

/// The evidence to note for a removed item, with the human's own words when they asked not to be emailed about it.
pub fn quiet_evidence(evidence: &str, human_words: Option<&str>) -> String {
    match human_words {
        Some(words) => format!(
            "{}; the human is not messaged, because they wrote: {}",
            evidence.trim(),
            words.trim()
        ),
        None => evidence.to_owned(),
    }
}

/// Remove one finished or cancelled item, leaving a one-line record in the body; returns the item as it was stored.
pub fn remove_item(task: &mut Task, item: &str, evidence: &str) -> Result<String, String> {
    let at = position(task, item).ok_or_else(|| {
        format!(
            "amh: {} has no such open item; copy it from `amh todo list`",
            task.name
        )
    })?;
    let stored = task.items.remove(at);
    // 🧑 "Should quote the item itself automatically instead."
    task.note(&removal_note(&stored, evidence));
    Ok(stored)
}

/// Reword one open item, keeping its authorship mark.
pub fn replace_item(task: &mut Task, old: &str, new: &str) -> Result<(), String> {
    let at = position(task, old).ok_or("amh: no such open item; copy it from `amh todo list`")?;
    let mut records = crate::requests::sync(task)?;
    let request = records
        .iter_mut()
        .find(|r| r.text == task.items[at] && r.stage != crate::requests::Stage::Resolved)
        .ok_or("amh: no such request")?;
    if request.stage != crate::requests::Stage::Added {
        return Err("amh: cannot reword a request awaiting resolution".to_owned());
    }
    let text = taskfile::mark(new, task.items[at].starts_with(HUMAN_MARK))?;
    if task
        .items
        .iter()
        .enumerate()
        .any(|(n, item)| n != at && item == &text)
    {
        return Err("amh: replacement duplicates another open item".to_owned());
    }
    request.text = text.clone();
    task.items[at] = text;
    crate::requests::write(task, &records);
    Ok(())
}

/// Move one open item from `source` to `to`.
#[cfg(test)]
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

/// What to print after the last open item of a running worker task goes: finishing items never closes a task.
// 🧑 "Fix these", on agent feedback: "Marking the open item done did not close the task, so its status remained running after completion."
pub fn close_hint(task: &Task) -> &'static str {
    let lasting = task.field("is_manager") == Some("true") || task.status() != "running";
    match task.items.is_empty() && !lasting {
        true => {
            "You have no open items left and your task still says `running`. If the task is finished, close it now with `amh task close`; if you only wait for something, say what with `amh task status TASK blocked --on WHAT`.\n"
        }
        false => "",
    }
}

/// The field that pins a task's row under one section of the task list.
const PIN: &str = "listed_under";

/// Pin the task's row under `section` of the task list (a heading without its colon), or unpin it with `auto`.
pub fn pin(task: &mut Task, section: &str) -> Result<(), String> {
    let wanted = section.trim().trim_end_matches(':').to_lowercase();
    task.fields.retain(|(key, _)| key != PIN);
    if wanted == "auto" {
        return Ok(());
    }
    task.fields.push((PIN.to_owned(), wanted));
    task.pinned().map(drop).ok_or_else(|| {
        format!("amh: no section `{section}`; use current, 'human pending', 'low priority', previous, or auto")
    })
}

/// The human's own task: items the human owns, which agents wait on with `--for human.md#N`; no agent runs it.
// 🧑 "First they need to create a task and let that someone own it, and block on this task, e.g. the human should have 67 tasks or something like that"
pub const HUMAN_TASK: &str = "human.md";

/// What a block names: a task file `NAME.md`, a request ID `NAME.md#N`, or a date `until RFC3339`; `None` for free text.
pub enum Block<'a> {
    Task(&'a str),
    Item(&'a str),
    Date(chrono::DateTime<chrono::Utc>),
}

pub fn block_of(on: &str) -> Option<Block<'_>> {
    let on = on.trim();
    if let Some(at) = on.strip_prefix("until ") {
        return chrono::DateTime::parse_from_rfc3339(at)
            .ok()
            .map(|at| Block::Date(at.with_timezone(&chrono::Utc)));
    }
    if on.contains(char::is_whitespace) {
        return None;
    }
    match on.split_once(".md#") {
        Some((_, n)) if n.parse::<u64>().is_ok() => Some(Block::Item(on)),
        _ if on.ends_with(".md") => Some(Block::Task(on)),
        _ => None,
    }
}

/// The next reminder of a task blocked on something that is no task: `RFC3339 #N` (`requests::schedule_text`).
pub const BLOCK_REMIND: &str = "block_reminder";

/// Set the status and what a blocked task waits for; a status change unpins the task's row and drops the block's reminder schedule.
pub fn set_status(task: &mut Task, status: &str, blocked_on: Option<&str>) -> Result<(), String> {
    let waits = blocked_on.is_some_and(|on| !on.is_empty());
    if (status == "blocked") != waits && status != "long_running" {
        return Err(
            "amh: `blocked` needs --on saying what it waits for; `running` takes no --on"
                .to_owned(),
        );
    }
    task.fields
        .retain(|(key, _)| key != "blocked_on" && key != BLOCK_REMIND && key != PIN);
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

/// The email text listing the human's items that were created and deleted in one call; empty when none is theirs.
// 🧑 “Pending items originated from the human need emails, ones from agents do not.”
// 🧑 "when we make agents add/remove pending items we urge them to do multiple at a time and also attach any other things to be emailed to the human. I don't want any additional latency from debounce"
pub fn changes_text(created: &[String], deleted: &[String]) -> String {
    let mut text = String::new();
    for (change, items) in [("created", created), ("deleted", deleted)] {
        let theirs: String = items
            .iter()
            .filter_map(|item| item.strip_prefix(HUMAN_MARK))
            .map(|item| format!("- {item}\n"))
            .collect();
        if !theirs.is_empty() {
            text += &format!("pending item {change}:\n{theirs}");
        }
    }
    text
}

/// An email body with the item changes under it.
pub fn with_changes(body: &str, changes: &str) -> String {
    match (body.trim().is_empty(), changes.is_empty()) {
        (_, true) => body.to_owned(),
        (true, false) => changes.to_owned(),
        (false, false) => format!("{}\n\n{changes}", body.trim_end()),
    }
}

/// Every disagreement between the task list and the task files that belong in it: the active ones and the pinned ones.
// 🧑 "Maybe we should have a validation script that agents should run after editing task file/TODO file?"
pub fn check(list: &str, all: &[Task]) -> Vec<String> {
    let shown = |task: &&Task| task.is_active() || task.pinned().is_some();
    let active: Vec<Task> = all.iter().filter(shown).cloned().collect();
    let active = active.as_slice();
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
        // 🧑 "Write done everything that got paused into a file in work_logs and put in low priority"
        // So a plain document, which is no task file, may be listed outside `current:`.
        let is_task = all.iter().any(|task| task.name == *name);
        let open = placed
            .iter()
            .any(|(section, _)| section == "current:" || (is_task && section != "previous:"));
        if open && !active.iter().any(|task| task.name == *name) {
            findings.push(format!(
                "{name}: listed as open work in TODO.md but it is done or not a task file"
            ));
        }
    }
    let mut shared: Vec<(&str, Vec<&str>)> = Vec::new();
    for task in active.iter().filter(|task| task.is_active()) {
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
        assert_eq!(add_item(&mut work, "write “the” code", false), Ok(None));
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
            remove_item(&mut work, "write the code", "tests\npass").as_deref(),
            Ok("write the code")
        );
        assert_eq!(work.items, ["🧑 ship it today"]);
        assert_eq!(
            work.body,
            "request\n\n(removed open item: write the code; evidence: tests pass)\n"
        );
        assert_eq!(
            removal_note("🧑 ship it", " "),
            "removed open item: 🧑 ship it"
        );
        let mut done = task("t_done.md", &[("status", "done"), ("runat", "x")], &[], "");
        assert_eq!(
            add_item(&mut done, "x", false),
            Err("amh: t_done.md is done; start a new task instead".to_owned())
        );
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
        assert_eq!(
            set_status(&mut work, "blocked", Some("human  reviewing docs/plan.md")),
            Ok(())
        );
        assert_eq!(
            (work.field("blocked_on"), work.section()),
            (Some("human reviewing docs/plan.md"), "human pending:")
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
    fn a_pin_overrides_the_section_until_the_status_changes() {
        let mut done = task("t_done.md", &[("status", "done"), ("runat", "x")], &[], "");
        assert_eq!(pin(&mut done, "Low priority:"), Ok(()));
        assert_eq!(done.section(), "low priority:");
        let list = "current:\n\nlow priority:\nt_done.md x\n\nprevious:\n";
        assert_eq!(check(list, &[done.clone()]), [""; 0]);
        let plain = "current:\n\nlow priority:\nnotes.md\n\nprevious:\n";
        assert_eq!(check(plain, &[]), [""; 0]);
        assert_eq!(
            check(
                &plain.replace("notes", "t_done"),
                &[task(
                    "t_done.md",
                    &[("status", "done"), ("runat", "x")],
                    &[],
                    ""
                )]
            )
            .len(),
            1
        );
        assert!(pin(&mut done, "later").is_err());
        assert_eq!(pin(&mut done, "auto"), Ok(()));
        assert_eq!(done.section(), "previous:");
        let mut work = worker();
        pin(&mut work, "low priority").unwrap();
        set_status(&mut work, "running", None).unwrap();
        assert_eq!(work.section(), "current:");
    }

    #[test]
    fn the_last_item_done_asks_a_running_worker_to_close() {
        let mut work = worker();
        assert_eq!(close_hint(&work).is_empty(), !work.items.is_empty());
        work.items.clear();
        assert!(close_hint(&work).contains("amh task close"));
        set_status(&mut work, "long_running", None).unwrap();
        assert_eq!(close_hint(&work), "");
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

    #[test]
    fn the_humans_item_changes_of_one_call_make_one_text() {
        let created = [
            "🧑 ship it".to_owned(),
            "write the code".to_owned(),
            "🧑 second".to_owned(),
        ];
        let deleted = ["🧑 old one".to_owned()];
        let text = changes_text(&created, &deleted);
        assert_eq!(
            text,
            "pending item created:\n- ship it\n- second\npending item deleted:\n- old one\n"
        );
        assert_eq!(changes_text(&created[1..2], &[]), "");
        assert_eq!(
            with_changes("Report.\n", &text),
            format!("Report.\n\n{text}")
        );
        assert_eq!(with_changes("Report.\n", ""), "Report.\n");
        assert_eq!(with_changes(" \n", &text), text);
    }

    #[test]
    fn an_item_is_found_with_or_without_its_human_mark() {
        let mut work = Task::parse("t.md", "---\nstatus: running\nrunat: omnigent://a\npending_task_items:\n  - 🧑 ship it\n  - write code\n---\n").unwrap();
        assert_eq!(
            remove_item(&mut work, "ship it", "").as_deref(),
            Ok("🧑 ship it")
        );
        assert!(work.body.ends_with("(removed open item: 🧑 ship it)\n"));
        assert_eq!(
            remove_item(&mut work, "🧑 write code", "").as_deref(),
            Ok("write code")
        );
        assert!(remove_item(&mut work, "ship", "").is_err());
    }

    #[test]
    fn a_quiet_removal_notes_the_humans_words() {
        assert_eq!(
            quiet_evidence("image fixed ", Some(" don't bother me afterwards")),
            "image fixed; the human is not messaged, because they wrote: don't bother me afterwards"
        );
        assert_eq!(quiet_evidence("image fixed", None), "image fixed");
    }
}
