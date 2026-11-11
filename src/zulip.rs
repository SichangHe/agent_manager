//! Human messages through the configured Zulip bot; Gmail remains external-mail intake.

use crate::{mail, settings};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, time::Duration};

type Settings = BTreeMap<String, String>;

// 🧑 "Set it up s.t. we do Zulip instead of emails, and start messaging me with Zulip"
pub fn enabled(settings: &Settings) -> bool {
    settings::get(settings, "human.transport") == "zulip"
}

pub(crate) fn credentials(settings: &Settings) -> Result<Settings, String> {
    let path = settings::get(settings, "zulip.env_file");
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read Zulip credentials: {error}"))?;
    let mut values = Settings::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if let Some((key, value)) = line.strip_prefix("export ").unwrap_or(line).split_once('=') {
            values.insert(
                key.trim().to_owned(),
                value.trim().trim_matches(['\'', '"']).to_owned(),
            );
        }
    }
    for key in ["ZULIP_BOT_EMAIL", "ZULIP_BOT_API_KEY", "ZULIP_SITE"] {
        if settings::get(&values, key).is_empty() {
            return Err(format!("Zulip credentials are missing {key}"));
        }
    }
    Ok(values)
}

pub(crate) fn api(
    settings: &Settings,
    method: &str,
    path: &str,
    fields: &[(&str, String)],
) -> Result<Value, String> {
    request(settings, method, path, fields, None)
}

/// Whether the human has not read bot message `id`, from its Zulip read receipts.
fn unread_by_human(settings: &Settings, id: u64) -> Result<bool, String> {
    let human = settings::get(settings, "zulip.human_id").parse::<u64>();
    let human = human.map_err(|_| "invalid zulip.human_id")?;
    let path = format!("messages/{id}/read_receipts");
    let receipts = match api(settings, "GET", &path, &[]) {
        // A message deleted since it was listed is no longer waiting to be read.
        Err(error) if error.contains("Invalid message") => return Ok(false),
        receipts => receipts?,
    };
    let readers = receipts["user_ids"]
        .as_array()
        .ok_or("Zulip gave no read receipts")?;
    Ok(!readers.iter().any(|reader| reader.as_u64() == Some(human)))
}

/// Each bot message in the human's channel that the human has not read, among the newest `CHECKED`, oldest first: its ID, topic, and text.
///
/// A message once read stays read, so read IDs are remembered in SQLite record `amh-zulip-read.txt` and not asked about again.
// 🧑 "I think I do have more than 16 unread messages. And so why is not the global message compression happening?"
pub fn unread(settings: &Settings, state_dir: &Path) -> Result<Vec<(u64, String, String)>, String> {
    const CHECKED: usize = 150;
    let bot = api(settings, "GET", "users/me", &[])?["user_id"]
        .as_u64()
        .ok_or("Zulip profile has no user ID")?;
    let stream = settings::get(settings, "zulip.stream_id").parse::<u64>();
    let stream = stream.map_err(|_| "counting unread Zulip messages needs zulip.stream_id")?;
    let narrow =
        json!([{"operator":"sender","operand":bot},{"operator":"stream","operand":stream}]);
    let result = api(
        settings,
        "GET",
        "messages",
        &[
            ("anchor", "newest".to_owned()),
            ("num_before", CHECKED.to_string()),
            ("num_after", "0".to_owned()),
            ("apply_markdown", "false".to_owned()),
            ("narrow", narrow.to_string()),
        ],
    )?;
    let messages = result["messages"]
        .as_array()
        .ok_or("Zulip response has no messages")?;
    let store = crate::store::Store {
        root: settings::work_logs(settings),
        state_dir: state_dir.to_owned(),
    };
    let known = store.record("amh-zulip-read.txt")?;
    let known: Vec<u64> = known.lines().filter_map(|id| id.parse().ok()).collect();
    let (mut read, mut waiting) = (Vec::new(), Vec::new());
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    for message in messages {
        let Some(id) = message["id"].as_u64() else {
            continue;
        };
        if known.contains(&id) || !unread_by_human(settings, id)? {
            read.push(id);
        } else {
            waiting.push((id, text(&message["subject"]), text(&message["content"])));
        }
    }
    let read = crate::database::Database::open(&state_dir.join("amh.sqlite3"))?.remember_ids(
        "amh-zulip-read.txt",
        &read,
        CHECKED,
    )?;
    waiting.retain(|(id, _, _)| !read.contains(id));
    Ok(waiting)
}

