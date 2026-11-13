//! Turn replies: after a human message, the agent's text at its next natural end of turn goes to the human.
// 🧑 "Agents really love using the terminal printout for reply, so let's automatically send whatever the agent said at the end of the turn. This also needs to be worked on s.t. pending item notifications and messages replacements work"
// 🧑 "get rid of the. Rule that does not send. The final. Agent output after. They call AMH message ... What agents should do is they should use AMH. To explicitly message the human if. They. Have something immediate that they need to, say before they. Finish their turn. But otherwise, they just do normal agent behavior."
// 🧑 "Have their end-of-turn message sent. If they haven't responded. Or had a end of turn, since. Last time the human. Ask them something. Counting the interruption from other messages, as not a end of turn."
// 🧑 "Messages that agents received from the system or other agents, should not be sent back to. Whoever that agent was."

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

type Settings = std::collections::BTreeMap<String, String>;
use crate::store::Store;
use crate::{actions, agents, omnigent, work};

/// A turn that has not ended this long after the latest human message is no longer answered.
const EXPIRY_S: i64 = 12 * 3600;
/// A session that stays `running` with an assistant text as its newest item for this long ended its turn and waits on background commands.
const QUIET_S: i64 = 180;
/// A reply that keeps failing to send is given up this long after the latest human message.
const RETRY_S: i64 = 600;

fn folder(state_dir: &Path) -> PathBuf {
    state_dir.join("amh-replies")
}

/// The time to pass to `expect`, taken before sending, so no text of the turn predates it.
pub fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The unanswered human message times in a record row, oldest first; empty for an unreadable or old-format agent row.
fn parse(row: &str) -> Vec<i64> {
    let mut words = row.split_whitespace();
    if words.next() != Some("human") {
        return Vec::new();
    }
    words.filter_map(|word| word.parse().ok()).collect()
}

/// Remember that the human sent the agent at `address` a message at `asked_s`; earlier unanswered ones stay.
pub fn expect(state_dir: &Path, address: &str, asked_s: i64) -> Result<(), String> {
    let id = omnigent::session(address)?;
    let dir = folder(state_dir);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let file = dir.join(id);
    let mut times: Vec<i64> = fs::read_to_string(&file)
        .map(|row| parse(&row))
        .unwrap_or_default()
        .into_iter()
        .filter(|at| asked_s - at <= EXPIRY_S)
        .collect();
    times.push(asked_s);
    let row: Vec<String> = times.iter().map(i64::to_string).collect();
    let temp = dir.join(format!(".{id}.{}", std::process::id()));
    fs::write(&temp, format!("human {}\n", row.join(" ")))
        .and_then(|()| fs::rename(&temp, &file))
        .map_err(|error| format!("cannot record the reply owed by {address}: {error}"))
}

fn text_of(item: &Value) -> Option<String> {
    let text: String = (item["type"] == "message" && item["role"] == "assistant")
        .then(|| item["content"].as_array())
        .flatten()?
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Some(text.trim().to_owned()).filter(|text| !text.is_empty())
}

/// The reply owed for human messages at `asked_s` from session items, newest first, and the times it answers; `None` while none has a natural end of turn yet. Pure.
///
/// A natural end of turn is an assistant text the agent stopped on by itself: the next item is a user message (the agent was idle when it came), or it is the newest item and `newest_ended`.
/// A message that interrupts a turn comes after a tool result, so the text before it is no natural end.
/// Each human message is answered by the first natural end after it; the distinct answers go out oldest first.
pub fn reply(items: &[Value], asked_s: &[i64], newest_ended: bool) -> Option<(String, Vec<i64>)> {
    // Other kinds, e.g. `resource_event`, say nothing about turns.
    let items: Vec<&Value> = items
        .iter()
        .filter(|item| {
            ["message", "function_call", "function_call_output"]
                .contains(&item["type"].as_str().unwrap_or_default())
        })
        .collect();
    let ends: Vec<(usize, i64, String)> = items
        .iter()
        .enumerate()
        .filter(|(at, _)| match at {
            0 => newest_ended,
            _ => items[at - 1]["type"] == "message" && items[at - 1]["role"] == "user",
        })
        .filter_map(|(at, item)| Some((at, item["created_at"].as_i64()?, text_of(item)?)))
        .collect();
    let mut answers: Vec<&(usize, i64, String)> = Vec::new();
    let mut answered = Vec::new();
    for asked in asked_s {
        // Strictly later: a text in the same second as the delivery is the previous turn's.
        let Some(first) = ends.iter().rev().find(|(_, end_s, _)| end_s > asked) else {
            continue;
        };
        answered.push(*asked);
        if !answers.iter().any(|known| known.0 == first.0) {
            answers.push(first);
        }
    }
    answers.sort_by_key(|answer| std::cmp::Reverse(answer.0));
    let texts: Vec<&str> = answers.iter().map(|answer| answer.2.as_str()).collect();
    (!answered.is_empty()).then(|| (texts.join("\n\n"), answered))
}

