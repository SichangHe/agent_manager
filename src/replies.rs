//! Turn replies: what an agent says at the end of a turn goes to whoever started that turn, unless the agent already told them with `amh tell`.
// 🧑 "Agents really love using the terminal printout for reply, so let's automatically send whatever the agent said at the end of the turn. This also needs to be worked on s.t. pending item notifications and messages replacements work"

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

type Settings = std::collections::BTreeMap<String, String>;
use crate::store::Store;
use crate::{actions, agents, omnigent, work};

/// The sender a reply goes to when the human started the turn.
pub const HUMAN: &str = "human";
/// A turn that has not ended this long after its delivery is no longer answered.
const EXPIRY_S: i64 = 12 * 3600;
/// A reply that keeps failing to send is given up this long after its delivery.
const RETRY_S: i64 = 600;

fn folder(state_dir: &Path) -> PathBuf {
    state_dir.join("amh-replies")
}

/// The time to pass to `expect`, taken before sending, so no text of the turn predates it.
pub fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Remember that `from` (`HUMAN` or an agent address) sent the agent at `address` a message at `since_s`, so its end-of-turn text goes back to `from`.
pub fn expect(state_dir: &Path, address: &str, from: &str, since_s: i64) -> Result<(), String> {
    let id = omnigent::session(address)?;
    let dir = folder(state_dir);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let temp = dir.join(format!(".{id}.{}", std::process::id()));
    fs::write(&temp, format!("{from} {since_s}\n"))
        .and_then(|()| fs::rename(&temp, dir.join(id)))
        .map_err(|error| format!("cannot record the reply expected from {address}: {error}"))
}

/// What a finished turn calls for.
#[derive(Debug, PartialEq)]
pub enum Reply {
    /// No assistant text since the delivery yet.
    Wait,
    /// The agent ran `amh tell` to the sender during the turn.
    Told,
    /// Send this text.
    Send(String),
}

/// Whether `arguments` of a tool call run `amh tell RECIPIENT` for one of `recipients`, with `--task-file NAME` allowed before it.
fn tells(arguments: &str, recipients: &[&str]) -> bool {
    arguments.match_indices("amh tell ").any(|(at, found)| {
        let mut words = arguments[at + found.len()..]
            .split(|c: char| c.is_whitespace() || c == '\\')
            .filter(|word| !word.is_empty());
        let mut word = words.next();
        while word.is_some_and(|word| word.starts_with("--task-file")) {
            if word == Some("--task-file") {
                words.next();
            }
            word = words.next();
        }
        word.is_some_and(|word| recipients.contains(&word))
    })
}

/// The reply of an ended turn from session items, newest first; `since_s` is the delivery time. Pure.
pub fn reply(items: &[Value], since_s: i64, recipients: &[&str]) -> Reply {
    let turn: Vec<&Value> = items
        .iter()
        .take_while(|item| item["created_at"].as_i64().is_some_and(|at| at >= since_s))
        .collect();
    let told = turn.iter().any(|item| {
        item["type"] == "function_call" && tells(&item["arguments"].to_string(), recipients)
    });
    let last = turn.iter().find_map(|item| {
        let text: String = (item["type"] == "message" && item["role"] == "assistant")
            .then(|| item["content"].as_array())
            .flatten()?
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Some(text.trim().to_owned()).filter(|text| !text.is_empty())
    });
    match (told, last) {
        (_, None) => Reply::Wait,
        (true, Some(_)) => Reply::Told,
        (false, Some(text)) => Reply::Send(text),
    }
}

