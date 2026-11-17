# Quota manager design (draft v3 for human approval)

(authored by agents unless marked 🧑)

## Goal

🧑 "BTW We now have Claude quota. Also, quota manager agent should notice these things. This needs a from-scratch design to work flawlessly" (manager_mail/zulip-638616011.txt)

Requirements from the manager's delegation (not the human's own words):
- detect each account's remaining quota and reset time automatically
  - accounts: Claude (personal, Midas), Codex/ChatGPT; Gemini excluded by the human
- notice when quota appears or runs out
- use the soonest-resetting quota first
- route work between Claude and Codex per the human's standing rules
  - the delegation says Codex is currently reserved for UI-paper experiments
- pause and resume workers automatically
- never waste or overrun quota
- tell the human only about decisions that need the human

Facts are in work_logs/quota_redesign_inventory.md. v1 was critiqued by an independent reviewer (ChatGPT, /tmp/quota_review.md); v2 adopts its fixes.

## Why from scratch

Today seven separate scripts and timers each own a slice of the job. None of them informs amh, and amh only reacts after failures. That is why returning Claude quota went unnoticed, readings are stale or malformed, a stale pause stalled PB news for about 5.5 hours, and "use before reset" targets were missed.

## Architecture: three roles, one owner each

1. Observer (`amh quota serve`, its own systemd service, so a provider timeout cannot stall amh-watch)
   - owns quota observation and the proxy's routing eligibility
   - writes one normalized snapshot file that everything else reads
2. amh (the existing watcher and start_task)
   - owns task→tool policy, admission, and pause/resume
   - reads the snapshot and never calls providers
3. Redeemer (part of the observer, journaled)
   - owns the only irreversible action: redeeming Codex reset credits

The core of each role is a pure function of (inputs, config, now) that returns actions. All policy is testable with fixtures and a fake clock.

## State model

Three entities, which v1 wrongly merged:
- credential: a proxy auth file (a filename, which can change)
- principal: the provider's stable identity for it
  - Claude: org uuid + account uuid from oauth/profile
  - Codex: account_id from the token
- bucket: one quota per principal
  - several credentials sharing a principal share one bucket, so quota is never double-counted

Bucket fields:
- windows: {kind 5h | 7d, state, used_pct, resets_at, observed_at}
  - state is active, not_active (e.g. Claude five_hour = null at 0% use, documented per provider), or invalid
  - invalid or missing-unexpected means the bucket is unknown, never available
- credits: count; expiry only where some source actually exposes it

Bucket status:
- available: fresh reading, all active windows below 99% on two consecutive readings (hysteresis)
- exhausted: a window at 100%, or explicit quota-limit evidence (below)
- unknown: reading older than its TTL (absolute time: 15 minutes), invalid, or 401

Reset times are wake-up hints, not truth. When a reset passes, the observer polls immediately. Only a fresh reading changes exhausted→available.

Quota is separate from rate and capacity. Only explicit quota-limit evidence changes quota state:
- Claude "session limit reached" text, or Unified Status=rejected
- Codex usage-limit errors

429 rate limits, "model at capacity" and 5xx stay with codex-throttle, codex-rate-adapt and fallback.rs as today.

Pool (per tool): available / exhausted (with next_reset hint) / unknown. A pool is the union of its eligible buckets.

Events come from diffing snapshots, with credential discovery kept separate from quota recovery:
- quota_appeared: a bucket goes exhausted or unknown→available
  - this is how "we now have Claude quota" gets noticed
- quota_ran_out
- credential_added
  - the new credential is denied by default until config classifies it, and the human is asked once

Storage:
- `~/.local/state/amh/quota-snapshot.json`: small, atomic
- `quota-events.jsonl`: append-only samples and events
- `quota-redeem.journal`
- nothing in /tmp

## Data sources

🧑 "The observer should get quota from the proxy, the proxy should cache quota requests"

- the proxy is the only thing that talks to the providers' usage endpoints
  - Codex: `wham/usage`; Claude: `oauth/usage` + `oauth/profile`
  - these endpoints spend no quota
  - the proxy already holds and refreshes every credential, so the observer never touches tokens
- the observer reads quota only from the proxy, through the management API (`/auth-files` `quota`, and the quota-load endpoint behind the management page)
- caching on the proxy side, for every caller including the human's management page:
  - one cached value per principal, TTL 5 minutes
  - concurrent requests for one principal share one in-flight fetch
  - on 429 or 5xx, keep and serve the last good value marked stale, then retry with exponential backoff and full jitter (base 1 minute, cap 30 minutes, honoring Retry-After)
  - 🧑 "Failed to load quota: 429 Rate limited. Please try again later. The proxy has been reporting this. Avoid this"
  - codex_switch_mgr.md implements this caching
  - assumption to verify: whether it can live in the existing front on port 18317 (codex-throttle) or needs a proxy patch
