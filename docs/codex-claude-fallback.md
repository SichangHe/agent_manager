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
    - failure 1 and 2 of an agent inside 10 min: send "retry" (`omnigent::send`), once per failure (`omnigent::updated_at` dedups; a session without a readable `updated_at` is skipped)
    - failures are counted as 429s and stream drops separately; both count toward a swap
    - every fresh failure of an agent that stays (swap held back by a probe, the cap, backoff, a refusal, or too few failures) gets the "retry" message
    - brakes before a swap (`Health::swap`)
        - normal path: the agent has 3 failures in 10 min and a probe that failed less than 5 min ago; with 3 failures piling up amh probes at once (at most every 5 min), so a proxy restart that Codex survives swaps nobody
        - override: 6 or more failures of one agent in 30 min swap it even while probes succeed, for the human's "if it’s taking too many tries, and the progress is way too slow you should switch to Claude"
        - both paths: at most 3 swap attempts per hour over all agents, failed attempts included; when the cap blocks one, the manager is told (once an hour)
        - a failed swap is retried at most 3 times, 15 min apart, per (agent, failure time); the 10 min window usually ends first, so a retry needs a new failure
        - failures are keyed by the session's `updated_at`; a session whose `updated_at` moves without a new failure can count twice
    - a swap is `agents::replace` with Claude (`launch.claude` model), same task file, original request plus open items
- never swapped (`fallback::refusal`); the manager is told once, the agent is left alone
    - managers or an unrecorded role
    - tasks started with `--model` or `--effort` (`model_pinned: true` in the task file, set by `start_task`)
    - sessions whose `model_override` is not the current `launch.codex.model`
    - remaining gaps: tasks started before `model_pinned` whose model equals the default look unpinned and are swapped; account requirements are not visible to amh (the proxy picks accounts)
- switch back: while unhealthy, `start_task` without `--tool`, `--model` and `--effort` starts workers on Claude (`fallback::default_tool`); explicit `--tool codex` is honored
    - every 15 min one probe: `POST /v1/responses` through the proxy, 16 output tokens, 20 s timeout
    - only HTTP 429, 5xx and timeouts mean Codex is down; other replies (401, 403, 404) and unreachable proxies are config errors
    - a failed probe marks Codex unhealthy; 2 successes in a row mark it healthy; failures and refusals recorded are kept
    - a probe that cannot run (missing key file, unreachable proxy, 4xx other than 429) is a config error: logged, the main manager told once, Codex not judged
    - a corrupt health file counts as unhealthy (the CLI only reads it; the watcher moves it aside to `.corrupt` and logs) with no known probe, so nothing is swapped until a probe succeeds
    - running agents are never switched mid-turn
- state: `amh-codex-health.json` in the state folder
- settings in `env.toml`: `fallback.proxy_key_file` (the proxy's client key file, read per probe, never logged), `fallback.proxy_url` (default `http://127.0.0.1:18318`)
