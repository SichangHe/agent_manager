# Quota manager

(authored by agents unless marked 🧑)

design and rationale: `docs/quota-manager-design.md` (v3, approved by the human)

code
- `src/quota_core.rs`: pure; parse, fold readings into buckets, pools, estimator, proxy actions, redemption rules, gate, tool choice, snapshot (de)serialization
- `src/quota_observer.rs`: input/output of `amh quota serve` and `amh quota status`; proxy client, files, redemption
- `src/quota_gate.rs`: amh side; the watcher's quota pass, the gate file, admission at `task start`, notices
- `src/quota.rs`: only a session's own limit evidence (Claude "session limit" text); cleared on `quota_appeared`
- `amh-quota.service`: template user unit for the observer

observer (`amh quota serve`, `--live` to act)
- one cycle every 5 min, sooner at a reset hint (exhausted bucket's reset + 5 s) or when `quota-poll-now` exists
    - `quota.rs` creates `quota-poll-now` when a session records a new limit error
- reads only the proxy management API: `GET /auth-files`
    - per credential: freshest `quota.signals` or `model_quotas.*.signals`, plus `cooldowns` with reason `credential_quota` (the proxy saw a quota error)
    - never reads tokens; `id_token` is read only for `chatgpt_account_id`, the Codex principal
- shadow mode (default): computes actions, logs `shadow: would ...` once per change, changes nothing
- live mode: sole writer of proxy `disabled` and `priority`; redeems
- files in the state folder
    - `quota-snapshot.json`: atomic, read by amh; older than 15 min means a dead observer, so every pool is unknown
    - `quota-events.jsonl`: samples and events; rotated at 8 MB to `.1`
    - `quota-redeem.journal`: write-ahead log of redemptions
    - `quota-gate.json`, `quota-notices.json`, `amh-quota-appeared.txt`: amh side

model
- credential: proxy auth file; principal: `codex:ACCOUNT_ID` or `claude:EMAIL`; bucket: one quota per principal
- window: 5h or 7d, `active` / `not_active` (Claude omits 5h at 0%) / `invalid`
- bucket status
    - exhausted: window at 100% with a future reset, or an active `credential_quota` cooldown
    - available: fresh (at most 15 min) reading, all active windows below 99%, on 2 consecutive polls
    - unknown: stale, invalid, empty, 401, or a full window whose reset passed; never available
    - a reading older than the exhaustion cannot end it
- only credentials the router may use form buckets: not denied, not unclassified, not disabled by someone else
- pool: available if any bucket is; else unknown if any is; else exhausted
- events by diffing: `quota_appeared`, `quota_ran_out`, `credential_added`, `predicted_waste`
- 429, capacity errors, and 5xx never mark quota exhausted; codex-throttle, codex-rate-adapt, `fallback.rs` keep them

proxy reconciliation
- exhausted: disable and remember in `ours`; available and in `ours`: enable; unknown: no change
- a credential disabled by anyone else is never enabled
- a disabled credential gets no traffic, so its signals may never refresh: one we disabled whose bucket went unknown is enabled again once its reset hint passed, to probe it
- an empty or corrupt proxy answer or snapshot ends the cycle with an error; `seen` and `ours` only grow
- priority among available buckets: soonest 7d reset first; 5h above 90% last; unknown 7d after known; equal keys share a rank; higher number wins
- denied (`quota.deny`) and unclassified new credentials stay disabled
    - the first run grandfathers all existing credentials; later ones need `quota.allow` or `quota.deny`

redemption
- authorized only with `quota.redeem = "true"` in live mode; Codex only
- one episode per exhaustion, named `PRINCIPAL@SINCE` (when it began); any journaled step blocks a second attempt
    - a principal also waits 6 h after its last attempt, so a flicker through unknown cannot redeem twice
    - an unreadable journal ends the cycle; the journal is never rotated
    - after `confirmed` the credential is enabled at once; unverified: the proxy may still hold its own `credential_quota` cooldown until the old reset time
- steps: `intent` (credit id, request id, count before) then the call then `called` then `confirmed` (credit no longer available)
    - `no_credit`, `rejected` (provider 4xx), `uncertain` end an episode
    - transport failure or crash after `intent` is `uncertain`: re-read once, else tell the human; never retried
- provider calls go through the proxy's `POST /api-call` with `$TOKEN$`, so the observer never holds tokens

amh side
- priority: task field `priority` (integer, default 0); `amh task start --priority N`, `amh task priority TASK.md N`
- the watcher's minute pass (`quota_gate::pass`) derives the gate from snapshot plus tasks; nothing accumulates
    - exhausted pool: all its workers wait
    - unknown pool: nobody new waits and nobody resumes; a running worker keeps running
    - projected to run out (remaining minus burn x hours to the soonest 7d reset, burn = rise per hour over the last hour): lowest-priority workers wait until the rest fit, equal burn per worker assumed; managers never wait
    - waiting means `omnigent::status` is `paused` and `omnigent::send` refuses: no nudges, mail held
    - workers waiting last pass with room now resume by priority, 2 per pass, one nudge each; the gate is keyed by address, so a migrated task gets no stale nudge
- admission (workers only: managers are never held back; a pinned model counts as a named tool): `task start` without `--tool` goes to the default pool, else the other if the default is closed; a named tool is never swapped, a closed pool refuses it
    - closed: not available, or projected to run out and the new priority does not exceed the lowest running worker's
- `quota.enforce = "true"` turns gating and admission on; otherwise the pass only logs `shadow: would ...`
- notices (the unknown-pool and waste ones only with `quota.enforce`) once per key via `quota-notices.json`, to `quota.notify_task` (default `quota_support.md`)
    - new credential, observer dead, proxy unreadable twice, actuator failed twice, pool unknown over 1 h with work waiting, uncertain or rejected redemption
    - predicted waste with nothing waiting goes to the main manager

settings (dotted keys, `env.toml` or `config.toml`)
- `quota.proxy_key_file` (required for the observer), `quota.proxy_url`
- `quota.allow`, `quota.deny`: comma lists of auth file names
- `quota.redeem`, `quota.enforce`: `"true"`
- `quota.notify_task`

tests: `cargo test quota`
- pure tables and random-input properties in `quota_core`; fake proxy on a real socket for full cycles, crash, timeout, shadow, new credential in `quota_observer`; gate and admission over task files in `quota_gate`
