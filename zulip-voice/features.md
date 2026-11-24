# Feature parity: web app and Neovim plugin
(authored by agents unless marked 🧑)

Web = `zulip-voice/v2/` (W). Neovim = `/ssd1/sichangheagent/nvim_zulip/lua/zulip/` (N). Both call the relay (`service/relay.mjs`). Statuses were checked by reading code on 2026-11-23. Update this file in the same commit as any user-visible change to either client.

## Reading and playback

- Highlight only the sentence being spoken. Web highlights the one sentence (`.s.now`). Neovim highlights the sentence too; its position inside a chunk is estimated from elapsed audio.
  - web: done (app.mjs `highlight`)
  - nvim: done (player.lua `chunk_range`, `sentence_at`)
- Follow: keep the spoken part in view, stop following when the user scrolls or moves the cursor.
  - web: done (app.mjs `scrollToNow`, `follow`, `updateJump` shows a jump button)
  - nvim: done (player.lua `highlight(run, true)`, `run.following`; `I` toggles)
- Play from a chosen message or sentence, tap or key again to pause and resume mid-chunk.
  - web: done (app.mjs `playFrom`, `togglePlay`)
  - nvim: done (conversation.lua, player.lua `toggle_pause`; keys `i`, `r`, `x`)
- Previous and next chunk, previous and next message, presses in quick succession add up; previous message first restarts the current one.
  - web: done (app.mjs `jump`, `navigate`)
  - nvim: done (player.lua, keys `o O a A`)
- Autoplay of unread messages from others when a conversation is open or a message arrives.
  - web: done (app.mjs `maybeAutoplay`, setting `set-auto`)
  - nvim: done (conversation.lua `player.autoplay`; `gA`, `:ZulipAutoplay`)
- Playback speed 0.5 to 3.0 in steps of 0.1.
  - web: done (app.mjs `setRate`)
  - nvim: done (player.lua `atempo` filter; keys `s S`)
- Message directive `[switch: TASK]` opens that task once the message has been read; the line is hidden.
  - web: done (app.mjs `pendingSwitch`, `runSwitch`)
  - nvim: done (conversation.lua line ~452)
- Read-position sync: reading marks messages read forward only, shared through Zulip read flags.
  - web: done (reads.mjs `markRead`, queued and retried offline)
  - nvim: done (conversation.lua mark-read on cursor passing the last line; player marks as spoken). No offline queue.
- Resume where you left a conversation.
  - web: done (app.mjs `voice2-pos`, watermark per topic, restored on reload)
  - nvim: done for the cursor, within the session (conversation.lua line ~70). unsure whether it survives a restart: no code writes it to disk.
- Earcons: ready, done, error tones.
  - web: done (recorder.mjs `EARCON_NOTES`, player.mjs `earcon`)
  - nvim: done, same notes as the web (voice.lua `EARCON_NOTES`: ready, done, cancel, command, tick, empty, error, beats); no send-failed, interrupted or updated tones
- Mode beats: a quiet double beat every 5 s while a session is silent, plus an on-screen "still listening" bar.
  - web: done (voicecontrol.mjs `MODE_PULSE_MS`, `MODE_BANNER_MS`)
  - nvim: done, beat only, no bar (handsfree.lua `pulse_tick`)
- Message edited or deleted while reading: stop, tone, re-read from start.
  - web: done (app.mjs `onMessageEdited`, `onMessageDeleted`)
  - nvim: unsure. Events reload the buffer (events.lua) and player re-highlights (`player.lua` line ~142), but no code re-reads edited text.
- Lock-screen and media controls, screen wake lock.
  - web: done (app.mjs `setMediaSession`, `syncWakeLock`)
  - nvim: n/a (terminal)
- Local speech: text-to-speech made on the user's machine instead of the service.
  - web: missing (no mac_tts reference in v2/ or the relay; speech comes from the relay `tts` op, voice.mjs `ttsBatch`)
  - nvim: done (local_tts.lua, option `voice.local_tts = { "mac-tts" }`)
- Image previews under their link, PDF first page.
  - web: done (view.mjs `attachmentsEl`; setting "Hide attachments"). PDF first page: unsure, not read.
  - nvim: done (viz.lua; needs snacks.nvim and a graphics terminal; `p` toggles)
- Open an uploaded HTML or file.
  - web: done (view.mjs attachment chips and links)
  - nvim: done (viz.lua `open`, `gx`; uses `viz` command if present)
- Tables read cell by cell.
  - web: done (using.html; text.mjs)
  - nvim: unsure, not checked in voice.lua `speech_text`.

## Voice control

