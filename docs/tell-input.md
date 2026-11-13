# Message input
(authored by agents unless marked 🧑)

message input
- `amh tell manager`, `human`, and `agent` accept `TEXT`, `--file FILE`, or standard input
- human messages need no subject: the thread is the task tag; `--subject` is accepted and ignored
    - 🧑 "Zulip does not need a title, it just needs a thread"
    - kept only as a report label naming a `zulip.report_streams` entry (PB news); `--report-label` sets it
- `--task-file TASK` works before or after subcommands
- manager messages retain the 600 character limit

PB news reports
- `stock_news_watch` can send with `--report-label 'pb news'` or `--report-label 'pb urgent'`
    - 🧑 "it's pb news, not pb new"; `pb new` stays accepted for old threads
- `--email` sends that one message by email under `human.transport=zulip`
    - 🧑 "Send the news via email like done before instead of through Zulip"
    - body with Markdown links `[label](url)` gets an HTML part showing only labels
    - email replies to `[pb news]`/`[pb urgent]` route to `stock_news_watch`
    - a later email with the same label and subject answers the previous one, so they form one thread
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
