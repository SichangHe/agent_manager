//! Talking to the Omnigent server, which runs every agent session: send, status, stop, launch.
// 🧑 "the Rust port would only support Omnigent, not tmux"

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const PREFIX: &str = "omnigent://";
/// Sent after a multi-line message: Claude Code shows such a message as pasted text and acts on it only when a typed line says to.
const FOLLOW_UP: &str = "The message above reached you through `amh`, the helper that carries the human's and your manager's requests to you; act on it.";
const WAIT: Duration = Duration::from_secs(90);
/// Agent starts, including wakes of a session whose runner is offline, allowed per minute on the whole machine.
// The main manager: "make amh enforce the safe rate itself: queue amh task start and session resumes (including the watcher's own retries) so the whole machine starts at most 2 agents per minute, whoever asks"
const STARTS_PER_MIN: usize = 2;

/// Wait until starting one more agent keeps the machine within `STARTS_PER_MIN`, then record the start.
///
/// The start times of the last minute live in `amh-starts.json` in the state folder, under a lock shared by every amh process.
fn start_slot() -> Result<(), String> {
    let path = crate::settings::state_dir().join("amh-starts.json");
    let given_up = Instant::now() + Duration::from_secs(1800);
    loop {
        let lock = std::fs::File::create(path.with_extension("lock"))
            .map_err(|e| format!("cannot open the start lock: {e}"))?;
        lock.lock()
            .map_err(|e| format!("cannot lock the start record: {e}"))?;
        let now_s = chrono::Utc::now().timestamp();
        let mut starts: Vec<i64> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        starts.retain(|at| now_s - at < 60);
        if starts.len() < STARTS_PER_MIN {
            starts.push(now_s);
            crate::taskfile::write(&path, &json!(starts).to_string())
                .map_err(|e| format!("cannot write the start record: {e}"))?;
            return Ok(());
        }
        drop(lock);
        if Instant::now() >= given_up {
            return Err(
                "waited 30 min for a start slot; the machine allows 2 agent starts per minute"
                    .to_owned(),
            );
        }
        let wait_s = (starts.iter().min().copied().unwrap_or(now_s) + 60 - now_s).max(1);
        std::thread::sleep(Duration::from_secs(wait_s as u64));
    }
}

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
    let current = api(
        settings,
        "GET",
        &format!(
            "/v1/sessions/{}?include_items=false&refresh_state=true",
            session(address)?
        ),
        None,
    )?;
    if quota_paused(address, &current, crate::quota::Attempt::Reserve)? {
        return Err(format!(
            "{address} is paused for its provider session quota; keep requests pending"
        ));
    }
    // A message to a session whose runner is offline starts that runner again.
    if current["runner_online"] != true {
        start_slot()?;
    }
    let answer = match post_message(settings, address, text) {
        Ok(answer) => answer,
        Err(error) => {
            if error.starts_with("Omnigent answered ") {
                quota_paused(address, &current, crate::quota::Attempt::Release)?;
            }
            return Err(error);
        }
    };
    if answer["queued"] != true {
        if answer["queued"] == false {
            quota_paused(address, &current, crate::quota::Attempt::Release)?;
        }
        return Err(format!("{address} did not queue the message: {answer}"));
    }
    if text.trim().contains('\n')
        && current["harness"]
            .as_str()
            .is_some_and(|harness| harness.contains("claude"))
    {
        let _ = post_message(settings, address, FOLLOW_UP);
    }
    Ok(())
}

fn session_error(session_value: &Value) -> &str {
    session_value["last_task_error"]
        .as_str()
        .or_else(|| session_value["last_task_error"]["message"].as_str())
        .unwrap_or_default()
}

fn quota_paused(
    address: &str,
    session_value: &Value,
    attempt: crate::quota::Attempt,
) -> Result<bool, String> {
    if session_value["status"].as_str().is_none() {
        return Err(
            "Omnigent returned an invalid session status; quota state is unchanged".to_owned(),
        );
    }
    let error = if session_value["status"] == "failed" {
        session_error(session_value)
    } else {
        ""
    };
    crate::quota::paused(
        &crate::settings::state_dir().join("amh-quota-pauses.json"),
        session(address)?,
        error,
        chrono::Utc::now(),
        session_value["updated_at"]
            .as_str()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&chrono::Utc))
            .or_else(|| {
                session_value["updated_at"]
                    .as_i64()
                    .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
            }),
        attempt,
    )
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
    // lost_work_mgr.md: "the problem check reported eng_tasks_mgr, pb_lost_three and stock_news_watch as 'gone' (runner_online=False), but all three were idle sessions; one message woke each". At 18:23 Oct 14 host_online was not true for them either.
    let parked = state == "idle";
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
        Ok(found) => match quota_paused(address, &found, crate::quota::Attempt::Observe) {
            Ok(true) => (
                "paused",
                format!(
                    "provider session quota: {}",
                    shown(&found["last_task_error"])
                ),
            ),
            Ok(false)
                if crate::quota::limited(session_error(&found)) && found["status"] == "failed" =>
            {
                (
                    "ready",
                    "provider session quota reset; one retry available".to_owned(),
                )
            }
            Ok(false) => classify(&found),
            Err(error) => ("paused", error),
        },
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
        // A question is for the human; accepting it without an answer leaves the agent waiting in its terminal.
        if !params["ask_user_question"].is_null() {
            continue;
        }
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

