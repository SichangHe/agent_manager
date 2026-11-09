//! Quota model and policy as pure functions of (proxy readings, previous snapshot, config, now). See `docs/quota-manager.md`.
// 🧑 "We just need some priority of tasks plus estimation of whether we'd run out"

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use serde_json::{Value, json};

/// The tools whose quota is managed; Gemini is out of scope.
pub const POOLS: [&str; 2] = ["claude", "codex"];
/// A reading older than this is unknown, never exhausted.
pub const TTL_S: i64 = 900;
/// Readings must be below this on `OK_STREAK` consecutive polls before a bucket turns available.
const OK_BELOW_PCT: f64 = 99.0;
const OK_STREAK: u32 = 2;
/// A 5h window above this ranks its bucket below the others until it resets.
const HOT_5H_PCT: f64 = 90.0;
/// Burn is measured over this long.
const HISTORY_S: i64 = 3600;
/// Less history than this gives no burn estimate.
const MIN_SPAN_S: i64 = 600;
/// Projected leftover per bucket, in percentage points, above which quota is predicted to expire unused.
const WASTE_PCT_PER_BUCKET: f64 = 20.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    H5,
    D7,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WinState {
    Active,
    /// Documented per provider: Claude omits the 5h window at 0% use.
    NotActive,
    Invalid,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Window {
    pub kind: Kind,
    pub state: WinState,
    pub used_pct: f64,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Available,
    Exhausted,
    Unknown,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Available => "available",
            Status::Exhausted => "exhausted",
            Status::Unknown => "unknown",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "available" => Status::Available,
            "exhausted" => Status::Exhausted,
            _ => Status::Unknown,
        }
    }
}

/// One proxy credential as the management API shows it.
#[derive(Clone, Debug)]
pub struct Reading {
    pub name: String,
    pub auth_index: String,
    pub pool: String,
    pub principal: String,
    pub account_id: String,
    pub disabled: bool,
    pub priority: Option<i64>,
    pub observed_at: Option<i64>,
    pub windows: Vec<Window>,
    /// End of an active `credential_quota` cooldown: the proxy itself saw a quota error.
    pub quota_until: Option<i64>,
    pub auth_error: bool,
}

