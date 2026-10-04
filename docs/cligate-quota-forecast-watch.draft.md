# Draft: CLI-Gate quota forecast watcher manager guide

Status: draft only. This guide is not governing manager policy until the human explicitly approves it.

Scope: use this only for notifications from the experimental CLI-Gate quota forecast watcher. Do not use it to infer quota from historical task records, and do not reopen closed quota implementation tasks.

## What the watcher measures

- Window: one or two hours of recent CLI-Gate receipt velocity.
- Forecast: current receipt velocity projected to the provider reset time.
- Target: theoretical token pace to expiry, with the desired actual pace between 70% and 95%.
- Notification: only `too_fast` and `too_slow` should notify the main manager.
- Suppression: same-signal notifications are debounced for at least one hour.

The watcher is unavailable when reset time, remaining-token estimate, or receipt velocity is not comparable enough. In that case, do not invent a pace decision.

## Manager response

For `too_fast`:

- Slow discretionary token-heavy experiments through their existing owners.
- Preserve essential work, human-owned panes, production safety work, and already-authorized critical operations.
- Do not create broad new coordination lanes just to pause work.
- Ask owners for scoped slowdowns or checkpoints instead of killing sessions blindly.

For `too_slow`:

- Pull forward useful deferred work through existing owners.
- Prefer bounded, already-planned tasks over exploratory bulk work.
- Keep paid, browser-mutating, or credential-sensitive work behind its normal approval and safety checks.
- Recheck quota after a major task completes or after the forecast materially changes.

For `on_track`:

- No manager action is required.

For `unavailable`:

- Use the normal quota summary/help output and its reliability metadata.
- Preserve capacity for important work until comparable target and velocity inputs exist.

## Approval boundary

This draft can be cited in an implementation status email as the proposed manager guide. It must not be treated as persistent operating policy or copied into the getagentsmd manager instructions without explicit human approval.
