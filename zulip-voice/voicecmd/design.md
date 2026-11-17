# voicecmd
(authored by agents)

purpose: experiments for hands-free voice commands (pause, resume, next, back, send) in the phone app; v2 untouched

files
- `dsp.mjs`: pure DSP (wav, resample, echo-path model, NLMS, energy VAD = same rule as v2 `recorder.mjs`)
- `aec-sim.mjs`: `node aec-sim.mjs`; synthetic echo of Kokoro TTS + user word "pause"; VAD raw vs NLMS-cancelled
- `mktimeline.mjs`: builds `clips/timeline.wav|json` from Kokoro clips (`clips/*.wav`, made via `127.0.0.1:8770/tts`)
- `kws.html` + `page.js`: phone test page (feature report, live VAD bar, TF.js speech-commands 18w, Play TTS button, echoCancellation toggle)
- `kws-test.mjs`: Playwright WebKit, fake mic = timeline; `BACKEND=cpu|webgl THR=0.75 node kws-test.mjs`
- setup: `npm i` (tfjs 4.22, speech-commands 0.5.4); model files in `models/18w/` from storage.googleapis.com/tfjs-models/tfjs/speech-commands/v0.3/browser_fft/18w

not testable off-phone
- real iOS echo cancellation (fake mic bypasses voice processing), real voices, battery

guided phone test (final direction: detect speech, send to server; no keyword model)
- `index.html` + `page.mjs`: one Start tap; echoCancellation ON then OFF; phases a (playback, silent), b (playback, user speaks), c (speech only)
  - metrics per phase: VAD-active fraction (v2 rule), level median/p90 dBFS; b and c audio go to relay `stt`; report sent with relay `log` (label `voicecmd`)
- `build.mjs`: `node build.mjs [DIR]` copies page, v2 `zulip.mjs`/`voice.mjs`, makes `speech.m4a` (AAC 32 kbps); default `/tmp/voicecmd-dist`
- `page-test.mjs`: WebKit run with real test relay; only the mic is faked (looped timeline), so its numbers are plumbing checks only