/// Tell an agent that just sent an update which of its task's earlier updates the human has not read, to merge them.
// 🧑 "Do agents get notification to compress their prior julip messages when sending messages and there are existing pending things." "Implement both. Make sane decisions."
pub fn consolidation_reminder(
    settings: &Settings,
    tag: &str,
    current: &str,
) -> Result<String, String> {
    if settings::get(settings, "zulip.consolidation_reminder") == "false" {
        return Ok(String::new());
    }
    // PB report topics stay separate updates, so nothing in them is merged: the `mail::replace_reminder` exemption (commit efc04df).
    if mail::REPORT_LABELS.contains(&tag) {
        return Ok(String::new());
    }
    let bot = api(settings, "GET", "users/me", &[])?["user_id"]
        .as_u64()
        .ok_or("Zulip profile has no user ID")?;
    let stream = settings::get(settings, "zulip.stream_id");
    let mut narrow = vec![json!({"operator":"sender","operand":bot})];
    if stream.is_empty() {
        let human: u64 = settings::get(settings, "zulip.human_id")
            .parse()
            .map_err(|_| "invalid zulip.human_id")?;
        narrow.push(json!({"operator":"dm","operand":human}));
    } else {
        let stream: u64 = stream.parse().map_err(|_| "invalid zulip.stream_id")?;
        narrow.push(json!({"operator":"stream","operand":stream}));
        narrow.push(json!({"operator":"topic","operand":tag}));
    }
    let result = api(
        settings,
        "GET",
        "messages",
        &[
            ("anchor", "newest".to_owned()),
            ("num_before", "100".to_owned()),
            ("num_after", "0".to_owned()),
            ("apply_markdown", "false".to_owned()),
            ("narrow", Value::Array(narrow).to_string()),
        ],
    )?;
    let messages = result["messages"]
        .as_array()
        .ok_or("Zulip response has no messages")?;
    // 🧑 "some agents have more than one unread messages sitting in my inbox. Why are they not notified to compress them?"
    const NEWEST: usize = 20;
    if current_is_report(messages, current) {
        return Ok(String::new());
    }
    let prior = retained_updates(messages, bot, tag, current);
    let mut ids = Vec::new();
    for id in prior
        .iter()
        .rev()
        .take(NEWEST)
        .filter_map(|message| message["id"].as_u64())
    {
        // A message the human replied after cannot be replaced, so asking to merge it only repeats.
        if unread_by_human(settings, id)? && !answered(settings, id)? {
            ids.push(format!("zulip:{id}"));
        }
    }
    if ids.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "The human has not read {} earlier update(s) of this task: {}. Merge them now: read them with `amh mail unread`, send ONE update that holds everything still worth reading from them and from the message you just sent, with --replaces for each of these IDs and for the message you just sent. Human messages, and yours that the human replied after, are never replaced.",
        ids.len(),
        ids.join(", ")
    ))
}

/// Whether the message's header line `[tag] title` has a PB report label as its title, as `mail::is_report` decides for an email subject.
fn is_report_update(message: &Value) -> bool {
    let header = message["content"]
        .as_str()
        .and_then(|body| body.lines().next());
    header.is_some_and(|line| mail::is_report(line.trim_end()))
}

/// Whether the message just sent, `current`, is a PB report, which is never merged.
fn current_is_report(messages: &[Value], current: &str) -> bool {
    messages.iter().any(|message| {
        message["id"]
            .as_u64()
            .is_some_and(|id| format!("zulip:{id}") == current)
            && is_report_update(message)
    })
}

/// The bot's earlier updates of the task, except PB reports, which stay separate and are never merged.
fn retained_updates(messages: &[Value], bot: u64, tag: &str, current: &str) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| {
            message["sender_id"].as_u64() == Some(bot)
                && message["id"]
                    .as_u64()
                    .is_some_and(|id| format!("zulip:{id}") != current)
                && message["content"]
                    .as_str()
                    .is_some_and(|body| body.starts_with(&format!("[{tag}] ")))
                && !is_report_update(message)
        })
        .cloned()
        .collect()
}

