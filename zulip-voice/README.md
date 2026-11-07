# Zulip voice
(authored by agents unless marked 🧑)

Talk to your AI agents and listen to their replies, from your phone, using Zulip.

## What it is

- a phone web app: read agent messages aloud, dictate replies, skip around
- a voice service on your server: speech to text (Whisper) and text to speech (Kokoro), on a GPU
- a Neovim plugin can use the same service: https://github.com/SichangHe/nvim_zulip
- Zulip carries everything, including audio, so the phone never connects to your server directly
  - no VPN, tunnel, or open port needed

## How it works

```
phone web app  <->  Zulip  <->  voice service (your server)  <->  GPU workers
 (your key)     text + audio     (a bot account)               Whisper, Kokoro
```

- agents post in one Zulip channel (one topic per agent); the web app reads that channel as you
- the voice service watches a private channel `voice-relay`
  - you dictate: the app uploads a short recording and posts a request there; the service transcribes it and replies
  - an agent posts: the service pre-renders its text to speech and uploads it, so playback starts at once
- audio is stored as Zulip uploads, capped at 100 MB, oldest read clips deleted first
- an agent can end a message with a line `[switch: TOPIC]`; the app then switches to that agent after reading

## Server setup

You need: a Zulip organization, a Linux machine with an NVIDIA GPU (two GPUs recommended), Node 22, `ffmpeg`, `uv`.

1. Zulip accounts
   - your human account; a bot account that the agents post as (create under Settings > Bots)
   - channel for agents, e.g. `agents` (private); every agent uses its own topic
   - channel `voice-relay` (private), both you and the bot subscribed
   - mute `voice-relay` for you and turn off all its notifications (the web app also does this on first login)
2. Credentials (never commit these)
   - human `~/.config/zulip-voice/zuliprc` (download from Zulip: Settings > Account & privacy > Show your API key): the service uses it only to clean up your own leftover messages and uploads
   - bot key in an env file, e.g. `~/.config/amh/zulip.env`: `ZULIP_SITE=https://YOUR.zulipchat.com`, `ZULIP_BOT_EMAIL=...`, `ZULIP_BOT_API_KEY=...`
3. GPU workers (`audio-service.py`, run with `uv run`), one process each:
   - text to speech: `VOICE_AUDIO_KIND=tts VOICE_AUDIO_PORT=8770 CUDA_VISIBLE_DEVICES=0`
   - speech to text: `VOICE_AUDIO_KIND=stt VOICE_AUDIO_PORT=8771 CUDA_VISIBLE_DEVICES=1 VOICE_STT_MODEL=turbo`
   - they listen on loopback only; the first start downloads models
4. Relay service: `node service/relay.mjs` with
   - `VOICE_HUMAN_ID` your Zulip user id, `VOICE_RELAY_STREAM_ID` id of `voice-relay`, `VOICE_TASK_STREAM_ID` id of the agents channel
   - `VOICE_RELAY_STATE` a private directory for its state and `client.log`
   - optional: `VOICE_MAX_UPLOAD_MB` (default 100), `VOICE_HUMAN_ZULIPRC` (default path above)
5. Keep all three running, for example as systemd user units (`systemctl --user enable --now ...`), with `Restart=always`
   - run `loginctl enable-linger $USER` so they survive logout
6. Check: the relay prints `voice service up` and, once a client pings, `ping` lines in `journalctl --user -u <unit>`

## Using the phone app

1. open the web app, tap Connect, paste your `zuliprc` (the `[api]` section: `site`, `email`, `key`)
   - get it in Zulip: Settings > Account & privacy > Manage your API key, then Download zuliprc
   - the key is stored only in this browser
2. tap once anywhere (iOS requires a tap before it plays sound)
3. controls
   - agent chip (top left): switch agent; badge counts unread elsewhere
   - dot (top right): connection; tap it for the debug view
   - tap a message or sentence: read from there on
   - previous / next: one chunk back or forward; hold for a whole message; tapping fast skips many
   - play/pause; `-` `+` speed in 0.1 steps; Auto: read new messages automatically
   - mic: dictate; check mark stops and keeps text, arrow sends at once, X discards
   - images and files show inline; swipe an image sideways to fold it; Settings can hide all
   - settings (top right gear): autoplay, speed, hide attachments, sign out
4. read state only moves forward: reading or skipping a message marks all earlier ones read

## Debugging

- debug view: tap the connection dot; shows connection, event queue, voice service, player, API call rate, and a timeline
  - Send to server: stores the log on the server (also sent automatically after outages and errors)
- self-test: debug view > Self-test; one tap runs every check (ping, audio unlock, speech, microphone, reconnect, resume) and sends a report
- server log: `$VOICE_RELAY_STATE/client.log` (keeps at most 2 files of 100 MB; delete entries with `node service/mark-analyzed.mjs`)
- relay log: `journalctl --user -u <relay unit>`
- tests: `v2/test/unit.mjs`, `v2/test/e2e.mjs [scenario]` (Playwright WebKit, real Zulip and GPU, private test channels)

## Limits

- Zulip rate limit is about 200 calls a minute per user; playback itself makes no calls
- browsers cannot read Zulip upload bytes, so the app cannot cache audio itself; it keeps a few downloaded clips in memory
- audio files are kept on Zulip until the 100 MB cap, then evicted
- English only; the web app is a personal client for one Zulip account

## More

- client design: `v2/design.md`; service protocol (for other clients such as nvim): `service/protocol.md`
- older design notes: `notes-history.md`
- build the static bundle: `node v2/build.mjs OUTPUT_DIR` (publishing is separate)
