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

/// Prompt for deliberate consolidation; retained messages are not evidence of human unread state.
// 🧑 "Do agents get notification to compress their prior julip messages when sending messages and there are existing pending things." "Implement both. Make sane decisions."
pub fn consolidation_reminder(
    settings: &Settings,
    tag: &str,
    current: &str,
) -> Result<String, String> {
    if settings::get(settings, "zulip.consolidation_reminder") == "false" {
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
    let prior = retained_updates(messages, bot, tag, current);
    if prior.is_empty() {
        return Ok(String::new());
    }
    let ids: Vec<String> = prior
        .iter()
        .filter_map(|message| message["id"].as_u64().map(|id| format!("zulip:{id}")))
        .collect();
    Ok(format!(
        "Earlier retained bot updates for this task (up to 100 recent messages checked): {}. Read them in the task's Zulip topic before replacing them. This does not establish whether the human read them. If they still describe pending work, consolidate their relevant details and all open requests into your next update; use --replaces for superseded IDs after preserving their content in the replacement. Human messages are never consolidation candidates. Do not resend only to satisfy this reminder.",
        ids.join(", ")
    ))
}

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
    let pending_folder = state.join("amh-zulip-pending");
    match fs::read_dir(&pending_folder) {
        Ok(entries) => {
            for entry in entries {
                let path = entry.map_err(|error| error.to_string())?.path();
                let pending: Value =
                    serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                        .map_err(|error| format!("invalid pending Zulip send: {error}"))?;
                if pending["tag"] == tag && pending["body"] == body && pending["subject"] == subject
                {
                    return Err(format!(
                        "the same Zulip send has uncertain delivery, retained at {}; verify delivery and resolve that payload before retrying",
                        path.display()
                    ));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot check pending Zulip sends: {error}")),
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
    if stream.is_empty() {
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
    let result = match api(settings, "POST", "messages", &fields) {
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
                "{error}; payload retained at {}. Delivery may be uncertain; verify Zulip before retrying",
                path.display()
            ));
        }
    };
    let id = result["id"]
        .as_u64()
        .ok_or("Zulip response has no message ID")?;
    if sender != mail::WATCHER {
        use std::io::Write;
        if let Err(error) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state.join(mail::SENT_LOG))
            .and_then(|mut log| {
                writeln!(
                    log,
                    "{} {}",
                    chrono::Utc::now().timestamp(),
                    task_topic(tag)
                )
            })
        {
            eprintln!("amh: Zulip message {id} sent, but recording it failed: {error}");
        }
    }
    Ok(format!("zulip:{id}"))
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
    for id in ids {
        let id = aliases.get(id).unwrap_or(id);
        if let Some(id) = id.strip_prefix("zulip:") {
            let id: u64 = id.parse().map_err(|_| "invalid Zulip replacement ID")?;
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
        let deleted = api(settings, "DELETE", &format!("messages/{id}"), &[]);
        match deleted {
            Ok(_) => {
                fs::remove_file(&path).map_err(|error| error.to_string())?;
            }
            Err(error) if error.contains("Invalid message(s)") => {
                fs::remove_file(&path).map_err(|error| error.to_string())?;
            }
            Err(error) => failures.push(error),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
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

#[cfg(test)]
mod tests {
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
