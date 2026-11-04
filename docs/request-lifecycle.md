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
    - legacy `status: blocked` and `blocked_on` remain readable but no longer suppress ready-item reminders
    - unrecorded legacy items remain ready; malformed request metadata does not produce reminders
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
