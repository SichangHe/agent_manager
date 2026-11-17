//! Quota pace: usage samples per account in SQLite, and whether each provider burns its quota fast enough to use it by each reset without running dry before.
// 🧑 "I think we are using quota only half speed or so to run out? Are you keeping track? How do you keep track, is it scientific? We need to delicately balance this to run out in time too"

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::quota_core::{Bucket, Kind, Status, WinState};

/// The conservative target leaves this much of each account's quota (in %) for the last hours before its reset, where burning faster is always possible.
// 🧑 "We can always burn quota faster near the end if needed, so being a bit conservative is nice"
pub const MARGIN_PCT: f64 = 5.0;
/// Hours of samples the burn rate is fitted over, by default.
pub const WINDOW_H: f64 = 6.0;
/// The shortest stretch of samples a rate is fitted from.
const MIN_SPAN_H: f64 = 0.5;
/// Reset times within this many seconds belong to one quota period (providers move them slightly).
const SAME_RESET_S: i64 = 900;

fn open(state_dir: &Path) -> Result<Connection, String> {
    let db = Connection::open(state_dir.join("quota-usage.sqlite3"))
        .map_err(|e| format!("amh: quota usage store: {e}"))?;
    db.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| format!("amh: quota usage store: {e}"))?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS samples (
             principal TEXT NOT NULL, pool TEXT NOT NULL, account TEXT NOT NULL, t INTEGER NOT NULL,
             status TEXT NOT NULL, h5 REAL, h5_reset INTEGER, d7 REAL, d7_reset INTEGER,
             PRIMARY KEY (principal, t));
         CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(|e| format!("amh: quota usage store: {e}"))?;
    Ok(db)
}

fn window(b: &Bucket, kind: Kind) -> (Option<f64>, Option<i64>) {
    b.windows
        .iter()
        .find(|w| w.kind == kind && w.state == WinState::Active)
        .map_or((None, None), |w| (Some(w.used_pct), w.resets_at))
}

fn insert(db: &Connection, b: &Bucket) -> Result<(), String> {
    let Some(t) = b.observed_at else {
        return Ok(());
    };
    let (h5, h5_reset) = window(b, Kind::H5);
    let (d7, d7_reset) = window(b, Kind::D7);
    if h5.is_none() && d7.is_none() && b.status != Status::Exhausted {
        return Ok(());
    }
    let account = b.creds.first().map_or("?", |c| c.trim_end_matches(".json"));
    db.execute(
        "INSERT OR IGNORE INTO samples VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            b.principal,
            b.pool,
            account,
            t,
            b.status.name(),
            h5,
            h5_reset,
            d7,
            d7_reset
        ],
    )
    .map_err(|e| format!("amh: quota usage store: {e}"))?;
    Ok(())
}

/// Keep one sample per account and proxy reading; the first call also takes in the samples `quota-events.jsonl` already holds.
pub fn record(state_dir: &Path, buckets: &[Bucket]) -> Result<(), String> {
    let mut db = open(state_dir)?;
    let tx = db.transaction().map_err(|e| format!("amh: {e}"))?;
    let backfilled: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM meta WHERE key = 'backfilled')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("amh: {e}"))?;
    if !backfilled {
        let text =
            std::fs::read_to_string(state_dir.join("quota-events.jsonl")).unwrap_or_default();
        for line in text.lines().filter(|l| l.contains("\"sample\"")) {
            if let Some(b) = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| Bucket::from_json(&v["sample"]))
            {
                insert(&tx, &b)?;
            }
        }
        tx.execute("INSERT INTO meta VALUES ('backfilled', '1')", [])
            .map_err(|e| format!("amh: {e}"))?;
    }
    for b in buckets {
        insert(&tx, b)?;
    }
    tx.commit().map_err(|e| format!("amh: {e}"))
}

struct Sample {
    t: i64,
    used: f64,
    reset: Option<i64>,
}

