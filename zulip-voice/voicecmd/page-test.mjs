/** `node page-test.mjs`: builds to /tmp/voicecmd-dist, runs the guided page in Playwright WebKit with the real Zulip relay (STT, log); fake = microphone hardware only (looped timeline.wav). */
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {extname, resolve} from 'node:path';
import {execFileSync} from 'node:child_process';
import {account, pw} from '../v2/test/lib.mjs';

const dist = '/tmp/voicecmd-dist', here = new URL('.', import.meta.url).pathname;
execFileSync('node', [here + 'build.mjs', dist]);
const types = {'.html': 'text/html', '.mjs': 'text/javascript', '.m4a': 'audio/mp4', '.wav': 'audio/wav'};
const server = createServer(async (q, s) => {
  try { const name = q.url.split('?')[0].replace(/^\/$/, '/index.html'); const p = name === '/mic.wav' ? here + 'clips/timeline.wav' : resolve(dist, '.' + name); const b = await readFile(p); s.writeHead(200, {'content-type': types[extname(p)] ?? 'application/octet-stream'}).end(b); } catch { s.writeHead(404).end(); }
}).listen(0);
const browser = await pw.webkit.launch({headless: true});
const context = await browser.newContext({...pw.devices['iPhone 14']});
const page = await context.newPage();
page.on('pageerror', e => console.log('pageerror', e.message));
await page.addInitScript(({account}) => {
  localStorage.setItem('zulip-voice-account', JSON.stringify(account));
  MediaDevices.prototype.getUserMedia = async function () {
    const ctx = new AudioContext(), buf = await ctx.decodeAudioData(await (await fetch('/mic.wav')).arrayBuffer());
    const dest = ctx.createMediaStreamDestination(), src = ctx.createBufferSource();
    src.buffer = buf; src.loop = true; src.connect(dest); src.start();
    return dest.stream;
  };
}, {account});
await page.goto(`http://127.0.0.1:${server.address().port}/`);
await page.click('#start');
for (let i = 0; i < 40 && !(await page.evaluate(() => window.__voicecmd.done)); i++) { await page.waitForTimeout(6000); console.log(i, await page.textContent('#prompt')); }
console.log(await page.textContent('#prompt'));
console.log((await page.textContent('#results')).split('\n\n').slice(1, 2).join('\n'));
console.log((await page.evaluate(() => window.__voicecmd.text ?? '')).length, 'chars');
console.log(await page.textContent('#delivery'));
console.log('sent:', JSON.stringify(await page.evaluate(() => window.__voicecmd.sent)));
await browser.close(); server.close();
