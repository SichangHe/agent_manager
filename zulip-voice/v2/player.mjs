/**
 * Clip queue on two always-reused `<audio>` elements, alternating. Clips and their URLs are prepared ahead, but the next element is
 * loaded only after the current clip ends: loading a second element while one plays stalls playback in WebKit (measured 1.2 s).
 * iOS lets an element play without a gesture only after it has played once from a gesture, so `unlock()` primes both
 * and nothing else ever creates an element. Cross-origin clips need real media elements (no CORS for fetch/WebAudio).
 *
 * Status: `idle` (nothing queued), `loading` (waiting for audio data), `playing`, `paused`, `blocked` (needs a tap), `error` (will retry).
 */
const SILENCE = 'data:audio/wav;base64,UklGRiQAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA=';
const AHEAD = 3;
const URL_FRESH_MS = 25000;
const PRERENDER_WAIT_MS = 2500;
const BATCH_CHUNKS = 6;
const STALL_MS = 7000;
const RETRY_CAP_MS = 8000;
const N_SLOTS = 6;      // audio elements, each holding one downloaded bundle: the client's audio cache (bytes themselves cannot be read cross-origin)
const WARM_AHEAD = 3;
const HEARD_FRACTION = 0.8;
const RESUME_BACKUP_MS = 2000;

