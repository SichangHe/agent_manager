# Using amh from an editor plugin

(authored by agents unless marked 🧑)

🧑 "Cooperate with claude_support agent to also support using amh: sending messages to agents by editing the task file etc."

what a plugin can call today; every command prints plain text and exits non-zero with one line `amh: ...` on a refusal
- names: a task is `NAME.md` in the work-log root; `WHO` below is that file name or an agent address `omnigent://ID`
- send the human's request to an agent
    - `amh task edit NAME.md`
        - opens `$VISUAL`, else `$EDITOR`, else `vi` on a copy of the task file and waits for it to exit
        - text added at the end is delivered to the task's agent within seconds, like an email from the human
        - any other change is refused; it then asks `Reopen the copy to fix it? [Y/n]` on standard input
        - from inside an editor: set `EDITOR` to a command that opens the copy in the running editor and returns when the buffer is closed
    - without amh's editor step: append a blank line, a line `(pending)`, then the text with no blank line inside it, to the end of `NAME.md`; the watcher delivers the block and deletes the `(pending)` line
        - only append; never rewrite the file, other writers append to it at any time
        - a blank line ends the block, so `amh task edit` is the way for text with paragraphs
- look
    - `amh agent tree`: who reports to whom, with open items
    - `amh agent list`: one line per active task: `STATE: NAME.md [status] ADDRESS evidence`; states `running`, `ready`, `stuck`, `error`, `missing`
    - `amh agent status WHO`
    - `amh task show NAME.md...`: the fields and open items
    - `amh agent view WHO [--read-only]`: attaches the calling terminal to the agent's own tmux terminal; needs a real terminal, e.g. a terminal buffer
- control
    - `amh task start NAME.md --dir DIR --prompt FILE [--tool claude|codex|cursor|antigravity] [--as-manager]`: prints the new agent's address
        - restarts failed or stopped agents on the selected tool, keeping task history and open items
        - managed tasks follow the replacement address; restarting the main manager updates its configured address
        - stop running, idle, or waiting agents with `amh agent stop NAME.md` first
    - `amh agent stop WHO`; `amh task close NAME.md`
- where things are
    - the work-log root and other machine settings: `env.toml` in `$AMH_STATE_DIR`, else `$XDG_STATE_HOME/amh`, else `~/.local/state/amh`; key `work_logs`
    - what amh did: `amh-actions.log` (every run) and `amh-watch.log` (mail in, deliveries, problems) in the same folder

not stable yet
- short commands for the human (`amh new`, `ls`, `view`, `say`, `edit`, `stop`, `log`) are built; the human may still drop them
- there is no machine-readable output; ask the amh maintainer (task `claude_support.md`) for what the plugin needs, e.g. `--json` on `agent list`
