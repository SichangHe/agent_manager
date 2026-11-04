//! The organization as a company: the human is CEO, managers are departments, workers report to them.

use std::collections::BTreeMap;

use crate::decide::Stall;
use crate::tasks::Task;

/// What one task looks like in the organization view.
fn line(task: &Task, held: Option<&str>, stall: Option<&Stall>) -> String {
    let role = if task.field("is_manager") == Some("true") {
        "manager"
    } else {
        "worker"
    };
    let mut state = task.status().to_owned();
    if let Some(why) = held {
        state = format!("held: {why}");
    } else if let Some(on) = task.field("blocked_on").filter(|on| !on.is_empty()) {
        state = format!(
            "{state}, waits on: {}",
            on.chars().take(80).collect::<String>()
        );
    }
    if stall.is_some_and(|stall| stall.reported) {
        state.push_str(", suspected stall (advisory)");
    }
    format!("{} [{role}] {} open; {state}", task.name, task.items.len())
}

/// Draw the organization from the main manager down, then list structural problems.
///
/// `held` gives why a task's agent is held, if it is; `stalls` holds the watcher's stall records.
// 🧑 "Organize everything and improve amh to give a company-like structure where human is CEO."
pub fn chart(
    tasks: &[Task],
    main_manager: &str,
    held: impl Fn(&Task) -> Option<String>,
    stalls: &BTreeMap<String, Stall>,
) -> String {
    let by_address: BTreeMap<&str, &Task> =
        tasks.iter().map(|task| (task.address(), task)).collect();
    let mut lines = vec!["human (CEO)".to_owned()];
    let mut drawn: Vec<&str> = Vec::new();
    let mut stack: Vec<(&Task, usize)> = tasks
        .iter()
        .filter(|task| task.address() == main_manager)
        .map(|task| (task, 1))
        .collect();
    while let Some((task, depth)) = stack.pop() {
        if drawn.contains(&task.name.as_str()) {
            continue;
        }
        drawn.push(&task.name);
        let why = held(task);
        lines.push(format!(
            "{}{}",
            "  ".repeat(depth),
            line(task, why.as_deref(), stalls.get(&task.name))
        ));
        let mut reports: Vec<&Task> = tasks
            .iter()
            .filter(|other| {
                other.field("managerat") == Some(task.address())
                    && other.address() != task.address()
            })
            .collect();
        reports.sort_by(|a, b| b.name.cmp(&a.name));
        stack.extend(reports.into_iter().map(|report| (report, depth + 1)));
    }
    let mut problems = Vec::new();
    for task in tasks {
        let manager = task.field("managerat").unwrap_or_default();
        let reports = tasks
            .iter()
            .filter(|other| {
                other.field("managerat") == Some(task.address()) && other.name != task.name
            })
            .count();
        if task.address() != main_manager && !by_address.contains_key(manager) {
            problems.push(format!(
                "{}: its manager {manager} is not an active task",
                task.name
            ));
        }
        if reports > 0 && task.field("is_manager") != Some("true") {
            problems.push(format!(
                "{}: recorded as a worker but {reports} tasks report to it",
                task.name
            ));
        }
        if !drawn.contains(&task.name.as_str()) && by_address.contains_key(manager) {
            let mut seen = vec![task.address()];
            let mut at = manager;
            let cycle = loop {
                if seen.contains(&at) {
                    break true;
                }
                let Some(up) = by_address.get(at) else {
                    break false;
                };
                seen.push(at);
                at = up.field("managerat").unwrap_or_default();
            };
            let why = if cycle {
                "a reporting cycle"
            } else {
                "a manager above it is not an active task"
            };
            problems.push(format!(
                "{}: not reachable from the main manager ({why})",
                task.name
            ));
        }
    }
    lines.push(String::new());
    if problems.is_empty() {
        lines.push("no structural problems".to_owned());
    } else {
        lines.push(format!("{} structural problems:", problems.len()));
        lines.extend(problems.into_iter().map(|problem| format!("  {problem}")));
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(name: &str, at: &str, manager: &str, is_manager: bool) -> Task {
        let text = format!(
            "---\nstatus: running\nrunat: {at}\nmanagerat: {manager}\nis_manager: '{is_manager}'\npending_task_items:\n  - one\n---\n"
        );
        crate::tasks::Task::parse(name, &text).unwrap()
    }

    #[test]
    fn chart_shows_ceo_departments_and_problems() {
        let tasks = [
            task("main.md", "omnigent://m", "", true),
            task("dept.md", "omnigent://d", "omnigent://m", true),
            task("w.md", "omnigent://w", "omnigent://d", false),
            task("boss.md", "omnigent://b", "omnigent://m", false),
            task("sub.md", "omnigent://s", "omnigent://b", false),
            task("lost.md", "omnigent://l", "omnigent://gone", false),
            task("a.md", "omnigent://a", "omnigent://c", false),
            task("c.md", "omnigent://c", "omnigent://a", false),
            task("under_lost.md", "omnigent://u", "omnigent://l", false),
        ];
        let held = |task: &Task| (task.name == "w.md").then(|| "quota".to_owned());
        let out = chart(&tasks, "omnigent://m", held, &BTreeMap::new());
        assert!(
            out.starts_with("human (CEO)\n  main.md [manager] 1 open; running\n"),
            "{out}"
        );
        assert!(
            out.contains("      w.md [worker] 1 open; held: quota"),
            "{out}"
        );
        assert!(
            out.contains("lost.md: its manager omnigent://gone is not an active task"),
            "{out}"
        );
        assert!(
            out.contains("boss.md: recorded as a worker but 1 tasks report to it"),
            "{out}"
        );
        assert!(
            out.contains("a.md: not reachable from the main manager (a reporting cycle)"),
            "{out}"
        );
        assert!(
            out.contains("under_lost.md: not reachable from the main manager (a manager above it is not an active task)"),
            "{out}"
        );
    }
}
