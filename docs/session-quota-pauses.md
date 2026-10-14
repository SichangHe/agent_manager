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
