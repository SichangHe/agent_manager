//! `amh quota serve` and `amh quota status`: the observer's input and output around `quota_core`. See `docs/quota-manager.md`.
// 🧑 "The observer should get quota from the proxy, the proxy should cache quota requests"

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::quota_core::{self as core, Act, Bucket, Reading, Snapshot, Status};
use crate::settings;
use crate::store::Store;
use crate::{agents, omnigent};

type Settings = BTreeMap<String, String>;

/// The observer reads the proxy this often, and sooner after a reset hint or a session's quota error.
const INTERVAL_S: u64 = 300;
/// Reset credits are listed this often to watch their expiry.
const CREDIT_POLL_S: i64 = 1800;
const POLL_NOW: &str = "quota-poll-now";
const PROXY_TIMEOUT_S: u64 = 30;
/// A failing proxy or actuator is told to the human after this many tries in a row.
const NOTICE_AFTER_FAILURES: u32 = 2;
const EVENTS_MAX_BYTES: u64 = 8 << 20;
const EVENTS: &str = "quota-events.jsonl";
const CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";

const NOTICES_FILE: &str = "quota-notices.json";
const APPEARED_RECORD: &str = "amh-quota-appeared.txt";

/// Tell once per `key`: the notices file remembers what was sent, and a failed send is tried again next time.
fn notice_to(settings: &Settings, store: &Store, key: &str, to: &str, text: &str) {
    let path = store.state_dir.join(NOTICES_FILE);
    let mut sent: serde_json::Map<String, Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if sent.contains_key(key) {
        return;
    }
    let delivered = agents::address_of(store, to)
        .and_then(|a| omnigent::send(settings, &a, &format!("amh quota: {text}")));
    match delivered {
        Ok(()) => {
            sent.insert(key.to_owned(), json!(chrono::Utc::now().timestamp()));
            if let Err(e) = crate::taskfile::write(&path, &Value::Object(sent).to_string()) {
                println!("quota: cannot record notice {key}: {e}");
            }
        }
        Err(e) => println!("quota: notice {key} not delivered to {to}: {e}"),
    }
}

/// Tell the task that owns quota questions.
fn notice(settings: &Settings, store: &Store, key: &str, text: &str) {
    let to = Some(settings::get(settings, "quota.notify_task"))
        .filter(|t| !t.is_empty())
        .unwrap_or("quota_support.md");
    notice_to(settings, store, key, to, text);
}

/// The watcher's quota pass: tell about a dead observer, and clear Claude's session limit evidence when its quota has appeared. Returns log lines.
pub fn watcher_pass(settings: &Settings, store: &Store) -> Vec<String> {
    let now = chrono::Utc::now().timestamp();
    let snapshot = load_snapshot(&store.state_dir);
    let mut lines = Vec::new();
    if snapshot
        .as_ref()
        .is_none_or(|s| now - s.generated_at > core::TTL_S)
    {
        let seen = snapshot.as_ref().map_or(0, |s| s.generated_at);
        notice(
            settings,
            store,
            &format!("observer_dead:{seen}"),
            "the quota snapshot is over 15 minutes old: the observer (`amh quota serve`) is not running.",
        );
    }
    let snapshot = snapshot.unwrap_or_default();
    let last = store.record(APPEARED_RECORD).unwrap_or_default();
    let recorded = |pool: &str| {
        last.lines()
            .find_map(|l| l.strip_prefix(&format!("{pool}="))?.parse::<i64>().ok())
            .unwrap_or_default()
    };
    let mut record = String::new();
    for (pool, at) in &snapshot.appeared_at {
        record.push_str(&format!("{pool}={at}\n"));
        if *at > recorded(pool) && pool == "claude" {
            match crate::quota::clear(&store.state_dir.join("amh-quota-pauses.json")) {
                Ok(()) => lines
                    .push("quota appeared for claude: cleared session limit evidence".to_owned()),
                Err(e) => lines.push(format!("cannot clear session limit evidence: {e}")),
            }
        }
    }
    if record != last
        && let Err(e) = store.set_record(APPEARED_RECORD, &record)
    {
        lines.push(format!("cannot record quota_appeared: {e}"));
    }
    lines
}

