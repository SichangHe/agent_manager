/** Playwright WebKit run: fake mic plays `clips/timeline.wav` in real time; prints features, inference speed and detections vs the known timeline. `BACKEND=cpu|webgl`, `THR=0.75`. */
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {extname, resolve} from 'node:path';

const pw = (await import('/ssd1/sichangheagent/pw157/node_modules/playwright/index.js')).default;
const root = new URL('.', import.meta.url).pathname;
const types = {'.html': 'text/html', '.js': 'text/javascript', '.json': 'application/json', '.wav': 'audio/wav'};
const server = createServer(async (q, s) => {
  try { const p = resolve(root, '.' + decodeURIComponent(q.url.split('?')[0]).replace(/^\/$/, '/kws.html')); const b = await readFile(p); s.writeHead(200, {'content-type': types[extname(p)] ?? 'application/octet-stream'}).end(b); } catch { s.writeHead(404).end(); }
}).listen(0);
const url = `http://127.0.0.1:${server.address().port}/kws.html?backend=${process.env.BACKEND ?? 'webgl'}&thr=${process.env.THR ?? 0.75}`;
const marks = JSON.parse(await readFile(root + 'clips/timeline.json'));

const browser = await pw[process.env.BROWSER ?? 'webkit'].launch({headless: true, args: process.env.BROWSER === 'chromium' ? ['--autoplay-policy=no-user-gesture-required'] : []});
const page = await (await browser.newContext({...pw.devices['iPhone 14']})).newPage();
page.on('pageerror', e => console.log('pageerror', e.message));
await page.addInitScript(mic => {
  // Only fake: microphone hardware. Real-time playback of the timeline into a MediaStream.
  MediaDevices.prototype.getUserMedia = async () => {
    const ctx = new AudioContext();
    const buf = await ctx.decodeAudioData(await (await fetch(mic)).arrayBuffer());
    const dest = ctx.createMediaStreamDestination(), src = ctx.createBufferSource();
    src.buffer = buf; src.connect(dest); src.start(); window.micStarted = performance.now();
    return dest.stream;
  };
}, '/clips/timeline.wav');
await page.goto(url);
await page.click('#aec');   // fake mic: constraints are ignored anyway
page.on('console', m => console.log('console', m.text().slice(0, 200)));
await page.click('#start');
await page.waitForFunction(() => window.R.ready || window.R.error, null, {timeout: 60000});
const feats = await page.evaluate(() => ({features: R.features, infer: R.infer, error: R.error}));
console.log(JSON.stringify({infer: feats.infer, error: feats.error, sampleRate: feats.features.sampleRate, wasmSimd: feats.features.wasmSimd, webgl2: feats.features.webgl2}));
console.log((await page.textContent('#log')).slice(-600));
const total = marks.at(-1).end + 2;
await page.waitForTimeout(total * 1000);
const {detections, vad, t0, micStarted} = await page.evaluate(() => ({detections: R.detections, vad: R.vad, t0: R.t0, micStarted: window.micStarted}));
const off = micStarted - t0;   // mic timeline start in page-relative ms
const at = d => (d.t - off) / 1000;
console.log('\nword -> detections within [start-0.3, end+0.8] s');
for (const m of marks) {
  const hits = detections.filter(d => at(d) >= m.start - 0.3 && at(d) <= m.end + 0.8).map(d => `${d.word}(${d.p})`);
  console.log(`${m.label.padEnd(8)} ${m.start.toFixed(1)}-${m.end.toFixed(1)}s: ${hits.join(' ') || '-'}`);
}
const vadOn = vad.filter(v => v.on).map(v => ((v.t - off) / 1000).toFixed(1));
console.log(`\nVAD onsets (s): ${vadOn.join(' ')}`);
console.log(`all detections: ${detections.map(d => `${at(d).toFixed(1)}:${d.word}`).join(' ')}`);
await browser.close(); server.close();
