//! Task files and the task list `TODO.md`: the only persistent records of who works on what.
//!
//! Pure text functions plus the atomic file write; reading, locking, and choosing files stay with the caller.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub const HUMAN_MARK: &str = "🧑 ";
pub const SECTIONS: [&str; 4] = ["current:", "human pending:", "low priority:", "previous:"];
const ITEMS_KEY: &str = "pending_task_items";
pub const MARKER: &str = "(pending)";

/// The parts of one task file: frontmatter fields in file order, open items, and the free-text body.
pub type Parts = (Vec<(String, String)>, Vec<String>, String);

/// Write a one-line YAML string: plain when that is unambiguous, else single-quoted.
fn scalar(value: &str) -> String {
    let special = |first: char| first.is_whitespace() || "-?:,[]{}#&*!|>'\"%@`".contains(first);
    let plain = value.chars().next().is_some_and(|first| !special(first))
        && !value.contains('\n')
        && !value.contains(": ")
        && !value.contains(" #")
        && !value.ends_with(':');
    let reserved =
        ["true", "false", "null", "yes", "no", "~"].contains(&value.to_lowercase().as_str());
    if plain && !reserved || value == "true" || value == "false" {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

/// Read a one-line YAML string: plain, single-quoted, or double-quoted.
fn unscalar(text: &str) -> Result<String, String> {
    let text = text.trim();
    let quoted =
        |quote: char| text.chars().count() > 1 && text.starts_with(quote) && text.ends_with(quote);
    if quoted('\'') {
        Ok(text[1..text.len() - 1].replace("''", "'"))
    } else if quoted('"') {
        serde_json::from_str(text)
            .map_err(|error| format!("bad double-quoted value {text}: {error}"))
    } else {
        Ok(text.to_owned())
    }
}

/// Split `key: value` or a bare `key:`; the key is one word.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    let word = !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_');
    match rest.strip_prefix(' ') {
        Some(value) if word => Some((key, value)),
        None if word && rest.is_empty() => Some((key, "")),
        _ => None,
    }
}

/// Split a task file into frontmatter and body; fail when it is not a task file.
pub fn parse(name: &str, text: &str) -> Result<Parts, String> {
    let end = text
        .starts_with("---\n")
        .then(|| {
            text.match_indices("\n---\n")
                .map(|(at, _)| at + 1)
                .find(|&at| at >= 5)
        })
        .flatten()
        .ok_or_else(|| format!("{name} has no frontmatter"))?;
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut items = Vec::new();
    let mut key = "";
    for line in text[4..end].lines() {
        if let (Some(item), true) = (line.strip_prefix("  - "), key == ITEMS_KEY) {
            items.push(unscalar(item)?);
            continue;
        }
        let Some((found, value)) = key_value(line) else {
            let shown: String = line.chars().take(80).collect();
            return Err(format!(
                "{name} has a frontmatter line that is neither `key: value` nor an open item: {shown}"
            ));
        };
        key = found;
        if key != ITEMS_KEY {
            let value = unscalar(value)?;
            match fields.iter_mut().find(|(known, _)| known == key) {
                Some(field) => field.1 = value,
                None => fields.push((key.to_owned(), value)),
            }
        }
    }
    if !["status", "runat"]
        .iter()
        .all(|need| fields.iter().any(|(known, _)| known == need))
    {
        return Err(format!("{name} records no status or no agent address"));
    }
    Ok((fields, items, text[end + 4..].to_owned()))
}

/// Write a task file from its parts.
pub fn render(fields: &[(String, String)], items: &[String], body: &str) -> String {
    let mut text = String::from("---\n");
    for (key, value) in fields {
        text += &format!("{key}: {}\n", scalar(value));
    }
    if items.is_empty() {
        text += &format!("{ITEMS_KEY}: []\n");
    } else {
        text += &format!("{ITEMS_KEY}:\n");
        for item in items {
            text += &format!("  - {}\n", scalar(item));
        }
    }
    text + "---\n" + body
}

/// Find each `(pending)` line outside code fences, with the lines after it up to the next blank line.
pub fn pending_blocks(body: &str) -> Vec<(usize, Vec<String>)> {
    let lines: Vec<&str> = body.split('\n').collect();
    let fences: Vec<usize> = (0..lines.len())
        .filter(|&at| lines[at].starts_with("```") || lines[at].starts_with("~~~"))
        .collect();
    let fenced = |at: usize| {
        fences
            .chunks_exact(2)
            .any(|pair| (pair[0]..pair[1]).contains(&at))
    };
    let ends_block = |line: &str| line.trim().is_empty() || line.trim() == MARKER;
    (0..lines.len())
        .filter(|&at| !fenced(at) && lines[at].trim() == MARKER)
        .map(|at| {
            let end = (at + 1..lines.len())
                .find(|&next| ends_block(lines[next]))
                .unwrap_or(lines.len());
            (
                at,
                lines[at + 1..end]
                    .iter()
                    .map(|&line| line.to_owned())
                    .collect(),
            )
        })
        .collect()
}

/// Normalize one item to a single line carrying exactly the right authorship mark.
// 🧑 "agents’ pending task item list distinguishes human requests from agent-made ones ... prepend a human emoji"
pub fn mark(item: &str, from_human: bool) -> Result<String, String> {
    let joined = item.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut text = joined.as_str();
    while let Some(rest) = text.strip_prefix(HUMAN_MARK.trim_end()) {
        text = rest.trim_start();
    }
    match (text.is_empty(), from_human) {
        (true, _) => Err("amh: the item is empty".to_owned()),
        (false, true) => Ok(format!("{HUMAN_MARK}{text}")),
        (false, false) => Ok(text.to_owned()),
    }
}

/// Put the task's row at the top of `section` of the task list, removing any other row for it; `None` when it is already right.
// 🧑 "the task reference moves from `TODO.md` `current` to the top of `previous`"
pub fn placed(
    list: &str,
    name: &str,
    address: &str,
    section: &str,
) -> Result<Option<String>, String> {
    let heading = |row: &str| row.trim().to_lowercase();
    let rows: Vec<&str> = list.split('\n').collect();
    let old: Vec<usize> = (0..rows.len())
        .filter(|&at| rows[at].split(' ').next() == Some(name))
        .collect();
    if let [at] = old[..] {
        let within = rows[..=at]
            .iter()
            .rev()
            .map(|row| heading(row))
            .find(|row| SECTIONS.contains(&row.as_str()));
        if within.as_deref() == Some(section) && rows[at].split_whitespace().nth(1) == Some(address)
        {
            return Ok(None);
        }
    }
    let mut rows: Vec<&str> = (0..rows.len())
        .filter(|at| !old.contains(at))
        .map(|at| rows[at])
        .collect();
    let mut at = rows
        .iter()
        .position(|row| heading(row) == section)
        .ok_or_else(|| format!("amh: TODO.md has no `{section}` section"))?
        + 1;
    while section != "previous:" && at < rows.len() && rows[at].trim().is_empty() {
        at += 1;
    }
    let row = format!("{name} {address}");
    rows.insert(at, &row);
    Ok(Some(rows.join("\n")))
}

/// Replace `path` atomically, keeping its permissions.
pub fn write(path: &Path, text: &str) -> std::io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.amh-tmp"));
    fs::write(&temp, text)?;
    let mode = fs::metadata(path)
        .map(|found| found.permissions().mode() & 0o7777)
        .unwrap_or(0o660);
    fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
    fs::rename(&temp, path)
}
