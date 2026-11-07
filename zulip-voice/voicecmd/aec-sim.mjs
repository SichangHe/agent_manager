/** Offline echo experiment (16 kHz): mic = echo of the app's TTS + noise (+ the user saying "pause" at 9 s). Compares VAD on raw mic vs after NLMS cancel. Needs `clips/` from Kokoro. */
import {readWav, resample, echoOf, nlms, rmsFrames, energyVad, rng, db, rms} from './dsp.mjs';

const HZ = 16000, FRAME_MS = 20, F = HZ * FRAME_MS / 1000;
const load = n => { const w = readWav(new URL(`clips/${n}.wav`, import.meta.url).pathname); return resample(w.x, w.hz, HZ); };
const play = load('speech'), word = load('pause');
const USER_AT_S = 9, userStart = USER_AT_S * HZ;
const norm = (x, target) => { const r = rms(x); return x.map(v => v * target / r); };
console.log(`playback ${(play.length / HZ).toFixed(1)}s rms ${rms(play).toFixed(3)}; word "pause" ${(word.length / HZ).toFixed(2)}s`);

function scenario({echoGain, delayMs, decayMs, userGain, taps}) {
  const echo = echoOf(play, HZ, {gain: echoGain, delayMs, decayMs});
  const rand = rng(7), noise = Float32Array.from(echo, () => rand() * 0.01);
  const user = new Float32Array(echo.length);
  const w = norm(word, 0.08 * userGain);
  user.set(w, userStart);
  const mic = echo.map((v, i) => v + noise[i] + user[i]);
  const res = nlms(play, mic, {taps});
  const userFrames = [Math.floor(userStart / F) - 1, Math.ceil((userStart + word.length) / F) + 1];
  const stat = sig => {
    const on = energyVad(rmsFrames(sig, HZ, FRAME_MS)), lv = rmsFrames(sig, HZ, FRAME_MS);
    const out = on.filter((_, i) => i < userFrames[0] || i > userFrames[1]);
    const inU = on.slice(userFrames[0], userFrames[1] + 1);
    const settle = Math.floor(3 * HZ / F);   // ignore first 3 s while the filter converges
    const outLate = on.filter((_, i) => i >= settle && (i < userFrames[0] || i > userFrames[1]));
    return {falseAll: out.filter(Boolean).length / out.length, falseAfter3s: outLate.filter(Boolean).length / outLate.length, userHit: inU.filter(Boolean).length / inU.length, lv};
  };
  const raw = stat(mic), cl = stat(res);
  // ERLE on echo-only part after convergence (6..9 s, before user), using the true echo as reference power.
  const seg = (x, a, b) => x.slice(a * HZ, b * HZ);
  const erle = db(rms(seg(echo, 6, 8.8)), rms(seg(res, 6, 8.8).map((v, i) => v - seg(noise, 6, 8.8)[i] * 0)));
  const userSnrRaw = db(rms(norm(word, 0.08 * userGain)), rms(seg(echo, 6, 8.8)));
  return {raw, cl, erle, userSnrRaw};
}

const cases = [
  {name: 'quiet echo   (gain 0.1)', echoGain: 0.1, delayMs: 30, decayMs: 100},
  {name: 'phone echo   (gain 0.3)', echoGain: 0.3, delayMs: 30, decayMs: 100},
  {name: 'loud echo    (gain 1.0)', echoGain: 1.0, delayMs: 30, decayMs: 100},
  {name: 'loud+long IR (gain 1.0, 250ms)', echoGain: 1.0, delayMs: 30, decayMs: 250},
];
const pct = v => (100 * v).toFixed(0).padStart(3) + '%';
for (const c of cases) for (const taps of [0, 2048]) {
  const r = scenario({...c, userGain: 1, taps: taps || 1});
  const label = taps ? 'NLMS 2048 taps (needs reference)' : 'no AEC (taps=1 stand-in skipped)';
  if (!taps) { console.log(`${c.name} | raw mic: false-VAD ${pct(r.raw.falseAll)}, user-word VAD ${pct(r.raw.userHit)}, user-vs-echo ${r.userSnrRaw.toFixed(1)} dB`); continue; }
  console.log(`${c.name} | ${label}: ERLE ${r.erle.toFixed(1)} dB, false-VAD ${pct(r.cl.falseAll)} (after 3 s ${pct(r.cl.falseAfter3s)}), user-word VAD ${pct(r.cl.userHit)}`);
}