/// Least-squares slope in %/h of `used` over time, with its uncertainty: the larger of the fit's standard error and the integer rounding of the readings (±0.5 % at each end, so ±1 % over the span).
pub fn fit(points: &[(f64, f64)]) -> Option<(f64, f64, f64)> {
    let n = points.len() as f64;
    let span = points.iter().map(|p| p.0).fold(f64::MIN, f64::max)
        - points.iter().map(|p| p.0).fold(f64::MAX, f64::min);
    if points.len() < 2 || span < MIN_SPAN_H {
        return None;
    }
    let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let slope = sxy / sxx;
    let se = if points.len() > 2 {
        let rss: f64 = points
            .iter()
            .map(|p| (p.1 - my - slope * (p.0 - mx)).powi(2))
            .sum();
        (rss / (n - 2.0) / sxx).sqrt()
    } else {
        0.0
    };
    Some((slope, se.max(1.0 / span), span))
}

/// One account's pace on one window: fitted burn, the burn that uses the rest exactly by the reset, and when it would run dry.
fn account_pace(
    samples: &[Sample],
    used: f64,
    reset: Option<i64>,
    exhausted: bool,
    now: i64,
) -> Value {
    let hours_left = reset.map(|r| ((r - now) as f64 / 3600.0).max(0.0));
    // Only this quota period: samples whose reset matches the current one, so a reset's drop to 0 is never fitted.
    let points: Vec<(f64, f64)> = samples
        .iter()
        .filter(|s| match (s.reset, reset) {
            (Some(a), Some(b)) => (a - b).abs() <= SAME_RESET_S,
            _ => true,
        })
        .map(|s| ((s.t - now) as f64 / 3600.0, s.used))
        .collect();
    let rate = fit(&points);
    let needed = match hours_left {
        _ if exhausted => Some(0.0),
        Some(h) if h > 0.0 => Some((100.0 - used).max(0.0) / h),
        _ => None,
    };
    let dry_in_h = match rate {
        Some((r, _, _)) if r > 0.0 && !exhausted => Some((100.0 - used).max(0.0) / r),
        _ => None,
    };
    json!({
        "used_pct": used, "resets_in_h": hours_left, "resets_at": reset, "exhausted": exhausted,
        "target_conservative_pct_per_h": match hours_left {
            _ if exhausted => Some(0.0),
            Some(h) if h > 0.0 => Some((100.0 - used - MARGIN_PCT).max(0.0) / h),
            _ => None,
        },
        "rate_pct_per_h": rate.map(|r| r.0), "rate_uncertainty": rate.map(|r| r.1), "fit_span_h": rate.map(|r| r.2),
        "n_samples": points.len(),
        "needed_pct_per_h": needed,
        "factor": match (rate, needed) { (Some(r), Some(n)) if n > 0.0 => Some(r.0 / n), _ => None },
        "runs_dry_before_reset": match (dry_in_h, hours_left) { (Some(d), Some(h)) => d < h, _ => false },
        "dry_in_h": dry_in_h,
    })
}

/// under, on, or over pace: whether the rate's uncertainty interval lies below, around, or above the needed rate.
fn verdict(rate: f64, unc: f64, needed: f64) -> &'static str {
    if needed <= 0.0 {
        return "nothing left to use";
    }
    if rate + unc < needed {
        "under"
    } else if rate - unc > needed {
        "over"
    } else {
        "on"
    }
}

