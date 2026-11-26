//! Monthly quota of the Claude API keys in the proxy config: use read from the proxy usage store, reset at the end of the UTC month, and a priority that holds the keys back until late in the month. See `docs/quota-manager.md`.

use std::path::PathBuf;

use chrono::{Datelike, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};

use crate::settings;

type Settings = std::collections::BTreeMap<String, String>;

/// Priority that keeps a key below every subscription account; set by the proxy agent.
pub const HELD_PRIORITY: i64 = -1;
/// Days before the month end from which the keys outrank the subscriptions, unless `quota.apikey_late_days` says otherwise.
const LATE_DAYS: f64 = 3.0;
/// Burn is measured over this long.
const BURN_H: i64 = 6;

/// USD per million tokens `(model, input, output, cache read)`, from the `claude-api` skill's model tables (`shared/models.md`, `shared/prompt-caching.md`), checked 2026-11-26. Cache writes cost `CACHE_WRITE_5M`/`CACHE_WRITE_1H` times the input price. Read prices of Fable 5 / Mythos 5 (a quarter of the 5.1 price), Opus 5 and earlier, Sonnet 5 / 4.6 and Haiku are the skill's ~0.1x input rule, not a listed price.
const PRICES: &[(&str, f64, f64, f64)] = &[
    ("claude-fable-5-2", 8.0, 40.0, 0.2),
    ("claude-mythos-5-2", 8.0, 40.0, 0.2),
    ("claude-fable-5-1", 10.0, 50.0, 0.25),
    ("claude-mythos-5-1", 10.0, 50.0, 0.25),
    ("claude-fable-5", 10.0, 50.0, 1.0),
    ("claude-mythos-5", 10.0, 50.0, 1.0),
    ("claude-opus-5-5", 4.0, 20.0, 0.2),
    ("claude-opus-5", 5.0, 25.0, 0.5),
    ("claude-opus-4-8", 5.0, 25.0, 0.5),
    ("claude-opus-4-7", 5.0, 25.0, 0.5),
    ("claude-opus-4-6", 5.0, 25.0, 0.5),
    ("claude-sonnet-5-5", 2.0, 10.0, 0.1),
    ("claude-sonnet-5", 2.0, 10.0, 0.2),
    ("claude-sonnet-4-6", 3.0, 15.0, 0.3),
    ("claude-haiku-5-5", 0.1, 0.5, 0.01),
    ("claude-haiku-4-5", 1.0, 5.0, 0.1),
];
const CACHE_WRITE_5M: f64 = 1.25;
const CACHE_WRITE_1H: f64 = 2.0;
/// Fast mode (`speed: "fast"`) is 2x on every model that has it.
const FAST_FACTOR: f64 = 2.0;
/// `claude-haiku-5-5` prompts above this many tokens use a 5x rate card.
const HAIKU_55_LONG_PROMPT: i64 = 100_000;
const HAIKU_55_LONG_FACTOR: f64 = 5.0;

/// One usage row, as far as the store records it: tokens by kind (`output` includes reasoning), cache writes with the 1h part split out of `cache_write`, and `speed`.
struct Req {
    model: String,
    uncached: i64,
    cache_read: i64,
    cache_write: i64,
    cache_write_1h: i64,
    output: i64,
    fast: bool,
}

/// USD of one request, or `None` if the model is not in `PRICES` (a trailing `-YYYYMMDD` is ignored).
fn cost_usd(r: &Req) -> Option<f64> {
    let model = r.model.rsplit_once('-').map_or(&*r.model, |(head, tail)| {
        if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
            head
        } else {
            &r.model
        }
    });
    let &(_, input, output, read) = PRICES.iter().find(|p| p.0 == model)?;
    let long = model == "claude-haiku-5-5"
        && r.uncached + r.cache_read + r.cache_write > HAIKU_55_LONG_PROMPT;
    let factor =
        if long { HAIKU_55_LONG_FACTOR } else { 1.0 } * if r.fast { FAST_FACTOR } else { 1.0 };
    let write_5m = r.cache_write - r.cache_write_1h;
    let usd_micro = r.uncached as f64 * input
        + r.cache_read as f64 * read
        + write_5m as f64 * input * CACHE_WRITE_5M
        + r.cache_write_1h as f64 * input * CACHE_WRITE_1H
        + r.output as f64 * output;
    Some(usd_micro * factor / 1e6)
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyUse {
    pub name: String,
    /// Position in the proxy's `claude-api-key` list, which is how the management API addresses a key.
    pub index: usize,
    pub auth_index: String,
    pub priority: i64,
    /// Used this UTC month, in the unit of `limit`: USD when `usd`, else tokens (`total` of the usage store).
    pub used: f64,
    /// `limit` and `used` are USD (`monthly_usd` is set), not tokens.
    pub usd: bool,
    /// Models seen this month that `PRICES` lacks, with request counts; their spend is missing from `used`.
    pub unpriced: Vec<String>,
    pub burn_per_h: f64,
    /// The monthly limit, from `quota.apikey.<name>.monthly_usd` (preferred) or `.monthly_tokens`: neither the proxy nor the Anthropic API exposes it.
    pub limit: Option<f64>,
    pub resets_at: i64,
}

