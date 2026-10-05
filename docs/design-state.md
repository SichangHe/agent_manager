# Design proposal: state model, request lifecycle, dependencies, events

(authored by agents unless marked 🧑; a proposal for the human to accept, change, or reject; nothing here is built)

sources: the human's words in `docs/vision.md` (`M/N:L` = email N, line L)

terms
- agent record: what a task file `NAME.md` is today: one agent, where it runs, its manager
- request: what an open item is today: one thing somebody asked an agent to do
    - 🧑 M/554:16 "'Add' task when agent put them into the queue, which should have a source (exact email/file edit)"
    - assumption to confirm: the human's "task" with three stages = one request, not one agent
- creator: who asked: `human` or an agent
- owner: the agent that must do it

goal
- 🧑 M/43:7 "I would like state consistency and state stored in SQLite."
- 🧑 M/585:3 "keep doing SQLite for now but also emit events for internal coordination"
- 🧑 M/480:21 "have the internal model of what is going on separate from all the interfacing the tools"
- 🧑 M/549:3 "notify task owners when all the tasks their task it is blocked on have been done"
- 🧑 M/570:6 "log all the events (resolution requested, etc.) and who did those"

state
- one SQLite file `amh.db` in the state folder; the only authority
- tables
    - `agent`: name, address, manager, role, status, tool
    - `request`: id, owner, creator, text, source, stage, created, resolved
        - source: the stored email and line range, or the command that added it
    - `blocks`: blocker request -> blocked request
    - `event`: id, time, actor, kind, subject, data
        - append-only; written in the same transaction as the change it records
    - `message`: queued deliveries to agents and to the human, with their outcome
        - replaces `(pending)` markers and the `amh-*.txt` records
- markdown
    - 🧑 M/587:3 "Keep everything displayed in markdown tho, so agents can grep them"
    - 🧑 M/480:11 "the markdown files are read only"
    - `NAME.md` per agent and `TODO.md` are rewritten from the database after each change
    - 🧑 M/591:9 "one file per task and include only human prompts"
    - resolved requests older than 30 days leave the database and stay in dated markdown (🧑 M/587:3, M/570:6)
    - 🧑 M/591:15 the markdown folder stays a git repository, committed periodically

request lifecycle
- stages (🧑 M/559:3 "only be three stages ... resolution requested and resolved")
    - `added` -> `resolution requested` -> `resolved`
    - `resolution requested` -> `added` when the creator rejects
- who moves it
    - owner: add (for a request it received), request resolution
    - creator: approve or reject (🧑 M/554:18 "need task creator approval")
    - owner is frozen while resolution is requested (🧑 M/570:4)
- creator = human
    - 🧑 M/554:16 "Both should notify the human"
    - open question 2 below: does the human approve, or is the owner's word final as today
- ownership
    - 🧑 M/555:3 the adder owns; a delegate "needs to explicitly transfer task ownership to themselves"
    - one owner per request, always (🧑 M/1730:18 "we will prevent them")
- keeping agents alive
    - an agent that owns an unresolved request, or created one, is not closed (🧑 M/554:18, M/570:4)

dependencies
- `amh` records "request A waits for request B"; free-text `blocked_on` goes away
    - 🧑 M/2230:8 "make it clear what the unblocking condition would be"
- a blocker counts as done only when `resolved` (🧑 M/560:11 "The latter")
- when the last blocker of a request resolves: its owner gets one message naming it
- a blocked owner gets no idle reminders (🧑 M/53:3, M/532:10)
- waiting on the human = waiting on a request owned by the human planner (🧑 M/652:3)
- refused at entry: a cycle; a blocker that does not exist
- a blocker whose owner is gone: reported to the manager once, like a missing agent today

events
- every change is one event; the kinds are an enum
    - request added / resolution requested / resolved / rejected / moved / blocked / unblocked
    - agent started / stopped / closed / rotated / problem found / problem gone
    - message queued / delivered / failed / rerouted; human mail received / sent
- actor: the caller's agent address, `watcher`, or `human`
- the watcher works from events instead of scanning files every 5 s
- `amh log` prints events, filtered by agent, request, or time; `amh-actions.log` stays for failed commands

logic and input/output
- 🧑 M/49:13 "have the logic separate from IO ... make it so that the program program can run in simulation"
- one pure function: `step(state, command or outside fact) -> (events, effects)`
    - effects are data: send this message, send this email, start this agent
    - a thin runner applies events to SQLite and carries out effects
- this makes a simulated run possible later without changing the logic
    - 🧑 M/2470:11 "I don't think there's need to write a bunch of fake tests right now": no simulator is built now; unit tests on `step` only

commands (names to settle)
- `amh todo add`, `amh todo done` keep working: `done` = request resolution
- new: `amh todo approve ID`, `amh todo reject ID REASON`, `amh todo take ID`, `amh todo wait ID --for ID`
- `amh todo list` shows id, stage, and what each request waits for

order of building, each step in production before the next
1. event table, written by today's actions next to the markdown (no behaviour change)
2. requests in SQLite; markdown items rendered from it; ids shown
3. stages with approve and reject
4. dependencies and the unblock message
5. messages table replaces `(pending)` markers; watcher driven by events
6. agent records in SQLite; task files fully generated

open questions for the human
1. is the three-stage "task" one request (an open item today), with the task file being one agent's list
2. for the human's own requests: must the human approve a resolution, or does the owner's `done` resolve it with an email, as today
3. the human sometimes types notes into a task file; with generated markdown that would have to go through email or a command instead
4. SQLite as the authority now, or first add stages, dependencies and the event log on top of today's markdown files (smaller, no generated files)