fn request(
    settings: &Settings,
    method: &str,
    path: &str,
    fields: &[(&str, String)],
    upload: Option<(&str, Vec<u8>)>,
) -> Result<Value, String> {
    let values = credentials(settings)?;
    let site = settings::get(&values, "ZULIP_SITE").trim_end_matches('/');
    if !site.starts_with("https://") {
        return Err("Zulip site must use HTTPS".to_owned());
    }
    let auth = STANDARD.encode(format!(
        "{}:{}",
        settings::get(&values, "ZULIP_BOT_EMAIL"),
        settings::get(&values, "ZULIP_BOT_API_KEY")
    ));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let url = format!("{site}/api/v1/{path}");
    let response = match method {
        "UPLOAD" => {
            let (boundary, bytes) = upload.ok_or("missing Zulip upload")?;
            agent
                .post(&url)
                .header("Authorization", &format!("Basic {auth}"))
                .header(
                    "Content-Type",
                    &format!("multipart/form-data; boundary={boundary}"),
                )
                .send(bytes)
        }
        "GET" => agent
            .get(&url)
            .header("Authorization", &format!("Basic {auth}"))
            .query_pairs(fields.iter().map(|(key, value)| (*key, value.as_str())))
            .call(),
        "DELETE" => agent
            .delete(&url)
            .header("Authorization", &format!("Basic {auth}"))
            .call(),
        _ => agent
            .post(&url)
            .header("Authorization", &format!("Basic {auth}"))
            .send_form(fields.iter().map(|(key, value)| (*key, value.as_str()))),
    };
    let mut response =
        response.map_err(|error| format!("Zulip {method} {path} failed: {error}"))?;
    let result: Value = response
        .body_mut()
        .read_json()
        .map_err(|error| format!("invalid Zulip response: {error}"))?;
    if result["result"] != "success" {
        return Err(format!("Zulip request failed: {}", result["msg"]));
    }
    Ok(result)
}

pub fn send(
    settings: &Settings,
    state: &Path,
    tag: &str,
    subject: &str,
    body: &str,
    sender: &str,
    attachments: &[std::path::PathBuf],
) -> Result<String, String> {
    if let Some(path) = pending_match(&state.join("amh-zulip-pending"), tag, subject, body)? {
        return Err(format!(
            "the same Zulip send has uncertain delivery, retained at {}; verify delivery and resolve that payload before retrying",
            path.display()
        ));
    }
    // 🤖 manager request: "make amh refuse duplicate human messages from one task within a short window"
    if sender != mail::WATCHER {
        recent_duplicate(state, tag, subject, body)?;
    }
    let recipient = settings::get(settings, "zulip.human_id");
    let id: u64 = recipient
        .parse()
        .map_err(|_| "zulip.human_id must be the human's numeric user ID")?;
    let mut content = format!("[{tag}] {}\n\n{body}", mail::bare(subject));
    let mut total_bytes = 0u64;
    for path in attachments {
        let size = fs::metadata(path)
            .map_err(|error| format!("cannot inspect attachment: {error}"))?
            .len();
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or("attachment size overflow")?;
        if total_bytes > 20 << 20 {
            return Err("attachments exceed 20 MB".to_owned());
        }
    }
    for path in attachments {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("attachment filename is not UTF-8")?;
        let filename = name.replace(['"', '\r', '\n', '\\'], "_");
        let boundary = format!(
            "amh-upload-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let mut bytes = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
        bytes.extend(fs::read(path).map_err(|error| format!("cannot read attachment: {error}"))?);
        bytes.extend(format!("\r\n--{boundary}--\r\n").as_bytes());
        let uploaded = request(
            settings,
            "UPLOAD",
            "user_uploads",
            &[],
            Some((&boundary, bytes)),
        )?;
        let url = uploaded["uri"].as_str().ok_or("Zulip upload has no URI")?;
        content.push_str(&format!(
            "\n\n[{}]({url})",
            filename.replace(['[', ']'], "_")
        ));
    }
    let stream = settings::get(settings, "zulip.stream_id");
    let mut fields = vec![("content", content)];
    // 🧑 "The PB news should be in its own channel. So is PB urgent."
    let report_stream =
        settings::get(settings, &format!("zulip.report_streams.{}", mail::bare(subject).trim()));
    if !report_stream.is_empty() {
        fields.extend([
            ("type", "stream".to_owned()),
            ("to", report_stream.to_owned()),
            ("topic", mail::bare(subject).trim().to_owned()),
        ]);
    } else if stream.is_empty() {
        fields.extend([
            ("type", "direct".to_owned()),
            ("to", json!([id]).to_string()),
        ]);
    } else {
        let stream: u64 = stream.parse().map_err(|_| "invalid zulip.stream_id")?;
        fields.extend([
            ("type", "stream".to_owned()),
            ("to", stream.to_string()),
            ("topic", tag.to_owned()),
        ]);
    }
    // 🧑 (via the manager) "make amh handle a non-JSON Zulip reply by checking delivery and retrying itself"
    let content = fields
        .iter()
        .find(|(name, _)| *name == "content")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let narrow = narrow_for(&fields);
    let mut sent = api(settings, "POST", "messages", &fields);
    for wait_s in [5, 20] {
        let Err(error) = &sent else { break };
        std::thread::sleep(std::time::Duration::from_secs(wait_s));
        sent = match delivered(settings, &narrow, &content) {
            Some(id) => Ok(json!({"id": id})),
            None if error.contains("invalid Zulip response")
                || error.contains("error sending request") =>
            {
                api(settings, "POST", "messages", &fields)
            }
            None => Err(error.clone()),
        };
    }
    let result = match sent {
        Ok(result) => result,
        Err(error) => {
            use std::io::Write;
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            let folder = state.join("amh-zulip-pending");
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&folder)
                .map_err(|failure| format!("{error}; cannot preserve Zulip send: {failure}"))?;
            let stamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
            let path = folder.join(format!("{stamp}.json"));
            let pending = json!({"tag":tag,"subject":subject,"body":body,"sender":sender,"fields":fields,"error":error,"status":"delivery_unknown_or_refused"});
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut file| file.write_all(pending.to_string().as_bytes()))
                .map_err(|failure| format!("{error}; cannot preserve Zulip send: {failure}"))?;
            return Err(format!(
                "{error}; payload retained at {}. The watcher will retry it automatically (and drop it after 24 h); do not resend it yourself",
                path.display()
            ));
        }
    };
    let id = result["id"]
        .as_u64()
        .ok_or("Zulip response has no message ID")?;
    if sender != mail::WATCHER
        && let Err(error) = mail::record_sent(settings, state, task_topic(tag))
    {
        eprintln!("amh: Zulip message {id} sent, but recording it failed: {error}");
    }
    Ok(format!("zulip:{id}"))
}