fn log(text: &str) {
    println!(
        "{} quota: {text}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

pub fn snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join("quota-snapshot.json")
}

pub fn load_snapshot(state_dir: &Path) -> Option<Snapshot> {
    let text = fs::read_to_string(snapshot_path(state_dir)).ok()?;
    serde_json::from_str(&text)
        .ok()
        .map(|v| Snapshot::from_json(&v))
}

fn list(settings: &Settings, key: &str) -> BTreeSet<String> {
    settings::get(settings, key)
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The proxy's management API, the observer's only source and only actuator.
pub struct Proxy {
    url: String,
    key: String,
    agent: ureq::Agent,
}

/// A provider call through the proxy: `Rejected` surely changed nothing; `Uncertain` may have.
#[derive(Debug)]
pub enum CallError {
    Rejected(String),
    Uncertain(String),
}

impl Proxy {
    pub fn new(settings: &Settings) -> Result<Self, String> {
        let file = settings::get(settings, "quota.proxy_key_file");
        let key = fs::read_to_string(file)
            .map_err(|e| format!("cannot read the proxy key file `quota.proxy_key_file`: {e}"))?;
        let url = Some(settings::get(settings, "quota.proxy_url"))
            .filter(|u| !u.is_empty())
            .unwrap_or("http://127.0.0.1:18318");
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(PROXY_TIMEOUT_S)))
            .build()
            .into();
        Ok(Self {
            url: format!("{url}/v0/management"),
            key: format!("Bearer {}", key.trim()),
            agent,
        })
    }

    fn json(
        &self,
        answer: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Value, CallError> {
        let mut answer = answer.map_err(|e| CallError::Uncertain(e.to_string()))?;
        let code = answer.status().as_u16();
        let text = answer
            .body_mut()
            .read_to_string()
            .map_err(|e| CallError::Uncertain(e.to_string()))?;
        match code {
            200..=299 => serde_json::from_str(&text)
                .map_err(|e| CallError::Uncertain(format!("unreadable answer: {e}"))),
            400..=499 => Err(CallError::Rejected(format!("HTTP {code}"))),
            _ => Err(CallError::Uncertain(format!("HTTP {code}"))),
        }
    }

    pub fn auth_files(&self) -> Result<Value, String> {
        let answer = self
            .agent
            .get(&format!("{}/auth-files", self.url))
            .header("Authorization", &self.key)
            .call();
        self.json(answer)
            .map_err(|e| format!("proxy auth-files: {e:?}"))
    }

    pub fn apply(&self, act: &Act) -> Result<(), String> {
        let (path, body) = match act {
            Act::Enable(name) => ("status", json!({"name": name, "disabled": false})),
            Act::Disable(name) => ("status", json!({"name": name, "disabled": true})),
            Act::Priority(name, rank) => ("fields", json!({"name": name, "priority": rank})),
        };
        let sent = self
            .agent
            .patch(&format!("{}/auth-files/{path}", self.url))
            .header("Authorization", &self.key)
            .send_json(body);
        self.json(sent)
            .map(|_| ())
            .map_err(|e| format!("{act:?}: {e:?}"))
    }

    /// A call to the provider that the proxy makes with the credential's own token, which the observer never sees.
    fn api_call(
        &self,
        r: &Reading,
        method: &str,
        url: &str,
        body: Option<Value>,
    ) -> Result<Value, CallError> {
        let mut payload = json!({"auth_index": r.auth_index, "method": method, "url": url,
            "header": {"Authorization": "Bearer $TOKEN$", "Chatgpt-Account-Id": r.account_id, "User-Agent": "codex_cli_rs", "Content-Type": "application/json"}});
        if let Some(body) = body {
            payload["data"] = json!(body.to_string());
        }
        let sent = self
            .agent
            .post(&format!("{}/api-call", self.url))
            .header("Authorization", &self.key)
            .send_json(payload);
        let relayed = self.json(sent)?;
        let text = relayed["body"].as_str().unwrap_or_default();
        match relayed["status_code"].as_i64() {
            Some(200..=299) => serde_json::from_str(text)
                .map_err(|e| CallError::Uncertain(format!("unreadable provider answer: {e}"))),
            Some(400..=499) => Err(CallError::Rejected(format!(
                "provider HTTP {}",
                relayed["status_code"]
            ))),
            _ => Err(CallError::Uncertain(format!(
                "provider HTTP {}",
                relayed["status_code"]
            ))),
        }
    }

    /// The available reset credits `(id, expires_at)`, earliest-expiring first.
    fn credits(&self, r: &Reading) -> Result<Vec<(String, String)>, CallError> {
        let answer = self.api_call(r, "GET", CREDITS_URL, None)?;
        let mut found: Vec<(String, String)> = answer["credits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["status"] == "available" && c["is_supported_by_plan"] != false)
            .filter_map(|c| {
                Some((
                    c["id"].as_str()?.to_owned(),
                    c["expires_at"].as_str().unwrap_or_default().to_owned(),
                ))
            })
            .collect();
        found.sort_by_key(|(_, expires)| (expires.is_empty(), expires.clone()));
        Ok(found)
    }
}

fn uuid4() -> Result<String, String> {
    let mut b = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| format!("no randomness: {e}"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// Everything the observer writes under the state folder.
pub struct Files {
    pub dir: PathBuf,
}

impl Files {
    /// Every journal entry; an unreadable journal is an error, never an empty one, because empty would allow a second redemption.
    fn journal(&self) -> Result<Vec<Value>, String> {
        let text = match fs::read_to_string(self.dir.join("quota-redeem.journal")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("cannot read the redemption journal: {e}")),
        };
        text.lines()
            .map(|l| {
                serde_json::from_str(l).map_err(|e| format!("corrupt redemption journal: {e}"))
            })
            .collect()
    }

    fn append(&self, name: &str, entry: Value) {
        let path = self.dir.join(name);
        if name == EVENTS && fs::metadata(&path).is_ok_and(|m| m.len() > EVENTS_MAX_BYTES) {
            let _ = fs::rename(&path, path.with_extension("jsonl.1"));
        }
        let line = format!("{entry}\n");
        let written = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(line.as_bytes()).and_then(|()| f.sync_all()));
        if let Err(e) = written {
            log(&format!("cannot append to {name}: {e}"));
        }
    }

    fn event(&self, now: i64, kind: &str, subject: &str, detail: Value) {
        self.append(
            EVENTS,
            json!({"t": now, "event": kind, "subject": subject, "detail": detail}),
        );
    }

    /// The journal is the redemption's write-ahead log: each step is on disk before the next one starts.
    fn step(&self, now: i64, episode: &str, step: &str, detail: Value) {
        self.append(
            "quota-redeem.journal",
            json!({"t": now, "episode": episode, "step": step, "detail": detail}),
        );
    }
}

/// What the observer remembers between cycles of one run.
#[derive(Default)]
pub struct Memory {
    proxy_failures: u32,
    failures: BTreeMap<String, u32>,
    intended: BTreeSet<String>,
    shadow_redeem: BTreeSet<String>,
    recovered: bool,
    /// Per principal: when the reset credits were last listed, and `(id, expiry)` of each.
    credits: BTreeMap<String, (i64, Vec<(String, i64)>)>,
}

/// `(cred name, usage before)` of an episode's last intent.
fn files_intent_cred(journal: &[Value], episode: &str) -> Option<(String, Option<core::Usage>)> {
    let e = journal
        .iter()
        .rev()
        .find(|e| e["episode"] == episode && e["step"] == "intent")?;
    Some((
        e["detail"]["cred"].as_str()?.to_owned(),
        e["detail"]["used"]
            .as_f64()
            .map(|u| (u, e["detail"]["window_end"].as_i64())),
    ))
}

/// The credit and the usage before the call, from the episode's last intent.
fn intent_of(files: &Files, episode: &str) -> Option<(String, Option<core::Usage>)> {
    let journal = files.journal().ok()?;
    let e = journal
        .iter()
        .rev()
        .find(|e| e["episode"] == episode && e["step"] == "intent")?;
    Some((
        e["detail"]["credit_id"].as_str()?.to_owned(),
        e["detail"]["used"]
            .as_f64()
            .map(|u| (u, e["detail"]["window_end"].as_i64())),
    ))
}

/// Redeemed means the credit left the provider's list and the quota fell: a vanished credit alone may have expired.
/// Records `confirmed` only with both; otherwise the episode stays `uncertain`.
fn reconcile(
    proxy: &Proxy,
    files: &Files,
    now: i64,
    episode: &str,
    reading: &Reading,
    used_now: Option<core::Usage>,
    record: bool,
) -> bool {
    let Some((credit, before)) = intent_of(files, episode) else {
        return false;
    };
    let gone = proxy
        .credits(reading)
        .is_ok_and(|left| !left.iter().any(|(id, _)| *id == credit));
    let reset = matches!((before, used_now), (Some(b), Some(n)) if core::quota_reset(b, n));
    let confirmed = gone && reset;
    if confirmed {
        files.step(now, episode, "confirmed", json!({}));
    } else if record {
        files.step(
            now,
            episode,
            "uncertain",
            json!({"credit_gone": gone, "quota_reset": reset}),
        );
    }
    confirmed
}

/// The reading's highest usage as the proxy reports it now.
fn fresh_used(proxy: &Proxy, reading: &Reading, now: i64) -> Option<core::Usage> {
    let answer = proxy.auth_files().ok()?;
    core::parse_auth_files(&answer, now)
        .iter()
        .find(|r| r.name == reading.name)
        .map(|r| core::usage(&r.windows))
}

/// Redeem one reset credit of a Codex bucket, journaled step by step. `want` names the credit (a deadline or a retry); without it the earliest-expiring one is used.
/// A rejected or uncertain call is retried by later cycles, with backoff, while the credit is still listed and unexpired.
/// Returns whether a call was attempted.
#[allow(clippy::too_many_arguments)]
fn redeem(
    settings: &Settings,
    store: &Store,
    proxy: &Proxy,
    files: &Files,
    now: i64,
    bucket: &Bucket,
    reading: &Reading,
    episode: &str,
    want: Option<&str>,
) -> bool {
    let credits = match proxy.credits(reading) {
        Ok(c) => c,
        Err(e) => {
            // Nothing was sent that could change anything, but the episode stays unjournaled so the next cycle looks again.
            log(&format!(
                "cannot list reset credits of {}: {e:?}",
                reading.name
            ));
            return false;
        }
    };
    let chosen = match want {
        Some(id) => credits.iter().find(|(c, _)| c == id).cloned(),
        None => credits.first().cloned(),
    };
    let Some((credit_id, expires)) = chosen else {
        if want.is_none() {
            files.step(now, episode, "no_credit", json!({}));
        }
        return false;
    };
    let request_id = match uuid4() {
        Ok(id) => id,
        Err(e) => {
            log(&e);
            return false;
        }
    };
    files.step(now, episode, "intent", json!({"cred": reading.name, "credit_id": credit_id, "expires": expires, "before": credits.len(), "request_id": request_id, "used": core::max_used(&bucket.windows), "window_end": core::usage(&bucket.windows).1}));
    // 🧑 "Try to use as much quota of team1@trymidas.ai as possible before the reset expiry in 3 days and use that reset before it expires"
    let left = core::parse_expiry(&expires).map(|at| at - now);
    let near = left.is_some_and(|l| l < 6 * 3600);
    let retry_note = "It will be retried with backoff until the credit expires.";
    let failed = |why: &str, what: &str| {
        let hours = left.map_or("an unknown time".to_owned(), |l| {
            format!("{:.1}h", l as f64 / 3600.0)
        });
        notice(
            settings,
            store,
            &format!("{what}:{episode}"),
            &format!(
                "Codex reset credit redemption for {} {what} ({why}); credit {credit_id} expires in {hours}. {retry_note}",
                reading.name
            ),
        );
        if near {
            notice(
                settings,
                store,
                &format!("deadline_failing:{credit_id}"),
                &format!(
                    "URGENT: reset credit {credit_id} of {} expires in {hours} and redemption keeps failing ({why}). Redeem it by hand if this goes on.",
                    reading.name
                ),
            );
        }
    };
    match proxy.api_call(
        reading,
        "POST",
        &format!("{CREDITS_URL}/consume"),
        Some(json!({"credit_id": credit_id, "redeem_request_id": request_id})),
    ) {
        Err(CallError::Rejected(why)) => {
            files.step(now, episode, "rejected", json!({"why": why}));
            failed(&why, "was rejected");
        }
        Err(CallError::Uncertain(why)) => {
            files.step(now, episode, "uncertain", json!({"why": why}));
            let used = fresh_used(proxy, reading, now);
            if !reconcile(proxy, files, now, episode, reading, used, false) {
                failed(&why, "is uncertain");
            }
        }
        Ok(_) => {
            files.step(now, episode, "called", json!({}));
            let used = fresh_used(proxy, reading, now);
            if !reconcile(proxy, files, now, episode, reading, used, true) {
                failed("sent, but the quota did not visibly reset", "is uncertain");
            }
        }
    }
    true
}

/// One observation: read the proxy, fold the readings, act on the proxy (live) or only log (shadow), publish the snapshot.
pub fn cycle(
    settings: &Settings,
    store: &Store,
    proxy: &Proxy,
    files: &Files,
    memory: &mut Memory,
    live: bool,
    now: i64,
) -> Result<Snapshot, String> {
    let answer = match proxy.auth_files() {
        Ok(a) => {
            memory.proxy_failures = 0;
            a
        }
        Err(e) => {
            memory.proxy_failures += 1;
            if memory.proxy_failures >= NOTICE_AFTER_FAILURES && live {
                notice(
                    settings,
                    store,
                    &format!("proxy_down:{}", now / 3600),
                    &format!("The quota observer cannot read the proxy: {e}"),
                );
            }
            return Err(e);
        }
    };
    let prev = match fs::read_to_string(snapshot_path(&files.dir)) {
        Ok(text) => Snapshot::from_json(
            &serde_json::from_str(&text).map_err(|e| format!("corrupt snapshot: {e}"))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot::default(),
        Err(e) => return Err(format!("cannot read the snapshot: {e}")),
    };
    let readings = core::parse_auth_files(&answer, now);
    if readings.is_empty() {
        return Err("the proxy listed no Claude or Codex credentials".to_owned());
    }
    let mut events = Vec::new();
    let names: BTreeSet<String> = readings.iter().map(|r| r.name.clone()).collect();
    // 🧑 "If we lack quota I'll just open another account": a new credential is used at once.
    if !prev.seen.is_empty() {
        for name in names.difference(&prev.seen) {
            events.push(core::Event {
                kind: "credential_added",
                subject: name.clone(),
            });
        }
    }
    let denied = list(settings, "quota.deny");
    let mut ours = prev.ours.clone();
    // Only credentials the router may use count towards a pool: not denied or disabled by someone else.
    let eligible: Vec<Reading> = readings
        .iter()
        .filter(|r| !denied.contains(&r.name) && (!r.disabled || ours.contains(&r.name)))
        .cloned()
        .collect();
    let (buckets, stepped) = core::step(&prev.buckets, &eligible, now);
    events.extend(stepped);
    let acts = core::proxy_actions(&buckets, &readings, &denied, &ours);
    let intended: BTreeSet<String> = acts.iter().map(|a| format!("{a:?}")).collect();
    for act in &acts {
        let key = format!("{act:?}");
        if !live {
            if !memory.intended.contains(&key) {
                files.event(now, "shadow_action", &key, json!({}));
                log(&format!("shadow: would {key}"));
            }
            continue;
        }
        match proxy.apply(act) {
            Ok(()) => {
                memory.failures.remove(&key);
                match act {
                    Act::Disable(n) => ours.insert(n.clone()),
                    Act::Enable(n) => ours.remove(n),
                    Act::Priority(..) => false,
                };
                files.event(now, "action", &key, json!({}));
            }
            Err(e) => {
                let n = memory.failures.entry(key.clone()).or_default();
                *n += 1;
                files.event(now, "action_failed", &key, json!({"error": e}));
                if *n == NOTICE_AFTER_FAILURES {
                    notice(
                        settings,
                        store,
                        &format!("actuator:{key}:{}", now / 86400),
                        &format!("The quota observer failed twice to change the proxy: {e}"),
                    );
                }
            }
        }
    }
    memory.intended = intended;
    let pools = core::pools(&buckets, now);
    let mut appeared_at = prev.appeared_at.clone();
    for e in &events {
        if e.kind == "quota_appeared" {
            appeared_at.insert(
                e.subject.split(' ').next().unwrap_or_default().to_owned(),
                now,
            );
        }
    }
    let authorized = live && settings::get(settings, "quota.redeem") == "true";
    if live && !memory.recovered {
        for episode in core::unfinished(&files.journal()?) {
            let credit_cred = files
                .journal()?
                .iter()
                .rev()
                .find(|e| e["episode"] == episode.as_str() && e["step"] == "intent")
                .and_then(|e| e["detail"]["cred"].as_str().map(str::to_owned));
            files.step(
                now,
                &episode,
                "uncertain",
                json!({"why": "the observer stopped between steps"}),
            );
            let reading = readings
                .iter()
                .find(|r| Some(&r.name) == credit_cred.as_ref());
            if !reading.is_some_and(|r| {
                reconcile(
                    proxy,
                    files,
                    now,
                    &episode,
                    r,
                    fresh_used(proxy, r, now),
                    true,
                )
            }) {
                notice(
                    settings,
                    store,
                    &format!("uncertain:{episode}"),
                    &format!(
                        "A Codex reset credit redemption ({episode}) was interrupted and is uncertain; it is retried while the credit is still listed. Check the provider's history."
                    ),
                );
            }
        }
        memory.recovered = true;
    }
    let credit_poll_due = |memory: &Memory, principal: &str| {
        memory
            .credits
            .get(principal)
            .is_none_or(|(at, _)| now - at >= CREDIT_POLL_S)
    };
    for bucket in &buckets {
        let journal = files.journal()?;
        let first = core::should_redeem(&journal, bucket, authorized, now);
        let retry = core::should_retry(&journal, bucket, authorized, now);
        if !first
            && !retry
            && bucket.pool == "codex"
            && bucket.status == Status::Exhausted
            && !live
            && memory.shadow_redeem.insert(bucket.principal.clone())
        {
            files.event(
                now,
                "shadow_action",
                &format!("would_redeem {}", bucket.principal),
                json!({}),
            );
        }
        let pick = |enabled: bool| {
            readings
                .iter()
                .find(|r| r.principal == bucket.principal && r.disabled != enabled)
        };
        let reading = pick(true).or_else(|| pick(false));
        // Settle earlier uncertain redemptions of this principal: confirmed once the credit is gone and the quota fell.
        if live && let Some(reading) = reading {
            for episode in core::uncertain_episodes(&journal) {
                let Some((cred, Some(before))) = files_intent_cred(&journal, &episode) else {
                    continue;
                };
                if cred == reading.name
                    && bucket.status != Status::Unknown
                    && core::quota_reset(before, core::usage(&bucket.windows))
                    && reconcile(
                        proxy,
                        files,
                        now,
                        &episode,
                        reading,
                        Some(core::usage(&bucket.windows)),
                        false,
                    )
                    && proxy.apply(&Act::Enable(reading.name.clone())).is_ok()
                {
                    ours.remove(&reading.name);
                }
            }
        }
        let Some(reading) = reading.filter(|_| live && bucket.pool == "codex") else {
            continue;
        };
        if first || retry {
            let (episode, want) = if retry {
                let ep = core::episode(bucket).unwrap_or_default();
                let id = intent_of(files, &ep).map(|(id, _)| id);
                (ep, id)
            } else {
                (core::episode(bucket).unwrap_or_default(), None)
            };
            if retry && want.is_none() {
                continue;
            }
            redeem(
                settings,
                store,
                proxy,
                files,
                now,
                bucket,
                reading,
                &episode,
                want.as_deref(),
            );
            memory.credits.remove(&bucket.principal);
            // The reset makes the credential usable now, and a disabled one gets no traffic to show it.
            if core::journal_steps(&files.journal()?, &episode).last() == Some(&"confirmed")
                && proxy.apply(&Act::Enable(reading.name.clone())).is_ok()
            {
                ours.remove(&reading.name);
            }
            continue;
        }
        // The deadline fallback: whatever the usage, a credit is redeemed before it expires.
        // 🧑 "Of course you should use the reset regardless of the usage. The 99% usage is a target to achieve not a restricting factor for whether to use the reset."
        if credit_poll_due(memory, &bucket.principal) {
            match proxy.credits(reading) {
                Ok(list) => {
                    let new: Vec<(String, i64)> = list
                        .iter()
                        .filter_map(|(id, at)| Some((id.clone(), core::parse_expiry(at)?)))
                        .collect();
                    let old = memory
                        .credits
                        .get(&bucket.principal)
                        .map(|(_, c)| c.clone())
                        .unwrap_or_default();
                    for id in core::unaccounted(&old, &new, &journal) {
                        notice(
                            settings,
                            store,
                            &format!("credit_lost:{id}"),
                            &format!(
                                "Codex reset credit {id} of {} left the provider's list and no redemption of ours is on record: it expired unused or was used elsewhere.",
                                bucket.principal
                            ),
                        );
                    }
                    memory.credits.insert(bucket.principal.clone(), (now, new));
                }
                Err(e) => log(&format!(
                    "cannot list reset credits of {}: {e:?}",
                    reading.name
                )),
            }
        }
        let known = memory
            .credits
            .get(&bucket.principal)
            .map(|(_, c)| c.clone())
            .unwrap_or_default();
        if !authorized {
            for (id, at) in &known {
                if now < *at && now >= at - core::DEADLINE_MARGIN_S {
                    notice(
                        settings,
                        store,
                        &format!("at_risk:{id}"),
                        &format!(
                            "Codex reset credit {id} of {} expires in {:.1}h and automatic redemption is off (`quota.redeem` is not true): redeem it by hand.",
                            bucket.principal,
                            (*at - now) as f64 / 3600.0
                        ),
                    );
                }
            }
        } else if let Some((episode, credit)) =
            core::deadline_pick(&journal, &bucket.principal, &known, now)
        {
            files.event(now, "deadline_redeem", &episode, json!({}));
            redeem(
                settings,
                store,
                proxy,
                files,
                now,
                bucket,
                reading,
                &episode,
                Some(&credit),
            );
            memory.credits.remove(&bucket.principal);
            if core::journal_steps(&files.journal()?, &episode).last() == Some(&"confirmed")
                && proxy.apply(&Act::Enable(reading.name.clone())).is_ok()
            {
                ours.remove(&reading.name);
            }
        }
    }
    let snapshot = Snapshot {
        generated_at: now,
        live,
        buckets,
        pools,
        seen: prev.seen.union(&names).cloned().collect(),
        ours,
        appeared_at,
    };
    for e in &events {
        files.event(now, e.kind, &e.subject, json!({}));
        log(&format!("{} {}", e.kind, e.subject));
    }
    for b in &snapshot.buckets {
        let sample = b.to_json();
        files.append(EVENTS, json!({"t": now, "sample": sample}));
    }
    crate::taskfile::write(&snapshot_path(&files.dir), &snapshot.to_json().to_string())
        .map_err(|e| format!("cannot write the snapshot: {e}"))?;
    Ok(snapshot)
}

/// Seconds until the next observation: the interval, or sooner when an exhausted bucket's reset hint is near.
fn wake_in_s(snapshot: &Snapshot, now: i64) -> u64 {
    let hint = snapshot
        .buckets
        .iter()
        .filter(|b| b.status == Status::Exhausted)
        .filter_map(|b| b.next_reset)
        .map(|at| at + 5 - now)
        .filter(|d| *d > 0)
        .min();
    hint.map_or(INTERVAL_S, |d| (d as u64).min(INTERVAL_S))
        .max(5)
}

/// Loop forever as its own service; `live` lets it change the proxy and redeem, otherwise it only logs what it would do.
pub fn serve(live: bool) -> u8 {
    log(if live {
        "observer started (live)"
    } else {
        "observer started (shadow: it logs intended actions and changes nothing)"
    });
    let mut memory = Memory::default();
    loop {
        let settings = match settings::load() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("amh: {e}");
                return 1;
            }
        };
        let store = Store::new(&settings);
        let files = Files {
            dir: settings::state_dir(),
        };
        let _ = fs::remove_file(files.dir.join(POLL_NOW));
        let now = chrono::Utc::now().timestamp();
        let wait_s = match Proxy::new(&settings)
            .and_then(|proxy| cycle(&settings, &store, &proxy, &files, &mut memory, live, now))
        {
            Ok(snapshot) => wake_in_s(&snapshot, now),
            Err(e) => {
                log(&e);
                INTERVAL_S.min(60)
            }
        };
        let began = Instant::now();
        while began.elapsed().as_secs() < wait_s && !files.dir.join(POLL_NOW).exists() {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn span(seconds: i64) -> String {
    match seconds.abs() {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d{}h", s / 86400, s % 86400 / 3600),
    }
}

/// The human's view: pools, buckets, and the latest events.
pub fn status_text(snapshot: &Snapshot, now: i64, events: &[Value]) -> String {
    let mut out = format!(
        "snapshot {} old, observer {}\n",
        span(now - snapshot.generated_at),
        if snapshot.live { "live" } else { "shadow" }
    );
    if now - snapshot.generated_at > core::TTL_S {
        out.push_str("STALE: the observer is not running; every pool counts as unknown\n");
    }
    for (name, pool) in snapshot.pools_at(now) {
        out.push_str(&format!("{name}: {}", pool.state.name()));
        if let Some(at) = pool.next_reset {
            out.push_str(&format!(", next reset in {}", span(at - now)));
        }
        out.push('\n');
        for b in snapshot.buckets.iter().filter(|b| b.pool == name) {
            let windows: Vec<String> = b
                .windows
                .iter()
                .filter(|w| w.state != core::WinState::NotActive)
                .map(|w| {
                    let kind = if w.kind == core::Kind::H5 { "5h" } else { "7d" };
                    let reset = w
                        .resets_at
                        .map_or(String::new(), |at| format!(" resets in {}", span(at - now)));
                    if w.state == core::WinState::Invalid {
                        format!("{kind} invalid")
                    } else {
                        format!("{kind} {:.0}%{reset}", w.used_pct)
                    }
                })
                .collect();
            let age = b.observed_at.map_or("never read".to_owned(), |at| {
                format!("read {} ago", span(now - at))
            });
            let who = b.creds.first().map_or("?", |c| c.trim_end_matches(".json"));
            out.push_str(&format!(
                "  {who}: {} for {}; {}; {age}\n",
                b.status.name(),
                span(now - b.since),
                windows.join(", ")
            ));
        }
    }
    for e in events {
        out.push_str(&format!(
            "{} ago  {} {}\n",
            span(now - e["t"].as_i64().unwrap_or(now)),
            e["event"].as_str().unwrap_or("?"),
            e["subject"].as_str().unwrap_or_default()
        ));
    }
    out
}

pub fn status(state_dir: &Path, now: i64) -> Result<String, String> {
    let snapshot = load_snapshot(state_dir)
        .ok_or("amh: no quota snapshot yet; is `amh quota serve` running?")?;
    let text = fs::read_to_string(state_dir.join("quota-events.jsonl")).unwrap_or_default();
    let mut events: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["event"].is_string())
        .collect();
    events.drain(..events.len().saturating_sub(10));
    Ok(status_text(&snapshot, now, &events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A fake management API on a real socket: serves `/auth-files`, records PATCH bodies, and plays the provider's reset-credit endpoints through `/api-call`.
    struct FakeProxy {
        url: String,
        log: Arc<Mutex<Vec<String>>>,
        files: Arc<Mutex<Value>>,
    }

    fn fake_proxy(files: Value, credits: Vec<Value>, consume_status: i64) -> FakeProxy {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let log = Arc::new(Mutex::new(Vec::new()));
        let files = Arc::new(Mutex::new(files));
        let (l, f) = (log.clone(), files.clone());
        let credits = Arc::new(Mutex::new(credits));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut head = String::new();
                let _ = reader.read_line(&mut head);
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) <= 2 {
                        break;
                    }
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let reply = if head.contains("/auth-files") && head.starts_with("GET") {
                    f.lock().expect("files").clone()
                } else if head.contains("/auth-files") {
                    l.lock().expect("log").push(format!(
                        "{} {}",
                        head.split(' ').nth(1).unwrap_or_default(),
                        body
                    ));
                    json!({"status": "ok"})
                } else if body["url"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("/consume"))
                {
                    l.lock().expect("log").push("consume".to_owned());
                    // 201: the credit vanishes and the call times out, but the quota does not reset.
                    if consume_status == 201 {
                        credits.lock().expect("credits").clear();
                    }
                    if consume_status == 200 {
                        credits.lock().expect("credits").clear();
                        let mut fs = f.lock().expect("files");
                        if let Some(list) = fs["files"].as_array_mut() {
                            for x in list {
                                x["quota"]["signals"]["X-Codex-Primary-Used-Percent"] = json!("0");
                                let end = x["quota"]["signals"]["X-Codex-Primary-Reset-At"]
                                    .as_str()
                                    .and_then(|t| t.parse::<i64>().ok())
                                    .unwrap_or_default();
                                x["quota"]["signals"]["X-Codex-Primary-Reset-At"] =
                                    json!((end + 7 * 86400).to_string());
                            }
                        }
                    }
                    json!({"status_code": if consume_status == 201 { 504 } else { consume_status }, "body": "{}"})
                } else {
                    let list = json!({"credits": credits.lock().expect("credits").clone()});
                    json!({"status_code": 200, "body": list.to_string()})
                };
                let text = reply.to_string();
                let mut out = stream;
                let _ = write!(
                    out,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        FakeProxy { url, log, files }
    }

    fn settings_for(proxy: &FakeProxy, dir: &Path, extra: &[(&str, &str)]) -> Settings {
        let key = dir.join("key");
        fs::write(&key, "secret\n").expect("key");
        let mut s: Settings = [
            ("quota.proxy_url", proxy.url.as_str()),
            ("quota.proxy_key_file", key.to_str().unwrap_or_default()),
            ("quota.notify_task", "none.md"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        s.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        s
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("amh-quota-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn codex(name: &str, used: &str, now: i64, disabled: bool) -> Value {
        let at = chrono::DateTime::from_timestamp(now - 20, 0)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        json!({"name": name, "provider": "codex", "email": name, "auth_index": name, "disabled": disabled,
            "id_token": {"chatgpt_account_id": format!("acct-{name}")},
            "quota": {"observed_at": at, "signals": {"X-Codex-Primary-Used-Percent": used, "X-Codex-Primary-Window-Minutes": "10080", "X-Codex-Primary-Reset-At": (now + 86400).to_string()}}})
    }

    fn store_for(dir: &Path) -> Store {
        Store::new(
            &[(
                "work_logs".to_owned(),
                dir.join("logs").to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
        )
    }

    #[test]
    fn full_cycle_against_a_fake_proxy_redeems_once_and_never_twice() {
        let dir = temp("cycle");
        let now = 1_794_000_000;
        let credit = json!({"id": "c1", "status": "available", "is_supported_by_plan": true, "expires_at": "2026-12-01T00:00:00Z"});
        let proxy = fake_proxy(
            json!({"files": [codex("a", "100", now, false), codex("b", "10", now, false)]}),
            vec![credit],
            200,
        );
        let settings = settings_for(&proxy, &dir, &[("quota.redeem", "true")]);
        let (store, files, mut memory) = (
            store_for(&dir),
            Files { dir: dir.clone() },
            Memory::default(),
        );
        let p = Proxy::new(&settings).expect("proxy");
        let first = cycle(&settings, &store, &p, &files, &mut memory, true, now).expect("cycle");
        assert!(first.buckets.iter().any(|b| b.status == Status::Exhausted));
        let calls = proxy.log.lock().expect("log").clone();
        assert!(
            calls
                .iter()
                .all(|c| !(c.contains("a") && c.contains("\"disabled\":true"))),
            "exhausted `a` is never disabled: {calls:?}"
        );
        assert_eq!(calls.iter().filter(|c| *c == "consume").count(), 1);
        let steps: Vec<String> = files
            .journal()
            .expect("journal")
            .iter()
            .filter_map(|e| e["step"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(steps, ["intent", "called", "confirmed"]);
        for t in 1..4 {
            cycle(
                &settings,
                &store,
                &p,
                &files,
                &mut memory,
                true,
                now + t * 300,
            )
            .expect("cycle");
        }
        assert_eq!(
            proxy
                .log
                .lock()
                .expect("log")
                .iter()
                .filter(|c| *c == "consume")
                .count(),
            1,
            "one redemption per exhaustion"
        );
        assert!(status(&dir, now + 5).expect("status").contains("codex:"));
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn timeout_is_uncertain_and_retried_with_backoff_while_the_credit_stays_listed() {
        let dir = temp("uncertain");
        let now = 1_794_000_000;
        let credit =
            json!({"id": "c1", "status": "available", "expires_at": "2026-12-01T00:00:00Z"});
        let proxy = fake_proxy(
            json!({"files": [codex("a", "100", now, false)]}),
            vec![credit],
            504,
        );
        let settings = settings_for(&proxy, &dir, &[("quota.redeem", "true")]);
        let (store, files, mut memory) = (
            store_for(&dir),
            Files { dir: dir.clone() },
            Memory::default(),
        );
        let p = Proxy::new(&settings).expect("proxy");
        for t in 0..3 {
            cycle(
                &settings,
                &store,
                &p,
                &files,
                &mut memory,
                true,
                now + t * 300,
            )
            .expect("cycle");
        }
        let steps: Vec<String> = files
            .journal()
            .expect("journal")
            .iter()
            .filter_map(|e| e["step"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(
            steps,
            [
                "intent",
                "uncertain",
                "intent",
                "uncertain",
                "intent",
                "uncertain"
            ],
            "the credit is still listed, so each backoff expiry retries"
        );
        assert_eq!(
            proxy
                .log
                .lock()
                .expect("log")
                .iter()
                .filter(|c| *c == "consume")
                .count(),
            3
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    fn expiring(now: i64, in_s: i64) -> Value {
        let at = chrono::DateTime::from_timestamp(now + in_s, 0)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        json!({"id": "c1", "status": "available", "is_supported_by_plan": true, "expires_at": at})
    }

    fn consumes(proxy: &FakeProxy) -> usize {
        proxy
            .log
            .lock()
            .expect("log")
            .iter()
            .filter(|c| *c == "consume")
            .count()
    }

    fn steps_of(files: &Files) -> Vec<String> {
        files
            .journal()
            .expect("journal")
            .iter()
            .filter_map(|e| e["step"].as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn credit_near_expiry_is_redeemed_at_low_usage_and_not_before() {
        let dir = temp("deadline");
        let now = 1_794_000_000;
        let proxy = fake_proxy(
            json!({"files": [codex("a", "10", now, false)]}),
            vec![expiring(now, 3 * 86400)],
            200,
        );
        let settings = settings_for(&proxy, &dir, &[("quota.redeem", "true")]);
        let (store, files, mut memory) = (
            store_for(&dir),
            Files { dir: dir.clone() },
            Memory::default(),
        );
        let p = Proxy::new(&settings).expect("proxy");
        cycle(&settings, &store, &p, &files, &mut memory, true, now).expect("cycle");
        assert_eq!(consumes(&proxy), 0, "three days left: not yet");
        // Two days later the credit has 10h left; the usage is still 10%.
        let later = now + 3 * 86400 - 10 * 3600;
        *proxy.files.lock().expect("files") = json!({"files": [codex("a", "10", later, false)]});
        cycle(&settings, &store, &p, &files, &mut memory, true, later).expect("cycle");
        assert_eq!(consumes(&proxy), 1);
        assert_eq!(steps_of(&files), ["intent", "called", "confirmed"]);
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn deadline_redemption_is_retried_after_a_rejection_until_it_works() {
        let dir = temp("deadline-retry");
        let now = 1_794_000_000;
        let proxy = fake_proxy(
            json!({"files": [codex("a", "10", now, false)]}),
            vec![expiring(now, 3600)],
            403,
        );
        let settings = settings_for(&proxy, &dir, &[("quota.redeem", "true")]);
        let (store, files, mut memory) = (
            store_for(&dir),
            Files { dir: dir.clone() },
            Memory::default(),
        );
        let p = Proxy::new(&settings).expect("proxy");
        for t in 0..3 {
            cycle(
                &settings,
                &store,
                &p,
                &files,
                &mut memory,
                true,
                now + t * 300,
            )
            .expect("cycle");
        }
        assert_eq!(consumes(&proxy), 3, "rejections are retried");
        assert!(
            steps_of(&files)
                .iter()
                .all(|s| s == "intent" || s == "rejected")
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn vanished_credit_without_a_quota_reset_is_not_confirmed() {
        let dir = temp("vanished");
        let now = 1_794_000_000;
        let proxy = fake_proxy(
            json!({"files": [codex("a", "10", now, false)]}),
            vec![expiring(now, 3600)],
            201,
        );
        let settings = settings_for(&proxy, &dir, &[("quota.redeem", "true")]);
        let (store, files, mut memory) = (
            store_for(&dir),
            Files { dir: dir.clone() },
            Memory::default(),
        );
        let p = Proxy::new(&settings).expect("proxy");
        cycle(&settings, &store, &p, &files, &mut memory, true, now).expect("cycle");
        let steps = steps_of(&files);
        assert!(steps.contains(&"uncertain".to_owned()), "{steps:?}");
        assert!(!steps.contains(&"confirmed".to_owned()), "{steps:?}");
        assert_eq!(consumes(&proxy), 1, "the vanished credit is not retried");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn crash_after_intent_is_recovered_as_uncertain_without_a_second_call() {
        let dir = temp("crash");
        let now = 1_794_000_000;
        let credit =
            json!({"id": "c1", "status": "available", "expires_at": "2026-12-01T00:00:00Z"});
        let proxy = fake_proxy(
            json!({"files": [codex("a", "100", now, false)]}),
            vec![credit],
            200,
        );
        let settings = settings_for(&proxy, &dir, &[("quota.redeem", "true")]);
        let (store, files) = (store_for(&dir), Files { dir: dir.clone() });
        let (readings, ep) = {
            let r = core::parse_auth_files(&json!({"files": [codex("a", "100", now, false)]}), now);
            let (b, _) = core::step(&[], &r, now);
            (r, core::episode(&b[0]).unwrap_or_default())
        };
        files.step(
            now,
            &ep,
            "intent",
            json!({"cred": readings[0].name, "credit_id": "c1", "before": 1}),
        );
        let p = Proxy::new(&settings).expect("proxy");
        cycle(
            &settings,
            &store,
            &p,
            &files,
            &mut Memory::default(),
            true,
            now,
        )
        .expect("cycle");
        assert_eq!(
            proxy
                .log
                .lock()
                .expect("log")
                .iter()
                .filter(|c| *c == "consume")
                .count(),
            0
        );
        let steps: Vec<String> = files
            .journal()
            .expect("journal")
            .iter()
            .filter_map(|e| e["step"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(steps.first().map(String::as_str), Some("intent"));
        assert!(steps.contains(&"uncertain".to_owned()));
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn shadow_mode_changes_nothing_and_unauthorized_never_redeems() {
        let dir = temp("shadow");
        let now = 1_794_000_000;
        let credit =
            json!({"id": "c1", "status": "available", "expires_at": "2026-12-01T00:00:00Z"});
        let proxy = fake_proxy(
            json!({"files": [codex("a", "100", now, false)]}),
            vec![credit.clone()],
            200,
        );
        let (store, files, mut memory) = (
            store_for(&dir),
            Files { dir: dir.clone() },
            Memory::default(),
        );
        let settings = settings_for(&proxy, &dir, &[("quota.redeem", "true")]);
        let p = Proxy::new(&settings).expect("proxy");
        cycle(&settings, &store, &p, &files, &mut memory, false, now).expect("cycle");
        assert!(
            proxy.log.lock().expect("log").is_empty(),
            "shadow neither patches nor redeems"
        );
        let live_unauthorized = settings_for(&proxy, &dir, &[]);
        cycle(
            &live_unauthorized,
            &store,
            &p,
            &files,
            &mut memory,
            true,
            now + 1,
        )
        .expect("cycle");
        assert!(
            !proxy
                .log
                .lock()
                .expect("log")
                .iter()
                .any(|c| c == "consume")
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn new_credential_is_used_at_once_and_a_proxy_restart_is_survived() {
        let dir = temp("added");
        let now = 1_794_000_000;
        let proxy = fake_proxy(
            json!({"files": [codex("a", "10", now, false)]}),
            vec![],
            200,
        );
        let (store, files, mut memory) = (
            store_for(&dir),
            Files { dir: dir.clone() },
            Memory::default(),
        );
        let settings = settings_for(&proxy, &dir, &[]);
        let p = Proxy::new(&settings).expect("proxy");
        cycle(&settings, &store, &p, &files, &mut memory, true, now).expect("cycle");
        *proxy.files.lock().expect("files") = json!({"files": [codex("a", "10", now + 300, false), codex("new", "10", now + 300, false)]});
        let snap =
            cycle(&settings, &store, &p, &files, &mut memory, true, now + 300).expect("cycle");
        assert!(snap.seen.contains("new"));
        assert!(
            !proxy
                .log
                .lock()
                .expect("log")
                .iter()
                .any(|c| c.contains("new") && c.contains("true")),
            "a new credential is never disabled"
        );
        let bad = Proxy {
            url: "http://127.0.0.1:1/v0/management".into(),
            ..Proxy::new(&settings).expect("proxy")
        };
        assert!(
            cycle(
                &settings,
                &store,
                &bad,
                &files,
                &mut memory,
                true,
                now + 600
            )
            .is_err()
        );
        let snap = cycle(&settings, &store, &p, &files, &mut memory, true, now + 900)
            .expect("cycle after the proxy came back");
        assert!(snap.seen.contains("new"), "state survives a proxy outage");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn status_text_names_pools_buckets_and_stale_snapshots() {
        let now = 1_794_000_000;
        let r = core::parse_auth_files(&json!({"files": [codex("a.json", "50", now, false)]}), now);
        let (b, _) = core::step(&[], &r, now);
        let snapshot = Snapshot {
            generated_at: now,
            pools: core::pools(&b, now),
            buckets: b,
            ..Snapshot::default()
        };
        let text = status_text(&snapshot, now + 60, &[]);
        assert!(
            text.contains("codex: unknown") && text.contains("a: unknown"),
            "{text}"
        );
        assert!(status_text(&snapshot, now + 5000, &[]).contains("STALE"));
    }
}
