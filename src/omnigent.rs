//! Talking to the Omnigent server, which runs every agent session.
// 🧑 "the Rust port would only support Omnigent, not tmux"

use std::collections::BTreeMap;

use serde_json::Value;

pub const PREFIX: &str = "omnigent://";

/// Ask the Omnigent server for `path` and return its JSON answer.
pub fn get(settings: &BTreeMap<String, String>, path: &str) -> Result<Value, String> {
    let base = settings
        .get("omnigent.url")
        .map(String::as_str)
        .filter(|url| !url.is_empty())
        .unwrap_or("http://127.0.0.1:6767");
    let mut request = ureq::get(format!("{base}{path}"));
    if let Some(token) = settings
        .get("omnigent.token")
        .filter(|token| !token.is_empty())
    {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut answer = request
        .call()
        .map_err(|error| format!("Omnigent did not answer GET {path}: {error}"))?;
    answer
        .body_mut()
        .read_json()
        .map_err(|error| format!("Omnigent gave no JSON for GET {path}: {error}"))
}
