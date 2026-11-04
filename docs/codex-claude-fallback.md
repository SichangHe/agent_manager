# Codex to Claude fallback

(authored by agents unless marked 🧑)

- 🧑 "I think we just need to retry aggressively on 429 if they actually succeed sometimes we need to strike a balance between aggressive retries and if it’s taking too many tries, and the progress is way too slow you should switch to Claude and later somehow decide when to switch back"
- off unless `fallback.codex_to_claude = "true"` in `config.toml`/`env.toml`
- evidence: a Codex session with `status` failed and `last_task_error` from Codex giving up
    - `{code: rate_limit_exceeded, message: "exceeded retry limit, last status: 429 Too Many Requests"}` or `{code: codex_turn_error, message: "Selected model is at capacity..."}` (`fallback::capacity_429`)
    - seen in Omnigent's `chat.db` labels and `GET /v1/sessions/ID` of harness `codex-native`; the proxy's "Codex is at capacity" arrives as the first form after Codex's own retries
    - also `{code: codex_turn_error, message: "stream disconnected before completion: stream closed before response.completed"}`, counted alike (6 such sessions in `chat.db`, Oct 15-25; none found for Nov 3 cx_gc)
    - the same code appears for Claude 429s, so the harness must be `codex-native`
- the watcher minute pass (`fallback::watch`, called in `Scanner::nudge`) over agents in problem `Error`
    - failure 1 and 2 inside 10 min: send "retry" (`omnigent::send`), once per failure (`updated_at` dedups)
    - failure 3 of one agent: `agents::replace` with Claude (`launch.claude` model), same task file, original request plus open items
    - 3 failures from any agents in 10 min mark Codex unhealthy
- never swapped (`fallback::refusal`, fails closed); the manager is told once, the agent is left alone
    - managers or an unrecorded role
    - tasks started with `--model` (`model_pinned: true` in the task file, set by `start_task`)
    - sessions whose `model_override` is not the current `launch.codex.model`
    - not knowable: a task started before `model_pinned` with `--model` equal to the default, and account requirements (the proxy picks accounts)
- switch back: while unhealthy, `start_task` without `--tool` starts workers on Claude (`fallback::default_tool`); explicit `--tool codex` is honored
    - every 15 min one probe: `POST /v1/responses` through the proxy, 16 output tokens
    - 2 successes in a row: healthy, failures forgotten; a failure resets the run
    - running agents are never switched mid-turn
- state: `amh-codex-health.json` in the state folder
- settings in `env.toml`: `fallback.proxy_key_file` (the proxy's client key file, read per probe, never logged), `fallback.proxy_url` (default `http://127.0.0.1:18318`)