/// The first instant of the next UTC month.
pub fn month_end(now: i64) -> i64 {
    let at = Utc.timestamp_opt(now, 0).single().unwrap_or_default();
    let (year, month) = if at.month() == 12 {
        (at.year() + 1, 1)
    } else {
        (at.year(), at.month() + 1)
    };
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .map_or(now, |end| end.timestamp())
}

fn month_start(now: i64) -> i64 {
    let at = Utc.timestamp_opt(now, 0).single().unwrap_or_default();
    Utc.with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0)
        .single()
        .map_or(now, |start| start.timestamp())
}

impl KeyUse {
    pub fn left(&self) -> Option<f64> {
        self.limit.map(|l| (l - self.used).max(0.0))
    }

    fn hours_left(&self, now: i64) -> f64 {
        (self.resets_at - now) as f64 / 3600.0
    }

    /// Units (USD or tokens) per hour that use the key up exactly at the month end.
    pub fn need_per_h(&self, now: i64) -> Option<f64> {
        self.left().map(|left| left / self.hours_left(now).max(1.0))
    }

    /// Held at `HELD_PRIORITY` until the last `late_days` of the month, then one above the best subscription rank (`top`) while quota is left, so it is used first and nothing expires unused.
    pub fn want_priority(&self, now: i64, top: i64, late_days: f64) -> i64 {
        let late = self.hours_left(now) <= late_days * 24.0;
        if late && self.left().is_none_or(|left| left > 0.0) {
            top + 1
        } else {
            HELD_PRIORITY
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "index": self.index, "auth_index": self.auth_index, "priority": self.priority,
            "used": self.used, "usd": self.usd, "unpriced": self.unpriced, "burn_per_h": self.burn_per_h, "limit": self.limit, "resets_at": self.resets_at})
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            name: v["name"].as_str()?.to_owned(),
            index: v["index"].as_u64()? as usize,
            auth_index: v["auth_index"].as_str().unwrap_or_default().to_owned(),
            priority: v["priority"].as_i64()?,
            used: v["used"].as_f64()?,
            usd: v["usd"].as_bool().unwrap_or_default(),
            unpriced: v["unpriced"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            burn_per_h: v["burn_per_h"].as_f64().unwrap_or_default(),
            limit: v["limit"].as_f64(),
            resets_at: v["resets_at"].as_i64()?,
        })
    }
}

pub fn late_days(settings: &Settings) -> f64 {
    settings::get(settings, "quota.apikey_late_days")
        .parse()
        .ok()
        .filter(|d: &f64| d.is_finite() && *d >= 0.0)
        .unwrap_or(LATE_DAYS)
}

fn usage_db_path(settings: &Settings) -> PathBuf {
    match settings::get(settings, "quota.usage_db") {
        "" => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".local/state/proxy-usage/usage.sqlite"),
        path => PathBuf::from(path),
    }
}

