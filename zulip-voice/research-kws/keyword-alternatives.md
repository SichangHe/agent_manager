# Keyword alternatives: recognition and false-positive rates
(authored by agents unless marked 🧑)

🧑 "we should put in our documentation other alternatives for keywords, and their recognition rates in full thing, true positive rate and false positive rate."

Setup (facts)
- spotter: sherpa-onnx zipformer keyword spotter (gigaspeech 3.3M), run as the app does: automatic gain, continuous stream plus an onset-primed second stream
- true positive rate (TPR): fraction of test clips where the word was spotted; each clip is the word said alone after a pause
    - 7 synthetic Kokoro voices x 7 conditions = 49 clips per word (7 per condition)
    - conditions: normal, fast (1.4x), slow (0.8x), quiet (-20 dB), noisy (white noise, std 0.03), pitch up (+15%), pitch down (-13%), speed unchanged
- false positive rate (FPR): spotter run over 4.9 h of speech (`fa16.npy`); any trigger of any word in the list counts
- three spotter settings (threshold / boost): A 0.25/0, B 0.15/0, C 0.1/2.0 (lower threshold + boost = more sensitive)
- code: `trig/cancelstudy.py` (round 1), `trig/cancelstudy10.py` (round 2), data `trig/cancelstudy*.json`

Limits (honest)
- synthetic voices, not real people: real-speaker TPR will differ, typically lower
- 7 clips per condition: one clip is 14 points; treat differences under ~15 points as noise
- 0 false accepts in 4.9 h means FPR below about 0.6 per hour (95% upper bound, rule of three: 3 / 4.9 h); it does not prove zero

Results, mean TPR over all 49 clips (round 2, words under consideration)
- word | A | B | C | false accepts in 4.9 h (all settings)
- macaroni | 0.96 | 0.96 | 0.96 | 0
- bamboo (current) | 0.88 | 0.88 | 0.88 | 0
- origami | 0.69 | 0.76 | 0.88 | 0
- karaoke | 0.61 | 0.73 | 0.90 | 0
- samurai | 0.39 | 0.45 | 0.53 | 0
- tsunami | 0.16 | 0.16 | 0.29 | 0
- all words: weakest condition is `noisy` (e.g. bamboo 0.43, macaroni 0.71)

Results, round 1 (mean TPR, settings A / B / C; 0 false accepts in 4.9 h for all)
- bamboo 0.90 / 0.90 / 0.90
- kaboom 0.84 / 0.84 / 0.86
- macaroon 0.80 / 0.88 / 0.84
- tamago 0.76 / 0.86 / 0.80
- alakazam 0.67 / 0.76 / 0.92
- bazooka 0.63 / 0.63 / 0.71
- boomerang 0.59 / 0.59 / 0.67
- kablooey 0.37 / 0.41 / 0.59
- banzai 0.41 / 0.43 / 0.53
- kerplunk 0.10 / 0.10 / 0.39
- wahoo 0.00 / 0.12 / 0.31
- bonanza 0.02 / 0.02 / 0.02
- kowabunga, "bamboo bamboo", "kaboom kaboom", "ba boom": about 0 (repeats and compounds are not spotted)

Takeaways (inference from the above)
- tsunami, the human's preference, is the worst of the round-2 words (TPR 0.16 to 0.29): the spotter model rarely hears it, even said clearly; it is not a safe cancel word
- best by TPR: macaroni (0.96); best 3+ syllable by the human's rules: macaroni (4 syllables, ends in a vowel, no double consonant, though "-ron-" is a nasal cluster)
- 0 false accepts for every word in 4.9 h means false positives are not what separates them; recognition is
- not covered: how often each word occurs in ordinary speech (a real-speech word-frequency check would be needed: "samurai", "karaoke" and "origami" are rarer than "macaroni")
