/**
 * Voice service: speech (STT, TTS) for any Zulip client of the human, passed through a private muted channel.
 * Protocol: `protocol.md`. Importing this module never starts the worker.
 */
import {mkdir, readFile, writeFile, rename, appendFile, stat} from 'node:fs/promises';
import {resolve} from 'node:path';
import {spawn} from 'node:child_process';
import {tmpdir} from 'node:os';
import {unlink} from 'node:fs/promises';
import {pathToFileURL} from 'node:url';
import {speechPieces} from './text.mjs';

const MAX_TEXT_CHARS = 10000;
const MAX_BATCH = 20;
const ORPHAN_SWEEP_MS = 1000 * Number(process.env.VOICE_ORPHAN_SWEEP_S || 300);   // listing 500 messages and attachments costs 3+ calls; deletes are capped below
const MAX_DELETES_PER_SWEEP = 40;
const ID_PATTERN = /^[a-zA-Z0-9_-]{8,100}$/;
const OPS = ['stt', 'tts', 'ack', 'ping', 'log'];
const MAX_LOG_REQUEST_CHARS = 9500;
const LOG_ROTATE_BYTES = 100 * 1024 * 1024;
const LOG_MAX_AGE_MS = 14 * 86400000;

/** The validated request in a channel message, or null when it is not a request from the human. */
export function parseRequest(message, humanId, streamId) {
  if (message.type !== 'stream' || message.stream_id !== streamId || message.sender_id !== humanId) return null;
  let request;
  try { request = JSON.parse(message.content); } catch { return null; }
  if (request?.voice !== 1 || !ID_PATTERN.test(request.id) || !OPS.includes(request.op) || message.subject !== request.op) return null;
  if (request.op === 'tts' && request.texts !== undefined) {
    if (!Array.isArray(request.texts) || !request.texts.length || request.texts.length > MAX_BATCH || request.texts.some(t => typeof t !== 'string' || !t.trim()) || request.texts.join('').length > MAX_TEXT_CHARS) return null;
  } else if (request.op === 'tts' && (typeof request.text !== 'string' || !request.text.trim() || request.text.length > MAX_TEXT_CHARS)) return null;
  if (request.op === 'stt' && typeof request.uri !== 'string') return null;
  if (request.op === 'log' && (typeof request.text !== 'string' || request.text.length > MAX_LOG_REQUEST_CHARS)) return null;
  if (request.op === 'ack' && !(request.ids ?? [request.id]).every(id => typeof id === 'string')) return null;
  return request;
}