/// The pace report: per provider and per account, from samples of the last `window_h` hours.
pub fn report(
    state_dir: &Path,
    buckets: &[Bucket],
    now: i64,
    window_h: f64,
) -> Result<Value, String> {
    // Takes in the samples quota-events.jsonl holds when the observer has not yet.
    record(state_dir, &[])?;
    let db = open(state_dir)?;
    let since = now - (window_h * 3600.0) as i64;
    let mut providers: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for b in buckets {
        let account = b
            .creds
            .first()
            .map_or("?", |c| c.trim_end_matches(".json"))
            .to_owned();
        let exhausted = b.status == Status::Exhausted;
        let mut per_window = serde_json::Map::new();
        for (kind, name, used_col, reset_col) in [
            (Kind::D7, "7d", "d7", "d7_reset"),
            (Kind::H5, "5h", "h5", "h5_reset"),
        ] {
            let (used, reset) = window(b, kind);
            let Some(used) = used.or(if exhausted { Some(100.0) } else { None }) else {
                continue;
            };
            let mut query = db
                .prepare(&format!("SELECT t, {used_col}, {reset_col} FROM samples WHERE principal = ?1 AND t >= ?2 AND {used_col} IS NOT NULL ORDER BY t"))
                .map_err(|e| format!("amh: {e}"))?;
            let samples: Vec<Sample> = query
                .query_map(params![b.principal, since], |r| {
                    Ok(Sample {
                        t: r.get(0)?,
                        used: r.get(1)?,
                        reset: r.get(2)?,
                    })
                })
                .map_err(|e| format!("amh: {e}"))?
                .collect::<Result<_, _>>()
                .map_err(|e| format!("amh: {e}"))?;
            per_window.insert(
                name.to_owned(),
                account_pace(&samples, used, reset, exhausted && used >= 99.5, now),
            );
        }
        if per_window.is_empty() {
            continue;
        }
        // The binding budget is the longest window the account reports: 7d when present, else 5h.
        let binding = if per_window.contains_key("7d") {
            "7d"
        } else {
            "5h"
        };
        providers.entry(b.pool.clone()).or_default().push(json!({
            "account": account, "provider": b.pool, "status": b.status.name(), "binding": binding, "windows": per_window,
        }));
    }
    let mut out = serde_json::Map::new();
    for (pool, accounts) in providers {
        let mut rate = 0.0;
        let mut unc2 = 0.0;
        let mut needed = 0.0;
        let mut unknown = 0;
        let mut absorbing = 0;
        let mut exhausted = Vec::new();
        let mut dry = Vec::new();
        let mut periods = Vec::new();
        for a in &accounts {
            let w = &a["windows"][a["binding"].as_str().unwrap_or("7d")];
            if let Some(reset_h) = w["resets_in_h"].as_f64() {
                let left = if w["exhausted"] == true {
                    0.0
                } else {
                    100.0 - w["used_pct"].as_f64().unwrap_or(100.0)
                };
                periods.push((left.max(0.0), reset_h));
            }
            if w["exhausted"] == true {
                exhausted.push(json!({"account": a["account"], "resets_in_h": w["resets_in_h"]}));
                continue;
            }
            absorbing += 1;
            needed += w["needed_pct_per_h"].as_f64().unwrap_or(0.0);
            match w["rate_pct_per_h"].as_f64() {
                Some(r) => {
                    rate += r;
                    unc2 += w["rate_uncertainty"].as_f64().unwrap_or(0.0).powi(2);
                }
                None => unknown += 1,
            }
            if w["runs_dry_before_reset"] == true {
                dry.push(json!({"account": a["account"], "dry_in_h": w["dry_in_h"], "resets_in_h": w["resets_in_h"]}));
            }
        }
        let unc = unc2.sqrt();
        let _ = needed;
        let (needed, ceiling) = band(&periods);
        let known = unknown < absorbing;
        let factor = (needed > 0.0 && known).then(|| rate / needed);
        let pool_verdict = if !known {
            "unknown"
        } else if rate + unc < needed {
            "under"
        } else if rate - unc > ceiling {
            "over"
        } else {
            "on"
        };
        let aim = rate.clamp(needed, ceiling.max(needed));
        out.insert(
            pool,
            json!({
                "rate_pct_per_h": rate, "rate_uncertainty": unc, "needed_pct_per_h": needed,
                "ceiling_pct_per_h": ceiling,
                "target_conservative_pct_per_h": band(&periods.iter().map(|(left, t)| ((left - MARGIN_PCT).max(0.0), *t)).collect::<Vec<_>>()).0,
                "headroom": {
                    "left_now_pct": periods.iter().map(|p| p.0).sum::<f64>(),
                    "refills": periods.iter().filter(|p| p.1 > 0.0).map(|p| json!({"in_h": p.1, "left_before_pct": p.0})).collect::<Vec<_>>(),
                },
                "factor": factor,
                "factor_low": factor.map(|_| (rate - unc).max(0.0) / needed),
                "factor_high": factor.map(|_| (rate + unc) / needed),
                "verdict": pool_verdict,
                "scale_concurrency_by": (known && rate > 0.0).then(|| aim / rate),
                "accounts_absorbing": absorbing, "accounts_without_rate": unknown,
                "exhausted": exhausted, "runs_dry_before_reset": dry,
                "accounts": accounts,
            }),
        );
    }
    Ok(json!({"generated_at": now, "window_h": window_h, "providers": out}))
}

