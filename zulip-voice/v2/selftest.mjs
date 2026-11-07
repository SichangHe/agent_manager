/**
 * One-tap self-test: runs every check in sequence, never waits forever, and sends one complete report to the server.
 * Every await goes through `w(label, promise, ms)`, so a hang is reported with what it was waiting for.
 * A 100 ms timer measures main-thread stalls; they are listed in the report.
 */
import {createZulip, createEvents} from './zulip.mjs';
import {createVoice} from './voice.mjs';
import {createRecorder} from './recorder.mjs';

const cfg = JSON.parse(localStorage.getItem('zulip-voice-account') || 'null');
const out = [];
const log = (cat, msg) => out.push(`${new Date().toISOString().slice(11, 23)} ${cat} ${msg}`);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const SILENCE = 'data:audio/wav;base64,UklGRiQAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA=';
const MAIN_STALL_MS = 500;
const $ = id => document.getElementById(id);

const stalls = [];
let lastTick = performance.now();
setInterval(() => { const now = performance.now(), lag = now - lastTick - 100; lastTick = now; if (lag > MAIN_STALL_MS && document.visibilityState === 'visible') { stalls.push({at: Date.now(), ms: Math.round(lag)}); log('stall', `main thread blocked ~${Math.round(lag)}ms`); } }, 100);

const zulip = cfg?.relayStreamId ? createZulip(cfg, log) : null;
const voice = zulip && createVoice({zulip, cfg, log});
let events = null, evState = [];
const ensureEvents = () => events ??= (() => {
  const e = createEvents({zulip, log, eventTypes: ['message'], onInit() {}, onEvent: ev => { if (ev.type === 'message') voice.onMessage(ev.message); }, onState: i => { if (evState.at(-1) !== i.state) evState.push(i.state); }});
  e.start();
  return e;
})();

/** `await` with a deadline: rejects `HANG waiting for <label> (<ms>ms)`. */
const w = (label, promise, ms = 15000) => { let timer; return Promise.race([promise, new Promise((_, rej) => { timer = setTimeout(() => rej(Error(`HANG waiting for ${label} (${ms}ms)`)), ms); })]).finally(() => clearTimeout(timer)); };
const T = t0 => Math.round(performance.now() - t0) + 'ms';
const until = (label, pred, ms) => w(label, (async () => { while (!pred()) await sleep(150); })(), ms);
const audioEl = () => Object.assign(new Audio(), {preload: 'auto'});
const playUrl = (el, url, label) => w(label, new Promise((resolve, reject) => { el.src = url; el.onended = () => resolve(performance.now()); el.onerror = () => reject(Error('media error ' + el.error?.code)); el.play().catch(reject); }), 30000);
const clip = async (text, label) => { const r = await w(label, voice.ttsBatch([text]), 30000); if (!r.ok) throw Error(label + ': ' + r.error); return r.clip; };
const urlOf = async c => (await voice.clipUrl(c)).url;
let els = [audioEl(), audioEl()];

/** Shows a countdown prompt and resolves true when `detected()` becomes true, false (skip) when it runs out. */
async function prompt(text, seconds, detected) {
  for (let left = seconds; left > 0; left--) {
    $('prompt').textContent = `${text} (${left}s, or it is skipped)`;
    if (detected()) { $('prompt').textContent = ''; return true; }
    await sleep(1000);
  }
  $('prompt').textContent = '';
  return false;
}