export function attachmentURL(uri, site) {
  const url = new URL(uri, site);
  if (url.origin !== new URL(site).origin || !/^\/user_uploads\/[^?#]+$/.test(url.pathname) || url.search || url.hash) throw Error('Invalid attachment URL');
  return url;
}

async function limitedBytes(response, maxBytes = 12 * 1024 * 1024) {
  if (!response.ok) throw Error('Audio request failed: HTTP ' + response.status);
  if (Number(response.headers.get('content-length')) > maxBytes) throw Error('Audio exceeds size limit');
  const chunks = [];
  let size = 0;
  for await (const chunk of response.body) {
    size += chunk.length;
    if (size > maxBytes) throw Error('Audio exceeds size limit');
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

/** Duration of 16-bit PCM WAV `bytes` in ms, or null. */
export function wavDurationMs(bytes) {
  if (bytes.length < 44 || bytes.toString('latin1', 0, 4) !== 'RIFF') return null;
  const byteRate = bytes.readUInt32LE(28);
  const dataAt = bytes.indexOf('data', 12, 'latin1');
  return byteRate && dataAt > 0 ? Math.round(bytes.readUInt32LE(dataAt + 4) / byteRate * 1000) : null;
}

/** `{header, pcm, byteRate}` of a 16-bit PCM WAV, or null. */
function wavParts(bytes) {
  if (bytes.length < 44 || bytes.toString('latin1', 0, 4) !== 'RIFF') return null;
  const dataAt = bytes.indexOf('data', 12, 'latin1');
  if (dataAt < 0) return null;
  const len = Math.min(bytes.readUInt32LE(dataAt + 4), bytes.length - dataAt - 8);
  return {header: bytes.subarray(0, dataAt), pcm: bytes.subarray(dataAt + 8, dataAt + 8 + len), byteRate: bytes.readUInt32LE(28)};
}

/** One WAV from several, plus each one's start offset in ms: one fetch gets a whole message and chunks stay seekable. */
export function bundleWavs(wavs) {
  const parts = wavs.map(wavParts);
  if (parts.some(p => !p)) throw Error('Unexpected audio format');
  const offsetsMs = [];
  let at = 0;
  for (const p of parts) { offsetsMs.push(Math.round(at / parts[0].byteRate * 1000)); at += p.pcm.length; }
  const pcm = Buffer.concat(parts.map(p => p.pcm));
  const header = Buffer.from(parts[0].header);
  header.writeUInt32LE(36 + pcm.length, 4);
  const data = Buffer.alloc(8);
  data.write('data', 0, 'latin1'); data.writeUInt32LE(pcm.length, 4);
  return {bytes: Buffer.concat([header, data, pcm]), offsetsMs, durationMs: Math.round(pcm.length / parts[0].byteRate * 1000)};
}

/** AAC-LC mono m4a (moov first, so playback starts before the download ends) from WAV bytes; about 10x smaller than 16-bit WAV. */
export async function encodeAac(wav, bitrate = '32k') {
  const path = resolve(tmpdir(), `voice-enc-${process.pid}-${Math.random().toString(36).slice(2)}.m4a`);
  await new Promise((done, fail) => {
    const ff = spawn('ffmpeg', ['-nostdin', '-v', 'error', '-f', 'wav', '-i', 'pipe:0', '-c:a', 'aac', '-b:a', bitrate, '-ac', '1', '-movflags', '+faststart', '-f', 'mp4', '-y', path], {stdio: ['pipe', 'ignore', 'pipe']});
    let err = '';
    ff.stderr.on('data', d => { err += d; });
    ff.on('error', fail);
    ff.on('close', code => code ? fail(Error('ffmpeg: ' + err.slice(0, 200))) : done());
    ff.stdin.on('error', () => {});
    ff.stdin.end(wav);
  });
  try { return await readFile(path); } finally { await unlink(path).catch(() => {}); }
}

/** Zulip calls that wait out HTTP 429 instead of failing the request. */
export function createAPI({site, email, key, fetchImpl = fetch}) {
  site = new URL(site).origin;
  if (!site.startsWith('https://')) throw Error('Zulip must use HTTPS');
  const authorization = 'Basic ' + Buffer.from(email + ':' + key).toString('base64');
  const calls = {};
  async function call(method, path, fields = {}, attempt = 0) {
    calls[method] = (calls[method] ?? 0) + 1;
    const form = fields instanceof FormData ? fields : new URLSearchParams(Object.entries(fields).map(([k, v]) => [k, typeof v === 'string' ? v : JSON.stringify(v)]));
    const response = await fetchImpl(site + '/api/v1/' + path + (method === 'GET' ? '?' + form : ''), {method, headers: {Authorization: authorization}, body: method === 'GET' ? undefined : form, signal: AbortSignal.timeout(path === 'events' ? 90000 : 20000), redirect: 'error'});
    const data = await response.json();
    if (response.status === 429 && attempt < 3) {
      const waitS = Number(data['retry-after'] ?? response.headers.get('retry-after')) || 5;
      console.log(`zulip 429 on ${method} ${path}; waiting ${waitS}s`);
      await new Promise(r => setTimeout(r, waitS * 1000));
      return call(method, path, fields, attempt + 1);
    }
    if (!response.ok || data.result !== 'success') {
      if (method === 'DELETE' && /^(messages|attachments)\/\d+$/.test(path) && (response.status === 404 || (data.code === 'BAD_REQUEST' && /^Invalid (message\(s\)|message|attachment)\.?$/.test(data.msg)))) return {result: 'success'};
      throw Error(`Zulip ${method} ${path.split('?')[0]} ${response.status} ${data.code ?? ''}`);
    }
    return data;
  }
  async function download(uri) {
    let url = attachmentURL(uri, site);
    const signal = AbortSignal.timeout(20000);
    for (let redirects = 0; redirects <= 4; redirects++) {
      const response = await fetchImpl(url, {headers: url.origin === site ? {Authorization: authorization} : {}, redirect: 'manual', signal});
      if (![301, 302, 303, 307, 308].includes(response.status)) return limitedBytes(response);
      const location = response.headers.get('location');
      await response.body?.cancel();
      if (!location) throw Error('Missing audio redirect');
      url = new URL(location, url);
      if (url.protocol !== 'https:' || url.username || url.password) throw Error('Invalid audio redirect');
    }
    throw Error('Too many audio redirects');
  }
  /** API calls made since the last read, by method. */
  const takeCalls = () => { const c = {...calls}; for (const k of Object.keys(calls)) delete calls[k]; return c; };
  return {site, call, download, takeCalls};
}

/** Runs async jobs with at most `limit` at once, lowest `priority` first. */
function createPool(limit) {
  const waiting = [];
  let running = 0;
  const pump = () => {
    while (running < limit && waiting.length) {
      waiting.sort((a, b) => a.priority - b.priority || a.seq - b.seq);
      const job = waiting.shift();
      running++;
      job.fn().catch(() => {}).finally(() => { running--; pump(); });
    }
  };
  let seq = 0;
  return {add(priority, fn) { waiting.push({priority, fn, seq: seq++}); pump(); }, stats: () => ({running, waiting: waiting.length})};
}

export async function createRelay({api, humanId, botId, streamId, stateDir, workers = {stt: 'http://127.0.0.1:8771', tts: 'http://127.0.0.1:8770'}, fetchImpl = fetch, now = Date.now, ttlMs = 30 * 86400000, maxActive = 200, maxUploadBytes = 100 * 1024 * 1024, humanApi = null, orphanTtlMs = 120000}) {
  if (!Number.isSafeInteger(streamId) || streamId <= 0) throw Error('Relay channel required');
  await mkdir(stateDir, {recursive: true, mode: 0o700});
  const statePath = resolve(stateDir, 'requests.json');
  let records = {};
  try { records = JSON.parse(await readFile(statePath, 'utf8')); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  for (const record of Object.values(records)) if (record.status === 'working') record.status = 'done';
  let saving = Promise.resolve();
  const save = () => { saving = saving.catch(() => {}).then(async () => { await writeFile(statePath + '.tmp', JSON.stringify(records), {mode: 0o600}); await rename(statePath + '.tmp', statePath); }); return saving; };
  const pools = {stt: createPool(2), tts: createPool(3)};
  const startedMs = now();
  let lastPruneMs = 0, lastOrphanMs = 0;

  async function transcribe(request, result) {
    const audio = await api.download(request.uri);
    const t = now();
    const response = await fetchImpl(workers.stt + '/stt', {method: 'POST', body: audio, signal: AbortSignal.timeout(15000)});
    if (!response.ok) throw Error('Transcription failed: HTTP ' + response.status);
    const data = await response.json();
    if (typeof data.text !== 'string') throw Error('Invalid transcript');
    result.text = data.text;
    return `bytes=${audio.length} stt=${now() - t}ms`;
  }

  /** Renders `texts` with the GPU worker into one WAV bundle and uploads it: `result` gets `offsetsMs`, `durationMs`, `texts`, upload links. */
  async function synthesize(request, result, record) {
    const t = now();
    const texts = request.texts ?? [request.text];
    const wavs = [];
    for (const text of texts) {
      const response = await fetchImpl(workers.tts + '/tts', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({text}), signal: AbortSignal.timeout(15000)});
      wavs.push(await limitedBytes(response));
    }
    const gpuMs = now() - t;
    const bundle = bundleWavs(wavs);
    Object.assign(result, {texts, offsetsMs: bundle.offsetsMs, durationMs: bundle.durationMs});
    const encoded = await encodeAac(bundle.bytes);
    await upload(encoded, request.id, result, record);
    return `chunks=${texts.length} tts=${gpuMs}ms enc+upload=${now() - t - gpuMs}ms wav=${bundle.bytes.length}B aac=${encoded.length}B (${Math.round(encoded.length / bundle.durationMs * 1000)}B/s)`;
  }

  /** Uploads `bytes` and fills `result.uri/attachment/audioURL`. */
  async function upload(bytes, id, result, record) {
    record.bytes = bytes.length;
    const form = new FormData();
    form.append('file', new Blob([bytes], {type: 'audio/mp4'}), 'voice-' + id + '.m4a');
    const uploaded = await api.call('POST', 'user_uploads', form);
    attachmentURL(uploaded.uri, api.site);
    record.uri = uploaded.uri;
    await save();
    result.uri = uploaded.uri;
    result.attachment = '[audio](' + uploaded.uri + ')';
    const temporary = await api.call('GET', uploaded.uri.slice(1));
    const audioURL = new URL(temporary.url, api.site);
    if (audioURL.origin !== api.site || !audioURL.pathname.startsWith('/user_uploads/temporary/') || audioURL.username || audioURL.password) throw Error('Invalid temporary audio URL');
    result.audioURL = audioURL.href;
    result.audioURLExpiresAt = now() + 55000;
  }

  /** Does the work for an accepted request, then posts the result message; never throws. */
  async function process(request, record, queuedMs) {
    const t = now();
    const result = {voice: 1, id: request.id, op: 'result', of: request.op};
    if (request.op === 'tts' && request.proactive) Object.assign(result, {proactive: true, bundle: true, messageId: request.messageId, firstIdx: request.firstIdx});
    let detail = '';
    try {
      detail = request.op === 'stt' ? await transcribe(request, result) : await synthesize(request, result, record);
    } catch (error) {
      result.error = 'Speech processing failed. Try a new request.';
      detail = 'error=' + error.message;
    }
    result.timings = {queueMs: queuedMs, workMs: now() - t};
    record.status = 'sending';
    await save();
    try {
      const sent = await api.call('POST', 'messages', {type: 'stream', to: streamId, topic: request.op, content: JSON.stringify(result)});
      record.responseId = sent.id;
      record.status = 'sent';
    } catch (error) { detail += ' post=' + error.message; }
    await save();
    console.log(`${request.op} id=${request.id.slice(0, 24)}${request.proactive ? ' proactive' : ''} queued=${queuedMs}ms work=${now() - t}ms ${detail}`);
  }

  /** Appends a client's debug text to `client.log`; at 1 MiB the file rotates to `client.log.1` (older data is dropped), so disk use stays under 2 MiB. */
  async function writeLog(request) {
    const path = resolve(stateDir, 'client.log');
    const size = await stat(path).then(s => s.size, () => 0);
    if (size > LOG_ROTATE_BYTES) await rename(path, path + '.1');   // two files of at most 100 MiB: about 200 MiB in all
    await appendFile(path, `=== ${new Date(now()).toISOString()} ${String(request.label ?? '').slice(0, 80)}\n${request.text}\n`, {mode: 0o600});
    const result = {voice: 1, id: request.id, op: 'result', of: 'log', bytes: request.text.length};
    const sent = await api.call('POST', 'messages', {type: 'stream', to: streamId, topic: 'log', content: JSON.stringify(result)});
    records[request.id] = {created: now(), status: 'sent', responseId: sent.id, acked: true};
    await save();
    console.log(`log id=${request.id.slice(0, 24)} ${request.label} bytes=${request.text.length}`);
  }

  /** Drops client-log entries older than 14 days or at/before the watermark in `client.log.analyzed` (see `mark-analyzed.mjs`). */
  async function pruneLogs() {
    const mark = Date.parse(await readFile(resolve(stateDir, 'client.log.analyzed'), 'utf8').catch(() => '')) || 0;
    for (const name of ['client.log', 'client.log.1']) {
      const path = resolve(stateDir, name);
      const text = await readFile(path, 'utf8').catch(() => null);
      if (!text) continue;
      const entries = text.split(/^(?==== )/m);
      const kept = entries.filter(e => { const t = Date.parse(e.slice(4, 28)); return !(t <= mark || now() - t > LOG_MAX_AGE_MS); });
      if (kept.length !== entries.length) { await writeFile(path + '.tmp', kept.join(''), {mode: 0o600}); await rename(path + '.tmp', path); console.log(`pruned ${entries.length - kept.length} log entries from ${name}`); }
    }
  }

  async function ping(request) {
    const health = {};
    for (const [kind, base] of Object.entries(workers)) health[kind] = await fetchImpl(base + '/health', {signal: AbortSignal.timeout(2000)}).then(r => r.ok ? 'ok' : 'http ' + r.status, () => 'down');
    const result = {voice: 1, id: request.id, op: 'result', of: 'ping', t: request.t, relayMs: now(), uptimeS: Math.round((now() - startedMs) / 1000), workers: health, queue: {stt: pools.stt.stats(), tts: pools.tts.stats()}, active: Object.values(records).filter(r => r.status !== 'done').length};
    const sent = await api.call('POST', 'messages', {type: 'stream', to: streamId, topic: 'ping', content: JSON.stringify(result)});
    records[request.id] = {created: now(), status: 'sent', responseId: sent.id};
    await save();
  }

  /** Deletes finished results and uploads: result messages one by one, uploads after one attachment listing. */
  async function cleanup(due) {
    const uris = [];
    for (const record of due) {
      if (record.responseId) { try { await api.call('DELETE', 'messages/' + record.responseId); delete record.responseId; } catch { continue; } }
      if (record.uri) uris.push(record);
      else record.status = 'done';
    }
    if (uris.length) {
      const {attachments} = await api.call('GET', 'attachments');
      for (const record of uris) {
        const item = attachments.find(a => '/user_uploads/' + a.path_id === record.uri);
        try { if (item) await api.call('DELETE', 'attachments/' + item.id); delete record.uri; record.status = 'done'; } catch { /* retried at the next sweep */ }
      }
    }
    await save();
  }

  return {
    async handle(message) {
      const request = parseRequest(message, humanId, streamId);
      if (!request) return;
      if (request.op === 'ack') {
        const acked = (request.ids ?? [request.id]).map(id => records[id]).filter(r => r && !r.proactive && r.status !== 'done');
        for (const record of acked) record.acked = true;
        await save();
        records[request.id] ??= {created: now(), status: 'done', ack: true};
        return;
      }
      if (records[request.id]) return;
      if (request.op === 'log') { await writeLog(request).catch(error => console.log('log failed: ' + error.message)); return; }
      if (request.op === 'ping') { await ping(request).catch(error => console.log('ping failed: ' + error.message)); return; }
      const inflight = Object.values(records).filter(r => r.status === 'working' || r.status === 'sending').length;
      if (inflight >= maxActive + (request.proactive ? 0 : 50)) return;
      const record = records[request.id] = {created: now(), status: 'working', proactive: !!request.proactive, messageId: request.messageId};
      await save();
      const enqueuedMs = now();
      pools[request.op].add(request.proactive ? 1 : 0, () => process(request, record, now() - enqueuedMs));
    },
    /** Pre-renders speech for a new agent message so playback starts instantly. */
    async prepare(message, taskStreamId) {
      if (message.type !== 'stream' || message.stream_id !== taskStreamId || message.sender_id !== botId) return;
      const pieces = speechPieces(message.content);
      // A small head bundle starts playback sooner; the rest of the message follows as one bundle.
      for (const [firstIdx, texts] of [[0, pieces.slice(0, 1)], [1, pieces.slice(1)]]) {
        if (!texts.length) continue;
        await this.handle({type: 'stream', stream_id: streamId, subject: 'tts', sender_id: humanId, content: JSON.stringify({voice: 1, id: `proactive_${message.id}_${firstIdx}`, op: 'tts', texts, proactive: true, messageId: message.id, firstIdx})});
      }
    },
    /**
     * Deletes anything in the relay channel (and relay-named uploads) that outlived its owner's cleanup, e.g. a client that vanished.
     * Requests and results go after `orphanTtlMs`, pre-rendered speech after `ttlMs`; each message is deleted with its sender's key.
     */
    async sweepOrphans() {
      const recent = await api.call('GET', 'messages', {narrow: [{operator: 'stream', operand: streamId}], anchor: 'newest', num_before: 500, num_after: 0, apply_markdown: false});
      let n = 0;
      for (const m of recent.messages ?? []) {
        let proactive = false;
        try { proactive = !!JSON.parse(m.content).proactive || String(JSON.parse(m.content).id).startsWith('proactive_'); } catch { /* not JSON */ }
        const age = now() - m.timestamp * 1000;
        if (age < (proactive ? ttlMs : orphanTtlMs)) continue;
        const owner = m.sender_id === botId ? api : m.sender_id === humanId ? humanApi : null;
        if (n >= MAX_DELETES_PER_SWEEP) break;
        if (owner && await owner.call('DELETE', 'messages/' + m.id).then(() => true, () => false)) n++;
      }
      let u = 0;
      for (const [owner, prefix] of [[api, 'voice-'], [humanApi, 'dictation-']]) {
        if (!owner) continue;
        const {attachments} = await owner.call('GET', 'attachments');
        for (const a of attachments) if (a.name.startsWith(prefix) && now() - a.create_time * 1000 > (prefix === 'voice-' ? ttlMs : orphanTtlMs) && await owner.call('DELETE', 'attachments/' + a.id).then(() => true, () => false)) u++;
      }
      if (n || u) console.log(`swept ${n} orphan messages and ${u} uploads`);
    },
    /** Records which agent messages the human has read, so their clips are evicted first. */
    markRead(ids) { for (const r of Object.values(records)) if (r.messageId && ids.includes(r.messageId)) r.read = true; },
    syncUnread(unreadIds) { for (const r of Object.values(records)) if (r.messageId) r.read = !unreadIds.has(r.messageId); },
    /** Reconciles interrupted posts, then deletes acked or expired items. */
    async expire() {
      for (const [id, record] of Object.entries(records)) {
        if (record.status === 'done' && now() - record.created > 86400000) delete records[id];
        else if (record.status === 'sending') {
          const recent = await api.call('GET', 'messages', {narrow: [{operator: 'stream', operand: streamId}], anchor: 'newest', num_before: 100, num_after: 0, apply_markdown: false});
          const sent = (recent.messages || []).find(m => { try { const body = JSON.parse(m.content); return m.sender_id === botId && body.id === id && body.op === 'result'; } catch { return false; } });
          if (sent) { record.responseId = sent.id; record.status = 'sent'; } else if (now() - record.created > 60000) record.status = 'done';
        }
      }
      const sent = Object.values(records).filter(r => r.status === 'sent');
      const age = r => now() - r.created;
      const full = new Set(sent.filter(r => (r.acked && !r.proactive) || age(r) > ttlMs || (!r.uri && age(r) > orphanTtlMs)));
      // Over the size cap: keep unread clips first (newest first), then read ones; evict the rest.
      let total = 0;
      for (const r of sent.filter(r => r.uri && !full.has(r)).sort((a, b) => (a.read ? 1 : 0) - (b.read ? 1 : 0) || b.created - a.created)) { total += r.bytes ?? 0; if (total > maxUploadBytes) full.add(r); }
      await cleanup([...full]);
      // Results of explicit requests are only needed for a moment; their clips stay until acked, the size cap or the TTL.
      for (const r of sent.filter(r => r.uri && !r.proactive && r.responseId && !full.has(r) && age(r) > orphanTtlMs)) {
        if (await api.call('DELETE', 'messages/' + r.responseId).then(() => true, () => false)) delete r.responseId;
      }
      await save();
      if (now() - lastOrphanMs > ORPHAN_SWEEP_MS) { lastOrphanMs = now(); await this.sweepOrphans(); }
      if (now() - lastPruneMs > 3600000) { lastPruneMs = now(); await pruneLogs().catch(error => console.log('log prune failed: ' + error.message)); }
    },
  };
}

/** The human's own credentials (zuliprc, server-side only) let the sweep delete messages and uploads the human's clients left behind. */
async function readHumanApi(env) {
  const path = env.VOICE_HUMAN_ZULIPRC || (process.env.HOME + '/.config/zulip-voice/zuliprc');
  try {
    const ini = Object.fromEntries((await readFile(path, 'utf8')).split('\n').map(l => l.match(/^\s*(email|key|site)\s*=\s*(.+?)\s*$/)).filter(Boolean).map(m => [m[1], m[2]]));
    return createAPI({site: ini.site, email: ini.email, key: ini.key});
  } catch { return null; }
}

export async function run(env = process.env) {
  const humanId = Number(env.VOICE_HUMAN_ID);
  if (!Number.isSafeInteger(humanId) || humanId <= 0 || !env.VOICE_RELAY_STATE) throw Error('Set VOICE_HUMAN_ID and VOICE_RELAY_STATE');
  const api = createAPI({site: env.ZULIP_SITE, email: env.ZULIP_BOT_EMAIL, key: env.ZULIP_BOT_API_KEY});
  const me = await api.call('GET', 'users/me');
  if (!me.is_bot || me.user_id === humanId) throw Error('Expected separate bot identity');
  const streamId = Number(env.VOICE_RELAY_STREAM_ID);
  const taskStreamId = Number(env.VOICE_TASK_STREAM_ID);
  const humanApi = await readHumanApi(env);
  console.log(humanApi ? 'human key loaded: orphan sweep covers client requests and recordings' : 'NO human key: client-owned messages and uploads are not swept');
  const relay = await createRelay({api, humanId, botId: me.user_id, streamId, stateDir: env.VOICE_RELAY_STATE, humanApi, ttlMs: 1000 * Number(env.VOICE_PROACTIVE_TTL_S || 6 * 3600), orphanTtlMs: 1000 * Number(env.VOICE_ORPHAN_TTL_S || 120), maxUploadBytes: 1048576 * Number(env.VOICE_MAX_UPLOAD_MB || 150)});
  const stats = {events: 0, errors: 0};
  let queue = null, backoffMs = 1000;
  const guard = (what, promise) => promise.catch(error => console.log(`${what} failed: ${error.message}`));
  /** Pre-renders every unread agent message (also the backlog after a restart); one listing call. */
  async function renderUnread() {
    if (!humanApi) return;
    const r = await humanApi.call('GET', 'messages', {narrow: [{operator: 'stream', operand: taskStreamId}, {operator: 'is', operand: 'unread'}], anchor: 'newest', num_before: 100, num_after: 0, apply_markdown: false});
    const mine = (r.messages ?? []).filter(m => m.sender_id === me.user_id);
    relay.syncUnread(new Set(mine.map(m => m.id)));
    for (const m of mine) await relay.prepare(m, taskStreamId);
  }
  guard('unread render', renderUnread());
  setInterval(() => guard('unread render', renderUnread()), 10 * 60000);
  setInterval(() => guard('sweep', relay.expire()), 60000);
  setInterval(() => console.log(`api calls last 60s: bot=${JSON.stringify(api.takeCalls())} human=${JSON.stringify(humanApi?.takeCalls() ?? {})}`), 60000);
  console.log(`voice service up: bot=${me.user_id} human=${humanId} relay=${streamId} task=${taskStreamId}`);
  for (;;) {
    try {
      if (!queue) {
        queue = await api.call('POST', 'register', {event_types: ['message', 'update_message_flags'], apply_markdown: false});
        const backlog = await api.call('GET', 'messages', {narrow: [{operator: 'stream', operand: streamId}], anchor: 'newest', num_before: 100, num_after: 0, apply_markdown: false});
        for (const message of backlog.messages || []) if (Date.now() - message.timestamp * 1000 < 3600000) await guard('backlog', relay.handle(message));
        console.log(`registered queue ${queue.queue_id}`);
      }
      const data = await api.call('GET', 'events', {queue_id: queue.queue_id, last_event_id: queue.last_event_id, dont_block: false});
      for (const event of data.events) {
        queue.last_event_id = event.id;
        if (event.type === 'update_message_flags' && event.flag === 'read' && event.op === 'add') relay.markRead(event.messages ?? []);
        if (event.type !== 'message') continue;
        stats.events++;
        await guard('handle', relay.handle(event.message));
        guard('prepare', relay.prepare(event.message, taskStreamId));
      }
      backoffMs = 1000;
    } catch (error) {
      stats.errors++;
      console.log(`event loop failed; re-registering in ${backoffMs}ms: ${error.message}`);
      queue = null;
      await new Promise(r => setTimeout(r, backoffMs));
      backoffMs = Math.min(backoffMs * 2, 30000);
    }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) run().catch(error => { console.error('Voice service startup failed: ' + error.message); process.exitCode = 1; });
