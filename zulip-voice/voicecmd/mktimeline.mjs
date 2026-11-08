/** Builds `clips/timeline.wav` (44.1 kHz mono) and `clips/timeline.json` (label, start s, end s) from the Kokoro clips. */
import {writeFileSync} from 'node:fs';
import {readWav, resample} from './dsp.mjs';

const HZ = 44100, GAP_S = 2;
const names = ['stop', 'go', 'pause', 'resume', 'next', 'back', 'send', 'yes', 'no', 'up', 'down', 'left', 'right', 'stop', 'go', 'speech', 'speech2'];
const parts = [], marks = [];
let at = HZ;   // 1 s lead-in
for (const n of names) {
  const w = readWav(new URL(`clips/${n}.wav`, import.meta.url).pathname), x = resample(w.x, w.hz, HZ);
  marks.push({label: n, start: at / HZ, end: (at + x.length) / HZ});
  parts.push([at, x]);
  at += x.length + GAP_S * HZ;
}
const out = new Int16Array(at);
for (const [i, x] of parts) x.forEach((v, j) => { out[i + j] = Math.max(-32768, Math.min(32767, Math.round(v * 32767))); });
const h = Buffer.alloc(44);
h.write('RIFF'); h.writeUInt32LE(36 + out.length * 2, 4); h.write('WAVEfmt ', 8); h.writeUInt32LE(16, 16); h.writeUInt16LE(1, 20); h.writeUInt16LE(1, 22);
h.writeUInt32LE(HZ, 24); h.writeUInt32LE(HZ * 2, 28); h.writeUInt16LE(2, 32); h.writeUInt16LE(16, 34); h.write('data', 36); h.writeUInt32LE(out.length * 2, 40);
writeFileSync(new URL('clips/timeline.wav', import.meta.url).pathname, Buffer.concat([h, Buffer.from(out.buffer)]));
writeFileSync(new URL('clips/timeline.json', import.meta.url).pathname, JSON.stringify(marks));
console.log(`timeline ${(at / HZ).toFixed(1)}s`);
