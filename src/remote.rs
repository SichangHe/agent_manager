//! Agents on several machines: each machine is known by its Zulip bot, and a message for an agent that is not on this machine goes through the agents' Zulip channel.
//!
//! Settings: `zulip.agent_stream` (the channel) and `zulip.main_bot` (the main machine's bot email) live in the shared `config.toml`; each machine's own bot login (`zulip.env_file`) lives in its `env.toml`.
// 🧑 "There should be a built-in command for AMH to send stuff to another computer. Maybe we need to change the task metadata to include which machine it runs on ... based on which bot it has inside the configuration or state of AMH. After that, the agents would send messages just like they are on a same machine. But, AMH would figure out whether they go through Zulip or not."

use std::collections::BTreeMap;

type Settings = BTreeMap<String, String>;

/// The agents' channel, or `None` when cross-machine messaging is off.
pub fn stream(settings: &Settings) -> Option<&str> {
    Some(crate::settings::get(settings, "zulip.agent_stream")).filter(|name| !name.is_empty())
}

/// This machine's bot email: its identity among machines.
pub fn me(settings: &Settings) -> Result<String, String> {
    crate::zulip::bot_email(settings)
}

/// This machine's name for task files: its bot email without `-bot@SITE`; `None` when cross-machine messaging is off.
pub fn machine(settings: &Settings) -> Option<String> {
    stream(settings)?;
    let email = me(settings).ok()?;
    let user = email.split('@').next().unwrap_or(&email);
    Some(user.strip_suffix("-bot").unwrap_or(user).to_owned())
}

/// Whether this machine is the main one: it takes the human's messages and email, and posts no task here claims.
/// With no `zulip.main_bot`, every machine is (one machine alone).
pub fn is_main(settings: &Settings) -> bool {
    match crate::settings::get(settings, "zulip.main_bot") {
        "" => true,
        main => me(settings).is_ok_and(|email| email == main),
    }
}

/// The topic for task file `who` when this machine has no such task and the agents' channel is on: the task's tag.
pub fn topic_for(settings: &Settings, store: &crate::store::Store, who: &str) -> Result<Option<String>, String> {
    if stream(settings).is_none() || !who.ends_with(".md") || store.contains(who)? {
        return Ok(None);
    }
    Ok(Some(who.trim_end_matches(".md").to_owned()))
}

/// Whether a failed lookup of `address` in this machine's Omnigent means the agent lives elsewhere:
/// Omnigent does not know the session, or, on a machine other than the main one, there is no Omnigent here.
pub fn elsewhere(settings: &Settings, lookup_error: &str) -> bool {
    stream(settings).is_some()
        && (lookup_error.contains("answered 404")
            || (!is_main(settings) && lookup_error.starts_with("Omnigent is unreachable")))
}

/// Post `text` in the agents' channel under `topic` (a task tag or an agent address); returns the message ID.
pub fn post(settings: &Settings, topic: &str, text: &str) -> Result<u64, String> {
    let stream = stream(settings).ok_or("zulip.agent_stream is not set")?;
    crate::zulip::post(settings, stream, topic, text)
}

/// Whether the human's thread `tag` belongs to a task on another machine: this machine has no such task,
/// and agents here already sent to it through the agents' channel (a post under topic `tag`).
// 🧑 (via main_manager.md) the human wrote in the mac_main_manager thread, "but amh found no task for that thread and sent it to me (fix that routing)"
pub fn thread_elsewhere(settings: &Settings, store: &crate::store::Store, tag: &str) -> Result<bool, String> {
    let Some(stream) = stream(settings) else {
        return Ok(false);
    };
    if store.contains(&format!("{tag}.md"))? {
        return Ok(false);
    }
    let narrow = serde_json::json!([{"operator":"stream","operand":stream},{"operator":"topic","operand":tag}]);
    let found = crate::zulip::api(
        settings,
        "GET",
        "messages",
        &[
            ("anchor", "newest".to_owned()),
            ("num_before", "1".to_owned()),
            ("num_after", "0".to_owned()),
            ("narrow", narrow.to_string()),
        ],
    )?;
    Ok(found["messages"].as_array().is_some_and(|messages| !messages.is_empty()))
}