- Start word ("attention"): pauses reading, ready tone, what you say next becomes the reply. Fuzzy match by sound.
  - web: done (grammar.mjs `afterStartWord`; also on-device in voicecontrol.mjs `onKwsHit`)
  - nvim: done (handsfree.lua `after_start_word`)
- End word ("tomato"): ends and sends; also ends after silence.
  - web: done (grammar.mjs `stripResumePhrase`, voicecontrol.mjs; long silence default 20 s)
  - nvim: done (handsfree.lua `strip_end_word`; long silence default 6 s, `voice.long_silence_s`)
- Cancel word ("macaroni", plus "cancel", "never mind", "scratch that", "forget it"): discards the reply or command anywhere.
  - web: done (grammar.mjs `isCancel`, `hasCancelWord`, `endsWithCancel`)
  - nvim: done (handsfree.lua `ends_with_cancel`, commands.lua)
- Command mode, entered with the command word "katana" (alone or "katana next two").
  - web: done (voicecontrol.mjs `enterCommandMode`, `commandFirst`)
  - nvim: done (handsfree.lua `command_words`, `run_commands`; commands.lua `parse`); confirmed aloud before acting (voice.lua `say`)
- Skip forward and backward with a count ("skip forward three", "next three", "back two").
  - web: done (grammar.mjs `navCommand`)
  - nvim: done (commands.lua `nav`, a port of `navCommand`)
- Previous and next message ("previous message", "next message").
  - web: done (grammar.mjs `navCommand` `whole`)
  - nvim: done (handsfree.lua `M.navigation`)
- Next and previous chunk, "again".
  - web: done (grammar.mjs `navCommand`)
  - nvim: done ("back", "next", "again", "repeat" in handsfree.lua)
- Restart: replay the current message from its first sentence.
  - web: done (grammar.mjs `navCommand` `restart`)
  - nvim: done (player.lua `restart_message`)
- Pause and resume by voice ("stop", "pause", "play", "resume").
  - web: done (voicecontrol.mjs lines ~326-327)
  - nvim: done (commands.lua `parse` pause/play)
- Switch to agent by fuzzy name, top three offered, pick "first/second/third" or "number one".
  - web: done (grammar.mjs `rankTopics`, `choiceAnswer`; optional "Switch to X?" confirm)
  - nvim: done (handsfree.lua `switch_to`, `ORDINALS`; uses switch.lua `matches`). No yes/no confirm.
- Switch back to the previous agent (voice, and a back-arrow button).
  - web: done (app.mjs `switchBack`, `SWITCH_BACK_RE`)
  - nvim: done by voice and `gvT` (handsfree.lua `switch_back`)
- Next agent with unread messages (voice, and a button with a count).
  - web: done (app.mjs `switchNextUnread`, `NEXT_AGENT_RE`)
  - nvim: done by voice and `gvn` (handsfree.lua `next_unread`)
- Send log by voice ("send log").
  - web: done (grammar.mjs `SEND_LOG_RE`, app.mjs `sendLog`)
  - nvim: done by voice (commands.lua) and by key (`gvL`, `:ZulipVoiceLog`, voice.lua `send_log`)
- On-device keyword spotting (words detected without uploading audio, so no round trip).
  - web: done (kws.mjs, voicecontrol.mjs `onKwsHit`; setting `set-kws`, about 300 MB)
  - nvim: missing (no spotter; words are found in server transcripts, so each costs an upload and a wait)
- Spoken confirmation ("Now on X", "paused") and "Command not recognized" with the command list.
  - web: done (voicecontrol.mjs `announceSwitch`, `sayAndMute`)
  - nvim: missing (status shown by notification only)
- Visual state of voice control.
  - web: done (voicecontrol.mjs `setVcState`, session text bar)
  - nvim: done (status line label `voice control: reply|listening`, handsfree.lua `M.label`)
- Echo and barge-in handling: a non-command speech does not leave playback paused.
  - web: done (voicecontrol.mjs `bargePaused`, `resumeBarge`)
  - nvim: unsure. README says use headphones; no equivalent code seen.

## Dictation

- Start and stop dictation by a button or key; text appears live in the draft piece by piece.
  - web: done (dictation.mjs, recorder.mjs)
  - nvim: done (voice.lua `dictate`, `<C-g>`, `p`)
- Send once after all pieces are transcribed.
  - web: done (dictation.mjs; arrow button)
  - nvim: done (voice.lua `send_when_done`, `<CR>` while recording)
- Discard a dictation.
  - web: done (X button)
  - nvim: done (voice.lua `cancel`, `<Esc>` in normal mode)
- Keep audio from before the first word.
  - web: unsure, not checked in recorder.mjs
  - nvim: done (1.2 s pre-roll, voice.lua)
