# Message input
(authored by agents unless marked 🧑)

message input
- `amh tell manager`, `human`, and `agent` accept `TEXT`, `--file FILE`, or standard input
- human email requires `--subject SUBJECT`
- `--task-file TASK` works before or after subcommands
- manager messages retain the 600 character limit

PB news reports
- `stock_news_watch` can send with `--report-label 'pb new'` or `--report-label 'pb urgent'`
- `--email` sends that one message by email under `human.transport=zulip`
    - 🧑 "Send the news via email like done before instead of through Zulip"
    - body with Markdown links `[label](url)` gets an HTML part showing only labels
    - email replies to `[pb new]`/`[pb urgent]` route to `stock_news_watch`
    - label becomes the body prefix and topic; task ownership and open items remain `stock_news_watch`
    - replies in either topic and quoted direct replies route to the same task
    - cleanup checks the owning task before deleting old reports
- other tasks and email transport reject this option before item changes or delivery

agent sender
- native Omnigent session identity wins
- without it, `--from NAME` identifies a script; otherwise `Store::own` resolves `--task-file TASK` or `OMO_AGENT_TASK_FILE` through the task database
    - the task's `runat` address labels the message
    - a missing task or empty address fails before sending

task launch parent
- `task start` uses explicit `--manager` first, then native caller, then the address of `--task-file` or `OMO_AGENT_TASK_FILE`
    - unresolved caller fails before launch; an empty caller address requires explicit `--manager`
