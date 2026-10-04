# Working with agents in tmux

(authored by human unless marked 🤖)

🤖 Agents normally run on Omnigent. This page only matters when an agent runs
in a tmux window, whose address is `SESSION:WINDOW`. Start one with
`amh task start --tmux SESSION` (Codex only).

Tmux sessions whose names start with `h` are human-owned.
Managers and agents MUST NEVER create, replace, restart, stop, or
move agents in
those sessions unless the human explicitly names the exact target and
requests that action.

Tmux sessions whose names start with `h` are reserved for the human.
Agents NEVER touch them, except when the human asks for an agent to talk to
directly, in which case the manager MUST place that agent in such a session.
E.g., if the agent is for `pb`, but the human wants to talk to it directly,
the manager MUST place it in `hpb`.

Each non-`h*`
tmux session MUST have a unique work dir matching the session name.
Try to keep tmux session names within 4 characters and be a bijection with
work dirs.
You MUST only spawn agents in tmux sessions that match their work dir.
ONLY EVER reuse existing tmux sessions and work dirs and
NEVER EVER create dirs without explicit human request or approval.

When the human names `xx` agent to be used, by default interpret it as
spawning/resuming an agent in `xx` tmux session.

If an agent is unclear or unresponsive, ask for a concise report; if needed,
inspect only the last few visible tmux lines as diagnostic output,
not authoritative state.

DO NOT directly call `tmux` commands unless `amh` is broken, in which
case report to the human immediately and spawn a worker to fix it.

When a non-human-owned pane reports `Selected model is at capacity`,
preserve that pane and task.
If retries of `resume` keep failing, switch the model in
the same live Codex pane or stop Codex and resume its session in that
same empty pane.
Launch a replacement pane only when the original pane is unrecoverable.
Report human-owned `h*` panes to the human without altering them.

For Codex, partially compact by sending the manager itself `/compact` via
`amh agent compact`.
Since this is in fact a full compaction,
the manager MUST run all relevant `getagentsmd` commands and
follow these instructions after compacting.