/// One quota per principal; several credentials of one principal share it.
#[derive(Clone, Debug)]
pub struct Bucket {
    pub principal: String,
    pub pool: String,
    pub creds: Vec<String>,
    pub windows: Vec<Window>,
    pub observed_at: Option<i64>,
    pub status: Status,
    pub since: i64,
    pub streak: u32,
    pub next_reset: Option<i64>,
    pub history: Vec<(i64, f64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    pub remaining_pct: f64,
    pub burn_pct_per_h: f64,
    pub hours_to_reset: f64,
    pub projected_pct: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    RunOut,
    Fits,
    Waste,
}

#[derive(Clone, Debug)]
pub struct Pool {
    pub state: Status,
    pub since: i64,
    pub next_reset: Option<i64>,
    pub n_buckets: usize,
    pub estimate: Option<Estimate>,
}

impl Pool {
    pub fn verdict(&self) -> Verdict {
        match &self.estimate {
            Some(e) if e.projected_pct < 0.0 => Verdict::RunOut,
            Some(e) if e.projected_pct >= WASTE_PCT_PER_BUCKET * self.n_buckets as f64 => {
                Verdict::Waste
            }
            _ => Verdict::Fits,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub kind: &'static str,
    pub subject: String,
}

fn epoch(text: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(text.as_str()?)
        .ok()
        .map(|at| at.timestamp())
}

fn signal<'a>(signals: &'a Value, key: &str) -> Option<&'a str> {
    signals.get(key)?.as_str()
}

/// Claude: `Anthropic-Ratelimit-Unified-{5h,7d}-*`, utilization as a fraction.
fn claude_windows(signals: &Value) -> Vec<Window> {
    let one = |kind: Kind, tag: &str| {
        let key = |what: &str| format!("Anthropic-Ratelimit-Unified-{tag}-{what}");
        let Some(util) = signal(signals, &key("Utilization")) else {
            let state = if kind == Kind::H5 {
                WinState::NotActive
            } else {
                WinState::Invalid
            };
            return Window {
                kind,
                state,
                used_pct: 0.0,
                resets_at: None,
            };
        };
        let used = util.parse::<f64>().ok().filter(|u| (0.0..=1.0).contains(u));
        let rejected = signal(signals, &key("Status")) == Some("rejected");
        let resets_at = signal(signals, &key("Reset")).and_then(|r| r.parse().ok());
        match (used, rejected) {
            (_, true) => Window {
                kind,
                state: WinState::Active,
                used_pct: 100.0,
                resets_at,
            },
            (Some(u), _) if resets_at.is_some() => Window {
                kind,
                state: WinState::Active,
                used_pct: (u * 1e4).round() / 100.0,
                resets_at,
            },
            _ => Window {
                kind,
                state: WinState::Invalid,
                used_pct: 0.0,
                resets_at,
            },
        }
    };
    if signals.as_object().is_none_or(|s| s.is_empty()) {
        return Vec::new();
    }
    vec![one(Kind::H5, "5h"), one(Kind::D7, "7d")]
}

/// Codex: `X-Codex-{Primary,Secondary}-*`; the window length says which is 5h and which weekly; length 0 means absent.
fn codex_windows(signals: &Value) -> Vec<Window> {
    ["Primary", "Secondary"]
        .iter()
        .filter_map(|which| {
            let key = |what: &str| format!("X-Codex-{which}-{what}");
            let minutes: i64 = signal(signals, &key("Window-Minutes"))?.parse().ok()?;
            let kind = match minutes {
                300 => Kind::H5,
                10080 => Kind::D7,
                0 => return None,
                _ => {
                    return Some(Window {
                        kind: Kind::D7,
                        state: WinState::Invalid,
                        used_pct: 0.0,
                        resets_at: None,
                    });
                }
            };
            let used = signal(signals, &key("Used-Percent")).and_then(|u| u.parse::<f64>().ok());
            let resets_at = signal(signals, &key("Reset-At")).and_then(|r| r.parse().ok());
            Some(match used.filter(|u| (0.0..=100.0).contains(u)) {
                Some(used_pct) => Window {
                    kind,
                    state: WinState::Active,
                    used_pct,
                    resets_at,
                },
                None => Window {
                    kind,
                    state: WinState::Invalid,
                    used_pct: 0.0,
                    resets_at,
                },
            })
        })
        .collect()
}

/// Turn the management API's `/auth-files` answer into readings, taking the freshest signals of each credential.
pub fn parse_auth_files(answer: &Value, now: i64) -> Vec<Reading> {
    let files = answer["files"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    files
        .iter()
        .filter_map(|file| {
            let pool = file["provider"].as_str().filter(|p| POOLS.contains(p))?;
            let name = file["name"].as_str().or(file["id"].as_str())?.to_owned();
            let account_id = file["id_token"]["chatgpt_account_id"]
                .as_str()
                .unwrap_or_default();
            let who = file["email"]
                .as_str()
                .filter(|e| !e.is_empty())
                .unwrap_or(&name);
            let principal = match (pool, account_id) {
                ("codex", id) if !id.is_empty() => format!("codex:{id}"),
                _ => format!("{pool}:{who}"),
            };
            let mut sources = vec![&file["quota"]];
            sources.extend(
                file["model_quotas"]
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.values()),
            );
            let (observed_at, signals) = sources
                .into_iter()
                .filter(|s| s["signals"].as_object().is_some_and(|m| !m.is_empty()))
                .map(|s| (epoch(&s["observed_at"]), &s["signals"]))
                .max_by_key(|(at, _)| *at)
                .unwrap_or((None, &Value::Null));
            let windows = if pool == "claude" {
                claude_windows(signals)
            } else {
                codex_windows(signals)
            };
            let quota_until = file["cooldowns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["reason"] == "credential_quota")
                .filter_map(|c| epoch(&c["retry_at"]))
                .filter(|until| *until > now)
                .max();
            let message = file["status_message"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            Some(Reading {
                auth_index: file["auth_index"].as_str().unwrap_or_default().to_owned(),
                pool: pool.to_owned(),
                principal,
                account_id: account_id.to_owned(),
                disabled: file["disabled"].as_bool().unwrap_or(false),
                priority: file["priority"].as_i64(),
                observed_at,
                windows,
                quota_until,
                auth_error: message.contains("401") || message.contains("unauthorized"),
                name,
            })
        })
        .collect()
}

/// The used percentage that binds: the weekly window if active, else the highest active one.
fn binding_used(windows: &[Window]) -> Option<f64> {
    let active = |w: &&Window| w.state == WinState::Active;
    windows
        .iter()
        .filter(active)
        .find(|w| w.kind == Kind::D7)
        .map(|w| w.used_pct)
        .or_else(|| {
            windows
                .iter()
                .filter(active)
                .map(|w| w.used_pct)
                .reduce(f64::max)
        })
}

enum Raw {
    Exhausted(Option<i64>),
    Ok,
    Borderline,
    Unknown,
}

fn classify(r: &Reading, prev: Option<&Bucket>, now: i64) -> Raw {
    if let Some(until) = r.quota_until {
        return Raw::Exhausted(Some(until));
    }
    let fresh = r
        .observed_at
        .is_some_and(|at| (0..=TTL_S).contains(&(now - at)));
    let predates_exhaustion = prev
        .filter(|b| b.status == Status::Exhausted)
        .filter(|b| r.observed_at.is_none_or(|at| at < b.since));
    if let Some(b) = predates_exhaustion {
        // Only a reading taken after the exhaustion can end it; with none, wait for the reset hint plus one TTL, then ask for a new poll.
        return match b.next_reset {
            Some(at) if now <= at + TTL_S => Raw::Exhausted(Some(at)),
            _ => Raw::Unknown,
        };
    }
    let invalid = r.windows.iter().any(|w| w.state == WinState::Invalid);
    if r.auth_error || !fresh || r.windows.is_empty() || invalid {
        return Raw::Unknown;
    }
    let active: Vec<&Window> = r
        .windows
        .iter()
        .filter(|w| w.state == WinState::Active)
        .collect();
    let full: Vec<&&Window> = active.iter().filter(|w| w.used_pct >= 100.0).collect();
    if full.iter().any(|w| w.resets_at.is_some_and(|at| at <= now)) {
        return Raw::Unknown;
    }
    if !full.is_empty() {
        return Raw::Exhausted(full.iter().filter_map(|w| w.resets_at).max());
    }
    if active.iter().all(|w| w.used_pct < OK_BELOW_PCT) {
        Raw::Ok
    } else {
        Raw::Borderline
    }
}

/// Fold the new readings into the previous buckets: statuses with hysteresis, then the transition events.
pub fn step(prev: &[Bucket], readings: &[Reading], now: i64) -> (Vec<Bucket>, Vec<Event>) {
    let mut by_principal: BTreeMap<&str, Vec<&Reading>> = BTreeMap::new();
    for r in readings {
        by_principal.entry(&r.principal).or_default().push(r);
    }
    let mut buckets = Vec::new();
    let mut events = Vec::new();
    for (principal, group) in by_principal {
        let old = prev.iter().find(|b| b.principal == principal);
        let freshest = group
            .iter()
            .max_by_key(|r| (r.quota_until, r.observed_at))
            .copied();
        let Some(r) = freshest else { continue };
        let raw = classify(r, old, now);
        let old_status = old.map_or(Status::Unknown, |b| b.status);
        let (status, streak, next_reset) = match raw {
            Raw::Exhausted(at) => (Status::Exhausted, 0, at),
            Raw::Unknown => (Status::Unknown, 0, None),
            Raw::Borderline => (old_status, 0, None),
            Raw::Ok => {
                let streak = old.map_or(0, |b| b.streak) + 1;
                let status = if old_status == Status::Available || streak >= OK_STREAK {
                    Status::Available
                } else {
                    old_status
                };
                (status, streak, None)
            }
        };
        let next_reset = next_reset.or_else(|| {
            r.windows
                .iter()
                .filter(|w| w.state == WinState::Active && w.kind == Kind::D7)
                .find_map(|w| w.resets_at)
        });
        let mut history: Vec<(i64, f64)> = old.map(|b| b.history.clone()).unwrap_or_default();
        if let (Some(used), true, true) = (
            binding_used(&r.windows),
            r.observed_at.is_some(),
            matches!(raw, Raw::Ok | Raw::Borderline | Raw::Exhausted(_)),
        ) {
            history.push((now, used));
        }
        history.retain(|(t, _)| now - t <= HISTORY_S);
        let since = old.filter(|b| b.status == status).map_or(now, |b| b.since);
        if let Some(old) = old {
            let kind = match (old.status, status) {
                (Status::Exhausted | Status::Unknown, Status::Available) => Some("quota_appeared"),
                (Status::Available | Status::Unknown, Status::Exhausted) => Some("quota_ran_out"),
                _ => None,
            };
            if let Some(kind) = kind {
                events.push(Event {
                    kind,
                    subject: format!("{} {principal}", r.pool),
                });
            }
        }
        buckets.push(Bucket {
            principal: principal.to_owned(),
            pool: r.pool.clone(),
            creds: group.iter().map(|r| r.name.clone()).collect(),
            windows: r.windows.clone(),
            observed_at: r.observed_at,
            status,
            since,
            streak,
            next_reset,
            history,
        });
    }
    (buckets, events)
}

/// Observed rise of the used percentage per hour over the last hour; drops (resets) are ignored.
fn burn_pct_per_h(history: &[(i64, f64)]) -> f64 {
    let (Some(first), Some(last)) = (history.first(), history.last()) else {
        return 0.0;
    };
    let span_s = last.0 - first.0;
    if span_s < MIN_SPAN_S {
        return 0.0;
    }
    let rise: f64 = history.windows(2).map(|w| (w[1].1 - w[0].1).max(0.0)).sum();
    rise * 3600.0 / span_s as f64
}

/// Per tool: the union of its available buckets; with none, unknown beats exhausted.
pub fn pools(buckets: &[Bucket], now: i64) -> BTreeMap<String, Pool> {
    POOLS
        .iter()
        .map(|name| {
            let mine: Vec<&Bucket> = buckets.iter().filter(|b| b.pool == *name).collect();
            let of = |s: Status| mine.iter().filter(move |b| b.status == s);
            let state = if of(Status::Available).next().is_some() {
                Status::Available
            } else if of(Status::Unknown).next().is_some() {
                Status::Unknown
            } else {
                Status::Exhausted
            };
            let next_reset = mine
                .iter()
                .filter(|b| b.status == state)
                .filter_map(|b| b.next_reset)
                .min();
            let live: Vec<&&Bucket> = of(Status::Available).collect();
            let remaining_pct = live
                .iter()
                .filter_map(|b| binding_used(&b.windows))
                .map(|u| 100.0 - u)
                .sum();
            let burn = live.iter().map(|b| burn_pct_per_h(&b.history)).sum::<f64>();
            let hours_to_reset = live
                .iter()
                .filter_map(|b| b.next_reset)
                .map(|at| (at - now).max(0) as f64 / 3600.0)
                .reduce(f64::min);
            let estimate =
                hours_to_reset
                    .filter(|_| state == Status::Available)
                    .map(|hours_to_reset| Estimate {
                        remaining_pct,
                        burn_pct_per_h: burn,
                        hours_to_reset,
                        projected_pct: remaining_pct - burn * hours_to_reset,
                    });
            let since = mine
                .iter()
                .filter(|b| b.status == state)
                .map(|b| b.since)
                .min()
                .unwrap_or(now);
            (
                name.to_string(),
                Pool {
                    state,
                    since,
                    next_reset,
                    n_buckets: mine.len(),
                    estimate,
                },
            )
        })
        .collect()
}

/// Proxy priority per credential of each available bucket: higher is preferred.
// 🧑 "Let the proxy agent make the proxy automatically prioritize accounts that would reset first"
/// Among available buckets the soonest weekly reset ranks first, because that is the quota that expires first; a bucket whose 5h window is above `HOT_5H_PCT` ranks below the rest, since it cannot absorb work now.
pub fn priorities(buckets: &[Bucket]) -> BTreeMap<String, i64> {
    let key = |b: &Bucket| {
        let hot = b
            .windows
            .iter()
            .any(|w| w.kind == Kind::H5 && w.state == WinState::Active && w.used_pct > HOT_5H_PCT);
        let reset = b
            .windows
            .iter()
            .find(|w| w.kind == Kind::D7 && w.state == WinState::Active)
            .and_then(|w| w.resets_at);
        (hot, reset.unwrap_or(i64::MAX))
    };
    let mut result = BTreeMap::new();
    for pool in POOLS {
        let mine: Vec<&Bucket> = buckets
            .iter()
            .filter(|b| b.pool == pool && b.status == Status::Available)
            .collect();
        let ranks: BTreeSet<_> = mine.iter().map(|b| key(b)).collect();
        let n = ranks.len() as i64;
        for b in mine {
            let rank = ranks.iter().position(|k| *k == key(b)).unwrap_or_default() as i64;
            result.extend(b.creds.iter().map(|c| (c.clone(), n - rank)));
        }
    }
    result
}

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Enable(String),
    Disable(String),
    Priority(String, i64),
}

/// What the proxy must change so its routing matches the buckets; the observer is the sole writer of `disabled` and `priority`.
///
/// Denied and unclassified credentials stay disabled; exhausted ones get disabled; available ones that we disabled get enabled again; a credential disabled by anyone else is left alone; unknown buckets change nothing.
pub fn proxy_actions(
    buckets: &[Bucket],
    readings: &[Reading],
    denied: &BTreeSet<String>,
    ours: &BTreeSet<String>,
) -> Vec<Act> {
    let ranks = priorities(buckets);
    let mut acts = Vec::new();
    for r in readings {
        let status = buckets
            .iter()
            .find(|b| b.principal == r.principal)
            .map(|b| b.status);
        if denied.contains(&r.name) {
            if !r.disabled {
                acts.push(Act::Disable(r.name.clone()));
            }
            continue;
        }
        match status {
            Some(Status::Exhausted) if !r.disabled => acts.push(Act::Disable(r.name.clone())),
            Some(Status::Available) => {
                if r.disabled && !ours.contains(&r.name) {
                    continue;
                }
                if r.disabled {
                    acts.push(Act::Enable(r.name.clone()));
                }
                if let Some(rank) = ranks.get(&r.name).filter(|rank| r.priority != Some(**rank)) {
                    acts.push(Act::Priority(r.name.clone(), *rank));
                }
            }
            _ => {}
        }
    }
    acts
}

/// The journaled steps of one redemption episode, oldest first.
pub fn journal_steps<'a>(journal: &'a [Value], episode: &str) -> Vec<&'a str> {
    journal
        .iter()
        .filter(|e| e["episode"] == episode)
        .filter_map(|e| e["step"].as_str())
        .collect()
}

