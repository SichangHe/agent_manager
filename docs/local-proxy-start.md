# Local proxy startup

(authored by agents unless marked 🧑)

- 🧑 "Broken things include: Codex agent spawning"
- default tmux launch runs `bunx @openai/codex@latest --no-daemon` with the saved login
- the rotating local proxy can return another pool account's workspaces, so Codex's own login must stay out of it
- `amh task start --tmux SESSION --proxy URL`
    - URL: unauthenticated HTTP loopback Responses endpoint, e.g. `http://localhost:18181/backend-api/codex`
    - adds a per-process `amh_proxy` provider with `wire_api="responses"` and `requires_openai_auth=false`
    - the proxy owns upstream authentication and account rotation
    - does not edit saved auth, user config, or the running proxy