/// What `auth_index` used in `[since, now]`: USD if `usd` (requests of models missing from `PRICES` count as 0 and are listed in the second value), else tokens. The store holds RFC 3339 `ts` with a local offset, so the index on `ts` narrows by day and `strftime` decides.
fn usage_since(
    db: &Connection,
    auth_index: &str,
    since: i64,
    usd: bool,
) -> Result<(f64, Vec<String>), String> {
    let mut stmt = db
        .prepare(
            "SELECT model, input_uncached, cache_read, cache_write, output, total,
                    COALESCE(json_extract(raw, '$.tokens.cache_creation_1h_tokens'), 0),
                    json_extract(raw, '$.speed') = 'fast'
             FROM req
             WHERE ts >= date(?1, 'unixepoch', '-1 day') AND CAST(strftime('%s', ts) AS INTEGER) >= ?1
               AND json_extract(raw, '$.auth_index') = ?2",
        )
        .map_err(|e| format!("proxy usage store: {e}"))?;
    let rows = stmt
        .query_map(params![since, auth_index], |row| {
            let get = |i| row.get::<_, Option<i64>>(i).map(Option::unwrap_or_default);
            Ok((
                Req {
                    model: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    uncached: get(1)?,
                    cache_read: get(2)?,
                    cache_write: get(3)?,
                    output: get(4)?,
                    cache_write_1h: get(6)?,
                    fast: get(7)? != 0,
                },
                get(5)?,
            ))
        })
        .map_err(|e| format!("proxy usage store: {e}"))?;
    let (mut sum, mut missing) = (0.0, std::collections::BTreeMap::<String, u64>::new());
    for row in rows {
        let (req, total) = row.map_err(|e| format!("proxy usage store: {e}"))?;
        if !usd {
            sum += total as f64;
        } else if let Some(c) = cost_usd(&req) {
            sum += c;
        } else if total > 0 {
            *missing.entry(req.model).or_default() += 1;
        }
    }
    Ok((
        sum,
        missing
            .into_iter()
            .map(|(m, n)| format!("{m} ({n} requests)"))
            .collect(),
    ))
}

/// The keys as the proxy lists them (`(auth_index, priority)` in config order, from `GET claude-api-key`), named by `quota.apikeys` in the same order, with their usage this month.
pub fn read(
    settings: &Settings,
    listed: &[(String, i64)],
    now: i64,
) -> Result<Vec<KeyUse>, String> {
    let names: Vec<&str> = settings::get(settings, "quota.apikeys")
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    if names.len() != listed.len() {
        return Err(format!(
            "`quota.apikeys` names {} keys but the proxy lists {}",
            names.len(),
            listed.len()
        ));
    }
    let path = usage_db_path(settings);
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot open the proxy usage store {}: {e}", path.display()))?;
    names
        .iter()
        .zip(listed)
        .enumerate()
        .map(|(index, (name, (auth_index, priority)))| {
            let get = |unit| {
                settings::get(settings, &format!("quota.apikey.{name}.{unit}"))
                    .parse::<f64>()
                    .ok()
            };
            let usd_limit = get("monthly_usd");
            let usd = usd_limit.is_some();
            let (used, unpriced) = usage_since(&db, auth_index, month_start(now), usd)?;
            Ok(KeyUse {
                name: (*name).to_owned(),
                index,
                auth_index: auth_index.clone(),
                priority: *priority,
                used,
                usd,
                unpriced,
                burn_per_h: usage_since(&db, auth_index, now - BURN_H * 3600, usd)?.0
                    / BURN_H as f64,
                limit: usd_limit.or_else(|| get("monthly_tokens")),
                resets_at: month_end(now),
            })
        })
        .collect()
}

/// `(key name, index, priority)` changes that bring each key's priority to `want_priority`; `top` is the highest subscription rank.
pub fn changes(keys: &[KeyUse], top: i64, now: i64, late_days: f64) -> Vec<(String, usize, i64)> {
    keys.iter()
        .map(|k| (k, k.want_priority(now, top, late_days)))
        .filter(|(k, want)| k.priority != *want)
        .map(|(k, want)| (k.name.clone(), k.index, want))
        .collect()
}

fn span_h(h: f64) -> String {
    if h >= 48.0 {
        format!("{:.1}d", h / 24.0)
    } else {
        format!("{h:.1}h")
    }
}