export function createPlayer({log, voice, onItemStart, onItemEnd, onIdle, onStatus}) {
  const slots = Array.from({length: N_SLOTS}, () => ({used: 0, el: Object.assign(new Audio(), {preload: 'auto', playsInline: true}), index: -1, ready: false, bundleId: null, chunk: null}));
  for (const slot of slots) { slot.el.setAttribute('playsinline', ''); slot.el.hidden = true; document.body.append(slot.el); }
  let items = [], index = -1, active = null, status = 'idle', gen = 0, unlocked = false, error = '';
  let rate = 1, lastTimeMs = 0, lastProgressMs = 0;
  const clips = new Map();
  try { navigator.audioSession.type = 'playback'; } catch { /* unsupported */ }

  const setStatus = next => { if (next !== status) { log('audio', `${status} -> ${next}${index >= 0 ? ' item ' + index : ''}${next === 'error' ? ' ' + error : ''}`); status = next; } onStatus?.({status, index, error}); };

  const localParts = new Map();   // bundles this client asked for on demand: `${msgId}:${firstIdx}` -> part

  /** `{part, startMs, endMs}`: the bundle holding item `it` (from the service's pre-render or an on-demand batch) and its time range. */
  function bundleOf(it) {
    const part = voice.bundleFor(it.msgId, it.idx) ?? [...localParts.values()].find(p => p.messageId === it.msgId && it.idx >= p.firstIdx && it.idx < p.firstIdx + p.texts.length);
    if (!part || part.texts[it.idx - part.firstIdx] !== it.text) return null;
    const k = it.idx - part.firstIdx;
    return {part, startMs: part.offsetsMs[k], endMs: part.offsetsMs[k + 1] ?? part.durationMs};
  }

  /** Waits briefly for the service's pre-rendered bundle, then asks for the next few chunks of this message in one batch request. */
  async function ensureBundle(i) {
    for (let waited = 0; !bundleOf(items[i]) && waited < PRERENDER_WAIT_MS; waited += 250) await new Promise(r => setTimeout(r, 250));
    if (bundleOf(items[i])) return null;
    const group = [];
    for (let j = i; j < items.length && group.length < BATCH_CHUNKS && items[j].msgId === items[i].msgId && items[j].idx === items[i].idx + group.length && !bundleOf(items[j]); j++) group.push(items[j]);
    const r = await voice.ttsBatch(group.map(g => g.text));
    if (!r.ok) return r;
    localParts.set(`${items[i].msgId}:${items[i].idx}`, {...r.clip, id: r.clip.id, messageId: items[i].msgId, firstIdx: items[i].idx});
    return null;
  }

  /** Starts (or reuses) the preparation of `i`'s audio: `entry.url` resolves to `{ok, url, bundle}` or `{ok: false, error}`. */
  function clipFor(i) {
    if (!items[i]) return null;
    let entry = clips.get(i);
    if (!entry) {
      entry = {url: (async () => {
        const fail = await ensureBundle(i);
        if (fail) return fail;
        let bundle = bundleOf(items[i]);
        let r = await voice.clipUrl(bundle.part, URL_FRESH_MS);
        if (!r.ok) {   // the upload is gone: the service bundle was dropped; render this message's chunks fresh
          const retry = await ensureBundle(i);
          if (retry) return retry;
          bundle = bundleOf(items[i]);
          r = await voice.clipUrl(bundle.part, URL_FRESH_MS);
        }
        return r.ok ? {...r, bundle} : r;
      })()};
      clips.set(i, entry);
      entry.url.then(r => log('audio', `clip ${i} ${r.ok ? 'bundle ' + r.bundle.part.id.slice(-18) : 'failed ' + r.error}`));
    }
    return entry;
  }
  const prefetch = from => { for (let i = from; i < Math.min(items.length, from + AHEAD); i++) clipFor(i); };

  /** Least recently used slot that is not playing. */
  const victim = () => slots.filter(s => s !== active).sort((a, b) => (a.ready ? 1 : 0) - (b.ready ? 1 : 0) || a.used - b.used)[0];

  /** Loads `url` into `slot` and resolves true once it can play. */
  async function load(slot, url) {
    slot.ready = false; slot.bundleId = null;
    slot.el.src = url;
    slot.el.playbackRate = rate;
    slot.el.load();
    return new Promise(resolve => {
      const done = ok => { slot.el.removeEventListener('canplay', onOk); slot.el.removeEventListener('error', onErr); clearTimeout(timer); resolve(ok); };
      const onOk = () => done(true), onErr = () => { log('audio', `load error code=${slot.el.error?.code} msg=${slot.el.error?.message} src=${String(url).slice(-40)}`); done(false); };
      const timer = setTimeout(() => done(false), 15000);
      slot.el.addEventListener('canplay', onOk); slot.el.addEventListener('error', onErr);
      if (slot.el.readyState >= 3) done(true);
    });
  }

  /** Resolves `{slot, seekMs, bundle}` for item `i`, reusing a slot that already holds its bundle, or `{error}`. */
  async function attach(i, myGen) {
    const known = bundleOf(items[i]);
    const held = known && slots.find(s => s.bundleId === known.part.id && s.ready);
    if (held) { held.used = Date.now(); return {slot: held, seekMs: known.startMs, bundle: known}; }
    const r = await clipFor(i).url;
    if (myGen !== gen) return {stale: true};
    if (!r.ok) { clips.delete(i); return {error: r.error}; }
    const slot = victim();
    const loaded = await load(slot, r.url);
    if (myGen !== gen) return {stale: true};
    if (!loaded) { clips.delete(i); localParts.delete(`${items[i].msgId}:${r.bundle.part.firstIdx}`); voice.dropPart?.(r.bundle.part); return {error: 'audio failed to load'}; }
    slot.ready = true; slot.bundleId = r.bundle.part.id; slot.used = Date.now();
    return {slot, seekMs: r.bundle.startMs, bundle: r.bundle};
  }

  /** Once the playing bundle is fully buffered, loads the next bundle into the idle slot so the hand-over is instant and survives a dead network. */
  let warming = false, pausedMs = null, pausedDone = false;
  async function warmNext() {
    if (warming || !active || status !== 'playing') return;
    const el = active.el;
    if (!(el.duration > 0) || !el.buffered.length || el.buffered.end(el.buffered.length - 1) < el.duration - 0.2) return;
    const seen = new Set([bundleOf(items[index])?.part.id]);
    warming = true;
    for (let i = index + 1; i < items.length && seen.size <= WARM_AHEAD; i++) {
      const bundle = bundleOf(items[i]);
      if (!bundle || seen.has(bundle.part.id)) continue;
      seen.add(bundle.part.id);
      if (slots.some(s => s.bundleId === bundle.part.id && s.ready)) continue;
      const idle = victim();
      const r = await clipFor(i).url;
      if (r.ok && idle !== active && await load(idle, r.url)) { idle.ready = true; idle.bundleId = bundle.part.id; idle.used = Date.now(); log('audio', `warmed ${bundle.part.id.slice(-18)}`); }
    }
    warming = false;
  }

  async function playIndex(i, attempt = 0) {
    const myGen = ++gen;
    index = i;
    const item = items[i];
    if (!item) { active = null; setStatus('idle'); onIdle?.(); return; }
    if (status !== 'paused') setStatus('loading');
    prefetch(i);
    const r = await attach(i, myGen);
    if (r.stale) return;
    if (r.error) {
      error = r.error;
      setStatus('error');
      await new Promise(res => setTimeout(res, Math.min(1000 * 2 ** attempt, RETRY_CAP_MS)));
      if (myGen === gen) playIndex(i, attempt + 1);
      return;
    }
    if (status === 'paused') { active = r.slot; r.slot.el.currentTime = r.seekMs / 1000; r.slot.chunk = {endMs: r.bundle.endMs}; return; }
    start(r.slot, myGen, r.seekMs, r.bundle);
  }

  function start(slot, myGen, seekMs, bundle) {
    if (pausedMs !== null && seekMs !== undefined) { seekMs = Math.max(seekMs, pausedMs - RESUME_BACKUP_MS); }
    pausedMs = null;
    for (const s of slots) if (s !== slot) s.el.pause();
    active = slot;
    slot.chunk = bundle ? {endMs: bundle.endMs} : null;
    if (seekMs !== undefined && Math.abs(slot.el.currentTime * 1000 - seekMs) > 120) slot.el.currentTime = seekMs / 1000;
    slot.el.playbackRate = rate;
    const promise = slot.el.play();
    lastProgressMs = performance.now(); lastTimeMs = 0;
    promise.then(() => {
      if (myGen !== gen) return;
      setStatus('playing');
      onItemStart?.(items[index], index);
      prefetch(index + 1);
    }, err => {
      if (myGen !== gen) return;
      log('audio', `play() rejected: ${err.name}`);
      if (err.name === 'NotAllowedError') setStatus('blocked'); else { error = err.name; setStatus('error'); }
    });
  }

  /** The current item finished (its audio ended, or playback crossed its chunk boundary inside a bundle). */
  function itemEnded() {
    const done = index;
    log('audio', `ended ${items[done]?.key}`);
    clips.delete(done);
    const before = gen;
    onItemEnd?.(items[done], done);
    if (gen !== before) return; // `onItemEnd` replaced the queue
    playIndex(done + 1);
  }

  for (const slot of slots) {
    slot.el.addEventListener('ended', () => {
      if (slot !== active) return;
      slot.chunk = null; slot.ready = false; slot.bundleId = null;
      itemEnded();
    });
    for (const name of ['waiting', 'stalled', 'error', 'suspend', 'emptied']) slot.el.addEventListener(name, () => { if (slot === active) log('audio', `el ${name} t=${slot.el.currentTime.toFixed(2)} rs=${slot.el.readyState}`); });
  }

  setInterval(() => {   // chunk boundaries inside a bundle
    if (status !== 'playing' || !active?.chunk || active.el.paused) return;
    if (active.el.currentTime * 1000 >= active.chunk.endMs - 25) { active.chunk = null; itemEnded(); } else warmNext();
  }, 40);

  setInterval(() => {
    if (status !== 'playing' || !active) return;
    const t = active.el.currentTime;
    if (t !== lastTimeMs) { lastTimeMs = t; lastProgressMs = performance.now(); }
    else if (performance.now() - lastProgressMs > STALL_MS) { log('audio', `stalled ${STALL_MS}ms at item ${index}; reloading`); clips.delete(index); active.index = -1; active.ready = false; active.bundleId = null; playIndex(index); }
  }, 1000);

  return {
    /** Call inside a user gesture. */
    unlock() {
      if (!unlocked) {
        unlocked = true;
        for (const slot of slots) if (slot !== active && !slot.ready) { slot.el.src = SILENCE; slot.el.play().then(() => { if (slot.el.src === SILENCE) slot.el.pause(); }, err => { if (err.name === 'NotAllowedError') unlocked = false; }); }
        log('audio', 'unlocked by gesture');
      }
      if (status === 'blocked') start(active, gen);
    },
    isUnlocked: () => unlocked,
    /** Replaces the queue and plays `startIndex`. */
    play(newItems, startIndex = 0) { items = newItems; clips.clear(); if (status === 'paused') status = 'idle'; playIndex(startIndex); },
    /** Adds items; if the queue had drained, plays the first new one. */
    append(more) { const wasIdle = status === 'idle' && index >= items.length - 1; items = items.concat(more); if (wasIdle && more.length) playIndex(items.length - more.length); else if (status === 'playing') prefetch(index + 1); },
    /** Remembers where in the chunk it stopped; a chunk that was mostly heard counts as heard on resume. */
    pause() {
      if (status !== 'playing' && status !== 'loading' && status !== 'blocked') return;
      const b = status === 'playing' && items[index] ? bundleOf(items[index]) : null;
      pausedMs = b && active ? active.el.currentTime * 1000 : null;
      pausedDone = !!b && pausedMs >= b.startMs + HEARD_FRACTION * (b.endMs - b.startMs);
      active?.el.pause(); setStatus('paused');
    },
    resume() { if (status === 'paused') { if (pausedDone) { pausedDone = false; pausedMs = null; if (active) active.chunk = null; setStatus('loading'); itemEnded(); return; } if (active?.ready) { setStatus('loading'); const myGen = ++gen; start(active, myGen); } else playIndex(index); } else if (status === 'error') playIndex(index); },
    retry() { if (status === 'error' || status === 'blocked') playIndex(index); },
    stop() { gen++; for (const s of slots) { s.el.pause(); s.index = -1; s.ready = false; s.bundleId = null; s.chunk = null; } clips.clear(); items = []; index = -1; active = null; setStatus('idle'); },
    /** Quiets playback while the user may be speaking (no effect where the platform ignores element volume, e.g. iOS). */
    setRate(value) { rate = value; for (const s of slots) { s.el.playbackRate = value; s.el.defaultPlaybackRate = value; } },
    currentTime: () => (status === 'playing' || status === 'paused') && active ? active.el.currentTime : 0,
    position: () => ({index, status, queued: items.length}),
    snapshot: () => ({status, index, queued: items.length, rate, unlocked, slots: slots.map(s => ({bundle: s.bundleId?.slice(-12), ready: s.ready, paused: s.el.paused, t: +s.el.currentTime.toFixed(1), rs: s.el.readyState})), clips: clips.size}),
  };
}
