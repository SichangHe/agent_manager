# Markdown request lifecycle

(authored by agents unless marked 🧑)

scope
- 🧑 "Yes, switch to SQLite later"
    - source: human reply to the markdown-first question in stored manager email ending `2566.txt`, question 4
- markdown remains the authority
    - `pending_task_items` retains exact compatible item text
    - `request_records` is a JSON array stored as one YAML scalar
    - record: stable id, text, creator, source, stage, blockers, evidence
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
- `todo reject ID --reason REASON` returns it to added
- resolution requested freezes rewording, dependencies, transfer and repeated completion
- `task move` offers ownership; `todo take ID` performs the transfer
    - two markdown after-images enter a recovery file before either file is replaced
    - next store lock completes interrupted transfer
- unresolved owners and creators cannot close their agent record
    - explicit human `amh stop` confirmation resolves the owner's items

dependencies and delivery
- `todo wait ID --for ID` adds dependency IDs; repeated flags permit several
    - rejects missing IDs and cycles; already resolved blockers need no edge
- only resolved requests release edges
    - when the final edge disappears, queue one owner notification
- `request_notifications` holds delivery intents in markdown
    - watcher retries each poll; relevant commands also retry
    - intent removed after successful delivery
    - crash after delivery but before removal can repeat the message
- request logic is pure in `requests`; actions and store handle IO

observability
- `amh-events.jsonl` records actor, time, event kind, request ID and data
    - runner address, else explicit `--task-file` identity, else terminal or login name
    - successful request changes and agent-record status/address changes
    - `amh log TASK` includes matching domain events
- `amh-actions.log` retains failed command diagnostics
- state replacement and event append are separate writes
    - an event IO failure reports failure after state was saved
    - events are diagnostics, not a replay authority
- resolved records and events remain on disk
    - SQLite migration, event-driven watcher and rolling archival remain later work

validation
- pure tests cover creator decisions, final dependency release and stable transfers
- isolated real CLI test covers legacy adoption, dependency cycle rejection, transfer, close refusal and human quiet resolution
    - no connectors, synthetic servers or email mocks
