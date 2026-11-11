# Turn replies
(authored by agents unless marked 🧑)

- 🧑 "Agents really love using the terminal printout for reply, so let's automatically send whatever the agent said at the end of the turn. This also needs to be worked on s.t. pending item notifications and messages replacements work"
- code: `src/replies.rs`

what
- an agent's last assistant text of a turn goes to whoever started the turn
    - human message delivered by the watcher → human
    - `amh tell agent` from an agent → that agent; via `agents::report` when it is the task's manager
- skipped when the turn ran `amh tell human` (human) or `amh tell manager|agent` (agent)
    - recipient = first word after `amh tell`, past `--task-file NAME`
    - so explicit messages are never repeated

how
- expectation: file `amh-replies/SESSION_ID` in the state dir, `FROM EPOCH_S`
    - time taken before sending; written after it succeeds
    - a later delivery overwrites it: the turn's text goes to the latest sender
- watcher poll `turn replies`: per file, when the session is `ready` (idle)
    - items since `EPOCH_S`, newest 500: an assistant text means the turn ended
    - none yet → wait; after 12 h → drop
    - file removed after sending only if unchanged meanwhile
    - failed send retried each poll until 10 min after delivery
- human: same `actions::tell_human` path as `amh tell human`
    - recorded and replaceable with `--replaces`; checked against `human.writers`
    - consolidation reminder, when any, is sent to the agent with the Message-ID
    - not a writer → goes to its manager instead
- agent: clipped to the manager message limit, labeled `TASK (end of turn):`

limits
- a turn with over 500 items may miss an earlier `amh tell` and send a duplicate
- turns not started by a delivery (reminders, item notices, starter prompts) send nothing
