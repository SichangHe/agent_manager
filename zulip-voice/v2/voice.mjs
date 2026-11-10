/** Client of the voice service (`service/protocol.md`): STT, TTS, ping through the relay channel. Results arrive via `onMessage`. */

const LOG_PART_CHARS = 9000;
const URL_TTL_MS = 50000;
const PREPARED_TTL_MS = 6 * 3600000;
const newId = prefix => prefix + '_' + Array.from(crypto.getRandomValues(new Uint8Array(9)), b => b.toString(16).padStart(2, '0')).join('');

export function createVoice({zulip, cfg, log}) {
  const pending = new Map();
  const bundles = new Map();

  /** Feed every event message; consumes this service's results. */
  /** `arrivedMs` is the result's server time, so backfilled clips' expired signed URLs get re-minted. */
  function onMessage(message) {
    if (message.stream_id !== cfg.relayStreamId || message.sender_id !== cfg.botId) return;
    let body;
    try { body = JSON.parse(message.content); } catch { return; }
    if (body?.voice !== 1 || body.op !== 'result') return;
    const waiter = pending.get(body.id);
    if (waiter) { pending.delete(body.id); waiter(body); }
    else if (body.proactive && body.bundle && body.audioURL) {
      const part = {...body, arrivedMs: Math.min(Date.now(), message.timestamp * 1000)};
      for (const id of new Set((body.spans ?? [{messageId: body.messageId}]).map(sp => sp.messageId))) {   // a bundle may hold the chunks of several messages (`spans`, in play order)
        const parts = bundles.get(id) ?? [];
        if (!parts.some(p => p.id === body.id)) parts.push(part);
        bundles.set(id, parts);
      }
      for (const [key, list] of bundles) if (Date.now() - list[0].arrivedMs > PREPARED_TTL_MS) bundles.delete(key);
    }
  }

  /** Loads clips the service pre-rendered while this client was not connected. */
  async function backfill() {
    const r = await zulip.call('GET', 'messages', {narrow: [{operator: 'stream', operand: cfg.relayStreamId}, {operator: 'topic', operand: 'tts'}], anchor: 'newest', num_before: 300, num_after: 0, apply_markdown: false});
    if (!r.ok) return;
    for (const m of r.data.messages) onMessage(m);
    log('relay', `backfilled ${bundles.size} prepared messages`);
  }

  /** Posts a request and waits for its result: `{ok: true, result}` or `{ok: false, error}`. */
  async function request(op, fields, timeoutMs, id = newId(op)) {
    const sentMs = performance.now();
    const waiting = new Promise(resolve => {
      pending.set(id, resolve);
      setTimeout(() => { if (pending.delete(id)) resolve(null); }, timeoutMs);
    });
    const posted = await zulip.call('POST', 'messages', {type: 'stream', to: cfg.relayStreamId, topic: op, content: JSON.stringify({voice: 1, id, op, ...fields})});
    if (!posted.ok) { pending.delete(id); return {ok: false, error: posted.error}; }
    const postedMs = performance.now() - sentMs;
    const result = await waiting;
    const totalMs = Math.round(performance.now() - sentMs);
    if (!result) { log('relay', `${op} ${id} timeout after ${totalMs}ms`); return {ok: false, error: `${op} timed out`}; }
    log('relay', `${op} ${id} ${result.error ? 'ERROR ' + result.error : 'ok'} total=${totalMs}ms post=${Math.round(postedMs)}ms relay queue=${result.timings?.queueMs}ms work=${result.timings?.workMs}ms`);
    return result.error ? {ok: false, error: result.error} : {ok: true, result, totalMs};
  }

  /** The bundle (one upload holding several chunks) containing chunk `idx` of message `msgId`, if the service rendered it. */
  function bundleFor(msgId, idx) {
    return bundles.get(msgId)?.find(p => chunkIndex(p, msgId, idx) >= 0);
  }
  /** Position of chunk `idx` of message `msgId` inside bundle `p` (its audio and `texts` order), or -1. */
  function chunkIndex(p, msgId, idx) {
    if (!p.spans) return p.messageId === msgId && idx >= p.firstIdx && idx < p.firstIdx + p.texts.length ? idx - p.firstIdx : -1;
    let base = 0;
    for (const sp of p.spans) { if (sp.messageId === msgId && idx >= sp.firstIdx && idx < sp.firstIdx + sp.n) return base + idx - sp.firstIdx; base += sp.n; }
    return -1;
  }

  /** On-demand speech for several chunks in ONE request: `{ok: true, clip}` with `clip.texts`/`offsetsMs`; used when the service has no bundle. */
  async function ttsBatch(texts) {
    for (let attempt = 0; attempt < 2; attempt++) {
      const r = await request('tts', {texts}, 25000);
      if (r.ok) return {ok: true, clip: {...r.result, arrivedMs: Date.now()}};
      if (attempt) return r;
    }
  }

  /**
   * Sends a voice reply without waiting for transcripts: uploads clips not yet transcribed (in parallel) and asks the service to join the pieces and post them.
   * `sendId` makes retries idempotent. `items`: `{text}` for finished transcripts, `{blob}` for the rest.
   */
  async function sendVoice({sendId, topic, streamId, items}) {
    await Promise.all(items.filter(i => i.blob && !i.uri).map(async i => {
      const form = new FormData(); form.append('file', i.blob, `dictation-${Date.now()}.wav`);
      const up = await zulip.call('POST', 'user_uploads', form, {timeoutMs: 30000});
      if (up.ok) i.uri = up.data.uri;
    }));
    if (items.some(i => i.blob && !i.uri)) return {ok: false, error: 'upload failed'};
    const wire = items.map(i => i.blob ? {uri: i.uri} : {text: i.text});
    const r = await request('send', {topic, ...(streamId ? {streamId} : {}), items: wire, attachment: wire.filter(i => i.uri).map(i => `[audio](${i.uri})`).join(' ')}, 90000, sendId);
    return r.ok ? {ok: true, messageId: r.result.messageId, empty: !!r.result.empty} : r;
  }

  const sttTimes = [];   // recent clips: `{at, roundTrip, work}` (work = the relay's transcription time, i.e. GPU contention)
  /** Median and p90 of the last 5 minutes of speech-to-text, for the debug sheet. */
  function sttStats() {
    const xs = sttTimes.filter(t => Date.now() - t.at < 300000), q = (k, f) => { const a = xs.map(f).sort((p, r) => p - r); return a.length ? a[Math.min(a.length - 1, Math.floor(a.length * k))] : 0; };
    return {n: xs.length, p50: q(0.5, t => t.roundTrip), p90: q(0.9, t => t.roundTrip), workP50: q(0.5, t => t.work)};
  }
  /** `{ok: true, text}` for a WAV blob. */
  async function stt(blob, prompt, kws) {
    const form = new FormData();
    form.append('file', blob, `dictation-${Date.now()}.wav`);
    const up = await zulip.call('POST', 'user_uploads', form, {timeoutMs: 30000});
    if (!up.ok) return {ok: false, error: up.error};
    for (let attempt = 0; attempt < 3; attempt++) {
      const r = await request('stt', {uri: up.data.uri, attachment: `[audio](${up.data.uri})`, ...(prompt ? {prompt} : {}), ...(kws ? {kws: true} : {})}, 60000);
      if (r.ok) { sttTimes.push({at: Date.now(), roundTrip: r.totalMs, work: r.result.timings?.workMs ?? 0}); if (sttTimes.length > 30) sttTimes.shift(); }
      if (r.ok) return {ok: true, text: r.result.text.trim(), timings: {...r.result.timings, roundTripMs: r.totalMs}, noSpeechProb: r.result.noSpeechProb, avgLogprob: r.result.avgLogprob, kws: r.result.kws};
      if (attempt === 2) return r;
    }
  }

  /** A playable URL for `clip`, minting a new one when the old one is about to expire; a deleted pre-rendered clip is forgotten so the next `tts` renders afresh. */
  async function clipUrl(clip, maxAgeMs = URL_TTL_MS) {
    if (Date.now() - clip.arrivedMs < maxAgeMs) return {ok: true, url: clip.audioURL};
    const r = await zulip.call('GET', clip.uri.slice(1));
    if (!r.ok) { for (const [key, list] of bundles) { const rest = list.filter(p => p !== clip); if (rest.length) bundles.set(key, rest); else bundles.delete(key); } return {ok: false, error: 'clip expired'}; }
    Object.assign(clip, {audioURL: new URL(r.data.url, cfg.site).href, arrivedMs: Date.now()});
    return {ok: true, url: clip.audioURL};
  }

  /** Sends debug text (tail-truncated) to the service's bounded `client.log`, then removes the request message from Zulip. */
  async function sendLog(label, text) {
    const parts = [];
    for (let at = 0; at < text.length; at += LOG_PART_CHARS) parts.push(text.slice(at, at + LOG_PART_CHARS));
    for (const [i, part] of parts.entries()) {   // numbered parts, appended by the service in order
      const r = await request('log', {label: parts.length > 1 ? `${label} part ${i + 1}/${parts.length}` : label, text: part}, 15000);
      if (!r.ok) return r;
    }
    return {ok: true};
  }

  async function ping() {
    const r = await request('ping', {t: Date.now()}, 15000);
    return r.ok ? {ok: true, ...r.result, totalMs: r.totalMs} : r;
  }

  return {sttStats, onMessage, backfill, sendVoice, bundleFor, chunkIndex, ttsBatch, preparedEntries: () => [...bundles.values()].flat(), sendLog, stt, clipUrl, ping, preparedCount: () => bundles.size, pendingCount: () => pending.size};
}