/// Send each owed reply whose turn ended naturally; called by the watcher every poll.
pub fn forward(settings: &Settings, store: &Store) -> Vec<String> {
    let Ok(entries) = fs::read_dir(folder(&store.state_dir)) else {
        return Vec::new();
    };
    let mut log = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if id.starts_with('.') {
            continue;
        }
        let Ok(row) = fs::read_to_string(&path) else {
            continue;
        };
        let done = |log: &mut Vec<String>, line: String| {
            // A human message recorded meanwhile changes the row, so its reply stays owed.
            if fs::read_to_string(&path).is_ok_and(|now| now == row)
                && let Err(error) = fs::remove_file(&path)
            {
                log.push(format!("cannot remove {}: {error}", path.display()));
            }
            log.push(line);
        };
        let asked_s = parse(&row);
        let address = format!("{}{}", omnigent::PREFIX, id);
        let Some(&latest_s) = asked_s.last() else {
            done(
                &mut log,
                format!("turn reply of {address}: not owed to the human; dropped"),
            );
            continue;
        };
        if now_s() - latest_s > EXPIRY_S {
            done(
                &mut log,
                format!("turn reply of {address}: no turn end within 12 h; dropped"),
            );
            continue;
        }
        let state = omnigent::fetched_status(settings, &address).0;
        if state != "ready" && state != "running" {
            continue;
        }
        let items = omnigent::api(
            settings,
            "GET",
            &format!("/v1/sessions/{id}/items?limit=500&order=desc"),
            None,
        );
        let items = match items {
            Ok(items) => items["data"].as_array().cloned().unwrap_or_default(),
            Err(error) => {
                log.push(format!(
                    "turn reply of {address}: cannot read items: {error}"
                ));
                continue;
            }
        };
        // Claude sessions stay `running` after the turn ends while background commands run.
        let quiet = items
            .iter()
            .find(|item| item["type"] != "resource_event")
            .is_some_and(|item| {
                text_of(item).is_some()
                    && item["created_at"]
                        .as_i64()
                        .is_some_and(|at| now_s() - at >= QUIET_S)
            });
        let Some((text, answered)) = reply(&items, &asked_s, state == "ready" || quiet) else {
            continue;
        };
        // Times recorded meanwhile stay owed.
        let settle = |log: &mut Vec<String>, line: String| {
            let left: Vec<String> = fs::read_to_string(&path)
                .map(|now| parse(&now))
                .unwrap_or_default()
                .into_iter()
                .filter(|at| !answered.contains(at))
                .map(|at| at.to_string())
                .collect();
            let kept = match left.is_empty() {
                true if !path.exists() => Ok(()),
                true => fs::remove_file(&path),
                false => {
                    let temp = path.with_file_name(format!(".{id}.settle"));
                    fs::write(&temp, format!("human {}\n", left.join(" ")))
                        .and_then(|()| fs::rename(&temp, &path))
                }
            };
            if let Err(error) = kept {
                log.push(format!("cannot update {}: {error}", path.display()));
            }
            log.push(line);
        };
        match send(settings, store, &address, &text) {
            Ok(to) => settle(&mut log, format!("turn reply of {address} sent to {to}")),
            Err(error) if now_s() - latest_s > RETRY_S => settle(
                &mut log,
                format!("turn reply of {address} failed; dropped: {error}"),
            ),
            Err(error) => log.push(format!("turn reply of {address} failed; retrying: {error}")),
        }
    }
    log
}