/// Send each ended turn's text to its sender; called by the watcher every poll.
pub fn forward(settings: &Settings, store: &Store) -> Vec<String> {
    let Ok(entries) = fs::read_dir(folder(&store.state_dir)) else {
        return Vec::new();
    };
    let mut log = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if id.starts_with('.') {
            continue;
        }
        let Ok(row) = fs::read_to_string(&path) else {
            continue;
        };
        let (from, since_s) = row.trim().split_once(' ').unwrap_or_default();
        let since_s: i64 = since_s.parse().unwrap_or_default();
        let address = format!("{}{id}", omnigent::PREFIX);
        let done = |log: &mut Vec<String>, line: String| {
            // A delivery recorded meanwhile starts a new expectation, so keep it.
            if fs::read_to_string(&path).is_ok_and(|now| now == row)
                && let Err(error) = fs::remove_file(&path)
            {
                log.push(format!("cannot remove {}: {error}", path.display()));
            }
            log.push(line);
        };
        if now_s() - since_s > EXPIRY_S {
            done(
                &mut log,
                format!("turn reply of {address}: no turn end within 12 h; dropped"),
            );
            continue;
        }
        if omnigent::fetched_status(settings, &address).0 != "ready" {
            continue;
        }
        let recipients: &[&str] = if from == HUMAN {
            &[HUMAN]
        } else {
            &["manager", "agent"]
        };
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
        match reply(&items, since_s, recipients) {
            Reply::Wait => {}
            Reply::Told => done(
                &mut log,
                format!("turn reply of {address}: already told {from}"),
            ),
            Reply::Send(text) => match send(settings, store, &address, from, &text) {
                Ok(to) => done(&mut log, format!("turn reply of {address} sent to {to}")),
                Err(error) if now_s() - since_s > RETRY_S => done(
                    &mut log,
                    format!("turn reply of {address} to {from} failed; dropped: {error}"),
                ),
                Err(error) => log.push(format!(
                    "turn reply of {address} to {from} failed; retrying: {error}"
                )),
            },
        }
    }
    log
}

/// Send `text`, the end-of-turn reply of the agent at `address`, to `from`; returns who got it.
fn send(
    settings: &Settings,
    store: &Store,
    address: &str,
    from: &str,
    text: &str,
) -> Result<String, String> {
    let task = store
        .active()?
        .into_iter()
        .find(|task| task.address() == address)
        .ok_or("no active task has this agent")?;
    let manager = task.field("managerat").unwrap_or_default().to_owned();
    if from == HUMAN && actions::writes_to_human(settings, &task)? {
        let (receipt, reminder) = actions::tell_human(settings, &task, "", text, &[], &[], None)?;
        if !reminder.is_empty() {
            let note = format!(
                "amh sent the text you ended your turn with to the human.\n{receipt}{reminder}"
            );
            omnigent::send(settings, address, &note)?;
        }
        return Ok(HUMAN.to_owned());
    }
    let to = if from == HUMAN {
        manager.as_str()
    } else {
        from
    };
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
    match to == manager {
        true => agents::report(settings, store, to, &message, false).map(|_| ()),
        false => omnigent::send(settings, to, &message),
    }?;
    Ok(to.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Reply, reply};
    use serde_json::json;

    #[test]
    fn turn_reply_needs_new_text_and_skips_explicit_tells() {
        let old = json!({"type":"message","role":"assistant","created_at":5,"content":[{"type":"output_text","text":"old"}]});
        let user = json!({"type":"message","role":"user","created_at":10,"content":[{"type":"input_text","text":"hi"}]});
        assert_eq!(
            reply(&[user.clone(), old.clone()], 10, &["human"]),
            Reply::Wait
        );
        let said = json!({"type":"message","role":"assistant","created_at":12,"content":[{"type":"output_text","text":" done \n"}]});
        assert_eq!(
            reply(&[said.clone(), user.clone(), old.clone()], 10, &["human"]),
            Reply::Send("done".to_owned())
        );
        let call = json!({"type":"function_call","created_at":11,"arguments":"{\"command\":\"amh tell --task-file x.md human --subject s 'hi'\"}"});
        assert_eq!(
            reply(&[said.clone(), call.clone(), user.clone()], 10, &["human"]),
            Reply::Told
        );
        assert_eq!(
            reply(
                &[said.clone(), call, user.clone()],
                10,
                &["manager", "agent"]
            ),
            Reply::Send("done".to_owned())
        );
        let other = json!({"type":"function_call","created_at":11,"arguments":"{\"command\":\"amh tell manager 'ask the human'\"}"});
        assert_eq!(
            reply(&[said, other, user], 10, &["human"]),
            Reply::Send("done".to_owned())
        );
    }
}