- the observer reads every 5 minutes, and once more right after a reset hint or a quota-limit error
  - a stale value never turns a bucket exhausted; it only turns it unknown after 15 minutes
- acceptance: zero 429s over 1 hour of live operation
- secondary evidence: the proxy's `quota.signals` response headers

Gemini is out of scope. 🧑 "Ignore Gemini"

Session errors identify a session's tool and pool, not an account, because the proxy picks accounts (`routing.session-affinity: false`). So they never mark a specific bucket exhausted. They only trigger an immediate re-read and pause that one session.

## Policies

1. Proxy routing (observer).
   - the observer is the sole writer of proxy `disabled` and `priority`
   - exhausted credentials are disabled in the proxy and re-enabled when they become available again
   - priority objective: minimize quota that expires unused
     - implementation: among eligible buckets, rank by the soonest 7d reset
     - a bucket whose 5h window is above 90% drops below the others until that window resets
     - counterexamples are enumerated in tests
2. Task priority plus run-out estimate (amh). 🧑 "We just need some priority of tasks plus estimation of whether we'd run out"
   - each task has a priority, set by its manager at start and changeable later
   - per pool (Claude, Codex), the estimator projects remaining quota at the next reset as remaining minus burn rate × time to reset
     - burn rate: the observed %/h over the last hour
   - projected to run out before the reset → stop admitting the lowest-priority work on that pool, and pause the lowest-priority running workers until the projection fits
   - projected to leave quota unused → admit more work by priority, and tell the main manager when there is not enough ready work
   - tool choice: a new task goes to the launch default (existing config); if that pool is projected to run out and the other is not, it goes to the other
     - no extra rule file, no tags; 🧑 on a tag rule: "No. Doesn't matter"
3. Pause/resume (amh).
   - pool gating is derived on every watcher pass from the snapshot plus the estimate, not stored as N pause records
     - so no stale pause can exist
   - paused or exhausted → no new admissions, no nudges, mail held
     - running sessions on an exhausted pool simply hit the provider limit and stop
   - room again → resume by priority with one nudge each, at most 2 starts per minute
     - only if its session is still the task's current runat (no duplicate after a migration)
   - quota.rs keeps only session-specific limit evidence, and clears it on quota_appeared for its pool
4. Reset credits (journaled, authorized). 🧑 "Yes" (to redeeming Codex resets automatically as soon as an account runs out)
   - when a bucket becomes exhausted and has a credit, redeem one, earliest-expiring when expiry is known
   - journal: intent(before_count) → call → uncertain → confirmed (count dropped and quota reset)
   - on uncertain, never retry automatically; re-read, and ask the human only if it stays ambiguous
   - guarantee: never knowingly redeem twice
5. Paid overage is out of scope. 🧑 "No paid overage yet. Don't worry about it. It does not make sense. If we lack quota I'll just open another account"
   - a new account added to the proxy is picked up automatically on the next read (credential_added)

## Telling the human (deduplicated, once per event)

- a new credential needs classification
- a pool is unknown for over 1 hour while work waits on it
- predicted waste with no ready work
- an uncertain redemption, or an actuator failing twice

Everything else goes to the event log and to `amh quota status`.

## Failure modes

- provider endpoint down or schema changed → bucket unknown; no admissions or resumes based on unknown; human told after 1 hour if work waits
- proxy down → observer actions retry; human told after 2 failures
- observer dead → snapshot goes stale → pools unknown → amh freezes new admissions, never invents quota; amh-watch alerts if the snapshot is over 15 minutes old
- clock jump or past reset hint → poll now
- crash mid-redemption → journal recovery (policy 6)
- old timers still writing proxy fields → removed in the same cutover

## Tests

- pure-function table tests: every policy, status transition and failure mode above
  - e.g. "Claude bucket resets → quota_appeared → paused Claude workers nudged once → human not told"
- parsers on recorded real responses, including null windows, malformed data and 401
- replay of recorded incidents: the PB news stale pause, Midas disable, the Oct 12 429 storm (must not mark quota exhausted)
- property tests:
  - a forbidden tool is never chosen
  - a human hold is never cleared
  - there is at most one active generation per task
  - unknown never yields available
  - no unauthorized redemption, no double redemption
- fault injection: crash between each redemption step, timeout after a successful redemption, partial PATCH, proxy restart, clock jumps
- fake proxy and fake provider endpoints for full cycles
- shadow mode before cutover: the observer logs observations and intended actions; check them against the invariants and against reality at the next real reset

## Replaced pieces

- removed: proxy-reset-priority, claude-usage account policy, claude-quota-pace, codex-load-watch budget part, codex-reset-watch, codex-reset-credit@, midas-reenable, /tmp pollers, docs/cligate-quota-planning.md
- kept: codex-throttle, codex-rate-adapt (rate, not quota), fallback.rs (capacity, restricted to allowed tools)
- `amh quota status` replaces codex-quota and claude-usage as the human view

## Open questions for the human

none currently
