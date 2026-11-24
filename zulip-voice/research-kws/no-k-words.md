# Start and cancel words without a K sound, in real noise
(authored by agents unless marked 🧑)

🧑 "In noise background, the K sound don't register at all. This is true for both the command start word, and the cancellation word. We need words that don't have the k sound. T sound is good, and vowels are good."
🧑 "You need to use mixed audio with water sound and garbled music and a market and so on, instead of white noise, which is trivial to beat"

Setup (facts)
- spotter as the app runs it (cancelstudy10.py `live`); settings A (threshold 0.25) and B (0.15, boost 2)
- each word said alone by 7 synthetic Kokoro voices; 7 clips per condition (one clip = 14 points)
- noise: ESC-50 water (pouring, drops, rain, waves, flush), 40 MUSAN music tracks, the 37 MUSAN free-sound background recordings (street, crowd; no market recording exists in it), 3 overlapping speakers from corpus_speech.wav
- conditions: each noise alone at 5 dB signal-to-noise; all four mixed at 10, 5 and 0 dB (0 dB = noise as loud as the word); plus clean, quiet, fast, slow, pitch up/down
- false accepts: 4.9 h of ordinary speech (fa16.npy)
- code and data: trig/study13.py, trig/study13.json, trig/confuse13.py, trig/confuse13.json

Results, setting A, mean over the 7 noisy conditions (false accepts in 4.9 h)
- tornado 0.92 (0); toronto 0.90 (0); tuesday 0.88 (4); potato 0.88 (0); tomato 0.84 (0); cancel 0.84 (5); katana 0.78 (0)
- tattoo 0.71; nintendo 0.65; patio 0.55; otto 0.51 (26); atari 0.37; photo 0.18 (4); tahiti, teatime, ottawa, toyota, tiramisu under 0.11
- hardest condition for everyone: all noise mixed at 0 dB (tornado, toronto 0.71; katana, cancel 0.57)
- cross-triggers, with attention, tomato and the candidates loaded together: none; each word's 91 clips fired only that word

Takeaways (inference)
- tornado and toronto are the best no-K words here and never fired falsely; about 14 points above katana in noise, which is one clip in seven per condition, so the gain is real but modest
- tuesday and cancel fire on ordinary speech (common words); potato sounds close to the end word tomato, so it is risky despite no measured cross-trigger
- limits: synthetic voices, 7 clips per condition; real voices on a phone will score lower