const steps = [
  {name: 'Microphone, recording and transcription (say a sentence)', async run() {
    const clips = [];
    // Runs first: iOS only starts the audio context inside the Start tap, so the recorder must start synchronously.
    const rec = createRecorder({log, onClip: b => clips.push(b), onLevel() {}});
    const t0 = performance.now();
    const s = await w('microphone permission and start (allow the prompt)', rec.start(), 60000);
    if (!s.ok) throw Error(s.error);
    const permMs = T(t0);
    await until('event queue live', () => events.info().state === 'live', 20000);
    $('prompt').textContent = 'Recording 8 s: say a sentence now';
    await sleep(8000); rec.stop(); $('prompt').textContent = '';
    if (!clips.length) throw Error('recorded no speech (mic started in ' + permMs + ')');
    const texts = [];
    for (const c of clips) { const t1 = performance.now(); const r = await w('speech to text', voice.stt(c), 40000); if (!r.ok) throw Error(r.error); texts.push(`"${r.text}" (${T(t1)})`); }
    return `mic start ${permMs}; ${clips.length} clip(s) ${clips.map(c => c.size + 'B').join(',')}; ${texts.join(' ')}`;
  }},
  {name: 'Account and relay ping', async run() {
    const me = await w('Zulip users/me', zulip.call('GET', 'users/me'));
    if (!me.ok) throw Error(me.error);
    await until('event queue live', () => events.info().state === 'live', 20000);
    const t0 = performance.now(); const p = await w('relay ping result', voice.ping(), 20000);
    if (!p.ok) throw Error(p.error);
    return `ping ${T(t0)}; stt=${p.workers.stt} tts=${p.workers.tts}; relay up ${p.uptimeS}s`;
  }},
  {name: 'Audio unlock (play 3 s after the tap, no new tap)', async run() {
    const c = await clip('Audio unlock check.', 'TTS clip');
    const url = await urlOf(c);
    await sleep(3000);
    const t0 = performance.now();
    await playUrl(els[1], url, 'element 2 playback');
    return `second element played without a tap; ${T(t0)}`;
  }},
  {name: 'Speech round trip and playback', async run() {
    const t0 = performance.now();
    const c = await clip('This is a speech round trip test from the server.', 'TTS clip');
    const ready = T(t0), t1 = performance.now();
    await playUrl(els[0], await urlOf(c), 'element 1 playback');
    return `audio ready ${ready}; played ${T(t1)}; durationMs=${c.durationMs}`;
  }},
  {name: 'Two clips: does loading the second stall the first?', async run() {
    const [a, b] = await w('two TTS clips', Promise.all([clip('The first clip is playing while the second one loads on another element.', 'clip A'), clip('Now the second clip follows the first.', 'clip B')]), 40000);
    const [ua, ub] = [await urlOf(a), await urlOf(b)];
    let last = performance.now(), maxFreeze = 0, lastT = -1;
    const timer = setInterval(() => { const now = performance.now(); if (els[0].currentTime !== lastT) { lastT = els[0].currentTime; last = now; } else if (!els[0].ended && !els[0].paused) maxFreeze = Math.max(maxFreeze, now - last); }, 100);
    setTimeout(() => { els[1].src = ub; els[1].load(); }, 1000);
    const endA = await playUrl(els[0], ua, 'clip A playback').finally(() => clearInterval(timer));
    els[1].src = ub;
    const startB = performance.now();
    await playUrl(els[1], ub, 'clip B playback');
    const detail = `first clip froze up to ${Math.round(maxFreeze)}ms while the second loaded; next clip started within ${Math.round(startB - endA)}ms of its end`;
    if (maxFreeze > 700) throw Error('STALL ' + detail);
    return detail;
  }},
  {name: 'Reconnect after airplane mode', optional: true, async run() {
    await until('event queue live', () => events.info().state === 'live', 20000);
    evState = [];
    if (!await prompt('Turn airplane mode ON for a few seconds, then OFF', 40, () => evState.some(s => s !== 'live'))) return 'SKIP: network loss not detected';
    const t0 = performance.now();
    await until('event queue live again after the network returned', () => events.info().state === 'live', 60000);
    const back = T(t0);
    const p = performance.now(); const r = await w('relay ping over the event queue', voice.ping(), 20000);
    if (!r.ok) throw Error(r.error);
    return `live again ${back} after network returned; ping ${T(p)}; states ${evState.join('>')}`;
  }},
  {name: 'Resume after lock or app switch', optional: true, async run() {
    if (!await prompt('Lock the phone or switch app for 15 s, then come back', 40, () => document.visibilityState === 'hidden')) return 'SKIP: page was not hidden';
    await until('coming back to the page', () => document.visibilityState === 'visible', 180000);
    const t0 = performance.now();
    await until('event queue live after resume', () => events.info().state === 'live' && events.info().lastOkAgoMs < 4000, 30000);
    const back = T(t0), p = performance.now();
    const r = await w('relay ping over the event queue', voice.ping(), 20000);
    if (!r.ok) throw Error(r.error);
    return `event loop live ${back} after return; ping ${T(p)}`;
  }},
];

