//! Durable session quota pauses; after a known reset, a retry every `RETRY_S` until the session changes.

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Clone, Copy)]
pub enum Attempt {
    Observe,
    Reserve,
    Release,
}

/// The provider's session quota error, excluding unrelated harness failures.
pub fn limited(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("session limit reached") || lower.contains("hit your session limit")
}

/// Resolve the next advertised local reset, including midnight and daylight saving time.
fn reset(error: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let after = error.split_once("resets ")?.1;
    let (clock, zone) = after.split_once(' ')?;
    let zone: chrono_tz::Tz = zone
        .trim()
        .trim_start_matches('(')
        .split(')')
        .next()?
        .parse()
        .ok()?;
    let mut normalized = clock.to_uppercase();
    if !normalized.contains(':') && (normalized.ends_with("AM") || normalized.ends_with("PM")) {
        normalized.insert_str(normalized.len() - 2, ":00");
    }
    let time = NaiveTime::parse_from_str(&normalized, "%I:%M%p")
        .or_else(|_| NaiveTime::parse_from_str(clock, "%H:%M"))
        .ok()?;
    let date = now.with_timezone(&zone).date_naive();
    let candidate = |date: chrono::NaiveDate| {
        zone.from_local_datetime(&date.and_time(time))
            .earliest()
            .map(|at| at.with_timezone(&Utc))
    };
    let today = candidate(date)?;
    if today > now {
        Some(today)
    } else {
        candidate(date + Duration::days(1))
    }
}

// 🧑 "Claude is getting \"Session limit reached”. Let the config agent handle this gracefully instead of keep spamming those agents"
/// Keep a pause across watcher restarts. Changed session evidence clears it; an unchanged error gets only one reset retry.
/// Seconds after an attempt past the reset before the next one, while the session still shows the same limit error.
const RETRY_S: i64 = 900;

pub fn paused(
    path: &Path,
    id: &str,
    error: &str,
    now: DateTime<Utc>,
    failed_at: Option<DateTime<Utc>>,
    attempt: Attempt,
) -> Result<bool, String> {
    if matches!(attempt, Attempt::Release) && !limited(error) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create quota state folder: {e}"))?;
    }
    let lock = std::fs::File::create(path.with_extension("lock"))
        .map_err(|e| format!("cannot open quota lock: {e}"))?;
    lock.lock()
        .map_err(|e| format!("cannot lock quota state: {e}"))?;
    let mut records: Value = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("invalid quota state: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("cannot read quota state: {e}")),
    };
    if !records.is_object() {
        return Err("invalid quota state: expected session records".to_owned());
    }
    let before = records.clone();
    let blocked = if !limited(error) {
        if let Some(map) = records.as_object_mut() {
            map.remove(id);
        }
        false
    } else {
        let failed_s = failed_at.map(|at| at.timestamp());
        let renewed =
            failed_s.is_some_and(|at| records[id]["failed_s"].as_i64().is_none_or(|old| at > old));
        if !matches!(attempt, Attempt::Release) && (records[id]["error"] != error || renewed) {
            records[id] = json!({"error": error, "failed_s": failed_s, "reset_s": reset(error, failed_at.unwrap_or(now)).map(|at| at.timestamp()), "attempted": false});
            // A session's quota error says the pool may have run out: ask the observer to read the proxy now.
            if let Some(parent) = path.parent() {
                let _ = std::fs::write(parent.join("quota-poll-now"), "");
            }
        }
        if matches!(attempt, Attempt::Release)
            && records[id]["error"] == error
            && records[id]["failed_s"].as_i64() == failed_s
        {
            records[id]["attempted"] = json!(false);
        }
        let due = records[id]["reset_s"]
            .as_i64()
            .is_some_and(|at| now.timestamp() >= at);
        // An attempt that left the session failed with the same error may be stale evidence, not a new limit, so another attempt follows after `RETRY_S`.
        // lost_work_mgr.md: "the stale pause lived in amh's state" ... "PB news skipped ticks for about 5.5 hours"
        let retry_due = records[id]["attempted_s"]
            .as_i64()
            .is_none_or(|at| now.timestamp() >= at + RETRY_S);
        let blocked = !due || (records[id]["attempted"] == true && !retry_due);
        if !blocked && matches!(attempt, Attempt::Reserve) {
            records[id]["attempted"] = json!(true);
            records[id]["attempted_s"] = json!(now.timestamp());
        }
        blocked
    };
    if records != before {
        crate::taskfile::write(path, &records.to_string())
            .map_err(|e| format!("cannot write quota state: {e}"))?;
    }
    Ok(blocked)
}