- Microphone choice.
  - web: unsure, browser picks the microphone; no chooser seen in settings
  - nvim: done (`:ZulipVoiceDevice`, `gvm`, `voice.device = "auto"`)
- Local speech-to-text on the user's machine (mac_stt).
  - web: missing (no mac_stt reference anywhere; STT goes through relay `stt`, voice.mjs `stt`)
  - nvim: done (voice.lua `stt` with `voice.local_stt = { "mac-stt" }`); untested on a Mac
- Unsent recordings survive reloads and are retried.
  - web: done (recorder.mjs queue, per using.html)
  - nvim: missing

## Navigation

- Switch conversation by fuzzy name picker.
  - web: done (app.mjs `openPicker`)
  - nvim: done (switch.lua, `t`, `:ZulipSwitch`)
- Follow a task tag to its topic.
  - web: n/a
  - nvim: done (conversation.lua `gf` on `[task]`)
- Load earlier messages.
  - web: done (app.mjs `loadEarlier`)
  - nvim: n/a (buffer shows `n_shown` newest; `R` reloads). Older messages are not reachable.

## Conversation list

- Priorities ordering from the human manager (lowest first, then newest).
  - web: done (model.mjs `allTopics`, `setPriorities`; app.mjs `checkVoice`)
  - nvim: done (sidebar.lua `refresh_priorities`, voice.lua `M.priorities`)
- Hide closed threads, unless unread or currently open.
  - web: done (model.mjs `setClosed`, `topics()`; keeps open topic)
  - nvim: done (sidebar.lua line ~72 with `voice.closed`). Hides unless unread. unsure: does not exempt the open thread (web does via `keep`).
- Unread counts.
  - web: done (view.mjs `topicRow`, `setUnread`)
  - nvim: done (sidebar.lua)
- Mark read without playing.
  - web: done (Mark read button, reads.mjs `markOne`)
  - nvim: done (cursor past the message)
- Live updates.
  - web: done (app.mjs `onEvent`)
  - nvim: done (events.lua, long polling)
- Muted channels hidden.
  - web: done (app.mjs `defaultChannels` leaves out muted subscriptions)
  - nvim: done (sidebar.lua `refresh_muted`)

## Messages and compose

- Send a reply by text.
  - web: done (app.mjs `send`)
  - nvim: done (compose buffer, `<CR>`, `:w`)
- Upload files and photos.
  - web: done (app.mjs `attach-input`, line ~672)
  - nvim: done (`:ZulipUpload PATH` in a compose buffer, conversation.lua `upload`); no photo picker
- Quote a selection.
  - web: unsure, not found
  - nvim: done (visual `<CR>`)
- Draft saved per topic.
  - web: done (app.mjs `saveDraft`)
  - nvim: done in the compose buffer while open; unsure across restarts
- Offline outbox: unsent messages retried.
  - web: done (app.mjs `saveOutbox`, `deliver`)
  - nvim: done (conversation.lua `outbox_add`, `outbox_flush`; kept on disk)
- New topic or direct message, editing, reactions.
  - web: missing (none found)
  - nvim: missing (README Limits)

## Settings

- Change start, end, cancel, command words and long-silence time.
  - web: done (settings sheet, index.html)
  - nvim: partly. Start word, end word, silence times are options and persist (state.lua). No cancel or command word exist.
- Settings remembered across restarts.
  - web: done (store.mjs)
  - nvim: done (state.lua, `zulip_voice.json`)
- Hide attachments.
  - web: done
  - nvim: done (`p`, `previews`)

## Reliability

- Reconnect and catch up after a dropped connection.
  - web: done (app.mjs `onInit` resync)
  - nvim: done (events.lua)
- Offline cache of conversations.
  - web: done (app.mjs `persist`, `model.snapshot`)
  - nvim: missing
- Debug log to the server; self-test.
  - web: done (app.mjs `sendLog`, `selftest.mjs`)
  - nvim: done (`:ZulipVoiceLog`, `:ZulipVoiceCheck`)
- Update check.
  - web: done (app.mjs `checkForUpdate`)
  - nvim: n/a

## Gaps

Neovim is missing, most important first:
- On-device keyword spotting: without `voice.local_stt`, every phrase costs an upload and a server round trip; with mac_stt it is transcribed on the laptop instead.
- Unsent-recording retry and offline cache of conversations (the outbox is done).

Web is missing, most important first:
- Local speech (mac_tts), which nvim has.
- Local speech-to-text (mac_stt), which nvim has.
- Microphone chooser: unsure.
- New topic, edit, reactions: missing in both.