/// The band a provider's total burn should stay in, in % of one account per hour, from each account's quota left and its reset (hours from now; exhausted accounts have 0 left).
/// Floor: by each reset, at least the quota expiring at or before it must be used, or it is lost: max over resets of expiring / hours.
/// Ceiling: by each reset, at most the quota available before it (all left now plus the full refills of accounts that reset earlier) can be used, or the provider runs dry and waits: min over resets of available / hours.
/// Load can move between accounts, so the provider is treated as one pool of equal-sized accounts.
pub fn band(periods: &[(f64, f64)]) -> (f64, f64) {
    let mut resets: Vec<f64> = periods.iter().map(|p| p.1).filter(|t| *t > 0.0).collect();
    resets.sort_by(f64::total_cmp);
    let left: f64 = periods.iter().map(|p| p.0).sum();
    let mut floor: f64 = 0.0;
    let mut ceiling = f64::INFINITY;
    for t in resets {
        let expiring: f64 = periods.iter().filter(|p| p.1 <= t).map(|p| p.0).sum();
        let refilled = 100.0 * periods.iter().filter(|p| p.1 < t).count() as f64;
        floor = floor.max(expiring / t);
        ceiling = ceiling.min((left + refilled) / t);
    }
    (floor, if ceiling.is_finite() { ceiling } else { 0.0 })
}

fn h(x: &Value) -> String {
    match x.as_f64() {
        Some(v) if v >= 48.0 => format!("{:.1}d", v / 24.0),
        Some(v) => format!("{v:.1}h"),
        None => "?".to_owned(),
    }
}

fn f(x: &Value) -> String {
    x.as_f64().map_or("?".to_owned(), |v| format!("{v:.2}"))
}

/// One line per provider, for `amh quota status`.
pub fn summary_lines(report: &Value) -> String {
    let mut out = String::new();
    for (pool, p) in report["providers"].as_object().into_iter().flatten() {
        out.push_str(&format!(
            "pace {pool}: {} pace, burning {} %/h; use at least {} %/h (else quota expires unused) and at most {} (else it runs dry before a refill) (x{}, {}–{}) over {} live account(s){}{}\n",
            p["verdict"].as_str().unwrap_or("?"),
            f(&p["rate_pct_per_h"]),
            f(&p["needed_pct_per_h"]),
            f(&p["ceiling_pct_per_h"]),
            f(&p["factor"]),
            f(&p["factor_low"]),
            f(&p["factor_high"]),
            p["accounts_absorbing"],
            match p["exhausted"].as_array().map_or(0, Vec::len) {
                0 => String::new(),
                n => format!(", {n} exhausted"),
            },
            match p["runs_dry_before_reset"].as_array().map_or(0, Vec::len) {
                0 => String::new(),
                n => format!(", {n} would run dry before reset"),
            },
        ));
    }
    out
}