/// The search narrow matching where a send's `fields` post to.
fn narrow_for(fields: &[(&str, String)]) -> Value {
    let value = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    if value("type") == "stream" {
        let to = value("to");
        let stream = to
            .parse::<u64>()
            .map(Value::from)
            .unwrap_or(Value::from(to));
        json!([{"operator":"stream","operand":stream},{"operator":"topic","operand":value("topic")}])
    } else {
        json!([{"operator":"dm","operand":value("to")}])
    }
}

const PENDING_MAX_AGE_S: u64 = 24 * 3600;

fn is_rate_limited(error: &str) -> bool {
    let error = error.to_lowercase();
    error.contains("rate limit") || error.contains("rate-limit") || error.contains("429")
}

/// Age of a `<nanos>.json` payload from its name.
fn pending_age_s(path: &Path, now_ns: i64) -> Option<u64> {
    let stamp: i64 = path.file_stem()?.to_str()?.parse().ok()?;
    Some((now_ns.saturating_sub(stamp) / 1_000_000_000).max(0) as u64)
}

/// Retry payloads that `send` retained. Returns log lines; stops the pass at the first rate limit.
pub fn drain_pending(settings: &Settings, state: &Path) -> Vec<String> {
    let folder = state.join("amh-zulip-pending");
    let mut paths: Vec<_> = match fs::read_dir(&folder) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|e| e == "json"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    paths.sort();
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let mut log = Vec::new();
    for path in paths {
        let name = path.display().to_string();
        if pending_age_s(&path, now_ns).is_some_and(|age| age > PENDING_MAX_AGE_S) {
            let stale = folder.join("stale");
            let moved = fs::create_dir_all(&stale)
                .and_then(|()| fs::rename(&path, stale.join(path.file_name().unwrap_or_default())));
            log.push(match moved {
                Ok(()) => format!("Zulip pending {name} older than 24 h; moved to stale/"),
                Err(error) => format!("Zulip pending {name}: cannot move to stale/: {error}"),
            });
            continue;
        }
        let Ok(pending) = fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).map_err(|e| e.to_string()))
        else {
            log.push(format!("Zulip pending {name}: unreadable; left in place"));
            continue;
        };
        let owned: Vec<(String, String)> = pending["fields"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|pair| {
                        Some((pair[0].as_str()?.to_owned(), pair[1].as_str()?.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let fields: Vec<(&str, String)> = owned
            .iter()
            .map(|(key, value)| (key.as_str(), value.clone()))
            .collect();
        let Some(content) = fields.iter().find(|(k, _)| *k == "content").map(|(_, v)| v.clone())
        else {
            log.push(format!("Zulip pending {name}: no content; left in place"));
            continue;
        };
        if delivered(settings, &narrow_for(&fields), &content).is_some() {
            let _ = forget(&path);
            log.push(format!("Zulip pending {name}: already delivered; removed"));
            continue;
        }
        match api(settings, "POST", "messages", &fields) {
            Ok(result) => {
                let _ = forget(&path);
                let tag = pending["tag"].as_str().unwrap_or_default();
                let sender = pending["sender"].as_str().unwrap_or_default();
                if sender != mail::WATCHER
                    && let Err(error) = mail::record_sent(settings, state, task_topic(tag))
                {
                    log.push(format!("Zulip pending {name}: sent, but recording failed: {error}"));
                }
                log.push(format!("Zulip pending {name}: resent as {}", result["id"]));
            }
            Err(error) if is_rate_limited(&error) => {
                log.push(format!("Zulip pending: rate limited, backing off: {error}"));
                break;
            }
            Err(error) => log.push(format!("Zulip pending {name}: retry failed: {error}")),
        }
    }
    log
}

/// Drop a queued deletion; another amh process (or the watcher) draining the same queue may have dropped it first.
fn forget(path: &std::path::Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
        _ => Ok(()),
    }
}

pub fn replace(settings: &Settings, ids: &[String]) -> Result<(), String> {
    let folder = settings::state_dir().join("amh-zulip-deletions");
    fs::create_dir_all(&folder)
        .map_err(|error| format!("cannot queue Zulip deletions: {error}"))?;
    let aliases: BTreeMap<String, String> =
        fs::read(settings::state_dir().join("amh-zulip-aliases.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
    let mut kept = Vec::new();
    for id in ids {
        let id = aliases.get(id).unwrap_or(id);
        if let Some(id) = id.strip_prefix("zulip:") {
            let id: u64 = id.parse().map_err(|_| "invalid Zulip replacement ID")?;
            if answered(settings, id)? {
                kept.push(format!("zulip:{id}"));
                continue;
            }
            fs::write(folder.join(id.to_string()), "")
                .map_err(|error| format!("cannot queue Zulip deletion: {error}"))?;
        }
    }
    let mut failures = Vec::new();
    for entry in
        fs::read_dir(&folder).map_err(|error| format!("cannot read Zulip deletions: {error}"))?
    {
        let path = entry.map_err(|error| error.to_string())?.path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<u64>().ok())
        else {
            continue;
        };
        match answered(settings, id) {
            Ok(true) => {
                forget(&path)?;
                kept.push(format!("zulip:{id}"));
                continue;
            }
            Err(error) => {
                failures.push(error);
                continue;
            }
            Ok(false) => {}
        }
        let deleted = api(settings, "DELETE", &format!("messages/{id}"), &[]);
        match deleted {
            Ok(_) => {
                forget(&path)?;
            }
            Err(error) if error.contains("Invalid message(s)") => {
                forget(&path)?;
            }
            Err(error) => failures.push(error),
        }
    }
    if !kept.is_empty() {
        failures.push(format!(
            "kept {} because the human replied after it; replacement cannot delete these messages",
            kept.join(", ")
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

// 🧑 "disable agent' absility to replace an old message after which I send an N reply"
/// Whether the human sent a message in bot message `id`'s topic after it.
fn answered(settings: &Settings, id: u64) -> Result<bool, String> {
    let message = match api(settings, "GET", &format!("messages/{id}"), &[]) {
        Err(error) if error.contains("Invalid message") => return Ok(false),
        message => message?,
    };
    let message = &message["message"];
    let human: u64 = settings::get(settings, "zulip.human_id")
        .parse()
        .map_err(|_| "invalid zulip.human_id")?;
    let narrow = match message["stream_id"].as_u64() {
        Some(stream) => json!([
            {"operator":"stream","operand":stream},
            {"operator":"topic","operand":message["subject"].as_str().unwrap_or_default()},
            {"operator":"sender","operand":human}
        ]),
        None => json!([{"operator":"dm","operand":human},{"operator":"sender","operand":human}]),
    };
    let after = api(
        settings,
        "GET",
        "messages",
        &[
            ("anchor", id.to_string()),
            ("num_before", "0".to_owned()),
            ("num_after", "1".to_owned()),
            ("include_anchor", "false".to_owned()),
            ("narrow", narrow.to_string()),
        ],
    )?;
    Ok(after["messages"]
        .as_array()
        .is_some_and(|found| !found.is_empty()))
}

pub fn incoming(settings: &Settings, cursor: u64) -> Result<Vec<Value>, String> {
    let human: u64 = settings::get(settings, "zulip.human_id")
        .parse()
        .map_err(|_| "invalid zulip.human_id")?;
    let stream = settings::get(settings, "zulip.stream_id");
    let conversation = if stream.is_empty() {
        json!({"operator":"dm","operand":human})
    } else {
        json!({"operator":"stream","operand":stream.parse::<u64>().map_err(|_| "invalid zulip.stream_id")?})
    };
    let narrow = json!([conversation,{"operator":"sender","operand":human}]);
    let result = api(
        settings,
        "GET",
        "messages",
        &[
            ("anchor", cursor.to_string()),
            ("num_before", "0".to_owned()),
            ("num_after", "100".to_owned()),
            ("include_anchor", "false".to_owned()),
            ("apply_markdown", "false".to_owned()),
            ("narrow", narrow.to_string()),
        ],
    )?;
    Ok(result["messages"]
        .as_array()
        .ok_or("Zulip response has no messages")?
        .clone())
}

/// Find a task header in a direct message or quoted reply; otherwise route to the main manager.
pub fn subject(body: &str) -> String {
    for line in body.lines() {
        let mut line = line.trim_start_matches(['>', ' ', '\t']);
        while let Some(rest) = mail::after_prefix(line) {
            line = rest.trim_start();
        }
        if let Some(tag) = mail::tag_of(line) {
            return format!("[{}] Zulip message", task_topic(tag));
        }
    }
    "Zulip message".to_owned()
}

/// Resolve the human-approved PB report labels to their owning task.
pub fn task_topic(topic: &str) -> &str {
    match mail::REPORT_LABELS.contains(&topic) {
        true => "stock_news_watch",
        false => topic,
    }
}

pub fn own_text(body: &str) -> String {
    let mut quoted = false;
    body.lines()
        .filter(|line| {
            let line = line.trim();
            if line.starts_with("```quote") {
                quoted = true;
                return false;
            }
            if quoted && line.starts_with("```") {
                quoted = false;
                return false;
            }
            !quoted
                && !(line.starts_with('@') && (line.ends_with("said:") || line.contains("[said](")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const DUPLICATE_WINDOW_S: i64 = 30 * 60;

/// Refuse an identical send from the same task within the window, and record this attempt.
/// Attempts are logged before posting, so the guard holds even when a retained payload is moved away.
fn recent_duplicate(state: &Path, tag: &str, subject: &str, body: &str) -> Result<(), String> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (tag, mail::bare(subject).trim(), body.trim()).hash(&mut hasher);
    let digest = format!("{:016x}", hasher.finish());
    let path = state.join("amh-zulip-recent.jsonl");
    let now = chrono::Utc::now().timestamp();
    let old = fs::read_to_string(&path).unwrap_or_default();
    let mut kept: Vec<String> = Vec::new();
    for line in old.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let at = entry["at"].as_i64().unwrap_or(0);
        if now - at >= DUPLICATE_WINDOW_S {
            continue;
        }
        if entry["digest"] == digest.as_str() {
            return Err(format!(
                "refused: this task already sent the human this exact message {} min ago; do not resend it. If it may not have arrived, ask your manager instead",
                (now - at) / 60
            ));
        }
        kept.push(line.to_owned());
    }
    kept.push(json!({"digest": digest, "at": now}).to_string());
    fs::write(&path, kept.join("\n") + "\n")
        .map_err(|error| format!("cannot record Zulip send attempt: {error}"))
}

/// The retained payload of an uncertain send with the same tag, subject and body, if any.
fn pending_match(
    folder: &Path,
    tag: &str,
    subject: &str,
    body: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot check pending Zulip sends: {error}")),
    };
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        // Only payload files count; a folder or note someone put there must not block every send.
        if !path.is_file() || path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let pending: Value =
            serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                .map_err(|error| format!("invalid pending Zulip send: {error}"))?;
        if pending["tag"] == tag && pending["body"] == body && pending["subject"] == subject {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// The ID of a recent message with exactly `content` in `narrow`, when an uncertain send did arrive.
fn delivered(settings: &Settings, narrow: &Value, content: &str) -> Option<u64> {
    let recent = api(
        settings,
        "GET",
        "messages",
        &[
            ("anchor", "newest".to_owned()),
            ("num_before", "10".to_owned()),
            ("num_after", "0".to_owned()),
            ("apply_markdown", "false".to_owned()),
            ("narrow", narrow.to_string()),
        ],
    )
    .ok()?;
    recent["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["content"].as_str().map(str::trim) == Some(content.trim()))
        .and_then(|message| message["id"].as_u64())
}

#[cfg(test)]
mod tests {
    #[test]
    fn identical_send_within_window_is_refused() -> Result<(), String> {
        let dir = std::env::temp_dir().join(format!("amh-dup-{}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        recent_duplicate(&dir, "a", "s", "body")?;
        assert!(recent_duplicate(&dir, "a", "s", "body").is_err());
        recent_duplicate(&dir, "b", "s", "body")?;
        recent_duplicate(&dir, "a", "s", "other")?;
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())
    }

    #[test]
    fn pending_check_skips_folders_and_other_files() -> Result<(), Box<dyn std::error::Error>> {
        let folder = std::env::temp_dir().join(format!("amh-pending-{}", std::process::id()));
        fs::create_dir_all(folder.join("done"))?;
        fs::write(folder.join("note.txt"), "x")?;
        assert_eq!(super::pending_match(&folder, "t", "s", "b")?, None);
        fs::write(
            folder.join("1.json"),
            json!({"tag":"t","subject":"s","body":"b"}).to_string(),
        )?;
        assert_eq!(
            super::pending_match(&folder, "t", "s", "b")?,
            Some(folder.join("1.json"))
        );
        fs::remove_dir_all(&folder)?;
        Ok(())
    }

    #[test]
    fn pending_age_and_rate_limit_detection() {
        let p = Path::new("/x/1000000000.json");
        assert_eq!(pending_age_s(p, 3_000_000_000), Some(2));
        assert_eq!(pending_age_s(Path::new("/x/note.json"), 5), None);
        assert!(is_rate_limited("Zulip request failed: \"API usage exceeded rate limit\""));
        assert!(!is_rate_limited("Zulip request failed: bad stream"));
    }
    #[test]
    fn narrow_matches_destination() {
        let dm = vec![("type", "direct".to_owned()), ("to", "[7]".to_owned())];
        assert_eq!(narrow_for(&dm)[0]["operator"], "dm");
        let st = vec![
            ("type", "stream".to_owned()),
            ("to", "5".to_owned()),
            ("topic", "t".to_owned()),
        ];
        assert_eq!(narrow_for(&st)[0]["operand"], 5);
        assert_eq!(narrow_for(&st)[1]["operand"], "t");
    }
    #[test]
    fn stale_payloads_move_without_network() -> Result<(), Box<dyn std::error::Error>> {
        let state = std::env::temp_dir().join(format!("amh-drain-{}", std::process::id()));
        let folder = state.join("amh-zulip-pending");
        fs::create_dir_all(&folder)?;
        fs::write(folder.join("1000.json"), "{}")?;
        let log = drain_pending(&Settings::new(), &state);
        assert!(log[0].contains("moved to stale/"), "{log:?}");
        assert!(folder.join("stale/1000.json").exists());
        assert!(!folder.join("1000.json").exists());
        fs::remove_dir_all(&state)?;
        Ok(())
    }
    use super::*;
    #[test]
    fn disabled_consolidation_needs_no_credentials() -> Result<(), String> {
        let settings = Settings::from([(
            "zulip.consolidation_reminder".to_owned(),
            "false".to_owned(),
        )]);
        assert_eq!(consolidation_reminder(&settings, "task", "zulip:1")?, "");
        Ok(())
    }
    #[test]
    #[ignore = "reads the configured live Zulip account without sending messages"]
    fn live_consolidation_reminder() -> Result<(), String> {
        let settings = settings::load()?;
        let reminder = consolidation_reminder(&settings, "amh_support_codex", "zulip:0")?;
        assert!(reminder.contains("Read them in the task's Zulip topic"));
        Ok(())
    }
    #[test]
    fn consolidation_keeps_only_prior_bot_updates_for_exact_task() {
        let messages = vec![
            json!({"id":1,"sender_id":7,"content":"[task] pending decision"}),
            json!({"id":2,"sender_id":8,"content":"[task] human reply"}),
            json!({"id":3,"sender_id":7,"content":"[task_other] other task"}),
            json!({"id":4,"sender_id":7,"content":"[task] just sent"}),
            json!({"sender_id":7,"content":"[task] malformed"}),
        ];
        assert_eq!(
            retained_updates(&messages, 7, "task", "zulip:4"),
            vec![messages[0].clone()]
        );
    }
    #[test]
    fn pb_report_topics_get_no_merge_reminder_offline() {
        // No settings: any Zulip request would fail on missing credentials, so `Ok("")` proves none was made.
        for label in mail::REPORT_LABELS {
            assert_eq!(
                consolidation_reminder(&Settings::new(), label, "zulip:1"),
                Ok(String::new())
            );
        }
        let error = consolidation_reminder(&Settings::new(), "task", "zulip:1").unwrap_err();
        assert!(error.contains("cannot read Zulip credentials"), "{error}");
    }
    #[test]
    fn consolidation_leaves_pb_reports_unmerged() {
        let messages = vec![
            json!({"id":1,"sender_id":7,"content":"[task] pb news\n\nreport"}),
            json!({"id":2,"sender_id":7,"content":"[task] Progress\n\nsee [x] pb news"}),
            json!({"id":3,"sender_id":7,"content":"[task] pb urgent"}),
            json!({"id":4,"sender_id":7,"content":"[task] pb new\n\nold label"}),
            json!({"id":5,"sender_id":7,"content":"[task] Next"}),
            json!({"id":6,"sender_id":7,"content":"[task] pb news\n\nreport"}),
        ];
        // Only the first line decides: a label in the body does not make an update a report.
        assert_eq!(
            retained_updates(&messages, 7, "task", "zulip:5"),
            vec![messages[1].clone()]
        );
        assert!(!current_is_report(&messages, "zulip:5"));
        assert!(!current_is_report(&messages, "zulip:2"));
        for current in ["zulip:1", "zulip:3", "zulip:4", "zulip:6"] {
            assert!(current_is_report(&messages, current), "{current}");
        }
        assert!(!current_is_report(&messages, "zulip:99"));
        assert!(!current_is_report(&[], "zulip:1"));
    }
    #[test]
    fn direct_and_quoted_task_replies_route() {
        for label in mail::REPORT_LABELS {
            assert_eq!(task_topic(label), "stock_news_watch");
            assert_eq!(
                subject(&format!("> [{label}] Daily report\nReply")),
                "[stock_news_watch] Zulip message"
            );
        }
        assert_eq!(task_topic("other"), "other");
        assert_eq!(
            subject("Re: Fwd: [pb urgent] Daily report"),
            "[stock_news_watch] Zulip message"
        );
        assert_eq!(subject("[pb new-ish] Unknown"), "Zulip message");
        assert_eq!(
            subject("[amh_support_codex] Set it up"),
            "[amh_support_codex] Zulip message"
        );
        assert_eq!(
            subject("@**bot** said:\n```quote\n[task_name] status\n```\nImplement"),
            "[task_name] Zulip message"
        );
        assert_eq!(
            subject("@**bot** [said](https://example.org):\n> [task_name] status\nImplement"),
            "[task_name] Zulip message"
        );
        assert_eq!(subject("for manager\nImplement"), "Zulip message");
    }
    #[test]
    fn routing_ignores_zulip_quote_attribution_and_fences() {
        let text = "@**bot** [said](https://example.org):\n```quote\n[task_name] status\n```\nreplace this agent";
        assert_eq!(own_text(text), "replace this agent");
        assert!(crate::decide::for_manager(&own_text(text)));
    }
}
