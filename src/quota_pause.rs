//! amh's side of quota: derive which workers wait, resume them by priority, admit new tasks, and tell about what needs telling. See `docs/quota-manager.md`.
// 🧑 "We just need some priority of tasks plus estimation of whether we'd run out"

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::quota_core::{self as core, Status, Verdict, Worker};
use crate::quota_observer::load_snapshot;
use crate::store::Store;
use crate::{agents, omnigent, settings, work};

type Settings = BTreeMap<String, String>;

/// The watcher rewrites the pause every pass; a pause older than this is from a dead watcher and ignored.
const PAUSE_TTL_S: i64 = 600;
/// Matches the machine-wide start limit of 2 per minute.
const MAX_RESUMES_PER_PASS: usize = 2;
/// A pool unknown this long, with work waiting on it, is told to the human.
const UNKNOWN_NOTICE_S: i64 = 3600;
const PAUSE_FILE: &str = "quota-pause.json";
const NOTICES_FILE: &str = "quota-notices.json";
const APPEARED_RECORD: &str = "amh-quota-appeared.txt";
const RESUME_TEXT: &str =
    "Quota is available again; you were waiting for it. Continue the work you were doing.";

fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

fn enforced(settings: &Settings) -> bool {
    settings::get(settings, "quota.enforce") == "true"
}

fn read_pause(state_dir: &Path) -> Value {
    fs::read_to_string(state_dir.join(PAUSE_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// Why the agent at `address` waits for quota, if the watcher's last pass paused it.
pub fn paused(address: &str) -> Option<String> {
    let pause = read_pause(&settings::state_dir());
    let fresh = pause["generated_at"]
        .as_i64()
        .is_some_and(|at| (0..=PAUSE_TTL_S).contains(&(now_s() - at)));
    (pause["enforced"] == true && fresh)
        .then(|| pause["paused"][address].as_str().map(str::to_owned))?
}

/// Tell once per `key`: the notices file remembers what was sent, and a failed send is tried again next time.
pub fn notice_to(settings: &Settings, store: &Store, key: &str, to: &str, text: &str) {
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
            sent.insert(key.to_owned(), json!(now_s()));
            if let Err(e) = crate::taskfile::write(&path, &Value::Object(sent).to_string()) {
                println!("quota: cannot record notice {key}: {e}");
            }
        }
        Err(e) => println!("quota: notice {key} not delivered to {to}: {e}"),
    }
}

/// Tell the task that owns quota questions.
pub fn notice(settings: &Settings, store: &Store, key: &str, text: &str) {
    let to = Some(settings::get(settings, "quota.notify_task"))
        .filter(|t| !t.is_empty())
        .unwrap_or("quota_support.md");
    notice_to(settings, store, key, to, text);
}

/// The workers of managed pools: active tasks with an agent at an Omnigent address.
fn workers(store: &Store) -> Result<Vec<Worker>, String> {
    Ok(store
        .active()?
        .iter()
        .filter(|t| work::expects_agent(t) && t.address().starts_with("omnigent://"))
        .filter_map(|t| {
            let pool = t.field("tool").filter(|tool| core::POOLS.contains(tool))?;
            Some(Worker {
                address: t.address().to_owned(),
                pool: Some(pool.to_owned()),
                priority: t
                    .field("priority")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or_default(),
                is_manager: t.field("is_manager") == Some("true"),
            })
        })
        .collect())
}

fn addresses(pause: &Value, key: &str) -> BTreeSet<String> {
    pause[key]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k.clone())
        .collect()
}

fn write_pause(
    store: &Store,
    now: i64,
    enforce: bool,
    paused: &BTreeMap<String, String>,
) -> Result<(), String> {
    let shadow: serde_json::Map<String, Value> =
        paused.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let written = json!({"generated_at": now, "enforced": enforce,
        "paused": if enforce { Value::Object(shadow.clone()) } else { json!({}) }, "shadow": shadow});
    crate::taskfile::write(&store.state_dir.join(PAUSE_FILE), &written.to_string())
        .map_err(|e| format!("quota pause: cannot write {PAUSE_FILE}: {e}"))
}