/// A bucket's exhaustion, named by the reset that ends it.
pub fn episode(b: &Bucket) -> Option<String> {
    (b.status == Status::Exhausted)
        .then(|| format!("{}@{}", b.principal, b.next_reset.unwrap_or_default()))
}

/// Whether to start redeeming for this bucket: authorized, Codex, exhausted, and nothing journaled for this episode, so no episode is ever redeemed twice.
// 🧑 "Yes" (to redeeming Codex resets automatically as soon as an account runs out)
pub fn should_redeem(journal: &[Value], b: &Bucket, authorized: bool) -> bool {
    authorized
        && b.pool == "codex"
        && episode(b).is_some_and(|ep| journal_steps(journal, &ep).is_empty())
}

/// Steps with no outcome: the observer crashed between them, so the redemption is uncertain and never retried.
pub fn unfinished(journal: &[Value]) -> Vec<String> {
    let episodes: BTreeSet<&str> = journal
        .iter()
        .filter_map(|e| e["episode"].as_str())
        .collect();
    episodes
        .into_iter()
        .filter(|ep| {
            matches!(
                journal_steps(journal, ep).last(),
                Some(&"intent" | &"called")
            )
        })
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Debug)]
pub struct Worker {
    pub address: String,
    pub pool: Option<String>,
    pub priority: i64,
    pub is_manager: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct Gate {
    /// Address to the reason it gets no nudges and no mail.
    pub gated: BTreeMap<String, String>,
    /// Addresses released this pass, highest priority first; each gets one nudge.
    pub resumed: Vec<String>,
}

/// Which workers wait, derived afresh from the pools on every watcher pass.
///
/// An exhausted or unknown pool gates all its workers. A pool projected to run out gates its lowest-priority workers (never managers) until the remaining ones fit, assuming each burns equally. Workers gated last pass that now have room come back by priority, `max_resumes` per pass.
pub fn gate(
    pools: &BTreeMap<String, Pool>,
    workers: &[Worker],
    prev_gated: &BTreeSet<String>,
    max_resumes: usize,
) -> Gate {
    let mut out = Gate::default();
    for pool in POOLS {
        let mut mine: Vec<&Worker> = workers
            .iter()
            .filter(|w| w.pool.as_deref() == Some(pool))
            .collect();
        mine.sort_by_key(|w| std::cmp::Reverse(w.priority));
        let Some(info) = pools.get(pool).filter(|p| p.state == Status::Available) else {
            let why = pools.get(pool).map_or("unknown", |p| p.state.name());
            out.gated.extend(
                mine.iter()
                    .map(|w| (w.address.clone(), format!("{pool} quota is {why}"))),
            );
            continue;
        };
        let running = mine
            .iter()
            .filter(|w| !prev_gated.contains(&w.address))
            .count()
            .max(1) as f64;
        let cap = match &info.estimate {
            Some(e) if e.burn_pct_per_h > 0.0 && e.hours_to_reset > 0.0 => {
                let per_worker = e.burn_pct_per_h / running;
                (e.remaining_pct / (per_worker * e.hours_to_reset))
                    .floor()
                    .max(0.0) as usize
            }
            _ => usize::MAX,
        };
        let mut shed = false;
        let mut kept = 0;
        let mut resumes = 0;
        for w in mine {
            let was_gated = prev_gated.contains(&w.address);
            if w.is_manager {
                kept += 1;
            } else if kept >= cap {
                shed = true;
                out.gated.insert(
                    w.address.clone(),
                    format!("{pool} quota is projected to run out before its reset"),
                );
                continue;
            } else {
                kept += 1;
            }
            if was_gated && !shed {
                if resumes >= max_resumes {
                    out.gated
                        .insert(w.address.clone(), "waiting its turn to resume".to_owned());
                } else {
                    resumes += 1;
                    out.resumed.push(w.address.clone());
                }
            }
        }
    }
    out
}

/// Where a new task goes: the default pool, else the other if the default is closed.
///
/// A pool is closed when it is not available, or projected to run out and the new task would not outrank its lowest-priority running worker.
pub fn choose_tool<'a>(
    default: &'a str,
    priority: i64,
    pools: &BTreeMap<String, Pool>,
    lowest_running: &BTreeMap<String, i64>,
) -> Result<&'a str, String> {
    let open = |pool: &str| {
        pools.get(pool).is_some_and(|p| {
            p.state == Status::Available
                && (p.verdict() != Verdict::RunOut
                    || lowest_running.get(pool).is_none_or(|low| priority > *low))
        })
    };
    if !POOLS.contains(&default) || open(default) {
        return Ok(default);
    }
    match POOLS.iter().find(|p| **p != default && open(p)) {
        Some(other) => Ok(other),
        None => Err(format!(
            "no open quota pool: {default} is closed and so is the other"
        )),
    }
}

