/**
 * Latency probe: pushes a synthetic start-word clip and a synthetic agent message through a real relay on the private test channels,
 * and appends one JSON line with per-stage milliseconds to `latency.jsonl` (rotated at 2 MiB). Run by `zulip-voice-probe.timer` every 5 minutes.
 * Needs: a relay serving the test channels (`zulip-voice-relay-vtest.service`), the human's zuliprc, the bot env file. Uses about 25 Zulip calls per run.
 */
import {readFile, writeFile, appendFile, rename, stat, mkdir} from 'node:fs/promises';
import {homedir} from 'node:os';
import {dirname} from 'node:path';

const RELAY = Number(process.env.VOICE_PROBE_RELAY_STREAM_ID || 652764);
const TASKS = Number(process.env.VOICE_PROBE_TASK_STREAM_ID || 652763);
const HUMAN_ID = Number(process.env.VOICE_HUMAN_ID || 1212497);
const OUT = process.env.VOICE_PROBE_OUT || homedir() + '/.local/state/zulip-voice/latency.jsonl';
const CLIP = process.env.VOICE_PROBE_CLIP || homedir() + '/.local/state/zulip-voice/probe-attention.wav';
const TIMEOUT_MS = 45000;

const kv = text => Object.fromEntries(text.split('\n').map(l => l.match(/^\s*(\w+)\s*=\s*(.+?)\s*$/)).filter(Boolean).map(m => [m[1], m[2].replace(/^['"]|['"]$/g, '')]));
const rc = kv(await readFile(homedir() + '/.config/zulip-voice/zuliprc', 'utf8'));
const env = kv(await readFile(homedir() + '/.config/amh/zulip.env', 'utf8'));
const site = rc.site.replace(/\/$/, '');
const as = (email, key) => {
  const auth = 'Basic ' + Buffer.from(email + ':' + key).toString('base64');
  return async (method, path, fields = {}) => {
    const body = fields instanceof FormData ? fields : new URLSearchParams(Object.entries(fields).map(([k, v]) => [k, typeof v === 'string' ? v : JSON.stringify(v)]));
    const r = await fetch(site + '/api/v1/' + path + (method === 'GET' ? '?' + body : ''), {method, headers: {Authorization: auth}, body: method === 'GET' ? undefined : body, signal: AbortSignal.timeout(20000)});
    const d = await r.json();
    if (d.result !== 'success') throw Error(`${method} ${path}: ${d.code ?? d.msg}`);
    return d;
  };
};
const human = as(rc.email, rc.key), bot = as(env.ZULIP_BOT_EMAIL, env.ZULIP_BOT_API_KEY);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const id = prefix => prefix + '_' + Math.random().toString(36).slice(2, 12) + Date.now().toString(36);

async function attentionClip() {
  try { return await readFile(CLIP); } catch { /* first run: synthesize once */ }
  const r = await fetch('http://127.0.0.1:8770/tts', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({text: 'Attention.'})});
  const wav = Buffer.from(await r.arrayBuffer());
  await mkdir(dirname(CLIP), {recursive: true}); await writeFile(CLIP, wav);
  return wav;
}

/** Waits for the relay's result message with `requestId` in `topic`; resolves `{ms, body}` measured from `since`. */
async function waitResult(topic, requestId, since) {
  for (let anchor = 'newest'; performance.now() - since < TIMEOUT_MS; await sleep(400)) {
    const r = await human('GET', 'messages', {narrow: [{operator: 'channel', operand: RELAY}, {operator: 'topic', operand: topic}], anchor, num_before: 12, num_after: 0, apply_markdown: false});
    for (const m of r.messages) { try { const b = JSON.parse(m.content); if (b.id === requestId && b.op === 'result') return {ms: Math.round(performance.now() - since), body: b, messageId: m.id}; } catch { /* not ours */ } }
  }
  return null;
}

const row = {t: new Date().toISOString()};
const cleanup = [];
try {
  // 1. start-word clip: upload, request, result (with keyword spotting)
  const wav = await attentionClip();
  let t = performance.now();
  const form = new FormData(); form.append('file', new Blob([wav], {type: 'audio/wav'}), `dictation-probe-${Date.now()}.wav`);
  const up = await human('POST', 'user_uploads', form);
  row.sw_upload_ms = Math.round(performance.now() - t);
  const rid = id('stt');
  t = performance.now();
  const posted = await human('POST', 'messages', {type: 'stream', to: RELAY, topic: 'stt', content: JSON.stringify({voice: 1, id: rid, op: 'stt', uri: up.uri, attachment: `[audio](${up.uri})`, kws: true})});
  cleanup.push(() => human('DELETE', 'messages/' + posted.id));
  row.sw_post_ms = Math.round(performance.now() - t);
  const res = await waitResult('stt', rid, t);
  if (res) { cleanup.push(() => bot('DELETE', 'messages/' + res.messageId)); Object.assign(row, {sw_post_to_result_ms: res.ms, sw_text: res.body.text, sw_kws: res.body.kws ?? null, sw_relay: res.body.timings}); row.sw_total_ms = row.sw_upload_ms + row.sw_post_ms + res.ms; }
  else row.sw_error = 'timeout';

  // 2. agent message -> pre-rendered audio ready (the relay's proactive bundle result)
  t = performance.now();
  const msg = await bot('POST', 'messages', {type: 'stream', to: TASKS, topic: 'probe', content: `[probe] Latency probe\n\nThis is a synthetic agent message used to time speech preparation. It has two sentences.`});
  cleanup.push(() => bot('DELETE', 'messages/' + msg.id));
  const bundleId = `proactive_${msg.id}_0`;
  const bundle = await waitResult('tts', bundleId, t);
  if (bundle) { Object.assign(row, {play_message_to_audio_ready_ms: bundle.ms, play_relay: bundle.body.timings, play_bytes_ok: !!bundle.body.audioURL}); cleanup.push(() => bot('DELETE', 'messages/' + bundle.messageId)); }
  else row.play_error = 'timeout';
} catch (error) { row.error = error.message; }

for (const fn of cleanup) await fn().catch(() => {});   // the relay's own sweep removes uploads and anything missed
try {
  await mkdir(dirname(OUT), {recursive: true});
  if ((await stat(OUT).catch(() => ({size: 0}))).size > 2 * 1024 * 1024) await rename(OUT, OUT + '.1');
  await appendFile(OUT, JSON.stringify(row) + '\n');
} catch (error) { console.error('could not write ' + OUT + ': ' + error.message); }
console.log(JSON.stringify(row));
process.exit(row.error || row.sw_error || row.play_error ? 1 : 0);
