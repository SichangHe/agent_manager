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

#[derive(Clone, Debug, PartialEq)]
pub struct KeyUse {
    pub name: String,
    /// Position in the proxy's `claude-api-key` list, which is how the management API addresses a key.
    pub index: usize,
    pub auth_index: String,
    pub priority: i64,
    /// Tokens used this UTC month (`total` of the usage store); the unit of `limit`.
    pub used: f64,
    pub burn_per_h: f64,
    /// The monthly limit, from `quota.apikey.<name>.monthly_tokens`: neither the proxy nor the Anthropic API exposes it.
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

    /// Tokens per hour that use the key up exactly at the month end.
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
            "used": self.used, "burn_per_h": self.burn_per_h, "limit": self.limit, "resets_at": self.resets_at})
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            name: v["name"].as_str()?.to_owned(),
            index: v["index"].as_u64()? as usize,
            auth_index: v["auth_index"].as_str().unwrap_or_default().to_owned(),
            priority: v["priority"].as_i64()?,
            used: v["used"].as_f64()?,
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

/// Tokens each `auth_index` used in `[since, now]`. The store holds RFC 3339 `ts` with a local offset, so the index on `ts` narrows by day and `strftime` decides.
fn tokens_since(db: &Connection, auth_index: &str, since: i64) -> Result<f64, String> {
    db.query_row(
        "SELECT COALESCE(SUM(total), 0) FROM req
         WHERE ts >= date(?1, 'unixepoch', '-1 day') AND CAST(strftime('%s', ts) AS INTEGER) >= ?1
           AND json_extract(raw, '$.auth_index') = ?2",
        params![since, auth_index],
        |row| row.get(0),
    )
    .map_err(|e| format!("proxy usage store: {e}"))
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
            let limit = settings::get(settings, &format!("quota.apikey.{name}.monthly_tokens"));
            Ok(KeyUse {
                name: (*name).to_owned(),
                index,
                auth_index: auth_index.clone(),
                priority: *priority,
                used: tokens_since(&db, auth_index, month_start(now))?,
                burn_per_h: tokens_since(&db, auth_index, now - BURN_H * 3600)? / BURN_H as f64,
                limit: limit.parse().ok(),
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
            let quota = match (k.limit, k.need_per_h(now)) {
                (Some(limit), Some(need)) => format!(
                    "{:.0} of {limit:.0} tokens used ({:.0}%), need {need:.0} tokens/h to use it up, burning {:.0}/h ({} pace)",
                    k.used,
                    100.0 * k.used / limit,
                    k.burn_per_h,
                    if k.burn_per_h < need { "under" } else { "on" }
                ),
                _ => format!(
                    "{:.0} tokens used, limit unknown (set `quota.apikey.{}.monthly_tokens`), burning {:.0}/h",
                    k.used, k.name, k.burn_per_h
                ),
            };
            format!(
                "api key {}: {quota}; monthly, resets in {}; priority {}\n",
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
    fn reads_tokens_per_auth_index_this_month() -> Result<(), Box<dyn std::error::Error>> {
        let path =
            std::env::temp_dir().join(format!("amh-apikey-test-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Connection::open(&path)?;
        db.execute_batch(
            "CREATE TABLE req(ts TEXT, total INT, raw TEXT); CREATE INDEX req_ts ON req(ts);",
        )?;
        for (ts, total, auth) in [
            ("2026-11-20T23:30:00.5-08:00", 10, "k1"), // 07:30 UTC on the 21st: this month
            ("2026-10-31T16:30:00-08:00", 99, "k1"),   // 00:30 UTC on Nov 1: this month
            ("2026-10-31T16:30:00-07:00", 77, "k1"),   // 23:30 UTC on Oct 31: last month
            ("2026-11-20T23:30:00-08:00", 5, "k2"),
        ] {
            db.execute(
                "INSERT INTO req VALUES (?1, ?2, ?3)",
                params![ts, total, json!({"auth_index": auth}).to_string()],
            )?;
        }
        let start = Utc
            .with_ymd_and_hms(2026, 11, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(tokens_since(&db, "k1", start)?, 109.0);
        std::fs::remove_file(&path)?;
        Ok(())
    }
}
