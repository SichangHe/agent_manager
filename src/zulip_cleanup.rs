//! Archive before deleting old, read bot messages and unreferenced bot uploads.

use crate::{requests, settings, store::Store, zulip};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{Duration, Instant},
};

type Settings = BTreeMap<String, String>;

fn archive(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        if let Some(ancestor) = parent.parent() {
            fs::File::open(ancestor)
                .and_then(|directory| directory.sync_all())
                .map_err(|e| e.to_string())?;
        }
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn preserved(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if path.exists() {
        let saved = fs::read(path).map_err(|e| e.to_string())?;
        if saved != bytes {
            return Err(format!("archive differs: {}", path.display()));
        }
        Ok(())
    } else {
        archive(path, bytes)
    }
}

fn old(timestamp: i64, cutoff_s: i64) -> bool {
    timestamp > 0 && timestamp < cutoff_s
}

fn upload_old(upload: &Value, cutoff_s: i64) -> bool {
    let Some(mut timestamp) = upload["create_time"].as_i64() else {
        return false;
    };
    if timestamp > 100_000_000_000 {
        timestamp /= 1000;
    }
    let references = upload.get("message_ids").or_else(|| upload.get("messages"));
    old(timestamp, cutoff_s)
        && references
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
}

fn archive_message_uploads(
    settings: &Settings,
    folder: &Path,
    message: &Value,
) -> Result<(), String> {
    let content = message["content"].as_str().ok_or("message lacks content")?;
    let message_id = message["id"].as_u64().ok_or("message lacks ID")?;
    if !content.contains("/user_uploads/") {
        return Ok(());
    }
    let response = zulip::api(settings, "GET", "attachments", &[])?;
    let uploads = response["attachments"]
        .as_array()
        .ok_or("invalid attachments")?;
    for segment in content.split("/user_uploads/").skip(1) {
        let path = segment
            .split(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '<' | '\'' | '"'))
            .next()
            .unwrap_or("");
        let upload = uploads
            .iter()
            .find(|upload| upload["path_id"].as_str() == Some(path))
            .ok_or("message references an upload not owned by AMH; retained")?;
        let link = format!("/user_uploads/{path}");
        if path.is_empty()
            || path.starts_with('/')
            || path.contains("..")
            || path.contains(['?', '#', '\\'])
        {
            return Err("invalid upload path".to_owned());
        }
        let id = upload["id"].as_u64().ok_or("upload lacks ID")?;
        let bytes = zulip::download(settings, &link, 20 * 1024 * 1024)?;
        if upload["size"].as_u64() != Some(bytes.len() as u64) {
            return Err("upload archive size differs; retained".to_owned());
        }
        preserved(&folder.join(format!("uploads/{id}.bin")), &bytes)?;
        preserved(
            &folder.join(format!("uploads/{id}-message-{message_id}.json")),
            &serde_json::to_vec_pretty(upload).map_err(|e| e.to_string())?,
        )?;
    }
    Ok(())
}