/// The watcher's quota pass: recompute the pause from the snapshot and the tasks, nudge the workers that may resume, clear session evidence on `quota_appeared`, and send the deduplicated notices. Returns log lines.
///
/// Without `quota.enforce = "true"` nothing is paused: the pass only records and logs what it would do.
pub fn pass(settings: &Settings, store: &Store) -> Vec<String> {
    let now = now_s();
    let enforce = enforced(settings);
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
            "the quota snapshot is over 15 minutes old: the observer (`amh quota serve`) is not running; every pool counts as unknown.",
        );
    }
    let snapshot = snapshot.unwrap_or_default();
    let pools = snapshot.pools_at(now);
    let workers = match workers(store) {
        Ok(w) => w,
        Err(e) => return vec![format!("quota pause: cannot list tasks: {e}")],
    };
    let old = read_pause(&store.state_dir);
    let prev = addresses(&old, if enforce { "paused" } else { "shadow" });
    let mut pause = core::pauses(&pools, &workers, &prev, MAX_RESUMES_PER_PASS);
    let held: BTreeSet<String> = pause.paused.keys().cloned().collect();
    // The file goes first and without the resumed workers: the nudge below is refused while they are still in it.
    if let Err(e) = write_pause(store, now, enforce, &pause.paused) {
        lines.push(e);
    }
    let mode = if enforce { "" } else { "shadow: would " };
    for address in held.difference(&prev) {
        lines.push(format!(
            "quota pause {mode}hold {address}: {}",
            pause.paused[address]
        ));
    }
    let mut unsent = false;
    for address in &pause.resumed {
        lines.push(format!("quota pause {mode}resume {address}"));
        if enforce && let Err(e) = omnigent::send(settings, address, RESUME_TEXT) {
            lines.push(format!("quota resume nudge to {address} failed: {e}"));
            // Still waiting, so the next pass resumes it again.
            pause.paused
                .insert(address.clone(), "resume nudge pending".to_owned());
            unsent = true;
        }
    }
    if unsent && let Err(e) = write_pause(store, now, enforce, &pause.paused) {
        lines.push(e);
    }
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
    for (name, pool) in &pools {
        let waiting = workers
            .iter()
            .any(|w| w.pool.as_deref() == Some(name) && held.contains(&w.address));
        if enforce
            && pool.state == Status::Unknown
            && waiting
            && now - pool.since > UNKNOWN_NOTICE_S
        {
            notice(
                settings,
                store,
                &format!("unknown:{name}:{}", pool.since),
                &format!("{name} quota has been unknown for over an hour while work waits on it."),
            );
        }
        if enforce
            && pool.verdict() == Verdict::Waste
            && !waiting
            && let Some(e) = &pool.estimate
        {
            let to = settings::get(settings, "main_manager");
            let text = format!(
                "{name} quota is projected to expire unused ({:.0}% points left at its reset in {:.0}h) and no work waits for it; start more ready work on {name}.",
                e.projected_pct, e.hours_to_reset
            );
            notice_to(
                settings,
                store,
                &format!("waste:{name}:{}", pool.next_reset.unwrap_or_default()),
                to,
                &text,
            );
        }
    }
    lines
}

