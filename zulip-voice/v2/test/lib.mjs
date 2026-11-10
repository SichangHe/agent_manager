/** Shared helpers for real-browser, real-Zulip, real-GPU tests. Uses the private test channels `vtest-tasks` and `vtest-relay` only. */
import {readFile} from 'node:fs/promises';
import {createServer} from 'node:http';
import {homedir} from 'node:os';
import {extname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

export const TASKS = Number(process.env.VTEST_TASKS || 652763);
export const RELAY = Number(process.env.VTEST_RELAY || 652764);
const dir = resolve(fileURLToPath(import.meta.url), '../..');
const playwrightPath = process.env.PLAYWRIGHT_MODULE || '/ssd1/sichangheagent/pw157/node_modules/playwright/index.js';
export const pw = (await import(playwrightPath)).default;

const parseEnv = text => Object.fromEntries(text.split('\n').map(l => l.match(/^(\w+)=(.*)$/)).filter(Boolean).map(m => [m[1], m[2].trim().replace(/^['"]|['"]$/g, '')]));
const ini = parseEnv(await readFile(homedir() + '/.config/zulip-voice/zuliprc', 'utf8'));
const botEnv = parseEnv(await readFile(homedir() + '/.config/amh/zulip.env', 'utf8'));
export const account = {site: ini.site.replace(/\/$/, ''), email: ini.email, key: ini.key, humanId: 1212497, botId: 1212502, streamId: TASKS, relayStreamId: RELAY};

/** Zulip REST as `who` (`human` or `bot`). */
let callsThisRun = 0, remaining = Infinity;
const MAX_CALLS_PER_RUN = Number(process.env.VTEST_MAX_CALLS || 500), RESERVE = 120;
export function zulipAs(who) {
  const [email, key] = who === 'bot' ? [botEnv.ZULIP_BOT_EMAIL, botEnv.ZULIP_BOT_API_KEY] : [account.email, account.key];
  const auth = 'Basic ' + Buffer.from(email + ':' + key).toString('base64');
  return async (method, path, fields = {}) => {
    const body = new URLSearchParams(Object.entries(fields).map(([k, v]) => [k, typeof v === 'string' ? v : JSON.stringify(v)]));
    if (++callsThisRun > MAX_CALLS_PER_RUN) throw Error(`test run exceeded ${MAX_CALLS_PER_RUN} Zulip calls (VTEST_MAX_CALLS); not running more`);
    for (; who === 'human' && remaining < RESERVE; ) { await sleep(5000); remaining += 20; }   // the human key is shared with the phone: back off below the reserve
    const r = await fetch(account.site + '/api/v1/' + path + (method === 'GET' ? '?' + body : ''), {method, headers: {Authorization: auth}, body: method === 'GET' ? undefined : body});
    if (who === 'human' && r.headers.has('x-ratelimit-remaining')) remaining = Number(r.headers.get('x-ratelimit-remaining'));
    const data = await r.json();
    if (data.code === 'RATE_LIMIT_HIT') { await sleep(Math.ceil((data['retry-after'] ?? 10)) * 1000 + 500); return zulipAs(who)(method, path, fields); }
    if (data.result !== 'success') throw Error(`${method} ${path}: ${data.msg}`);
    return data;
  };
}
export const bot = zulipAs('bot'), human = zulipAs('human');
export const agentSays = (topic, content) => bot('POST', 'messages', {type: 'stream', to: TASKS, topic, content}).then(r => r.id);

/** Deletes every message in the test task channel (best effort). */
export async function wipeTasks() {
  const r = await human('GET', 'messages', {narrow: [{operator: 'stream', operand: TASKS}], anchor: 'newest', num_before: 200, num_after: 0, apply_markdown: false});
  for (const m of r.messages) await zulipAs(m.sender_id === account.botId ? 'bot' : 'human')('DELETE', 'messages/' + m.id).catch(() => {});
}

/** Static server for the client directory (or `dist`). */
export async function serve(root = process.env.DIST || dir, files = {}) {
  const types = {'.html': 'text/html', '.mjs': 'text/javascript', '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm'};
  const server = createServer(async (req, res) => {
    const path = req.url.split('?')[0].replace(/\/$/, '/index.html');
    if (files[path]) { res.writeHead(200, {'Content-Type': 'audio/wav'}).end(files[path]); return; }
    try { const body = await readFile(resolve(root, '.' + path)); res.writeHead(200, {'Content-Type': types[extname(path)] || 'application/octet-stream'}).end(body); } catch { res.writeHead(404).end(); }
  });
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  return {url: 'http://127.0.0.1:' + server.address().port + '/', close: () => new Promise(r => server.close(r))};
}

/** Opens the client in iPhone-sized WebKit (or `BROWSER=chromium`) with the test account; `prefs` seeds localStorage. */
export async function openClient({url, prefs = {}, browserName = process.env.BROWSER || 'webkit', mic = null, storageState}) {
  const type = pw[browserName];
  
  const browser = await type.launch({headless: true, args: browserName === 'chromium' ? ['--autoplay-policy=no-user-gesture-required'] : []});
  const context = await browser.newContext({...pw.devices['iPhone 14'], ...(browserName === 'chromium' ? {isMobile: false, defaultBrowserType: undefined} : {}), storageState});
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', e => errors.push('pageerror: ' + e.message));
  page.on('console', m => { if (m.type() === 'error') errors.push('console: ' + m.text()); });
  await page.addInitScript(({account, prefs, mic}) => {
    if (!sessionStorage.getItem('seeded')) {
      sessionStorage.setItem('seeded', '1');
      localStorage.setItem('zulip-voice-account', JSON.stringify(account));
      for (const [k, v] of Object.entries({'voice2-channels': [], ...prefs})) localStorage.setItem(k, JSON.stringify(v));
    }
    if (mic) { // The only fake: microphone hardware. Plays the recorded WAV into getUserMedia.
      MediaDevices.prototype.getUserMedia = async function () {
        const ctx = new AudioContext();
        const buf = await ctx.decodeAudioData(await (await fetch(mic)).arrayBuffer());
        const dest = ctx.createMediaStreamDestination();
        const src = ctx.createBufferSource(); src.buffer = buf; src.loop = true; src.connect(dest); src.start();   // looped, like a live microphone that keeps producing sound
        return dest.stream;
      };
    }
  }, {account, prefs, mic});
  await page.goto(url);
  return {browser, context, page, errors, close: () => browser.close()};
}

export const sleep = ms => new Promise(r => setTimeout(r, ms));
/** Polls `fn` in the page until truthy; throws with `what` and the client's debug timeline tail on timeout. */
export async function waitFor(page, what, fn, timeoutMs = 20000, arg) {
  const start = Date.now();
  for (;;) {
    if (await page.evaluate(fn, arg)) return Date.now() - start;
    if (Date.now() - start > timeoutMs) throw Error(`timeout waiting for ${what}\n` + (await page.evaluate(() => window.__voice.log.text())).split('\n').slice(-25).join('\n'));
    await sleep(100);
  }
}
export const flagsOf = async id => (await human('GET', 'messages/' + id, {apply_markdown: false})).message.flags;
/** A real speech WAV for the fake microphone, made by the TTS worker. */
export async function speechWav(text) {
  const r = await fetch('http://127.0.0.1:8770/tts', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({text})});
  return Buffer.from(await r.arrayBuffer());
}
/** Waits until the server shows `id` as read. */
export async function serverRead(id, timeoutMs = 8000) {
  for (const start = Date.now(); Date.now() - start < timeoutMs; await sleep(1000)) if ((await flagsOf(id)).includes('read')) return true;
  return false;
}

/** Uploads bytes as `who`; returns the `/user_uploads/...` uri. */
export async function uploadAs(who, name, type, bytes) {
  const [email, key] = who === 'bot' ? [botEnv.ZULIP_BOT_EMAIL, botEnv.ZULIP_BOT_API_KEY] : [account.email, account.key];
  const f = new FormData(); f.append('file', new Blob([bytes], {type}), name);
  const r = await fetch(account.site + '/api/v1/user_uploads', {method: 'POST', headers: {Authorization: 'Basic ' + Buffer.from(email + ':' + key).toString('base64')}, body: f});
  return (await r.json()).uri;
}

/** `wav` padded with `ms` of silence on both sides (a spoken word in a quiet room). */
export function padWav(wav, ms) {
  const dataAt = wav.indexOf('data'), byteRate = wav.readUInt32LE(28), pcm = wav.subarray(dataAt + 8);
  const zeros = Buffer.alloc(Math.round(byteRate * ms / 1000) & ~1);
  const data = Buffer.concat([zeros, pcm, zeros]), head = Buffer.from(wav.subarray(0, dataAt + 8));
  head.writeUInt32LE(data.length, dataAt + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  return Buffer.concat([head, data]);
}
