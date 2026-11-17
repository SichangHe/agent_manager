# Request lifecycle and SQLite storage

(authored by agents unless marked 🧑)

scope
- 🧑 "Yes, switch to SQLite later"
    - source: human reply to the markdown-first question in stored manager email ending `2566.txt`, question 4
- SQLite snapshots are the authority
    - one-time import keeps task history, task index, event history and pending notifications
    - task snapshots, domain events and queued notifications commit together
    - ownership transfer updates both owners in one transaction
    - markdown is regenerated after commits and repaired on the next locked operation
        - body lines typed into a task file since the last export are appended to the task in SQLite first, so a regenerate never drops them; the last export is the record `export:NAME`
    - current events are rendered into `amh_views/events.md`; archived events remain in dated Markdown
    - legacy resolved requests without completion dates start retention at migration
        - this is a retention date, not a claim about their historical completion time
    - `pending_task_items` retains exact compatible item text
    - `request_records` is a JSON array stored as one YAML scalar
    - record: stable id, text, creator, source, stage, blockers, evidence, resolution time
    - IDs survive ownership transfer; `request_next` prevents reuse
- legacy items enter the model without changing text
    - human mark means creator `human`
    - other legacy creators default to the current owner, with original provenance explicitly unknown

transitions
- `todo add` records the caller as creator; `--from-human` records `human`
- `todo done` accepts item text or ID
    - human: resolves immediately, with one email for the batch
    - agent: requests resolution, retains the item, queues a message to its creator
- `todo approve ID` resolves a request created by the caller
    - also before the owner asked for resolution: that withdraws the request, so a creator whose worker is gone can still close
- `todo reject ID --reason REASON` returns it to added
- resolution requested freezes rewording, dependencies, transfer and repeated completion
- `task move` offers ownership; `todo take ID` performs the transfer
    - both owners and their events share one SQLite transaction
- unresolved owners and creators cannot close their agent record
    - explicit human `amh stop` confirmation resolves the owner's items

dependencies and delivery
- blocking applies to individual request items; other ready items keep receiving reminders
    - reminders omit requests awaiting dependencies or resolution approval
    - legacy `status: blocked` still pauses the whole task; a `long_running` task's `blocked_on` does not suppress ready-item reminders
    - unrecorded legacy items remain ready; malformed request metadata falls back to reminding about all open items
- `todo wait ID --for ID` adds dependency IDs; repeated flags permit several
    - rejects missing IDs and cycles; already resolved blockers need no edge
- only resolved requests release edges
    - when the final edge disappears, queue one owner notification
- SQLite messages holds durable delivery intents
    - typed and guest-mail blocks use channel `block`; stored human mail uses channel `human`
    - standalone `(pending)` markers are input compatibility syntax, removed atomically when the block is queued; code-fenced examples remain text
    - existing pending blocks migrate once, retaining their text and distinct duplicate prompts
    - guest intake commits a stable receipt with the queued block; a saved file alone does not suppress retry after a failed queue commit
    - close refuses queued blocks; explicit human stop cancels them as well as queued human mail
    - compatible `request_notifications` remains in task snapshots
    - watcher retries each poll; relevant commands also retry
    - outcome and attempt count survive restarts
    - crash after delivery but before recording its outcome can repeat the message
- recorded event sequence triggers dependency work and pending block scans
    - committed events notify the local `amh-events.sock` listener; wakeups contain no task data
    - notifications are advisory and nonblocking; SQLite remains the durable queue
    - the watcher waits for a notification or its next connector poll, coalescing notifications
    - connector intake remains rate-limited to its existing polling interval
    - connector polls, failed-delivery retries and external agent-health checks remain periodic
- request logic is pure in `requests`; actions and store handle IO

observability
- SQLite events records actor, time, event kind, request ID and data
    - runner address, else explicit `--task-file` identity, else terminal or login name
    - successful request changes and agent-record status/address changes
    - `amh log TASK` includes matching domain events
- `amh-actions.log` retains failed command diagnostics
- SQLite snapshots are state authority; events support coordination and diagnosis
- watcher records import legacy files once; concurrent imports cannot overwrite newer records
    - sent-message lines append atomically for concurrent senders
    - failed imports preserve their files and return errors
    - failure to record a sent message keeps its successful send result, avoiding duplicate retries
- rolling archival and periodic Markdown commits: `archival.md`

validation
- pure tests cover creator decisions, final dependency release and stable transfers
- isolated real CLI test covers legacy adoption, dependency cycle rejection, transfer, close refusal and human quiet resolution
    - no connectors, synthetic servers or email mocks

keeping lists short
- 🧑 "I previously noticed a widespread problem of agents keeping lots of TODOs around. Let the relevant agent study this and how to prevent this"
- 🧑 "Standing rules should be documented instead of being recorded as tasks. This should be in the suitable instructions"
    - ACCEPT: a standing rule is no item; write it into the suitable instructions or ask the manager
- 🧑 "I think tasks can block on other tasks, a date, or else should cause reminders to the responsible agent. Let's disallow blocking on someone. First they need to create a task and let that someone own it, and block on this task"
- waits name a dependency, never someone
    - item: `amh todo wait ID --for OTHER_ID` (request IDs, incl. the human's) or `--until DATE` (blocker `until: RFC3339`)
    - task: `amh task status T blocked --on OTHER.md | --on NAME.md#N | --until DATE` (`blocked_on: until RFC3339`)
    - free text refused with `WAIT_HOW` (actions.rs): give that someone an item and wait for it
- the human's items: task `human.md` (`tasks::HUMAN_TASK`, runat `retired`, no agent)
    - agents add with `amh todo ask-human WHAT` → prints `human.md#N`; text `WHAT (asked by TASK)`
    - the human lists with `amh human`, resolves with `amh human --done ID`; resolving is final whoever asked (`requests::finish`)
- watcher `actions::maintain` every 5 min, under the lock
    - releases a task blocked on a closed task, a resolved item, or a passed date: `running` (long_running stays) and its agent is told
    - drops passed `until:` item waits and tells the task
    - 🧑 "Maybe 1hr, base 1.5?": old free-text waits (`on: WHAT [remind RFC3339 #N]`) and old free-text task blocks (`block_reminder`) are reminded after 1 h, then ×1.5 each time
    - `amh todo wait ID --ask-human` turns an old free-text wait into the human's item plus a dependency
    - dates `added_at` on records: first `request added` event, else the pass time
- flags: over 15 open items or ready items over 7 d old
    - idle reminder adds the counts and the 3 oldest with the choice: done, wait on a dependency, or move a standing rule into instructions
    - `amh ls` rows show `(N open, M ready over 7 d)`
- adding an item that matches an open one, ignoring the human mark, quotes and spacing, is skipped with a note