const results = [];
const report = final => `voice self-test ${final ? 'FINAL' : 'partial'} ${new Date().toISOString()}\n${navigator.userAgent}\n` +
  steps.map((s, i) => results[i] ? `${results[i].status} ${s.name} (${results[i].ms}ms) ${results[i].detail}` : `PENDING ${s.name}`).join('\n') +
  `\nmain-thread stalls >${MAIN_STALL_MS}ms: ${stalls.length ? stalls.map(s => s.ms + 'ms').join(',') : 'none'}\n\n` + out.slice(-60).join('\n');
let sending = Promise.resolve();
/** Sends are chained, so the last (complete) report always arrives last. */
const send = (label, text) => sending = sending.then(async () => { for (let n = 0; n < 3; n++) { const r = await voice.sendLog(label, text); if (r.ok) return r; await sleep(1000); } return {ok: false}; });

const list = $('steps');
const nodes = steps.map(s => { const el = document.createElement('div'); el.className = 'step'; el.innerHTML = '<h2></h2><div class="res">waiting</div>'; el.querySelector('h2').textContent = s.name; list.append(el); return el.querySelector('.res'); });

async function start() {
  $('start').disabled = true;
  // Gesture work first: prime audio elements inside the tap.
  for (const el of els) { el.src = SILENCE; el.play().then(() => el.pause(), () => {}); }
  if (!voice) { $('summary').textContent = 'No stored account: open the app and sign in first.'; return; }
  ensureEvents();
  for (const [i, s] of steps.entries()) {
    nodes[i].className = 'res run'; nodes[i].textContent = 'running…';
    const t0 = performance.now(); let r;
    try { const detail = await w(s.name, s.run(), 240000); r = {status: detail.startsWith('SKIP') ? 'SKIP' : 'PASS', detail}; }
    catch (e) { r = {status: /^HANG/.test(e.message) ? 'HANG' : 'FAIL', detail: e.message}; }
    r.ms = Math.round(performance.now() - t0); results[i] = r;
    log('step', `${s.name}: ${r.status} ${r.detail}`);
    nodes[i].className = 'res ' + (r.status === 'PASS' ? 'pass' : r.status === 'SKIP' ? 'run' : 'fail');
    nodes[i].textContent = `${r.status} (${r.ms}ms)\n${r.detail}`;
    $('prompt').textContent = '';
    if (r.status === 'HANG' || r.status === 'FAIL') send('selftest-partial', report(false));
  }
  const bad = results.filter(r => r.status === 'HANG' || r.status === 'FAIL').length;
  $('summary').textContent = `${bad ? bad + ' problem(s)' : 'All checks passed'}; stalls: ${stalls.length}. Sending report…`;
  const text = report(true);
  window.__selftest.text = text;
  const sent = await send('selftest-final', text);
  $('summary').textContent = `${bad ? bad + ' problem(s)' : 'All checks passed'}; stalls: ${stalls.length}. Report ${sent.ok ? 'sent to server' : 'NOT sent (use Copy)'}.`;
  window.__selftest.done = true;
}
$('start').onclick = start;
$('copy').onclick = async e => { const text = window.__selftest.text ?? report(false); try { await navigator.clipboard.writeText(text); e.target.textContent = 'Copied'; } catch { window.prompt('Copy:', text); } setTimeout(() => { e.target.textContent = 'Copy'; }, 2000); };
window.__selftest = {results, out, stalls};