/// One line per key for `amh quota status` and `amh quota pace`.
pub fn lines(keys: &[KeyUse], now: i64) -> String {
    keys.iter()
        .map(|k| {
            let (unit, per_h, n) = if k.usd { ("USD", "USD/h", 2) } else { ("tokens", "tokens/h", 0) };
            let quota = match (k.limit, k.need_per_h(now)) {
                (Some(limit), Some(need)) => format!(
                    "{:.n$} of {limit:.n$} {unit} used ({:.0}%), need {need:.n$} {per_h} to use it up, burning {:.n$}/h ({} pace)",
                    k.used,
                    100.0 * k.used / limit,
                    k.burn_per_h,
                    if k.burn_per_h < need { "under" } else { "on" }
                ),
                _ => format!(
                    "{used:.n$} {unit} used, limit unknown (set `quota.apikey.{name}.monthly_usd` or `.monthly_tokens`), burning {burn:.n$}/h",
                    used = k.used, name = k.name, burn = k.burn_per_h
                ),
            };
            let unpriced = if k.unpriced.is_empty() {
                String::new()
            } else {
                format!("; UNPRICED, missing from spend: {}", k.unpriced.join(", "))
            };
            format!(
                "api key {}: {quota}; monthly, resets in {}; priority {}{unpriced}\n",
                k.name,
                span_h(k.hours_left(now)),
                k.priority
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(used: f64, limit: Option<f64>, priority: i64) -> KeyUse {
        KeyUse {
            name: "claude-1".into(),
            index: 0,
            auth_index: "a".into(),
            priority,
            used,
            usd: false,
            unpriced: Vec::new(),
            burn_per_h: 0.0,
            limit,
            resets_at: month_end(
                Utc.with_ymd_and_hms(2026, 11, 21, 6, 0, 0)
                    .unwrap()
                    .timestamp(),
            ),
        }
    }

    #[test]
    fn month_end_rolls_over_december() {
        let dec = Utc
            .with_ymd_and_hms(2026, 12, 31, 23, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            month_end(dec),
            Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        );
    }

    #[test]
    fn held_until_late_then_raised_until_used_up() {
        let at = |day| {
            Utc.with_ymd_and_hms(2026, 11, day, 6, 0, 0)
                .unwrap()
                .timestamp()
        };
        let k = key(0.0, Some(100.0), -1);
        assert_eq!(k.want_priority(at(21), 2, 3.0), HELD_PRIORITY);
        assert_eq!(k.want_priority(at(28), 2, 3.0), 3);
        assert_eq!(
            key(100.0, Some(100.0), 3).want_priority(at(28), 2, 3.0),
            HELD_PRIORITY
        );
        assert_eq!(key(0.0, None, -1).want_priority(at(28), 0, 3.0), 1);
        assert!(changes(&[k.clone()], 2, at(21), 3.0).is_empty());
        assert_eq!(
            changes(&[k], 2, at(29), 3.0),
            vec![("claude-1".to_owned(), 0, 3)]
        );
    }

    #[test]
    fn reads_use_per_auth_index_this_month() -> Result<(), Box<dyn std::error::Error>> {
        let path =
            std::env::temp_dir().join(format!("amh-apikey-test-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Connection::open(&path)?;
        db.execute_batch(
            "CREATE TABLE req(ts TEXT, model TEXT, input_uncached INT, cache_read INT, cache_write INT, output INT, total INT, raw TEXT); CREATE INDEX req_ts ON req(ts);",
        )?;
        for (ts, total, auth) in [
            ("2026-11-20T23:30:00.5-08:00", 10, "k1"), // 07:30 UTC on the 21st: this month
            ("2026-10-31T16:30:00-08:00", 99, "k1"),   // 00:30 UTC on Nov 1: this month
            ("2026-10-31T16:30:00-07:00", 77, "k1"),   // 23:30 UTC on Oct 31: last month
            ("2026-11-20T23:30:00-08:00", 5, "k2"),
        ] {
            db.execute(
                "INSERT INTO req (ts, total, raw) VALUES (?1, ?2, ?3)",
                params![ts, total, json!({"auth_index": auth}).to_string()],
            )?;
        }
        let start = Utc
            .with_ymd_and_hms(2026, 11, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(usage_since(&db, "k1", start, false)?.0, 109.0);
        // sonnet-5-5: 1M uncached $2 + 1M read $0.1 + 1M 5m write $2.5 + 1M 1h write $4 + 1M output $10.
        db.execute(
            "INSERT INTO req VALUES ('2026-11-20T23:30:00-08:00', 'claude-sonnet-5-5-20261101', 1000000, 1000000, 2000000, 1000000, 4000000, ?1)",
            params![json!({"auth_index": "k3", "tokens": {"cache_creation_1h_tokens": 1000000}}).to_string()],
        )?;
        db.execute(
            "INSERT INTO req VALUES ('2026-11-20T23:30:00-08:00', 'mystery-1', 1, 1, 1, 1, 4, ?1)",
            params![json!({"auth_index": "k3"}).to_string()],
        )?;
        let (usd, unpriced) = usage_since(&db, "k3", start, true)?;
        assert!((usd - 18.6).abs() < 1e-9, "{usd}");
        assert_eq!(unpriced, ["mystery-1 (1 requests)"]);
        std::fs::remove_file(&path)?;
        Ok(())
    }
}
