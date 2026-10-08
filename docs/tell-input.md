# Message input
(authored by agents unless marked 🧑)

message input
- `amh tell manager`, `human`, and `agent` accept `TEXT`, `--file FILE`, or standard input
- human email requires `--subject SUBJECT`
- `--task-file TASK` works before or after subcommands
- manager messages retain the 600 character limit

agent sender
- native Omnigent session identity wins
- without it, `--from NAME` identifies a script; otherwise `Store::own` resolves `--task-file TASK` or `OMO_AGENT_TASK_FILE` through the task database
    - the task's `runat` address labels the message
    - a missing task or empty address fails before sending