/// One question an agent waits on: its prompt ID, and each part's text with its choices as `(label, description)`.
pub type Question = (String, Vec<(String, Vec<(String, String)>)>);

/// The questions with choices that an agent asked through `AskUserQuestion` and still waits on.
// 🧑 "Choices like that should have been automatically sent to me."
pub fn pending_questions(settings: &Settings, address: &str) -> Result<Vec<Question>, String> {
    let id = session(address)?;
    let found = api(
        settings,
        "GET",
        &format!("/v1/sessions/{id}?include_items=false"),
        None,
    )?;
    let mut questions = Vec::new();
    for prompt in found["pending_elicitations"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let (Some(elicitation), Some(parts)) = (
            prompt["elicitation_id"].as_str(),
            prompt["params"]["ask_user_question"]["questions"].as_array(),
        ) else {
            continue;
        };
        let parts = parts
            .iter()
            .map(|part| {
                let choices = part["options"].as_array().into_iter().flatten();
                let choices =
                    choices.map(|choice| (shown(&choice["label"]), shown(&choice["description"])));
                (shown(&part["question"]), choices.collect())
            })
            .collect();
        questions.push((elicitation.to_owned(), parts));
    }
    Ok(questions)
}

/// Give the human's answers, `(question, answer)` pairs, to the question an agent waits on.
pub fn answer_question(
    settings: &Settings,
    address: &str,
    elicitation: &str,
    answers: &[(String, String)],
) -> Result<(), String> {
    let content: serde_json::Map<String, Value> = answers
        .iter()
        .map(|(question, answer)| (question.clone(), json!(answer)))
        .collect();
    post_event(
        settings,
        address,
        "approval",
        json!({"elicitation_id": elicitation, "action": "accept", "content": content}),
    )
    .map(|_| ())
}

/// Stop an agent by ending its session.
///
/// Omnigent refuses to stop a session whose terminal has already vanished; such a session runs nothing, so that counts as stopped.
/// Returns what to tell the caller: nothing, or that Omnigent refused because the agent's terminal was already gone.
pub fn stop(settings: &Settings, address: &str) -> Result<&'static str, String> {
    let Err(refusal) = post_event(settings, address, "stop_session", json!({})) else {
        return Ok("");
    };
    let resources = api(
        settings,
        "GET",
        &format!("/v1/sessions/{}/resources", session(address)?),
        None,
    )
    .map_err(|_| refusal.clone())?;
    if has_running_terminal(&resources) {
        Err(refusal)
    } else {
        Ok(
            "already stopped: its terminal is gone, so no agent process runs; Omnigent may still list the session as idle with its runner online\n",
        )
    }
}

/// Whether a session's resource list holds a terminal that still runs. Pure.
pub fn has_running_terminal(resources: &Value) -> bool {
    let listed = resources["data"].as_array().into_iter().flatten();
    listed
        .into_iter()
        .any(|resource| resource["type"] == "terminal" && resource["metadata"]["running"] == true)
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
    start_slot()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_quota_errors_match_live_session_format() {
        let found = json!({
            "status": "failed",
            "last_task_error": {"code": "native_turn_error", "message": "Session limit reached"}
        });
        assert!(crate::quota::limited(session_error(&found)));
    }

    #[test]
    fn a_session_without_a_running_terminal_runs_nothing() {
        let terminal =
            |running: bool| json!({"type": "terminal", "metadata": {"running": running}});
        let environment = json!({"type": "environment", "metadata": {}});
        assert!(has_running_terminal(
            &json!({"data": [environment, terminal(true)]})
        ));
        assert!(!has_running_terminal(
            &json!({"data": [environment, terminal(false)]})
        ));
        assert!(!has_running_terminal(&json!({"data": []})));
        assert!(!has_running_terminal(&Value::Null));
    }
}