// 🧑 "Implement both. Make sane decisions."
pub fn run(settings: &Settings, store: &Store) -> Result<String, String> {
    let started = Instant::now();
    let days: i64 = settings
        .get("zulip.cleanup_days")
        .map(String::as_str)
        .unwrap_or("30")
        .parse()
        .map_err(|_| "invalid zulip.cleanup_days")?;
    if days == 0 {
        return Ok("Zulip cleanup disabled".to_owned());
    }
    if !(1..=36500).contains(&days) {
        return Err("zulip.cleanup_days must be 0 or 1..36500".to_owned());
    }
    let stream: u64 = settings::get(settings, "zulip.stream_id")
        .parse()
        .map_err(|_| "Zulip cleanup requires an AMH stream")?;
    let human: u64 = settings::get(settings, "zulip.human_id")
        .parse()
        .map_err(|_| "invalid Zulip human ID")?;
    let bot = zulip::api(settings, "GET", "users/me", &[])?["user_id"]
        .as_u64()
        .ok_or("Zulip bot has no ID")?;
    let completed: BTreeSet<String> = store
        .all()?
        .into_iter()
        .map(|task| {
            let safe = task.status() == "done" && task.items.is_empty();
            (task, safe)
        })
        .map(|(task, safe)| {
            let records = requests::read(&task)?;
            Ok(
                (safe && records.iter().all(|r| r.stage == requests::Stage::Resolved))
                    .then(|| task.tag().to_owned()),
            )
        })
        .collect::<Result<Vec<_>, String>>()?
        .into_iter()
        .flatten()
        .collect();
    let cutoff_s = chrono::Utc::now().timestamp() - days * 86400;
    let folder = store.state_dir.join("zulip-archive");
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    fs::File::open(&store.state_dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())?;
    let cursor_path = store.state_dir.join("zulip-cleanup-cursor.txt");
    let mut cursor: u64 = match fs::read_to_string(&cursor_path) {
        Ok(raw) => raw.trim().parse().unwrap_or(0),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            0
        }
        Err(e) => return Err(e.to_string()),
    };
    let narrow =
        json!([{"operator":"stream","operand":stream},{"operator":"sender","operand":bot}]);
    let mut n_messages = 0;
    'pages: for _ in 0..5 {
        let page = zulip::api(
            settings,
            "GET",
            "messages",
            &[
                ("anchor", cursor.to_string()),
                ("include_anchor", "false".to_owned()),
                ("num_before", "0".to_owned()),
                ("num_after", "100".to_owned()),
                ("apply_markdown", "false".to_owned()),
                ("narrow", narrow.to_string()),
            ],
        )?;
        let messages = page["messages"]
            .as_array()
            .ok_or("invalid cleanup messages")?;
        if messages.is_empty() {
            cursor = 0;
            break;
        }
        for message in messages {
            if started.elapsed() >= Duration::from_secs(15) {
                break 'pages;
            }
            let id = message["id"].as_u64().ok_or("message has no ID")?;
            if id <= cursor {
                return Err("cleanup pagination did not advance".to_owned());
            }
            cursor = id;
            if !old(message["timestamp"].as_i64().unwrap_or(0), cutoff_s) {
                cursor = 0;
                break;
            }
            if message["sender_id"].as_u64() != Some(bot)
                || message["stream_id"].as_u64() != Some(stream)
                || !message["subject"]
                    .as_str()
                    .is_some_and(|topic| completed.contains(topic))
            {
                continue;
            }
            let receipts = match zulip::api(
                settings,
                "GET",
                &format!("messages/{id}/read_receipts"),
                &[],
            ) {
                Ok(receipts) => receipts,
                Err(_) => continue,
            };
            if !receipts["user_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id.as_u64() == Some(human)))
            {
                continue;
            }
            preserved(
                &folder.join(format!("messages/{id}.json")),
                &serde_json::to_vec_pretty(message).map_err(|e| e.to_string())?,
            )?;
            let topic = message["subject"].as_str().ok_or("message has no topic")?;
            if archive_message_uploads(settings, &folder, message).is_err() {
                continue;
            }
            let _lock = store.lock()?;
            let current_task = store.load(&format!("{topic}.md"))?;
            if current_task.status() != "done"
                || !current_task.items.is_empty()
                || requests::read(&current_task)?
                    .iter()
                    .any(|request| request.stage != requests::Stage::Resolved)
            {
                continue;
            }
            zulip::api(settings, "DELETE", &format!("messages/{id}"), &[])?;
            n_messages += 1;
        }
        if cursor == 0 {
            break;
        }
    }
    fs::write(cursor_path, cursor.to_string()).map_err(|e| e.to_string())?;
    let pending_dir = store.state_dir.join("amh-zulip-pending");
    let pending = match fs::read_dir(pending_dir) {
        Ok(entries) => entries
            .map(|entry| {
                let path = entry.map_err(|e| e.to_string())?.path();
                fs::read_to_string(path).map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, String>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.to_string()),
    };
    let uploads = zulip::api(settings, "GET", "attachments", &[])?;
    let uploads_started = Instant::now();
    let mut n_uploads = 0;
    let mut n_retained = 0;
    let upload_cursor_path = store.state_dir.join("zulip-cleanup-upload-cursor.txt");
    let mut upload_cursor: u64 = match fs::read_to_string(&upload_cursor_path) {
        Ok(raw) => raw.trim().parse().unwrap_or(0),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            0
        }
        Err(e) => return Err(e.to_string()),
    };
    let mut candidates: Vec<&Value> = uploads["attachments"]
        .as_array()
        .ok_or("invalid attachments")?
        .iter()
        .filter(|u| upload_old(u, cutoff_s))
        .filter(|u| u["id"].as_u64().is_some_and(|id| id > upload_cursor))
        .collect();
    candidates.sort_by_key(|u| u["id"].as_u64());
    if candidates.is_empty() {
        upload_cursor = 0;
    }
    for upload in candidates.into_iter().take(20) {
        if uploads_started.elapsed() >= Duration::from_secs(15) {
            break;
        }
        let id = upload["id"].as_u64().ok_or("upload lacks ID")?;
        upload_cursor = id;
        let path = upload["path_id"].as_str().ok_or("upload lacks path")?;
        if pending.iter().any(|payload| payload.contains(path)) {
            continue;
        }
        if path.is_empty()
            || path.starts_with('/')
            || path.contains("..")
            || path.contains(['?', '#', '\\'])
        {
            continue;
        }
        let bytes =
            match zulip::download(settings, &format!("/user_uploads/{path}"), 20 * 1024 * 1024) {
                Ok(bytes) => bytes,
                Err(_) => {
                    n_retained += 1;
                    continue;
                }
            };
        if upload["size"].as_u64() != Some(bytes.len() as u64) {
            return Err("upload archive size differs; retained".to_owned());
        }
        preserved(&folder.join(format!("uploads/{id}.bin")), &bytes)?;
        preserved(
            &folder.join(format!("uploads/{id}.json")),
            &serde_json::to_vec_pretty(upload).map_err(|e| e.to_string())?,
        )?;
        let current = zulip::api(settings, "GET", "attachments", &[])?;
        if current["attachments"].as_array().is_some_and(|items| {
            items.iter().any(|u| {
                u["id"].as_u64() == Some(id)
                    && u["path_id"] == upload["path_id"]
                    && u["size"] == upload["size"]
                    && upload_old(u, cutoff_s)
            })
        }) {
            zulip::api(settings, "DELETE", &format!("attachments/{id}"), &[])?;
            n_uploads += 1;
        }
    }
    fs::write(upload_cursor_path, upload_cursor.to_string()).map_err(|e| e.to_string())?;
    Ok(format!(
        "Zulip cleanup archived/deleted {n_messages} messages and {n_uploads} uploads; {n_retained} uploads retained after download failure"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upload_age_and_reference_schemas() {
        assert!(upload_old(
            &json!({"create_time":1000,"message_ids":[]}),
            2000
        ));
        assert!(upload_old(
            &json!({"create_time":1_000_000_000_000i64,"messages":[]}),
            1_000_000_001
        ));
        assert!(!upload_old(
            &json!({"create_time":1000,"message_ids":[1]}),
            2000
        ));
        assert!(!upload_old(&json!({"create_time":1000}), 2000));
        assert!(!upload_old(&json!({"create_time":0,"messages":[]}), 2000));
    }
    #[test]
    fn archive_retry_and_conflict_preserve_original() -> Result<(), String> {
        let folder =
            std::env::temp_dir().join(format!("amh-zulip-archive-test-{}", std::process::id()));
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let path = folder.join("message.json");
        preserved(&path, b"original")?;
        preserved(&path, b"original")?;
        assert!(preserved(&path, b"different").is_err());
        assert_eq!(fs::read(&path).map_err(|e| e.to_string())?, b"original");
        fs::remove_dir_all(folder).map_err(|e| e.to_string())?;
        Ok(())
    }
}