/// Send `text`, the end-of-turn reply of the agent at `address`, to the human, or to its manager when it may not write to the human; returns who got it.
fn send(settings: &Settings, store: &Store, address: &str, text: &str) -> Result<String, String> {
    let task = store
        .active()?
        .into_iter()
        .find(|task| task.address() == address)
        .ok_or("no active task has this agent")?;
    if actions::writes_to_human(settings, &task)? {
        let (receipt, reminder) = actions::tell_human(settings, &task, "", text, &[], &[], None)?;
        if !reminder.is_empty() {
            let note = format!(
                "amh sent the text you ended your turn with to the human.\n{receipt}{reminder}"
            );
            omnigent::send(settings, address, &note)?;
        }
        return Ok("human".to_owned());
    }
    let manager = task.field("managerat").unwrap_or_default().to_owned();
    let clipped: String = text.chars().take(work::MANAGER_MESSAGE_CHARS_MAX).collect();
    let more = if text.chars().count() > work::MANAGER_MESSAGE_CHARS_MAX {
        "…"
    } else {
        ""
    };
    let message = work::envelope(
        address,
        &format!("{} (end of turn): {clipped}{more}", task.name),
    );
    agents::report(settings, store, &manager, &message, false)?;
    Ok(manager)
}

#[cfg(test)]
mod tests {
    use super::reply;
    use serde_json::{Value, json};

    fn said(at: i64, text: &str) -> Value {
        json!({"type":"message","role":"assistant","created_at":at,"content":[{"type":"output_text","text":text}]})
    }
    fn user(at: i64) -> Value {
        json!({"type":"message","role":"user","created_at":at,"content":[]})
    }
    fn tool(at: i64) -> Value {
        json!({"type":"function_call","created_at":at,"arguments":"amh tell human x"})
    }
    fn output(at: i64) -> Value {
        json!({"type":"function_call_output","created_at":at})
    }

    #[test]
    fn owed_reply_is_the_first_natural_end_after_each_human_message() {
        let asked = [10];
        assert_eq!(reply(&[user(10), said(5, "old")], &asked, true), None);
        // Still working: newest text not known to have ended the turn.
        assert_eq!(reply(&[said(12, "working"), user(10)], &asked, false), None);
        // A mid-turn `amh tell human` does not cancel the reply.
        let event = json!({"type":"resource_event","created_at":15});
        let ended = [event, said(14, " done \n"), output(13), tool(12), user(10)];
        assert_eq!(
            reply(&ended, &asked, true),
            Some(("done".to_owned(), vec![10]))
        );
        // uiclient_steer.md: the answer ended the turn; a notification started another turn, whose text is not owed.
        let steer = [
            said(30, "later"),
            user(29),
            said(20, "answer"),
            output(15),
            tool(12),
            user(10),
        ];
        assert_eq!(
            reply(&steer, &asked, true),
            Some(("answer".to_owned(), vec![10]))
        );
        // A message interrupting a turn after a tool result is no end; the text before it is not sent.
        let interrupted = [
            said(30, "final"),
            output(28),
            user(25),
            output(22),
            said(20, "mid"),
            user(10),
        ];
        assert_eq!(
            reply(&interrupted, &asked, true),
            Some(("final".to_owned(), vec![10]))
        );
        // Two human messages answered by two natural ends, oldest first.
        let two = [said(40, "second"), user(35), said(20, "first"), user(10)];
        assert_eq!(
            reply(&two, &[10, 35], true),
            Some(("first\n\nsecond".to_owned(), vec![10, 35]))
        );
        assert_eq!(
            reply(&two[1..], &[10, 35], false),
            Some(("first".to_owned(), vec![10]))
        );
        // A text in the same second as the delivery belongs to the previous turn.
        assert_eq!(reply(&[user(10), said(10, "old")], &asked, true), None);
        // Both asked before one natural end: one answer.
        assert_eq!(
            reply(&[said(40, "both"), user(12), user(10)], &[10, 12], true),
            Some(("both".to_owned(), vec![10, 12]))
        );
    }
}
