# Voice-control stress test
(authored by agents unless marked 🧑)

🧑 "Stress our full voice control capabilities"

Method (facts)
- every control word and command said alone by 7 synthetic Kokoro voices under 7 conditions (normal, fast 1.4x, slow 0.8x, quiet -20 dB, noisy white noise std 0.03, pitch +15%, pitch -13%): 49 clips per phrase
- (a) on-device keyword spotter: the app's own keyword pieces and threshold 0.25 (`v2/kws.mjs`), same live simulation as the earlier word studies
- (b) speech to text: the production worker (Qwen3-ASR), then the app's own grammar (`v2/grammar.mjs`) classifies the transcript the way command mode does; correct = the right command
- code: `trig/stress.py`, `trig/classify.mjs`, `trig/stress_fa.py`; data `trig/stress.json`, `trig/stress_report.json`, `trig/stress_misses.txt`

Limits: synthetic voices (real speakers will differ, likely worse); 7 clips per cell (one clip = 14 points); words said alone after a pause, not in the middle of speech; not covered: the on-device 45-phrase command spotter (yes/no/numbers), real room noise, speakers bleeding into the microphone.

Control words (true positive, %)
- word | on device | via speech to text
- attention | 100 | 100
- katana | 86 | 100
- tomato | 88 | 98
- macaroni | 96 | 100
- on-device misses are in slow, noisy and pitch-shifted speech (71 to 86 for katana and tomato); the server transcript catches them, at the cost of an upload

Commands, speech to text then grammar (correct, %)
- next 100, again 100, next message 100, previous message 100, skip forward three 100
- play 96, switch to human manager 98, next agent 98, switch back 98, send log 98, cancel 98
- back 90 (noisy 57), stop 86
- full reply "attention please check the build and tell me what failed tomato": 84 (noisy 14, slow and pitch-up 86, all other conditions 100)

Misses (what the transcript was)
- stop: "Snob", "Snow", "Znobby", "Zdob", "Snobble": one voice says it so that it is heard as these in all 7 conditions (6 of 7 clips right per condition)
- play: "Plane"; back: "That", "Mark", "Bats"; send log: "Send love"; switch back: "Which box"; switch to: "Which to human manager"
- cancel: "Cancelled." is not recognised as cancel
- "Next agenter." fell through to a plain "next" (skips one chunk, not an agent)
- noise (std 0.03) is the weak spot for every path; with the full reply it drops to 14% (the transcript becomes "bill"/other words, or an extra word)

False positives
- keyword spotter over 4.9 h of speech (the same stream as the earlier word studies), app settings: ATTENTION 5 false accepts (about 1 per hour), KATANA 0, TOMATO 0, MACARONI 0
- caveat: the stream is generated and agent speech; its text contains the word "attention" at least once, so some of the 5 may be real utterances, not errors; I did not align the triggers with the text
- inference: katana, tomato and macaroni are safe at this exposure (below about 0.6 per hour, 95% bound); the start word is the only one that fires on ordinary speech, but triggering it only opens a reply session that closes by itself after 8 s of silence

Takeaways (inference)
- start word, cancel word and the navigation commands are reliable on clean speech; the weak points are "stop", "back" and everything in noise
- cheap fixes, not done: accept "cancelled"; accept "next agenter"; treat "snob/snow" as "stop" (risky: may fire on normal speech)
