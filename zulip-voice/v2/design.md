# Voice client v2
(authored by agents unless marked 🧑)

why a rewrite
- 🧑 "Replace this agent. This app is all garbage. Redesign and rebuild from scratch and only reuse code as suitable"
- 🧑 "Stop suggesting using a tunnel. We are using the Zulip servers as the tunnel. The real problem is not Zulip. It is garbage front end."
- 🧑 "Having some delay between sending and receiving or playing things is normal, but the UI should not be glitched"
- old client (`../index.html` etc.) stays in place and keeps working; v2 shares only the account key `zulip-voice-account` and the relay protocol

shape
- static files, no framework, no server; browser talks to Zulip directly as the human
- speech goes through the voice service (`../service/protocol.md`), a consumer-agnostic API shared with nvim
- files (each one concern)
  - `text.mjs`: pure; message parsing, markdown blocks, speech chunks; same file used by the service (`speechPieces`)
  - `model.mjs`: pure state; messages per topic, unread, play items
  - `zulip.mjs`: REST client returning `{ok,...}` unions; event loop with watchdog
  - `voice.mjs`: STT/TTS/ping over the relay channel; pre-rendered clip cache
  - `player.mjs`: clip queue on two `<audio>` elements
  - `recorder.mjs`: mic to 16 kHz WAV clips cut at pauses
  - `view.mjs`: DOM builders; `app.mjs`: wiring only
  - `log.mjs`: debug timeline; `build.mjs`: hashed static bundle
- tests: `test/unit.mjs`, `test/e2e.mjs` (real WebKit iPhone viewport, real Zulip, relay and GPU); see `test/lib.mjs`

screen
- messages fill the screen; top bar hides while scrolling down
- agent chip (top left) opens the agent list; dot shows connection; tap dot for debug
- agent text is plain, the human's is a bubble; unread agent messages have a dot
- dock: previous, play/pause, next, speed (0.1 steps, 0.5 to 3.0), Auto; composer with mic and send
  - dock transport row hides when scrolled away and idle
- account setup, autoplay and speed live in Settings; debug is one tap from the connection dot

reading
- reading unit = chunk: a few sentences of one markdown block, at most 240 chars; titles and code blocks are not spoken
- tap a message or sentence: read from there and continue through all later agent messages
- previous/next move by message; previous restarts the current message first
- autoplay on (default, only the user turns it off): reads only unread agent messages of the open agent, oldest first
  - new messages append to the running queue; playback never turns autoplay off
  - a message is read after its last chunk ends (flag sent to Zulip in batches, retried until accepted, persisted)
  - interrupted or paused messages stay unread
  - needs one tap after page load (iOS); the banner says so instead of failing silently
- `[switch: TASK]` line in a new agent message: hidden; after that message is spoken the app switches to TASK (toast with Back)

audio engine (`player.mjs`)
- iOS plays an element without a gesture only after it played once from a gesture
  - `unlock()` primes both elements on the first `touchend`/`click`; no element is ever created later
- clips are requested up to 3 ahead; the next element loads only when the current one ends
  - why: loading a second element during playback stalled WebKit playback 1.2 s (measured); gap now about 150-220 ms
- `NotAllowedError` gives status `blocked` and a tap banner; load/network errors give `error` and retry 3 times with backoff
- stall watchdog: no `currentTime` progress for 7 s reloads the clip

connection (`zulip.mjs`)
- one event queue for task messages, read flags, and relay results
- long poll with 65 s client timeout, quick (non-blocking) first poll after every restart
- restart triggers (`kick`): `online`, `pageshow`, page visible, frozen timer (>10 s gap), stalled poll
- queue lost: re-register, reload the open agent, treat missing messages as new
- failures back off 0.5 s doubling to 8 s; state `live`, `retrying`, `offline` shown by the dot
- all HTTP calls log method, path, status and ms into the timeline

dictation
- tap mic: audio is cut into clips at 0.7 s pauses or 10 s; each clip uploads and transcribes independently, text appears in order
- check button stops and keeps text; send button stops, waits for pending clips, and sends once
- cancel discards; failed clip shows a retry banner; playback pauses while dictating and resumes after

