# Quota manager

(authored by agents unless marked 🧑)

design and rationale: `docs/quota-manager-design.md` (v3, approved by the human)

code
- `src/quota_core.rs`: pure; parse, fold readings into buckets, pools, proxy actions, redemption rules, snapshot (de)serialization
- `src/quota_observer.rs`: input/output of `amh quota serve` and `amh quota status`; proxy client, files, redemption, notices, the watcher's pass
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
    - `quota-notices.json`, `amh-quota-appeared.txt`: amh side
- the watcher's minute pass (`quota_observer::watcher_pass`) clears Claude's session limit evidence on `quota_appeared` and tells about a dead observer
- amh never pauses or holds back workers for pool quota; a session's own limit pause stays (`docs/session-quota-pauses.md`)
- notices once per key via `quota-notices.json`, to `quota.notify_task` (default `quota_support.md`)
    - new credential, observer dead, proxy unreadable twice, actuator failed twice, uncertain or rejected redemption

settings (dotted keys, `env.toml` or `config.toml`)
- `quota.proxy_key_file` (required for the observer), `quota.proxy_url`
- `quota.allow`, `quota.deny`: comma lists of auth file names
- `quota.redeem`: `"true"`
- `quota.notify_task`

tests: `cargo test quota`
- pure tables and random-input properties in `quota_core`; fake proxy on a real socket for full cycles, crash, timeout, shadow, new credential in `quota_observer`