impl Bucket {
    pub fn to_json(&self) -> Value {
        let windows: Vec<Value> = self
            .windows
            .iter()
            .map(|w| {
                let state = match w.state {
                    WinState::Active => "active",
                    WinState::NotActive => "not_active",
                    WinState::Invalid => "invalid",
                };
                json!({"kind": if w.kind == Kind::H5 { "5h" } else { "7d" }, "state": state, "used_pct": w.used_pct, "resets_at": w.resets_at})
            })
            .collect();
        json!({"principal": self.principal, "pool": self.pool, "creds": self.creds, "windows": windows,
            "observed_at": self.observed_at, "status": self.status.name(), "since": self.since, "streak": self.streak,
            "next_reset": self.next_reset, "history": self.history})
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let windows = v["windows"]
            .as_array()?
            .iter()
            .map(|w| Window {
                kind: if w["kind"] == "5h" {
                    Kind::H5
                } else {
                    Kind::D7
                },
                state: match w["state"].as_str() {
                    Some("active") => WinState::Active,
                    Some("not_active") => WinState::NotActive,
                    _ => WinState::Invalid,
                },
                used_pct: w["used_pct"].as_f64().unwrap_or_default(),
                resets_at: w["resets_at"].as_i64(),
            })
            .collect();
        Some(Self {
            principal: v["principal"].as_str()?.to_owned(),
            pool: v["pool"].as_str()?.to_owned(),
            creds: v["creds"]
                .as_array()?
                .iter()
                .filter_map(|c| c.as_str().map(str::to_owned))
                .collect(),
            windows,
            observed_at: v["observed_at"].as_i64(),
            status: Status::parse(v["status"].as_str().unwrap_or_default()),
            since: v["since"].as_i64().unwrap_or_default(),
            streak: v["streak"].as_u64().unwrap_or_default() as u32,
            next_reset: v["next_reset"].as_i64(),
            history: v["history"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p[0].as_i64()?, p[1].as_f64()?)))
                .collect(),
        })
    }
}

impl Pool {
    pub fn to_json(&self) -> Value {
        let estimate = self.estimate.as_ref().map(|e| {
            json!({"remaining_pct": e.remaining_pct, "burn_pct_per_h": e.burn_pct_per_h,
                "hours_to_reset": e.hours_to_reset, "projected_pct": e.projected_pct})
        });
        let verdict = match self.verdict() {
            Verdict::RunOut => "run_out",
            Verdict::Fits => "fits",
            Verdict::Waste => "waste",
        };
        json!({"state": self.state.name(), "since": self.since, "next_reset": self.next_reset, "n_buckets": self.n_buckets,
            "estimate": estimate, "verdict": verdict})
    }

    pub fn from_json(v: &Value) -> Self {
        let estimate = v["estimate"].is_object().then(|| Estimate {
            remaining_pct: v["estimate"]["remaining_pct"].as_f64().unwrap_or_default(),
            burn_pct_per_h: v["estimate"]["burn_pct_per_h"].as_f64().unwrap_or_default(),
            hours_to_reset: v["estimate"]["hours_to_reset"].as_f64().unwrap_or_default(),
            projected_pct: v["estimate"]["projected_pct"].as_f64().unwrap_or_default(),
        });
        Self {
            state: Status::parse(v["state"].as_str().unwrap_or_default()),
            since: v["since"].as_i64().unwrap_or_default(),
            next_reset: v["next_reset"].as_i64(),
            n_buckets: v["n_buckets"].as_u64().unwrap_or_default() as usize,
            estimate,
        }
    }
}

/// What the observer publishes and everything else reads.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub generated_at: i64,
    pub live: bool,
    pub buckets: Vec<Bucket>,
    pub pools: BTreeMap<String, Pool>,
    /// Credentials ever seen; the first run grandfathers them.
    pub seen: BTreeSet<String>,
    /// Credentials added later that no config classified yet; they stay disabled.
    pub unclassified: BTreeSet<String>,
    /// Credentials the observer disabled, which it alone may enable again.
    pub ours: BTreeSet<String>,
    /// Per pool, when `quota_appeared` last happened.
    pub appeared_at: BTreeMap<String, i64>,
}

