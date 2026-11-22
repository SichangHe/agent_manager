/**
 * 🧑 "The speech now. Repeats the first half of agent messages. And then ... it just cuts off" (2026-11-22).
 * Headless replay of the logged sequence: the client renders a local batch for chunks 2..7, then the service's own bundles for the same
 * chunks arrive. Fake audio elements play the timeline of the URL actually loaded, so a slot labelled with one bundle but holding
 * another's audio is heard as the wrong sentence. Asserts that every chunk starts with its own sentence. Run: node test/player-label.mjs
 */
import assert from 'node:assert/strict';

const SPEED = 4;   // audio seconds per real second
const parts = new Map();   // url -> part
class FakeAudio extends EventTarget {
  constructor() { super(); this.src = ''; this.paused = true; this.readyState = 0; this._t = 0; this._at = 0; this.playbackRate = 1; }
  get part() { return parts.get(this.src); }
  get duration() { return this.part ? this.part.durationMs / 1000 : NaN; }
  get buffered() { const d = this.duration; return d > 0 ? {length: 1, end: () => d} : {length: 0}; }
  get currentTime() { const t = this.paused ? this._t : this._t + (performance.now() - this._at) / 1000 * SPEED; return Math.min(t, this.duration || 0); }
  set currentTime(v) { this._t = v; this._at = performance.now(); }
  get ended() { return this.part && this.currentTime >= this.duration; }
  setAttribute() {}
  load() { this.readyState = 0; this._t = 0; const src = this.src; setTimeout(() => { if (this.src === src && parts.has(src)) { this.readyState = 4; this.dispatchEvent(new Event('canplay')); } }, 30); }
  play() { this._t = this.currentTime; this._at = performance.now(); this.paused = false; clearTimeout(this._end);
    const left = (this.duration - this._t) / SPEED * 1000; if (left > 0) this._end = setTimeout(() => { if (!this.paused) { this._t = this.duration; this.paused = true; this.dispatchEvent(new Event('ended')); } }, left);
    return Promise.resolve(); }
  pause() { if (this.paused) return; this._t = this.currentTime; this.paused = true; clearTimeout(this._end); this.dispatchEvent(new Event('pause')); }
  /** The sentence at the playhead. */
  heard() { const p = this.part, t = this.currentTime * 1000; if (!p) return null; let k = p.offsetsMs.findLastIndex(o => o <= t + 1); return p.texts[k] ?? null; }
}
globalThis.Audio = FakeAudio;
globalThis.document = {slots: new Set(), body: {append(el) { globalThis.document.slots.add(el); }}};
globalThis.navigator ??= {};

const MSG = 1, texts = Array.from({length: 8}, (_, i) => `sentence ${i}`), dur = i => (i === 0 ? 24000 : 4000) + i * 37;
const mkPart = (id, first, n) => { const ts = texts.slice(first, first + n), offsetsMs = []; let t = 0; for (let k = 0; k < n; k++) { offsetsMs.push(t); t += dur(first + k); }
  const p = {id, messageId: MSG, firstIdx: first, texts: ts, offsetsMs, durationMs: t, audioURL: `u:${id}`, arrivedMs: Date.now()}; parts.set(p.audioURL, p); return p; };
const service = [mkPart('svc0', 0, 1), mkPart('svc1', 1, 1)];
let batches = 0;
const voice = {
  bundleFor: (m, idx, text) => service.findLast(p => m === p.messageId && idx >= p.firstIdx && idx < p.firstIdx + p.texts.length && (text === undefined || p.texts[idx - p.firstIdx] === text)),
  chunkIndex: (p, m, idx) => p.messageId === m && idx >= p.firstIdx && idx < p.firstIdx + p.texts.length ? idx - p.firstIdx : -1,
  ttsBatch: async ts => { await new Promise(r => setTimeout(r, 200)); const first = texts.indexOf(ts[0]); const p = mkPart(`local${batches++}`, first, ts.length); return {ok: true, clip: p}; },
  clipUrl: async p => ({ok: true, url: p.audioURL}),
};
setTimeout(() => service.push(mkPart('svc2', 2, 1), mkPart('svc3', 3, 4), mkPart('svc7', 7, 1)), 4000);   // the service's bundles arrive after the local batch

const {createPlayer} = await import('../player.mjs');
const lines = [], wrong = [];
let done;
const finished = new Promise(r => { done = r; });
const player = createPlayer({
  log: (tag, msg) => lines.push(`${(performance.now() / 1000).toFixed(2)} ${tag} ${msg}`),
  voice,
  onItemStart: (it, i) => setTimeout(() => { const el = [...document.slots].find(e => !e.paused); const h = el?.heard(); lines.push(`  item ${i} heard "${h}"`); if (h !== it.text) wrong.push(`chunk ${i} expected "${it.text}" heard "${h}"`); }, 60),
  onIdle: () => done(),
});
const items = texts.map((text, idx) => ({msgId: MSG, idx, text, key: `${MSG}:${idx}`}));
player.play(items, 0);
await Promise.race([finished, new Promise(r => setTimeout(r, 30000))]);
if (wrong.length) { console.log(lines.join('\n')); console.log('FAIL\n' + wrong.join('\n')); process.exit(1); }
console.log(`ok: all ${items.length} chunks started with their own sentence`); process.exit(0);
