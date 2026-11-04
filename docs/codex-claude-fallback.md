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
    - brakes before a swap (`Health::swap`)
        - the agent has 3 failures and the latest probe failed; with 3 failures piling up amh probes at once (at most every 5 min), so a proxy restart that Codex survives swaps nobody and the agent gets a retry message
        - at most 3 swaps per hour over all agents; when the cap blocks one, the manager is told (once an hour)
        - a failed swap is retried at most 3 times, 15 min apart, per (agent, failure time)
    - a swap is `agents::replace` with Claude (`launch.claude` model), same task file, original request plus open items
- never swapped (`fallback::refusal`); the manager is told once, the agent is left alone
    - managers or an unrecorded role
    - tasks started with `--model` or `--effort` (`model_pinned: true` in the task file, set by `start_task`)
    - sessions whose `model_override` is not the current `launch.codex.model`
    - remaining gaps: tasks started before `model_pinned` whose model equals the default look unpinned and are swapped; account requirements are not visible to amh (the proxy picks accounts)
- switch back: while unhealthy, `start_task` without `--tool`, `--model` and `--effort` starts workers on Claude (`fallback::default_tool`); explicit `--tool codex` is honored
    - every 15 min one probe: `POST /v1/responses` through the proxy, 16 output tokens, 20 s timeout
    - a failed probe marks Codex unhealthy; 2 successes in a row mark it healthy; failures and refusals recorded are kept
    - a probe that cannot run (missing key file, unreachable proxy) is a config error: logged, the main manager told once, Codex not judged
    - a corrupt health file is moved aside to `.corrupt`, logged, and counts as unhealthy with no known probe, so nothing is swapped until a probe succeeds
    - running agents are never switched mid-turn
- state: `amh-codex-health.json` in the state folder
- settings in `env.toml`: `fallback.proxy_key_file` (the proxy's client key file, read per probe, never logged), `fallback.proxy_url` (default `http://127.0.0.1:18318`)
