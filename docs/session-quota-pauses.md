# Session quota pauses

(authored by agents unless marked 🧑)

- 🧑 "Claude is getting \"Session limit reached”. Let the config agent handle this gracefully instead of keep spamming those agents"
- `quota::paused` persists provider session-limit evidence per Omnigent session
  - named reset timezone determines the next reset, including midnight and daylight saving changes
  - session `updated_at` anchors historical failures; absent timestamps use first observation
  - one send after reset consumes the retry; while the session still shows the same limit error, another send is allowed every 15 min (`RETRY_S`)
    - lost_work_mgr.md: "the stale pause lived in amh's state" ... "PB news skipped ticks for about 5.5 hours": a session can keep its old error after the reset, so waiting for changed evidence alone never ended the pause
  - explicit HTTP rejection or a nonqueued response releases the reserved retry; uncertain transport outcomes retain it
  - a newer failure timestamp recalculates reset even when the error text repeats; stale releases cannot alter that new pause
  - unknown reset waits for changed evidence
- `omnigent::status` returns `paused` while blocked and `ready` when the reset retry is available
  - no failure notices or idle reminders while paused
  - send checks the pause before posting; Claude alone receives the paste submission followup
- pending blocks and unanswered human mail retain their records during pauses
- agent start refuses same-tool replacement of a quota paused session, preserving its requests
  - explicit different-tool migration remains available

start rate
- 🤖 the main manager: "make amh enforce the safe rate itself ... so the whole machine starts at most 2 agents per minute, whoever asks"
- `omnigent::start_slot` gates `launch` and any `send` to a session whose runner is offline (that send restarts the runner)
    - start times of the last minute in `amh-starts.json` in the state folder, under `amh-starts.lock`; a caller over the limit sleeps until a slot frees, giving up after 30 min
    - the watcher never waits (`NO_WAIT`): its send over the limit fails and is retried on a later poll
    - except the delivery of the human's messages and typed blocks (`send_now`), which never waits

partial-compaction moves
- 🧑 "Can we have an additional ID of some sort that would be consistent across partial compaction that amh would use? Otherwise we can let managers refer to agents by their task file names and do hooks to correct session ID mapping"
- opc labels a session it left with `omnigent_partial_compact.moved_to` = the fork's session ID; moves can chain
- `omnigent::moved_to` follows the chain (at most 20 steps)
- every send goes to the end of the chain
- the watcher's minute pass (`agents::follow_moves`) rewrites the task's `runat` and `session_id`, and its workers' `managerat`
- after following a move the watcher retires every session the chain left (`omnigent::retire`: stop event, then `archived: true`; history kept)
- `amh agent retire ADDRESS` does the same by hand; refused for a task's current agent
