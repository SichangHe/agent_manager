/** Pure DSP for the voice-command experiments: WAV read, resampling, echo-path model, NLMS echo canceller, energy VAD. */
import {readFileSync} from 'node:fs';

/** 16-bit mono PCM WAV to `{hz, x: Float32Array}`. */
export function readWav(path) {
  const b = readFileSync(path);
  const at = b.indexOf('data') + 8;
  const x = new Float32Array((b.length - at) >> 1);
  for (let i = 0; i < x.length; i++) x[i] = b.readInt16LE(at + 2 * i) / 32768;
  return {hz: b.readUInt32LE(24), x};
}

/** Linear resampling (good enough for speech tests). */
export function resample(x, from, to) {
  const out = new Float32Array(Math.floor(x.length * to / from));
  for (let i = 0; i < out.length; i++) {
    const p = i * from / to, j = Math.floor(p), f = p - j;
    out[i] = x[j] * (1 - f) + (x[j + 1] ?? x[j]) * f;
  }
  return out;
}

/** Deterministic PRNG so runs are reproducible. */
export function rng(seed) { let s = seed; return () => (s = (s * 1664525 + 1013904223) >>> 0) / 4294967296 - 0.5; }

/** Speaker+room model: soft-clip speaker, then `delayMs` of silence and a `decayMs` exponentially decaying random impulse response. */
export function echoOf(ref, hz, {gain, delayMs, decayMs, seed = 1}) {
  const rand = rng(seed), d = Math.floor(delayMs * hz / 1000), n = Math.floor(decayMs * hz / 1000);
  const h = new Float32Array(d + n);
  for (let i = 0; i < n; i++) h[d + i] = rand() * Math.exp(-6 * i / n);
  h[d] = 1;
  const sp = ref.map(v => Math.tanh(2 * v) / 2);
  const out = new Float32Array(ref.length);
  for (let i = 0; i < out.length; i++) { let a = 0; for (let k = 0; k < h.length && k <= i; k++) if (h[k]) a += h[k] * sp[i - k]; out[i] = a * gain; }
  return out;
}

/** NLMS adaptive filter: returns `mic - estimated echo`. `freeze(i)` true skips adaptation (double-talk guard). */
export function nlms(ref, mic, {taps, mu = 0.3, eps = 1e-4}) {
  const w = new Float32Array(taps), out = new Float32Array(mic.length);
  let energy = 0;
  for (let i = 0; i < mic.length; i++) {
    let y = 0;
    for (let k = 0; k < taps; k++) y += w[k] * (ref[i - k] ?? 0);
    const e = mic[i] - y;
    out[i] = e;
    energy += (ref[i] ?? 0) ** 2 - (ref[i - taps] ?? 0) ** 2;
    // Adapt only if the error is not much larger than the echo estimate (user speaking = double talk).
    if (Math.abs(e) < 2.5 * Math.abs(y) + 1e-3) { const g = mu * e / (energy + eps); for (let k = 0; k < taps; k++) w[k] += g * (ref[i - k] ?? 0); }
  }
  return out;
}

/** Frame RMS values (`frameMs` frames). */
export function rmsFrames(x, hz, frameMs = 20) {
  const n = Math.floor(hz * frameMs / 1000), out = [];
  for (let i = 0; i + n <= x.length; i += n) { let s = 0; for (let j = 0; j < n; j++) s += x[i + j] ** 2; out.push(Math.sqrt(s / n)); }
  return out;
}

/** The app's recorder VAD (`recorder.mjs`): level > max(0.012, 3 x running noise floor). Returns per-frame booleans. */
export function energyVad(levels, {minLevel = 0.012, noise0 = 0.004} = {}) {
  let noise = noise0;
  return levels.map(l => { const on = l > Math.max(minLevel, noise * 3); if (!on) noise = noise * 0.98 + l * 0.02; return on; });
}

export const db = (a, b) => 20 * Math.log10((a + 1e-12) / (b + 1e-12));
export const rms = x => Math.sqrt(x.reduce((s, v) => s + v * v, 0) / x.length);