/// The full text report of `amh quota pace`.
pub fn text(report: &Value) -> String {
    let mut out = format!("fitted over the last {} h of samples\n", report["window_h"]);
    out += &summary_lines(report);
    for (pool, p) in report["providers"].as_object().into_iter().flatten() {
        for a in p["accounts"].as_array().into_iter().flatten() {
            for (name, w) in a["windows"].as_object().into_iter().flatten() {
                let mark = if a["binding"] == name.as_str() {
                    "*"
                } else {
                    " "
                };
                out.push_str(&format!(
                    "  {pool} {}{mark} {name}: {:.0}% used, reset in {}; ",
                    a["account"].as_str().unwrap_or("?"),
                    w["used_pct"].as_f64().unwrap_or(0.0),
                    h(&w["resets_in_h"]),
                ));
                if w["exhausted"] == true {
                    out.push_str("exhausted, absorbs nothing until the reset\n");
                    continue;
                }
                out.push_str(&format!(
                    "burn {} ± {} %/h ({} samples over {}), need {} %/h, x{}",
                    f(&w["rate_pct_per_h"]),
                    f(&w["rate_uncertainty"]),
                    w["n_samples"],
                    h(&w["fit_span_h"]),
                    f(&w["needed_pct_per_h"]),
                    f(&w["factor"]),
                ));
                if w["runs_dry_before_reset"] == true {
                    out.push_str(&format!(", runs dry in {}", h(&w["dry_in_h"])));
                }
                out.push('\n');
            }
        }
    }
    out + "(* the window the verdict uses; x = burn / needed; scale agent concurrency by 1/x to land on pace)\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_recovers_a_slope_and_bounds_it_by_integer_rounding() {
        // 2 %/h over 5 h, read as integers.
        let points: Vec<(f64, f64)> = (0..=60)
            .map(|i| {
                (
                    i as f64 / 12.0 - 5.0,
                    (10.0 + 2.0 * i as f64 / 12.0).round(),
                )
            })
            .collect();
        let (slope, unc, span) = fit(&points).expect("fit");
        assert!((slope - 2.0).abs() < 0.1, "{slope}");
        assert!((unc - 0.2).abs() < 1e-9 && (span - 5.0).abs() < 1e-9);
        assert!(fit(&[(0.0, 1.0), (0.1, 2.0)]).is_none());
    }

    #[test]
    fn pace_ignores_samples_from_before_a_reset_and_flags_running_dry() {
        let now = 1_000_000;
        let reset = now + 10 * 3600;
        let mut samples = vec![Sample {
            t: now - 5 * 3600,
            used: 90.0,
            reset: Some(now - 4 * 3600),
        }];
        samples.extend((0..=36).map(|i| Sample {
            t: now - 3 * 3600 + i * 300,
            used: (60.0 + 10.0 * i as f64 / 12.0).round(),
            reset: Some(reset),
        }));
        let p = account_pace(&samples, 90.0, Some(reset), false, now);
        assert!((p["rate_pct_per_h"].as_f64().unwrap() - 10.0).abs() < 0.5);
        assert!((p["needed_pct_per_h"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(p["runs_dry_before_reset"], true);
        assert_eq!(verdict(10.0, 0.3, 1.0), "over");
        assert_eq!(verdict(0.5, 0.3, 1.0), "under");
        assert_eq!(verdict(0.9, 0.3, 1.0), "on");
        let gone = account_pace(&samples, 100.0, Some(reset), true, now);
        assert_eq!(gone["needed_pct_per_h"], 0.0);
    }
}

#[cfg(test)]
mod band_tests {
    #[test]
    fn staggered_resets_set_a_floor_and_a_ceiling() {
        // One live account with 81 % left resetting in 154 h; one exhausted account refilling in 41.5 h.
        let (floor, ceiling) = super::band(&[(81.0, 154.0), (0.0, 41.5)]);
        assert!((floor - 81.0 / 154.0).abs() < 1e-9);
        // Before the refill at 41.5 h only 81 % is there (at most 81/41.5 %/h).
        // After it, 181 % over 154 h is the tighter limit.
        assert!((ceiling - 181.0 / 154.0).abs() < 1e-9);
        // An account about to reset with much left raises the floor.
        let (floor, _) = super::band(&[(50.0, 10.0), (90.0, 100.0)]);
        assert!((floor - 5.0).abs() < 1e-9);
    }
}
