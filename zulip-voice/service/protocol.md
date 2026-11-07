# Voice service protocol
(authored by agents unless marked 🧑)

- 🧑 "We should separate out the voice services and allow the NeoVim Zulip plugin to also use it."
- one service (`relay.mjs`) turns audio into text and text into audio for any client of the human's Zulip account
  - clients: web client `../v2/`, `nvim_zulip` (Lua, plain `curl` is enough)
  - why Zulip is the transport: the phone cannot reach the server directly; Zulip is reachable from everywhere
- workers it calls on loopback: `audio-service.py` (Kokoro TTS :8770, Whisper STT :8771), user units `zulip-voice-audio`, `zulip-voice-stt`
- service unit: `zulip-voice-relay` (bot identity, outbound only; no listening port)

transport
- private channel `voice-relay`; human subscription muted, all notifications off (clients verify and repair on start)
- request: stream message from the human to topic == `op`, content = JSON with `"voice":1`, `"id"`, `"op"`
  - `id`: unique string `[a-zA-Z0-9_-]{8,100}`; same id is never processed twice
  - messages from anyone else, other topics, or other channels are ignored
- result: stream message from the bot to topic `tts`/`stt`/`ping`, content = JSON `{"voice":1,"id":<request id>,"op":"result","of":<op>,...}`
  - find it by event queue, or `GET messages` narrow channel+topic, match `id`
  - `error` (string) present when processing failed; retry with a new id
  - `timings`: `{queueMs, workMs}` inside the service, for debugging latency
- results and uploads are deleted after `ack`, or after 15 minutes

ops
- `stt`: `{"op":"stt","id":..,"uri":"/user_uploads/...","attachment":"[audio](/user_uploads/...)"}`
  - first upload the audio with `POST /api/v1/user_uploads`, pass the returned `uri`
  - `attachment` (same uri as a markdown link) is required: Zulip only grants the service access to an upload that a message links
  - audio: WAV, MP4/AAC or WebM; at most 12 MiB and 120 s; English
  - result: `text` (empty = no speech)
- `tts`: `{"op":"tts","id":..,"text":"One sentence."}`
  - at most 10000 chars; one sentence or chunk (<= 1800 chars) per request keeps latency low
  - result: `audioURL` (signed temporary URL, valid about 60 s; play it directly in a media element), `uri`, `durationMs`, `text`
  - mint a fresh URL while the upload exists: `GET /api/v1/<uri without leading slash>` returns `{"url":..}`
- `ack`: `{"op":"ack","id":..,"ids":[request ids]}` deletes those results and uploads now (`ids` optional, defaults to `id`)
- `log`: `{"op":"log","id":..,"label":"..","text":"<=9500 chars"}` appends client debug text to `client.log` in the service state dir
  - rotates at 1 MiB to `client.log.1` (disk use under 2 MiB); result `{"of":"log","bytes":n}` is tiny and cleaned like other results
  - clients should delete their own request message after the result (the web client does)
- `ping`: `{"op":"ping","id":..,"t":<client ms>}`
  - result: `t` echoed, `relayMs`, `uptimeS`, `workers` (`stt`,`tts` = `ok`/`down`), `queue`, `active`

pre-rendered speech for agent messages
- when the bot posts in the task channel, the service renders each speakable chunk of it as a `tts` request with `"proactive":true`
  - id `proactive_<messageId>_<chunkIdx>`, extra result fields `messageId`, `sentence` (= chunkIdx)
  - chunking and `speechText` cleaning: `text.mjs` (`speechPieces`); clients import the same file so text matches exactly
  - proactive results are shared: `ack` never deletes them; they expire after 15 minutes
  - a client that connects late reads them with `GET messages` on topic `tts`

limits and ordering
- STT: 2 at once; TTS: 3 at once; explicit requests run before proactive ones
- at most 200 live items; the oldest proactive ones are evicted first
- Zulip HTTP 429 is waited out, not failed
- state: `requests.json` in `VOICE_RELAY_STATE` (ids seen, upload and result message ids for cleanup)

nvim sketch
- STT: `curl -F file=@clip.wav .../user_uploads` then `curl -d type=stream -d to=<relay id> -d topic=stt -d content='{"voice":1,"id":"nv_1234567","op":"stt","uri":"..."}'`, then poll `GET messages` on topic `stt` for that id
- TTS: same with topic `tts`; download `audioURL` and play with any player