/// Forget every session's limit evidence: all of it is Claude's, and the pool just got quota back.
pub fn clear(path: &Path) -> Result<(), String> {
    match std::fs::read_to_string(path) {
        Ok(_) => {
            crate::taskfile::write(path, "{}").map_err(|e| format!("cannot write quota state: {e}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot read quota state: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_pause_survives_restart_and_retries_once() -> Result<(), Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!("amh-quota-test-{}.json", std::process::id()));
        let before = DateTime::parse_from_rfc3339("2026-10-07T07:30:00Z")?.with_timezone(&Utc);
        let after = before + Duration::minutes(11);
        let error = "You've hit your session limit · resets 12:40am (America/Los_Angeles)";
        assert!(paused(
            &path,
            "claude",
            error,
            before,
            None,
            Attempt::Observe
        )?);
        assert!(paused(
            &path,
            "claude",
            error,
            before,
            None,
            Attempt::Reserve
        )?);
        assert!(!paused(
            &path,
            "claude",
            error,
            after,
            None,
            Attempt::Reserve
        )?);
        assert!(paused(
            &path,
            "claude",
            error,
            after,
            None,
            Attempt::Reserve
        )?);
        // The same stale error after the attempt gets another attempt once `RETRY_S` has passed.
        let later = after + Duration::seconds(RETRY_S);
        assert!(!paused(
            &path,
            "claude",
            error,
            later,
            None,
            Attempt::Reserve
        )?);
        assert!(paused(
            &path,
            "claude",
            error,
            later,
            None,
            Attempt::Reserve
        )?);
        assert!(!paused(&path, "claude", "", after, None, Attempt::Observe)?);
        assert!(!paused(
            &path,
            "old",
            error,
            after,
            Some(before),
            Attempt::Reserve
        )?);
        assert!(paused(
            &path,
            "old",
            error,
            after,
            Some(before),
            Attempt::Reserve
        )?);
        assert!(!paused(
            &path,
            "old",
            error,
            after,
            Some(before),
            Attempt::Release
        )?);
        assert!(!paused(
            &path,
            "old",
            error,
            after,
            Some(before),
            Attempt::Reserve
        )?);
        assert!(paused(
            &path,
            "old",
            error,
            after,
            Some(after),
            Attempt::Observe
        )?);
        assert!(paused(
            &path,
            "old",
            error,
            after,
            Some(before),
            Attempt::Release
        )?);
        assert!(!paused(
            &path,
            "old",
            error,
            after + Duration::days(1),
            Some(after),
            Attempt::Reserve
        )?);
        assert!(paused(
            &path,
            "claude",
            "Session limit reached",
            after,
            None,
            Attempt::Observe
        )?);
        std::fs::remove_file(path)?;
        std::fs::remove_file(
            std::env::temp_dir().join(format!("amh-quota-test-{}.lock", std::process::id())),
        )?;
        Ok(())
    }

    /// lost_work_mgr.md: "the stale pause lived in amh's state" ... "PB news skipped ticks for about 5.5 hours".
    #[test]
    fn quota_appeared_clears_stale_pauses_so_nothing_stays_stalled()
    -> Result<(), Box<dyn std::error::Error>> {
        let path =
            std::env::temp_dir().join(format!("amh-quota-clear-{}.json", std::process::id()));
        let at = DateTime::parse_from_rfc3339("2026-10-07T07:30:00Z")?.with_timezone(&Utc);
        let error = "You've hit your session limit · resets 11:00pm (America/Los_Angeles)";
        assert!(paused(&path, "pb", error, at, Some(at), Attempt::Observe)?);
        clear(&path)?;
        clear(&path.with_extension("missing"))?;
        let again = paused(
            &path,
            "pb",
            error,
            at + Duration::hours(24),
            Some(at),
            Attempt::Observe,
        )?;
        assert!(
            !again,
            "a cleared pause comes back only from fresh evidence, and its reset has passed"
        );
        std::fs::remove_file(&path)?;
        std::fs::remove_file(path.with_extension("lock"))?;
        let _ = std::fs::remove_file(path.with_file_name("quota-poll-now"));
        Ok(())
    }

    #[test]
    fn reset_crosses_midnight_and_changes_offset() -> Result<(), Box<dyn std::error::Error>> {
        let now = DateTime::parse_from_rfc3339("2026-11-01T06:55:00Z")?.with_timezone(&Utc);
        assert!(reset("resets 1am (America/Los_Angeles)", now).is_some());
        assert_eq!(
            reset("resets 12:40am (America/Los_Angeles)", now).map(|at| at.to_rfc3339()),
            Some("2026-11-01T07:40:00+00:00".to_owned())
        );
        assert!(!limited("model overloaded"));
        Ok(())
    }
}
