# Control words in noise: what helps and what does not
(authored by agents unless marked 🧑)

🧑 "I think switching keyword is not really the solution. The real solution here should be to have a local. Algorithm to clean up the noise before feeding it to the speech recognition software."

Setup (facts): generated Kokoro voices (7), words said alone, noise = white / pink / speech babble mixed at 10, 5, 0 dB SNR (relative to the word), plus clean; the on-device spotter used by the app (sherpa-onnx zipformer, `v2/kws.mjs` words, katana threshold 0.15 boost 2). Cells hold 84 clips (4 words) or 21 clips (one word); one clip is 1.2 or 4.8 points. Code: `trig/denoise_study_big.py`, `trig/sens_study.py`; data `trig/denoise_study*.json`, `trig/sens_study.json`.

1. A local denoiser in front of the spotter (mean hit rate over the 4 control words, all noise cells)
- none 82; RNNoise model bd 82; FFT denoiser (ffmpeg afftdn) 80; RNNoise model sh 77; high-pass + RNNoise sh 73
- no denoiser beats doing nothing; the differences of 0-2 points are inside the noise of the test; two make it worse
- hardest case, babble at 0 dB (someone talking over the word): 44 for all of them
- inference: the spotter was trained on noisy speech; denoisers remove noise but also leave artifacts it was not trained on
- the phone already requests the browser's own suppression (`noiseSuppression: true`), so an extra stage would be a second one

2. A more sensitive spotter, then server confirmation of near misses (katana hit rate, all noise cells)
- threshold 0.25: 71; 0.15 (now): 75; 0.10: 76; 0.06: 77; 0.03: 77
- recall stops at 77 because in noise the model does not decode the word at all (no score to lower the bar for); so server confirmation of near misses cannot recover them

3. Another keyword (round 2, `keyword-alternatives.md`, `cmdword/`): the best candidates reach 94-97 mean in the earlier noise test against 81 for katana, but the false-accept check is still running

4. The server path (production speech-to-text, then the app's grammar), measured in `voice-control-stress.md`: katana 100 in every condition including noise; "attention", "macaroni" 100
- cost: an upload and a transcript per clip (a Zulip call each, about 1.5-2 s)
