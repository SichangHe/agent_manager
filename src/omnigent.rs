//! Talking to the Omnigent server, which runs every agent session: send, status, stop, launch.
// 🧑 "the Rust port would only support Omnigent, not tmux"

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const PREFIX: &str = "omnigent://";
/// Sent after a multi-line message: Claude Code shows such a message as pasted text and acts on it only when a typed line says to.
const FOLLOW_UP: &str = "The message above reached you through `amh`, the helper that carries the human's and your manager's requests to you; act on it.";
const WAIT: Duration = Duration::from_secs(90);

type Settings = BTreeMap<String, String>;

/// The session part of an agent address.
fn session(address: &str) -> Result<&str, String> {
    address.strip_prefix(PREFIX).ok_or_else(|| {
        format!("{address} is not an Omnigent session; amh reaches agents on Omnigent only")
    })
}

/// Call the Omnigent server and return its JSON answer: a POST when there is a `body`, else a GET.
pub fn api(
    settings: &Settings,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let base = settings
        .get("omnigent.url")
        .map(String::as_str)
        .filter(|url| !url.is_empty())
        .unwrap_or("http://127.0.0.1:6767");
    let url = format!("{base}{path}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let token = settings
        .get("omnigent.token")
        .filter(|token| !token.is_empty())
        .map(|token| format!("Bearer {token}"));
    let sent = match body {
        Some(body) => {
            let mut request = agent.post(&url).header("Content-Type", "application/json");
            if let Some(token) = &token {
                request = request.header("Authorization", token);
            }
            request.send_json(body)
        }
        None => {
            let mut request = agent.get(&url);
            if let Some(token) = &token {
                request = request.header("Authorization", token);
            }
            request.call()
        }
    };
    let mut answer = sent.map_err(|error| format!("Omnigent is unreachable at {url}: {error}"))?;
    let status = answer.status().as_u16();
    let text = answer.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        let shown: String = text.chars().take(300).collect();
        return Err(format!(
            "Omnigent answered {status} for {method} {path}: {shown}"
        ));
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn post_event(
    settings: &Settings,
    address: &str,
    kind: &str,
    data: Value,
) -> Result<Value, String> {
    api(
        settings,
        "POST",
        &format!("/v1/sessions/{}/events", session(address)?),
        Some(&json!({"type": kind, "data": data})),
    )
}

fn post_message(settings: &Settings, address: &str, text: &str) -> Result<Value, String> {
    post_event(
        settings,
        address,
        "message",
        json!({"role": "user", "content": [{"type": "input_text", "text": text}]}),
    )
}

/// Deliver `text` to an agent as its next user message.
pub fn send(settings: &Settings, address: &str, text: &str) -> Result<(), String> {
    let answer = post_message(settings, address, text)?;
    if answer["queued"] != true {
        return Err(format!("{address} did not queue the message: {answer}"));
    }
    if text.trim().contains('\n') {
        let _ = post_message(settings, address, FOLLOW_UP);
    }
    Ok(())
}

/// A JSON value as the evidence line shows it.
fn shown(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Classify a session as `running`, `ready` (idle), `stuck` (waiting on an unanswered prompt), `error`, or `missing`, with one line of evidence. Pure.
pub fn classify(session: &Value) -> (&'static str, String) {
    let state = session["status"].as_str().unwrap_or_default();
    let evidence = format!(
        "session_status={} harness={} runner_online={}",
        shown(&session["status"]),
        shown(&session["harness"]),
        shown(&session["runner_online"])
    );
    if state == "failed" {
        let error: String = shown(&session["last_task_error"])
            .chars()
            .take(300)
            .collect();
        return ("error", format!("{evidence} last_task_error={error}"));
    }
    // A session waiting for someone to answer a prompt reads no messages until it is answered.
    if let Some(prompt) = session["pending_elicitations"]
        .as_array()
        .and_then(|pending| pending.first())
    {
        let message = shown(&prompt["params"]["message"]).replace("**", "");
        let preview: String = shown(&prompt["params"]["content_preview"])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect();
        return (
            "stuck",
            format!("{evidence} waiting_on={message}: {preview}"),
        );
    }
    // Omnigent takes an idle session's runner offline and brings it back on the next message, so that session is `ready`.
    let parked = state == "idle" && session["host_online"] == true;
    if session["runner_online"] != true && !parked {
        return ("missing", evidence);
    }
    let class = match state {
        "running" | "waiting" => "running",
        "idle" => "ready",
        _ => "missing",
    };
    (class, evidence)
}

/// The state of the agent at `address` and one line of evidence.
pub fn status(settings: &Settings, address: &str) -> (&'static str, String) {
    let found = session(address).and_then(|id| {
        api(
            settings,
            "GET",
            &format!("/v1/sessions/{id}?include_items=false&refresh_state=true"),
            None,
        )
    });
    match found {
        Ok(session) => classify(&session),
        Err(error) => ("missing", error),
    }
}

/// Answer yes to every permission prompt an agent waits on; returns what was approved. Other kinds of prompt are left alone.
// 🧑 "make Omnigent run every harness without sandbox and with no permission asking"
// 🧑 "Sounds good, but should not be the default, which is no question any time at all"
pub fn approve_prompts(settings: &Settings, address: &str) -> Result<Vec<String>, String> {
    let id = session(address)?;
    let found = api(
        settings,
        "GET",
        &format!("/v1/sessions/{id}?include_items=false"),
        None,
    )?;
    let mut approved = Vec::new();
    for prompt in found["pending_elicitations"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let params = &prompt["params"];
        let is_permission = params["phase"] == "pre_tool_use"
            && params["policy_name"]
                .as_str()
                .is_some_and(|name| name.ends_with("_permission"));
        if let (true, Some(elicitation)) = (is_permission, prompt["elicitation_id"].as_str()) {
            post_event(
                settings,
                address,
                "approval",
                json!({"elicitation_id": elicitation, "action": "accept"}),
            )?;
            let preview: String = shown(&params["content_preview"])
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(200)
                .collect();
            approved.push(format!(
                "{}: {preview}",
                shown(&params["message"]).replace("**", "")
            ));
        }
    }
    Ok(approved)
}

/// Stop an agent by ending its session.
pub fn stop(settings: &Settings, address: &str) -> Result<(), String> {
    post_event(settings, address, "stop_session", json!({})).map(|_| ())
}

/// The command-line arguments that make a tool run with no sandbox and no permission prompts.
// 🧑 "make Omnigent run every harness without sandbox and with no permission asking"
fn unrestricted(tool: &str) -> Vec<String> {
    let args: &[&str] = match tool {
        "cursor" => &[
            "--force",
            "--sandbox",
            "disabled",
            "--trust",
            "--approve-mcps",
        ],
        "codex" => &[
            "--dangerously-bypass-approvals-and-sandbox",
            "--config",
            "shell_environment_policy.inherit=all",
        ],
        _ => &["--dangerously-skip-permissions"],
    };
    let mut args: Vec<String> = args.iter().map(|&arg| arg.to_owned()).collect();
    if tool == "codex" {
        // 🧑 "After the codex update, path no longer works, e.g. agents don't have getagentsmd on path"
        let (home, path) = (
            std::env::var("HOME").unwrap_or_default(),
            std::env::var("PATH").unwrap_or_default(),
        );
        args.extend([
            "--config".to_owned(),
            format!("shell_environment_policy.set.PATH=\"{home}/.config/bin:{path}\""),
        ]);
    }
    args
}

/// Start an agent with no prompt and return its address.
pub fn launch(
    settings: &Settings,
    tool: &str,
    model: &str,
    effort: &str,
    workdir: &str,
    title: &str,
) -> Result<String, String> {
    let wanted = format!("{tool}-native-ui");
    let agents = api(settings, "GET", "/v1/agents?limit=1000", None)?;
    let hosts = api(settings, "GET", "/v1/hosts", None)?;
    let found: Vec<&Value> = agents["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|agent| agent["name"] == wanted.as_str())
        .collect();
    let online: Vec<&Value> = hosts["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|host| host["status"] == "online")
        .collect();
    let (&[agent], &[host]) = (&found[..], &online[..]) else {
        return Err(format!(
            "Omnigent has {} `{wanted}` agents and {} online hosts; need exactly one of each",
            found.len(),
            online.len()
        ));
    };
    let request = json!({
        "agent_id": agent["id"],
        "host_id": host["host_id"],
        "workspace": workdir,
        "title": title,
        "model_override": if tool == "cursor" { format!("{model}-{effort}") } else { model.to_owned() },
        "reasoning_effort": effort,
        "terminal_launch_args": unrestricted(tool),
        "labels": if tool == "codex" { json!({"omnigent.codex_native.bypass_sandbox": "1"}) } else { json!({}) },
    });
    let created = api(settings, "POST", "/v1/sessions", Some(&request))?;
    let id = created["id"]
        .as_str()
        .ok_or_else(|| format!("Omnigent created no session: {created}"))?;
    let address = format!("{PREFIX}{id}");
    let deadline = Instant::now() + WAIT;
    while api(
        settings,
        "GET",
        &format!("/v1/sessions/{id}?include_items=false"),
        None,
    )?["runner_online"]
        != true
    {
        if Instant::now() > deadline {
            return Err(format!(
                "{address} was created but its runner never came online; stop it before retrying"
            ));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(address)
}

/// Whether a session's recent history holds the last line of `text`.
pub fn saw_message(settings: &Settings, address: &str, text: &str) -> Result<bool, String> {
    let last: Vec<char> = text
        .trim()
        .lines()
        .last()
        .unwrap_or_default()
        .chars()
        .collect();
    let tail: String = last[last.len().saturating_sub(80)..].iter().collect();
    let quoted = Value::from(tail).to_string();
    let needle = &quoted[1..quoted.len() - 1];
    let items = api(
        settings,
        "GET",
        &format!(
            "/v1/sessions/{}/items?limit=100&order=desc",
            session(address)?
        ),
        None,
    )?;
    Ok(items["data"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item.to_string().contains(needle)))
}

/// Where the calling agent runs: its Omnigent session.
pub fn own_address() -> Result<String, String> {
    match std::env::var("OMNIGENT_RUNNER_PRIMARY_SESSION_ID") {
        Ok(id) if !id.is_empty() => Ok(format!("{PREFIX}{id}")),
        _ => Err("amh: cannot tell which agent is calling; pass --task-file NAME.md".to_owned()),
    }
}

/// The model and effort a tool starts with: `launch.TOOL` in `config.toml`, else the built-in default.
// 🧑 "Default should be opus 5.5 low" ... "gpt-6-luna high for managers and gpt-6.1-sol low for workers" ... "Cursor should use the latest Grok with high by default and not enable fast, Antigravity should use the latest Gemini"
pub fn launch_default(
    settings: &Settings,
    tool: &str,
    as_manager: bool,
) -> Result<(String, String), String> {
    let (model, effort) = match (tool, as_manager) {
        ("claude", _) => ("claude-opus-5-5", "low"),
        ("codex", false) => ("gpt-6.1-sol", "low"),
        ("codex", true) => ("gpt-6-luna", "high"),
        ("cursor", _) => ("grok-4.7", "high"),
        ("antigravity", _) => ("gemini-3.8-flash-high", "high"),
        _ => return Err(format!("amh does not know the tool {tool}")),
    };
    let set = |key: &str, fallback: &str| {
        settings
            .get(&format!("launch.{tool}.{key}"))
            .filter(|value| !value.is_empty())
            .map_or(fallback.to_owned(), String::clone)
    };
    let (model, effort) = (set("model", model), set("effort", effort));
    Ok(if as_manager {
        (set("manager_model", &model), set("manager_effort", &effort))
    } else {
        (model, effort)
    })
}