/// The tool a new task starts with: the default, or the other pool when the default is closed.
///
/// A tool the caller named is never swapped; a closed pool refuses it. Without `quota.enforce` the default stands.
pub fn admit(
    settings: &Settings,
    store: &Store,
    tool: &str,
    named: bool,
    priority: i64,
) -> Result<String, String> {
    if !enforced(settings) {
        return Ok(tool.to_owned());
    }
    let pools = load_snapshot(&store.state_dir)
        .unwrap_or_default()
        .pools_at(now_s());
    let mut lowest: BTreeMap<String, i64> = BTreeMap::new();
    let pause = read_pause(&store.state_dir);
    for w in workers(store)?
        .iter()
        .filter(|w| !w.is_manager && pause["paused"][&w.address].is_null())
    {
        if let Some(pool) = &w.pool {
            let low = lowest.entry(pool.clone()).or_insert(w.priority);
            *low = (*low).min(w.priority);
        }
    }
    let chosen = core::choose_tool(tool, priority, &pools, &lowest)
        .map_err(|e| format!("amh: {e}; retry later or see `amh quota status`"))?;
    if named && chosen != tool {
        return Err(format!(
            "amh: the {tool} quota pool is closed; start without --tool to let amh pick, or see `amh quota status`"
        ));
    }
    Ok(chosen.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota_core::{Pool, Snapshot};

    fn pool(state: Status) -> Pool {
        Pool {
            state,
            since: now_s(),
            next_reset: None,
            n_buckets: 1,
            estimate: None,
        }
    }

    fn setup(name: &str, tasks: &[(&str, &str, &str, &str)]) -> (Store, Settings) {
        let dir = std::env::temp_dir().join(format!("amh-pause-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("logs")).expect("dir");
        for (file, address, tool, extra) in tasks {
            let text =
                format!("---\nstatus: running\nrunat: {address}\ntool: {tool}\n{extra}---\n");
            fs::write(dir.join("logs").join(file), text).expect("task");
        }
        let snapshot = Snapshot {
            generated_at: now_s(),
            pools: BTreeMap::from([
                ("codex".to_owned(), pool(Status::Exhausted)),
                ("claude".to_owned(), pool(Status::Available)),
            ]),
            ..Snapshot::default()
        };
        fs::write(
            dir.join("quota-snapshot.json"),
            snapshot.to_json().to_string(),
        )
        .expect("snapshot");
        let store = Store {
            root: dir.join("logs"),
            state_dir: dir,
        };
        (
            store,
            [("quota.enforce".to_owned(), "true".to_owned())].into(),
        )
    }

    #[test]
    fn the_pause_holds_the_exhausted_pools_workers_and_admission_moves_to_the_open_pool() {
        let (store, settings) = setup(
            "pause",
            &[
                ("a.md", "omnigent://a", "codex", "priority: 1\n"),
                ("b.md", "omnigent://b", "claude", ""),
                ("m.md", "omnigent://m", "codex", "is_manager: true\n"),
                ("c.md", "omnigent://c", "cursor", ""),
            ],
        );
        let lines = pass(&settings, &store);
        assert!(
            lines.iter().any(|l| l.contains("omnigent://a")),
            "{lines:?}"
        );
        let pause = read_pause(&store.state_dir);
        assert_eq!(
            addresses(&pause, "paused"),
            ["omnigent://a", "omnigent://m"].map(String::from).into()
        );
        assert_eq!(
            admit(&settings, &store, "codex", false, 0),
            Ok("claude".to_owned())
        );
        assert!(admit(&settings, &store, "codex", true, 0).is_err());
        assert_eq!(
            admit(&settings, &store, "cursor", true, 0),
            Ok("cursor".to_owned())
        );
        assert_eq!(
            admit(&BTreeMap::new(), &store, "codex", false, 0),
            Ok("codex".to_owned()),
            "without enforcement the default stands"
        );
        fs::remove_dir_all(&store.state_dir).expect("cleanup");
    }

    #[test]
    fn without_enforcement_the_pause_is_only_a_shadow() {
        let (store, _) = setup("shadow", &[("a.md", "omnigent://a", "codex", "")]);
        let lines = pass(&BTreeMap::new(), &store);
        assert!(
            lines.iter().any(|l| l.contains("shadow: would hold")),
            "{lines:?}"
        );
        let pause = read_pause(&store.state_dir);
        assert!(addresses(&pause, "paused").is_empty());
        assert_eq!(addresses(&pause, "shadow").len(), 1);
        fs::remove_dir_all(&store.state_dir).expect("cleanup");
    }
}