impl Snapshot {
    pub fn to_json(&self) -> Value {
        json!({"version": 1, "generated_at": self.generated_at, "live": self.live,
            "buckets": self.buckets.iter().map(Bucket::to_json).collect::<Vec<_>>(),
            "pools": self.pools.iter().map(|(k, p)| (k.clone(), p.to_json())).collect::<serde_json::Map<_, _>>(),
            "seen": self.seen, "unclassified": self.unclassified, "ours": self.ours, "appeared_at": self.appeared_at})
    }

    pub fn from_json(v: &Value) -> Self {
        let set = |key: &str| -> BTreeSet<String> {
            v[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        };
        Self {
            generated_at: v["generated_at"].as_i64().unwrap_or_default(),
            live: v["live"].as_bool().unwrap_or_default(),
            buckets: v["buckets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Bucket::from_json)
                .collect(),
            pools: v["pools"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, p)| (k.clone(), Pool::from_json(p)))
                .collect(),
            seen: set("seen"),
            unclassified: set("unclassified"),
            ours: set("ours"),
            appeared_at: v["appeared_at"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, t)| Some((k.clone(), t.as_i64()?)))
                .collect(),
        }
    }

    /// The pools as amh may trust them: an old snapshot means a dead observer, so every pool is unknown.
    pub fn pools_at(&self, now: i64) -> BTreeMap<String, Pool> {
        if (0..=TTL_S).contains(&(now - self.generated_at)) {
            return self.pools.clone();
        }
        let stale = Pool {
            state: Status::Unknown,
            since: self.generated_at,
            next_reset: None,
            n_buckets: 0,
            estimate: None,
        };
        POOLS
            .iter()
            .map(|p| (p.to_string(), stale.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_000_000;

    fn claude_file(name: &str, h5: Option<&str>, d7: &str, age_s: i64) -> Value {
        let at = DateTime::from_timestamp(NOW - age_s, 0)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        let mut signals = json!({
            "Anthropic-Ratelimit-Unified-7d-Utilization": d7,
            "Anthropic-Ratelimit-Unified-7d-Reset": (NOW + 86400).to_string(),
            "Anthropic-Ratelimit-Unified-7d-Status": "allowed",
        });
        if let Some(h5) = h5 {
            signals["Anthropic-Ratelimit-Unified-5h-Utilization"] = json!(h5);
            signals["Anthropic-Ratelimit-Unified-5h-Reset"] = json!((NOW + 3600).to_string());
        }
        json!({"name": name, "provider": "claude", "email": name, "auth_index": name, "disabled": false,
            "quota": {"observed_at": at, "signals": signals}})
    }

    fn codex_file(name: &str, primary_pct: &str, minutes: &str, cooldown: Option<i64>) -> Value {
        let at = DateTime::from_timestamp(NOW - 30, 0)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        let cooldowns: Vec<Value> = cooldown
            .map(|u| {
                json!({"scope": "credential", "reason": "credential_quota",
                "retry_at": DateTime::from_timestamp(u, 0).map(|t| t.to_rfc3339())})
            })
            .into_iter()
            .collect();
        json!({"name": name, "provider": "codex", "email": name, "auth_index": name, "disabled": false,
            "id_token": {"chatgpt_account_id": format!("acct-{name}")}, "cooldowns": cooldowns,
            "model_quotas": {"m": {"observed_at": at, "signals": {
                "X-Codex-Primary-Used-Percent": primary_pct, "X-Codex-Primary-Window-Minutes": minutes,
                "X-Codex-Primary-Reset-At": (NOW + 7200).to_string(),
                "X-Codex-Secondary-Used-Percent": "0", "X-Codex-Secondary-Window-Minutes": "0"}}}})
    }

    fn read(files: Vec<Value>) -> Vec<Reading> {
        parse_auth_files(&json!({"files": files}), NOW)
    }

    fn run(prev: &[Bucket], files: Vec<Value>, now: i64) -> (Vec<Bucket>, Vec<Event>) {
        step(prev, &read(files), now)
    }

    #[test]
    fn parses_null_five_hour_as_not_active_and_garbage_as_invalid() {
        let r = read(vec![
            claude_file("a", None, "0.58", 10),
            claude_file("b", Some("oops"), "0.1", 10),
            claude_file("c", Some("1.7"), "0.1", 10),
        ]);
        assert_eq!(r[0].windows[0].state, WinState::NotActive);
        assert_eq!(r[0].windows[1].used_pct, 58.0);
        assert_eq!(r[1].windows[0].state, WinState::Invalid);
        assert_eq!(r[2].windows[0].state, WinState::Invalid);
    }

    #[test]
    fn parses_codex_windows_by_length_and_absent_zero_window() {
        let r = read(vec![
            codex_file("x", "47", "10080", None),
            codex_file("y", "100", "300", None),
        ]);
        assert_eq!(r[0].windows.len(), 1);
        assert_eq!(r[0].windows[0].kind, Kind::D7);
        assert_eq!(r[1].windows[0].kind, Kind::H5);
        assert_eq!(r[1].principal, "codex:acct-y");
    }

    #[test]
    fn malformed_and_empty_signals_are_unknown_never_available() {
        let empty =
            json!({"name": "e", "provider": "claude", "email": "e", "quota": {"signals": {}}});
        let mut b = Vec::new();
        for i in 0..5 {
            (b, _) = run(
                &b,
                vec![empty.clone(), claude_file("m", Some("zzz"), "0.1", 10)],
                NOW + i,
            );
            assert!(b.iter().all(|b| b.status == Status::Unknown));
        }
    }

    #[test]
    fn stale_reading_is_unknown_after_ttl_not_exhausted() {
        let (b, _) = run(
            &[],
            vec![claude_file("a", Some("1.0"), "0.2", TTL_S + 1)],
            NOW,
        );
        assert_eq!(b[0].status, Status::Unknown);
    }

    #[test]
    fn available_needs_two_consecutive_ok_readings() {
        let (b1, e1) = run(&[], vec![claude_file("a", Some("0.1"), "0.2", 10)], NOW);
        assert_eq!(b1[0].status, Status::Unknown);
        let (b2, e2) = run(
            &b1,
            vec![claude_file("a", Some("0.1"), "0.2", 10)],
            NOW + 300,
        );
        assert_eq!(b2[0].status, Status::Available);
        assert!(e1.is_empty());
        assert_eq!(
            e2,
            vec![Event {
                kind: "quota_appeared",
                subject: "claude claude:a".into()
            }]
        );
    }

    #[test]
    fn borderline_99_keeps_previous_status_and_resets_streak() {
        let ok = || claude_file("a", Some("0.5"), "0.2", 10);
        let (b, _) = run(&[], vec![ok()], NOW);
        let (b, _) = run(&b, vec![ok()], NOW + 1);
        let (b, e) = run(
            &b,
            vec![claude_file("a", Some("0.995"), "0.2", 10)],
            NOW + 2,
        );
        assert_eq!(
            (b[0].status, b[0].streak, e.len()),
            (Status::Available, 0, 0)
        );
    }

    #[test]
    fn full_window_is_exhausted_and_recovers_only_after_two_fresh_ok_readings() {
        let ok = |at: i64| claude_file("a", Some("0.5"), "0.2", -at);
        let (b, _) = run(&[], vec![ok(0)], NOW);
        let (b, _) = run(&b, vec![ok(1)], NOW + 1);
        let (b, e) = run(&b, vec![claude_file("a", Some("1.0"), "0.2", 10)], NOW + 2);
        assert_eq!(b[0].status, Status::Exhausted);
        assert_eq!(e[0].kind, "quota_ran_out");
        let (b, e) = run(&b, vec![ok(3)], NOW + 3);
        assert_eq!(b[0].status, Status::Exhausted);
        assert!(e.is_empty());
        let (b, e) = run(&b, vec![ok(4)], NOW + 4);
        assert_eq!(
            (b[0].status, e[0].kind),
            (Status::Available, "quota_appeared")
        );
    }

    #[test]
    fn reading_older_than_the_exhaustion_does_not_recover_it() {
        let (b, _) = run(&[], vec![codex_file("x", "100", "300", None)], NOW);
        assert_eq!(b[0].status, Status::Exhausted);
        let mut old = codex_file("x", "10", "300", None);
        old["model_quotas"]["m"]["observed_at"] =
            json!(DateTime::from_timestamp(NOW - 100, 0).map(|t| t.to_rfc3339()));
        let mut b = b;
        for i in 1..4 {
            (b, _) = run(&b, vec![old.clone()], NOW + i);
        }
        assert_eq!(b[0].status, Status::Exhausted);
    }

    #[test]
    fn proxy_quota_cooldown_is_exhaustion_evidence_even_without_signals() {
        let (b, _) = run(
            &[],
            vec![codex_file("x", "10", "300", Some(NOW + 600))],
            NOW,
        );
        assert_eq!(
            (b[0].status, b[0].next_reset),
            (Status::Exhausted, Some(NOW + 600))
        );
    }

    #[test]
    fn rate_limit_429_storm_does_not_exhaust() {
        let mut f = claude_file("a", Some("0.5"), "0.2", 10);
        f["cooldowns"] = json!([{"scope": "credential", "reason": "rate_limit", "retry_at": "2099-01-01T00:00:00Z"}]);
        f["failed"] = json!(57000);
        f["status"] = json!("error");
        f["status_message"] = json!("429 Too Many Requests");
        let (b, _) = run(&[], vec![f.clone()], NOW);
        let (b, _) = run(&b, vec![f], NOW + 1);
        assert_eq!(b[0].status, Status::Available);
    }

    #[test]
    fn status_401_is_unknown() {
        let mut f = claude_file("a", Some("0.1"), "0.1", 10);
        f["status_message"] = json!("401 unauthorized");
        let (b, _) = run(&[], vec![f], NOW);
        assert_eq!(b[0].status, Status::Unknown);
    }

    #[test]
    fn credentials_of_one_principal_share_one_bucket() {
        let mut a = codex_file("x", "10", "300", None);
        let mut b = a.clone();
        a["name"] = json!("one");
        b["name"] = json!("two");
        let (buckets, _) = run(&[], vec![a, b], NOW);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].creds.len(), 2);
    }

    fn bucket(name: &str, pool: &str, status: Status, h5: f64, d7_reset: Option<i64>) -> Bucket {
        let mut windows = vec![Window {
            kind: Kind::H5,
            state: WinState::Active,
            used_pct: h5,
            resets_at: Some(NOW + 100),
        }];
        if let Some(r) = d7_reset {
            windows.push(Window {
                kind: Kind::D7,
                state: WinState::Active,
                used_pct: 30.0,
                resets_at: Some(r),
            });
        }
        Bucket {
            principal: format!("{pool}:{name}"),
            pool: pool.into(),
            creds: vec![name.into()],
            windows,
            observed_at: Some(NOW),
            status,
            since: NOW,
            streak: 2,
            next_reset: d7_reset,
            history: vec![],
        }
    }

    #[test]
    fn priority_prefers_soonest_weekly_reset_and_enumerated_counterexamples() {
        let p = priorities(&[
            bucket("late", "claude", Status::Available, 10.0, Some(NOW + 9000)),
            bucket("soon", "claude", Status::Available, 10.0, Some(NOW + 1000)),
            bucket("tie", "claude", Status::Available, 10.0, Some(NOW + 1000)),
            bucket("hot", "claude", Status::Available, 95.0, Some(NOW + 10)),
            bucket("no7d", "claude", Status::Available, 10.0, None),
            bucket("gone", "claude", Status::Exhausted, 10.0, Some(NOW + 1)),
            bucket("other", "codex", Status::Available, 10.0, Some(NOW + 5000)),
        ]);
        assert_eq!(p["soon"], p["tie"]);
        assert!(
            p["soon"] > p["late"] && p["late"] > p["no7d"],
            "soonest 7d first, unknown 7d last"
        );
        assert!(
            p["no7d"] > p["hot"],
            "a hot 5h window ranks below even an unknown weekly reset"
        );
        assert!(!p.contains_key("gone"), "exhausted buckets get no priority");
        assert_eq!(p["other"], 1, "pools rank separately");
        assert_eq!(p["hot"], 1);
    }

    fn reading(name: &str, principal: &str, disabled: bool, priority: Option<i64>) -> Reading {
        Reading {
            name: name.into(),
            auth_index: String::new(),
            pool: "claude".into(),
            principal: principal.into(),
            account_id: String::new(),
            disabled,
            priority,
            observed_at: Some(NOW),
            windows: vec![],
            quota_until: None,
            auth_error: false,
        }
    }

    #[test]
    fn proxy_actions_disable_exhausted_enable_ours_and_leave_the_humans_alone() {
        let buckets = [
            bucket("ok", "claude", Status::Available, 10.0, Some(NOW + 1)),
            bucket("out", "claude", Status::Exhausted, 100.0, Some(NOW + 1)),
            bucket("back", "claude", Status::Available, 10.0, Some(NOW + 50)),
            bucket("human", "claude", Status::Available, 10.0, Some(NOW + 60)),
            bucket("unk", "claude", Status::Unknown, 10.0, None),
        ];
        let readings = [
            reading("ok", "claude:ok", false, Some(3)),
            reading("out", "claude:out", false, None),
            reading("back", "claude:back", true, None),
            reading("human", "claude:human", true, None),
            reading("unk", "claude:unk", false, Some(7)),
            reading("new", "claude:new", false, None),
        ];
        let denied = BTreeSet::from(["new".to_owned()]);
        let ours = BTreeSet::from(["back".to_owned()]);
        let acts = proxy_actions(&buckets, &readings, &denied, &ours);
        assert!(acts.contains(&Act::Disable("out".into())));
        assert!(acts.contains(&Act::Enable("back".into())));
        assert!(acts.contains(&Act::Disable("new".into())));
        assert!(
            !acts
                .iter()
                .any(|a| matches!(a, Act::Priority(n, _) if n == "ok")),
            "already at its rank"
        );
        assert!(acts.contains(&Act::Priority("back".into(), 2)));
        assert!(
            !acts
                .iter()
                .any(|a| matches!(a, Act::Enable(n) if n == "human"))
        );
        assert!(
            !acts
                .iter()
                .any(|a| matches!(a, Act::Disable(n) | Act::Enable(n) if n == "unk"))
        );
    }

    #[test]
    fn proxy_actions_are_idempotent_once_applied() {
        let buckets = [bucket(
            "a",
            "claude",
            Status::Available,
            10.0,
            Some(NOW + 1),
        )];
        let ranks = priorities(&buckets);
        let readings = [reading("a", "claude:a", false, Some(ranks["a"]))];
        assert!(proxy_actions(&buckets, &readings, &BTreeSet::new(), &BTreeSet::new()).is_empty());
    }

    fn pool(state: Status, estimate: Option<Estimate>) -> Pool {
        Pool {
            state,
            since: NOW,
            next_reset: Some(NOW + 3600),
            n_buckets: 1,
            estimate,
        }
    }

    fn est(remaining: f64, burn: f64, hours: f64) -> Estimate {
        Estimate {
            remaining_pct: remaining,
            burn_pct_per_h: burn,
            hours_to_reset: hours,
            projected_pct: remaining - burn * hours,
        }
    }

    #[test]
    fn burn_ignores_resets_and_short_history() {
        assert_eq!(burn_pct_per_h(&[(0, 10.0), (300, 12.0)]), 0.0);
        let h = [(0, 10.0), (1200, 20.0), (2400, 5.0), (3600, 15.0)];
        assert!((burn_pct_per_h(&h) - 20.0).abs() < 1e-9);
    }

    #[test]
    fn estimator_verdicts() {
        let at = |e| pool(Status::Available, Some(e)).verdict();
        assert_eq!(at(est(50.0, 10.0, 6.0)), Verdict::RunOut);
        assert_eq!(at(est(50.0, 1.0, 10.0)), Verdict::Waste);
        assert_eq!(at(est(50.0, 6.0, 8.0)), Verdict::Fits);
    }

    fn w(address: &str, pool: &str, priority: i64) -> Worker {
        Worker {
            address: address.into(),
            pool: Some(pool.into()),
            priority,
            is_manager: false,
        }
    }

    #[test]
    fn exhausted_or_unknown_pool_gates_all_its_workers_only() {
        let pools = BTreeMap::from([
            ("codex".to_owned(), pool(Status::Exhausted, None)),
            ("claude".to_owned(), pool(Status::Available, None)),
        ]);
        let g = gate(
            &pools,
            &[
                w("a", "codex", 1),
                w("b", "claude", 1),
                Worker {
                    pool: None,
                    ..w("c", "x", 1)
                },
            ],
            &BTreeSet::new(),
            2,
        );
        assert_eq!(g.gated.keys().collect::<Vec<_>>(), vec!["a"]);
        let g = gate(
            &BTreeMap::new(),
            &[w("b", "claude", 1)],
            &BTreeSet::new(),
            2,
        );
        assert!(
            g.gated.contains_key("b"),
            "a missing pool is unknown, never open"
        );
    }

    #[test]
    fn run_out_sheds_lowest_priority_workers_but_not_managers() {
        // 5 sessions burn 8 %/h together, 10 % left, 2 h to reset: only 3 sessions fit, so the lowest-priority worker waits.
        let pools = BTreeMap::from([(
            "codex".to_owned(),
            pool(Status::Available, Some(est(10.0, 8.0, 2.0))),
        )]);
        let mut ws = vec![
            w("p1", "codex", 1),
            w("p5", "codex", 5),
            w("p3", "codex", 3),
            w("p9", "codex", 9),
        ];
        ws.push(Worker {
            is_manager: true,
            ..w("m", "codex", -100)
        });
        let g = gate(&pools, &ws, &BTreeSet::new(), 2);
        assert!(!g.gated.contains_key("m"));
        assert_eq!(g.gated.keys().collect::<Vec<_>>(), vec!["p1"]);
    }

    #[test]
    fn resume_is_by_priority_two_per_pass_and_ungated_workers_are_never_nudged() {
        let pools = BTreeMap::from([("claude".to_owned(), pool(Status::Available, None))]);
        let ws = vec![
            w("a", "claude", 1),
            w("b", "claude", 9),
            w("c", "claude", 5),
            w("fresh", "claude", 0),
        ];
        let prev: BTreeSet<String> = ["a", "b", "c"].map(String::from).into();
        let g = gate(&pools, &ws, &prev, 2);
        assert_eq!(g.resumed, vec!["b", "c"]);
        assert_eq!(g.gated.keys().collect::<Vec<_>>(), vec!["a"]);
        let g2 = gate(&pools, &ws, &g.gated.keys().cloned().collect(), 2);
        assert_eq!(g2.resumed, vec!["a"]);
        assert!(g2.gated.is_empty());
    }

    #[test]
    fn claude_bucket_resets_then_paused_workers_resume_once() {
        let (b, _) = run(&[], vec![claude_file("a", Some("1.0"), "0.2", 10)], NOW);
        let ws = vec![w("s1", "claude", 1)];
        let g = gate(&pools(&b, NOW), &ws, &BTreeSet::new(), 2);
        let prev: BTreeSet<String> = g.gated.keys().cloned().collect();
        assert_eq!(prev.len(), 1);
        let ok = |at: i64| claude_file("a", Some("0.1"), "0.2", -at);
        let (b, _) = run(&b, vec![ok(1)], NOW + 1);
        let (b, e) = run(&b, vec![ok(2)], NOW + 2);
        assert_eq!(e[0].kind, "quota_appeared");
        let g = gate(&pools(&b, NOW + 2), &ws, &prev, 2);
        assert_eq!(g.resumed, vec!["s1"]);
        let g = gate(&pools(&b, NOW + 3), &ws, &BTreeSet::new(), 2);
        assert!(
            g.resumed.is_empty() && g.gated.is_empty(),
            "nudged once only"
        );
    }

    #[test]
    fn tool_choice_follows_the_default_unless_closed() {
        let open = pool(Status::Available, None);
        let shut = pool(Status::Exhausted, None);
        let run_out = pool(Status::Available, Some(est(5.0, 5.0, 5.0)));
        let ps = |c: &Pool, x: &Pool| {
            BTreeMap::from([
                ("codex".to_owned(), c.clone()),
                ("claude".to_owned(), x.clone()),
            ])
        };
        let low = BTreeMap::new();
        assert_eq!(
            choose_tool("codex", 0, &ps(&open, &open), &low),
            Ok("codex")
        );
        assert_eq!(
            choose_tool("codex", 0, &ps(&shut, &open), &low),
            Ok("claude")
        );
        assert!(choose_tool("codex", 0, &ps(&shut, &shut), &low).is_err());
        assert_eq!(
            choose_tool(
                "codex",
                0,
                &ps(&run_out, &open),
                &BTreeMap::from([("codex".to_owned(), 3)])
            ),
            Ok("claude")
        );
        assert_eq!(
            choose_tool(
                "codex",
                9,
                &ps(&run_out, &open),
                &BTreeMap::from([("codex".to_owned(), 3)])
            ),
            Ok("codex")
        );
        assert_eq!(
            choose_tool("cursor", 0, &ps(&shut, &shut), &low),
            Ok("cursor")
        );
    }

    fn entry(ep: &str, step: &str) -> Value {
        json!({"episode": ep, "step": step})
    }

    #[test]
    fn redemption_requires_authorization_codex_and_a_fresh_episode() {
        let b = bucket("x", "codex", Status::Exhausted, 100.0, Some(NOW + 5));
        let ep = episode(&b).unwrap_or_default();
        assert!(should_redeem(&[], &b, true));
        assert!(!should_redeem(&[], &b, false));
        assert!(!should_redeem(
            &[],
            &bucket("x", "claude", Status::Exhausted, 100.0, Some(NOW + 5)),
            true
        ));
        assert!(!should_redeem(
            &[],
            &bucket("x", "codex", Status::Available, 1.0, Some(NOW + 5)),
            true
        ));
        for step in ["intent", "called", "uncertain", "confirmed", "no_credit"] {
            assert!(!should_redeem(&[entry(&ep, step)], &b, true), "{step}");
        }
        let later = bucket("x", "codex", Status::Exhausted, 100.0, Some(NOW + 700_000));
        assert!(
            should_redeem(&[entry(&ep, "confirmed")], &later, true),
            "a new episode may redeem"
        );
    }

    #[test]
    fn crash_between_any_two_steps_is_found_and_never_retried() {
        let b = bucket("x", "codex", Status::Exhausted, 100.0, Some(NOW + 5));
        let ep = episode(&b).unwrap_or_default();
        for path in [vec!["intent"], vec!["intent", "called"]] {
            let journal: Vec<Value> = path.iter().map(|s| entry(&ep, s)).collect();
            assert_eq!(unfinished(&journal), vec![ep.clone()]);
            assert!(!should_redeem(&journal, &b, true));
        }
        let done: Vec<Value> = ["intent", "called", "confirmed"]
            .iter()
            .map(|s| entry(&ep, s))
            .collect();
        assert!(unfinished(&done).is_empty());
    }

    #[test]
    fn random_inputs_never_make_unknown_available_or_gate_a_foreign_tool() {
        let mut seed = 12345u64;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut buckets = Vec::new();
        for i in 0..400 {
            let age = [10, TTL_S + 5, 100_000][next(3) as usize];
            let h5 = ["0.0", "0.5", "0.995", "1.0", "x"][next(5) as usize];
            let file = claude_file("p", Some(h5), ["0.1", "1.0", "bad"][next(3) as usize], age);
            let (b, events) = run(&buckets, vec![file], NOW + i);
            for bucket in &b {
                if bucket.status == Status::Available {
                    let fresh = bucket.observed_at.is_some_and(|at| NOW + i - at <= TTL_S);
                    assert!(fresh && bucket.windows.iter().all(|w| w.state != WinState::Invalid));
                }
            }
            assert!(
                events.iter().all(|e| e.kind != "quota_appeared")
                    || b[0].status == Status::Available
            );
            buckets = b;
        }
        let pools = BTreeMap::new();
        let ws = vec![Worker {
            pool: None,
            ..w("cursor", "x", 0)
        }];
        assert!(gate(&pools, &ws, &BTreeSet::new(), 2).gated.is_empty());
    }

    #[test]
    fn clock_jump_back_and_a_past_reset_hint_make_readings_unknown_not_available() {
        let (b, _) = run(
            &[],
            vec![claude_file("a", Some("0.1"), "0.1", 10)],
            NOW - 10_000,
        );
        assert_eq!(b[0].status, Status::Unknown);
        let mut f = claude_file("a", Some("1.0"), "0.1", 10);
        f["quota"]["signals"]["Anthropic-Ratelimit-Unified-5h-Reset"] =
            json!((NOW - 5).to_string());
        let (b, _) = run(&[], vec![f], NOW);
        assert_eq!(
            b[0].status,
            Status::Unknown,
            "a full window whose reset passed needs a new poll"
        );
    }

    #[test]
    fn dead_observer_makes_every_pool_unknown() {
        let (b, _) = run(&[], vec![codex_file("x", "10", "300", None)], NOW);
        let s = Snapshot {
            generated_at: NOW,
            buckets: b.clone(),
            pools: pools(&b, NOW),
            ..Snapshot::default()
        };
        let back = Snapshot::from_json(&s.to_json());
        assert_eq!(back.pools_at(NOW + 10).len(), 2);
        assert!(
            back.pools_at(NOW + TTL_S + 1)
                .values()
                .all(|p| p.state == Status::Unknown)
        );
    }

    /// Replay of the 2026-11-04 Midas incident: the account was disabled at 7d 100%, and came back after its reset.
    #[test]
    fn midas_is_disabled_when_out_and_enabled_again_after_its_reset() {
        let out = |at: i64| claude_file("midas", None, "1.0", -at);
        let (b, _) = run(&[], vec![out(0)], NOW);
        let r = read(vec![out(0)]);
        let acts = proxy_actions(&b, &r, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(acts, vec![Act::Disable("midas".into())]);
        let ours = BTreeSet::from(["midas".to_owned()]);
        let mut disabled = read(vec![out(0)]);
        disabled[0].disabled = true;
        assert!(
            proxy_actions(&b, &disabled, &BTreeSet::new(), &ours).is_empty(),
            "still out: stays disabled"
        );
        let back = |at: i64| claude_file("midas", None, "0.0", -at);
        let (b, _) = run(&b, vec![back(1)], NOW + 1);
        assert!(
            proxy_actions(&b, &disabled, &BTreeSet::new(), &ours).is_empty(),
            "one reading is not enough"
        );
        let (b, e) = run(&b, vec![back(2)], NOW + 2);
        assert_eq!(e[0].kind, "quota_appeared");
        let acts = proxy_actions(&b, &disabled, &BTreeSet::new(), &ours);
        assert!(acts.contains(&Act::Enable("midas".into())));
    }

    #[test]
    fn json_round_trip() {
        let (b, _) = run(&[], vec![codex_file("x", "10", "300", None)], NOW);
        let back = Bucket::from_json(&b[0].to_json());
        assert_eq!(back.map(|b| b.to_json()), Some(b[0].to_json()));
        let p = pools(&b, NOW);
        assert_eq!(
            Pool::from_json(&p["codex"].to_json()).to_json(),
            p["codex"].to_json()
        );
    }
}
