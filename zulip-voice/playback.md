# Voice interaction
(authored by agents unless marked 🧑)

reading
- 🧑 "Only read aloud unread messages in autoplay and give ways to allow re-reading earlier stuff"
- autoplay queries the open topic's Zulip unread replies, oldest first
  - loads older replies into view before speaking
  - completed whole-message playback saves Zulip's read flag; cancellation leaves it unread
  - failed read writes retry without repeating completed speech
- 🧑 "Skip titles"
  - omit first-line `[task]` titles
- sentence tap starts rereading; highlight begins when audio playback starts
  - manual scrolling suspends following; Follow reading restores it
- Compose expands controls; playback speed persists locally
- Stop audio disables autoplay and hands-free

hands-free
- 🧑 "listen constantly and identify when I Say pause"
- enabled toggle records complete speech segments while the page is visible
  - silence threshold 600 ms; segment limit 6 s; bounded queue of 4
  - server transcription recognizes standalone pause
- pause stops playback and speaks the response prompt
  - wait for the prompt before replying
  - resume autoplay strips the command, submits the draft once, then restarts the interrupted sentence
  - interruption within its first second restarts the previous sentence
- hiding the page stops microphone capture; reconnecting never replays voice commands
- actual iOS permission, speaker echo, background suspension and Bluetooth behavior require device evaluation
  - echo cancellation requested; headphones reduce speaker feedback
  - server processing latency delays recognition; no instantaneous interruption claim

connection recovery
- 🧑 "The client also needs to be able to handle connection drops in intermittently"
- requests retry transient failures with exponential backoff; header shows reconnecting/offline
- IndexedDB stores submissions and recordings, scoped to Zulip account and mock/live mode
  - submission ID and recipient stay fixed; server receipts prevent duplicate sends
  - queued draft is locked until delivery or explicit Stop retries
  - recordings become reviewable drafts in their original topic after recovery
  - recovered voice commands are not executed
- draft text persists in localStorage; audio remains until transcription reaches durable draft storage
- storage unavailable means no send; storage eviction by the browser cannot be prevented
- tests: `test.mjs`, `recovery-test.mjs`, `handsfree-test.mjs`, `audio-test.py`
  - mock-only browser sends; local generated-audio roundtrip
  - no live sends or production read-flag changes in tests
  - browser tests accept `PLAYWRIGHT_MODULE`; hands-free tests need the running local speech worker