sending
- optimistic bubble; POST carries `local_id` and the queue id; the echo event removes the bubble
- uncertain failures (timeout, 5xx) check the topic for the same text before a retry is allowed to resend

debug (`conn` dot)
- status block: page visibility, event loop state, queue id, last ok age, voice ping, player slots
- timeline: connection, HTTP, events, relay requests with relay-side `queueMs`/`workMs`, audio element events, mic clips
- persisted (last 1500 lines), so a reload keeps the previous session
- buttons: Copy all, Test voice (ping), Reconnect, Clear

deploy
- `node build.mjs DIR` writes `index.html` plus hashed modules (`DIST=DIR node test/e2e.mjs` tests the bundle)
- publish (needs the human's approval): clone the Pages repo, `git rm` its old files, copy DIR in, commit, push

self-test and log shipping
- `test.html` + `selftest.mjs`: tap-to-run phone checks (ping, audio unlock, TTS, two-clip stall, mic+STT, airplane reconnect, lock resume)
  - results are sent to the service after every step; Copy also works
- debug "Send to server" and an automatic send after each outage (at most every 2 min) use the relay `log` op
- server file: `~/.local/state/zulip-voice/relay/client.log` (rotates at 1 MiB); client log in localStorage capped at 800 lines

cleanup (storage)
- clients delete their own request messages right after the result; relay deletes results after ack or 120 s, pre-rendered speech after 15 min
- relay sweep (every 15 s) deletes anything left in the relay channel past those ages, plus `dictation-*` (human) and `voice-*` (bot) uploads
  - needs the human key (`VOICE_HUMAN_ZULIPRC`, default `~/.config/zulip-voice/zuliprc`, server side only); env `VOICE_ORPHAN_TTL_S`, `VOICE_PROACTIVE_TTL_S`
- test: `test/e2e.mjs relayCleanup` (channel and uploads drain to empty)
- self-test is one Start tap; the mic step runs first because iOS audio contexts only start inside that tap

audio delivery and API budget (Zulip allows about 200 calls/min per user)
- constraint: browsers cannot `fetch` Zulip upload bytes cross-origin (no CORS); only `<audio>` elements load them
  - so the unit of delivery is a media file the element downloads itself; bytes cannot be pre-held as Blobs
- the service renders each agent message into bundles: a head bundle (chunk 0) then one tail bundle (all other chunks), AAC-LC mono 32 kbps `.m4a`
  - about 4.7 kB/s against 48 kB/s for the old 16-bit WAV (10x smaller); `offsetsMs` give each chunk's start inside the file
  - playback seeks inside one element: skip and chunk hand-over need no new request; the next bundle is loaded into the idle element once the current one is fully buffered
- the service renders all unread agent messages (also the backlog after a restart, listing via the human key every 10 min)
  - clips have no TTL; total cap 100 MB on Zulip; over the cap, read messages' clips go first, then oldest
- the client discovers bundles from the event stream and one backfill call per (re)connect; if none exists it asks for up to 6 chunks in ONE `tts` request (`texts[]`)
- calls per message on the phone: 0 for playback (events carry the bundle links), 1 for the read flag (batched every 15 s)
- calls per message on the service (bot key): upload 1, link 1, result post 1; deletes only at eviction
- service sweeps: expire every 60 s (no calls when nothing is due), orphan sweep every 5 min (3 list calls, at most 40 deletes)
- client no longer deletes its own request messages or sends acks; the sweep removes them after 2 min
- idle: only the event long poll; `e2e apiBudget` asserts under 60 calls/min over a 10 message session and at most 2 idle calls in 20 s
- 429: the client stops calling until Retry-After, shows "rate limiting" in the status strip; the relay waits and retries

reading state
- read flags are a forward-only watermark per topic: reading or scrolling past message N reads every earlier unread one
- last listening position (topic, message, chunk) is kept in localStorage on every chunk; reload resumes there
- scrolled-past messages (user-driven scroll only) become read

dictation clips are 16 kHz 8-bit mu-law WAV (16 kB/s, half of 16-bit); send never goes out with a failed clip
