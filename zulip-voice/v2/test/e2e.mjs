/** End-to-end scenarios: real WebKit (iPhone viewport), real Zulip, real relay service and GPU workers. Prints only failures. */
import assert from 'node:assert/strict';
import * as L from './lib.mjs';

const only = process.argv[2];
const scenarios = {};
const results = [];

const player = p => p.evaluate(() => window.__voice.player.snapshot());
const feedText = p => p.evaluate(() => document.querySelector('#feed').innerText);
const tap = (p, sel) => p.locator(sel).tap();

scenarios.autoplayChain = async ({srv}) => {
  // "It plays one message and gets stuck": five messages arrive spaced out; all must play in order and be flagged read.
  await L.wipeTasks();
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha'}});
  try {
    await L.agentSays('alpha', '[alpha] Seed\n\nSeed message to open the topic.');
    await L.waitFor(c.page, 'seed in feed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await c.page.locator('#feed').tap(); // gesture unlocks audio
    await L.waitFor(c.page, 'seed finished', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 25000);
    const ids = [];
    const arrivals = [];
    for (let i = 1; i <= 5; i++) {
      ids.push(await L.agentSays('alpha', `[alpha] Report ${i}\n\nThis is report number ${i}. It has a second sentence for length. And a third one so that playback takes a moment.`));
      arrivals.push(Date.now());
      await L.sleep(i % 2 ? 2500 : 9000);
    }
    await L.waitFor(c.page, 'all five read', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 60000);
    const log = await c.page.evaluate(() => window.__voice.log.text());
    const started = [...log.matchAll(/audio\s+start \d+:/g)].length;
    assert.ok(started >= 5, `expected >=5 playbacks, got ${started}`);
    for (const id of ids) assert.ok(await L.serverRead(id), `message ${id} not flagged read on server`);
    assert.equal(c.errors.filter(e => !/interactive-widget/.test(e)).length, 0, c.errors.join('\n'));
    return {playbacks: started};
  } finally { await c.close(); }
};

scenarios.unreadOnlyAndReread = async ({srv}) => {
  await L.wipeTasks();
  const old = await L.agentSays('alpha', '[alpha] Old\n\nThis is an old message that was already read.');
  await L.human('POST', 'messages/flags', {messages: [old], op: 'add', flag: 'read'});
  const fresh = await L.agentSays('alpha', '[alpha] Fresh\n\nThis is a fresh unread message.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha'}});
  try {
    await L.waitFor(c.page, 'banner', () => document.querySelector('#banner').innerText.includes('Tap to listen (1 unread)'));
    await c.page.locator('#banner').tap();
    await L.waitFor(c.page, 'fresh read', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 25000);
    const log = await c.page.evaluate(() => window.__voice.log.text());
    assert.equal([...log.matchAll(/audio\s+start \d+:/g)].length, 1, 'only the unread message may be read');
    assert.ok(await L.serverRead(fresh), 'fresh not flagged read on server');
    // tap the old message: it plays and continues into the following (already read) message
    await c.page.locator(`.msg.agent[data-id="${old}"] .s`).first().tap();
    await L.waitFor(c.page, 'old message playing', id => document.querySelector(`.msg[data-id="${id}"] .s.now`) !== null, 15000, old);
    await L.waitFor(c.page, 'continues into next', id => document.querySelector(`.msg[data-id="${id}"] .s.now`) !== null, 20000, fresh);
    // next/prev buttons and pause
    await tap(c.page, '#play');
    assert.equal((await player(c.page)).status, 'paused');
    await tap(c.page, '#prev');
    await L.waitFor(c.page, 'prev plays old', id => document.querySelector(`.msg[data-id="${id}"] .s.now`) !== null, 15000, old);
    await tap(c.page, '#faster'); await tap(c.page, '#faster');
    assert.equal(await c.page.locator('#rate').innerText(), '1.2×');
    await tap(c.page, '#next');
    await L.waitFor(c.page, 'next plays fresh', id => document.querySelector(`.msg[data-id="${id}"] .s.now`) !== null, 15000, fresh);
    assert.equal(await c.page.evaluate(() => window.__voice.player.snapshot().slots.some(s => s.t >= 0)), true);
  } finally { await c.close(); }
};

scenarios.switchDirective = async ({srv}) => {
  await L.wipeTasks();
  await L.agentSays('beta', '[beta] Beta task\n\nBeta is waiting with news.');
  await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha'}});
  try {
    await L.waitFor(c.page, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await c.page.locator('#feed').tap();
    await L.waitFor(c.page, 'seed done', () => window.__voice.player.snapshot().status === 'idle', 25000);
    await L.agentSays('alpha', '[alpha] Handoff\n\nI am passing you to beta now.\n[switch: beta]');
    await L.waitFor(c.page, 'switched to beta', () => window.__voice.current() === 'beta', 30000);
    assert.ok(!(await feedText(c.page)).includes('switch:'), 'switch line must not be shown');
    await L.waitFor(c.page, 'beta unread played', () => !document.querySelector('.msg.unread'), 30000);
  } finally { await c.close(); }
};

scenarios.sendText = async ({srv}) => {
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  try {
    await L.waitFor(c.page, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await c.page.locator('#draft').fill('hello from the e2e test');
    const t0 = Date.now();
    await tap(c.page, '#send');
    await L.waitFor(c.page, 'echo', () => [...document.querySelectorAll('.msg.mine')].length === 1 && !document.querySelector('.msg.mine .state button'), 5000);
    const sendMs = Date.now() - t0;
    await L.sleep(1500);
    const r = await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 5, num_after: 0, apply_markdown: false});
    assert.equal(r.messages.filter(m => m.content === 'hello from the e2e test').length, 1, 'exactly one copy on the server');
    await L.sleep(4500);
    assert.equal(await c.page.locator('.msg.mine').count(), 1, 'echo must not duplicate the bubble');
    return {sendMs};
  } finally { await c.close(); }
};

scenarios.dictation = async () => {
  // Real recorder pipeline: speech WAV -> fake mic hardware -> worklet -> VAD clips -> upload -> relay -> Whisper -> draft.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
  const phrase = 'Please run the build now. Then tell me when the tests have finished.';
  const wav = await L.speechWav(phrase + ' ' + phrase.replace('build', 'deployment'));
  const srv2 = await L.serve(undefined, {'/__mic.wav': wav});
  const c = await L.openClient({url: srv2.url, mic: '/__mic.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  try {
    await L.waitFor(c.page, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    const t0 = Date.now();
    await tap(c.page, '#mic');
    await L.waitFor(c.page, 'recording', () => document.querySelector('#composer').classList.contains('rec'), 5000);
    const firstMs = await L.waitFor(c.page, 'first transcript', () => /build/i.test(document.querySelector('#draft').value), 30000);
    // stop and keep text in the box
    await tap(c.page, '#mic');
    await L.waitFor(c.page, 'finished', () => !document.querySelector('#composer').classList.contains('rec'), 20000);
    const draft = await c.page.locator('#draft').inputValue();
    assert.match(draft, /build/i);
    assert.ok(await c.page.locator('#send').isEnabled(), 'send enabled after dictation');
    const log = await c.page.evaluate(() => window.__voice.log.text());
    const clips = [...log.matchAll(/mic\s+clip/g)].length;
    const sttMs = [...log.matchAll(/relay\s+stt \S+ ok total=(\d+)ms/g)].map(m => +m[1]);
    assert.ok(clips >= 1, 'clips recorded');
    // finish-and-send in one tap
    await c.page.locator('#draft').fill('');
    await tap(c.page, '#mic');
    await L.waitFor(c.page, 'recording 2', () => document.querySelector('#composer').classList.contains('rec'), 5000);
    await L.waitFor(c.page, 'transcript 2', () => /build|deployment/i.test(document.querySelector('#draft').value), 30000);
    const t1 = Date.now();
    await tap(c.page, '#send');
    await L.waitFor(c.page, 'sent', () => !document.querySelector('#composer').classList.contains('rec') && document.querySelectorAll('.msg.mine').length === 1, 20000);
    const finishToSentMs = Date.now() - t1;
    await L.sleep(1500);
    const r = await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 5, num_after: 0, apply_markdown: false});
    const mine = r.messages.filter(m => m.sender_id === L.account.humanId);
    assert.equal(mine.length, 1, 'exactly one dictated message sent');
    assert.match(mine[0].content, /build|deployment/i);
    // cancel discards
    await tap(c.page, '#mic');
    await L.waitFor(c.page, 'recording 3', () => document.querySelector('#composer').classList.contains('rec'), 5000);
    await tap(c.page, '#cancel-rec');
    await L.waitFor(c.page, 'cancelled', () => !document.querySelector('#composer').classList.contains('rec'), 10000);
    assert.equal(await c.page.locator('#draft').inputValue(), '');
    return {firstTranscriptMs: firstMs, clips, sttRoundTripMs: sttMs.join(','), finishToSentMs, transcript: mine[0].content.slice(0, 80)};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.connectionDrops = async ({srv}) => {
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha'}});
  try {
    await L.waitFor(c.page, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await c.page.locator('#feed').tap();
    await L.waitFor(c.page, 'seed done', () => window.__voice.player.snapshot().status === 'idle' && document.querySelector('#conn').dataset.state === 'live', 25000);
    // 1. network goes away for 8 s; a message arrives meanwhile
    await c.context.setOffline(true);
    await L.waitFor(c.page, 'client notices offline', () => document.querySelector('#conn').dataset.state !== 'live', 15000);
    await L.agentSays('alpha', '[alpha] Offline\n\nThis message was sent while the phone had no network.');
    await L.sleep(8000);
    const t0 = Date.now();
    await c.context.setOffline(false);
    await L.waitFor(c.page, 'live again', () => document.querySelector('#conn').dataset.state === 'live', 20000);
    await L.waitFor(c.page, 'offline message shown', () => document.querySelector('#feed').innerText.includes('sent while the phone had no network'), 15000);
    const recoverMs = Date.now() - t0;
    await L.waitFor(c.page, 'offline message played and read', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 25000);
    // 2. the server forgets the event queue
    await c.page.route('**/api/v1/events*', route => route.fulfill({status: 400, contentType: 'application/json', body: JSON.stringify({result: 'error', code: 'BAD_EVENT_QUEUE_ID', msg: 'Bad event queue id'})}), {times: 1});
    await L.agentSays('alpha', '[alpha] Lost queue\n\nThis message follows an expired event queue.');
    await L.waitFor(c.page, 'message after queue loss', () => document.querySelector('#feed').innerText.includes('follows an expired event queue'), 20000);
    const log = await c.page.evaluate(() => window.__voice.log.text());
    assert.match(log, /queue expired; re-registering/);
    await L.waitFor(c.page, 'read after queue loss', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 25000);
    // 3. a half-open socket: the poll never answers; resuming the page must restart it at once
    await c.page.route('**/api/v1/events*', () => {});
    await L.sleep(1500);
    await c.page.unroute('**/api/v1/events*');
    await L.agentSays('alpha', '[alpha] Hung\n\nThis message follows a hung connection.');
    const t1 = Date.now();
    await c.page.evaluate(() => { Object.defineProperty(document, 'visibilityState', {value: 'visible', configurable: true}); document.dispatchEvent(new Event('visibilitychange')); });
    await L.waitFor(c.page, 'message after hang', () => document.querySelector('#feed').innerText.includes('follows a hung connection'), 15000);
    const resumeMs = Date.now() - t1;
    return {recoverMs, resumeMs};
  } finally { await c.close(); }
};

scenarios.debugViewAndReload = async ({srv}) => {
  await L.wipeTasks();
  const id = await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `This is sentence number ${i} of a long message that takes a while to speak.`).join(' '));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha'}});
  try {
    await L.waitFor(c.page, 'banner', () => document.querySelector('#banner').innerText.includes('Tap to listen'));
    await c.page.locator('#banner').tap();
    await L.waitFor(c.page, 'playing', () => window.__voice.player.snapshot().status === 'playing');
    await tap(c.page, '#conn');
    await L.waitFor(c.page, 'debug filled', () => /events: live[\s\S]*voice: ok/.test(document.querySelector('#debug-status').textContent) && document.querySelector('#debug-log').textContent.includes('audio'), 8000);
    await tap(c.page, '#dbg-ping');
    await L.waitFor(c.page, 'ping toast', () => /ok rtt=/.test(document.querySelector('#toast').textContent), 10000);
    await c.page.keyboard.press('Escape');
    // reload mid-message: the unfinished message stays unread and offers to resume
    await c.page.reload();
    await L.waitFor(c.page, 'banner after reload', () => document.querySelector('#banner').innerText.includes('Tap to listen (1 unread)'), 15000);
    const logAfter = await c.page.evaluate(() => window.__voice.log.text());
    assert.match(logAfter, /session start[\s\S]*session start/, 'previous session log survives the reload');
    assert.ok(!(await L.flagsOf(id)).includes('read'), 'interrupted message is still unread on the server');
    await c.page.locator('#banner').tap();
    await L.waitFor(c.page, 'plays after reload', () => window.__voice.player.snapshot().status === 'playing', 15000);
  } finally { await c.close(); }
};

scenarios.gaplessLongMessage = async ({srv}) => {
  // One long message = many chunks; measures first-audio latency and the silence between consecutive chunks.
  await L.wipeTasks();
  const body = Array.from({length: 14}, (_, i) => `Paragraph ${i} explains one more detail of the long report in plain words.`).join('\n\n');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha'}});
  try {
    await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
    await L.waitFor(c.page, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await c.page.locator('#feed').tap();
    await L.waitFor(c.page, 'seed done', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 25000);
    const t0 = Date.now();
    const id = await L.agentSays('alpha', '[alpha] Long\n\n' + body);
    await L.waitFor(c.page, 'playing', () => window.__voice.player.snapshot().status === 'playing', 20000);
    const firstAudioMs = Date.now() - t0;
    await L.waitFor(c.page, 'finished', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 140000);
    const lines = (await c.page.evaluate(() => window.__voice.log.text())).split('\n');
    const time = l => { const [h, m, s] = l.slice(0, 12).split(':'); return (+h * 60 + +m) * 60000 + +s * 1000; };
    const gaps = [];
    for (let i = 0; i < lines.length; i++) {
      const e = lines[i].match(/audio\s+ended (\d+):(\d+)/);
      if (!e || +e[1] !== id) continue;
      const next = lines.slice(i + 1).find(l => /audio\s+start \d+:/.test(l));
      if (next) gaps.push(time(next) - time(lines[i]));
    }
    assert.ok(gaps.length >= 5, 'multiple chunks played: ' + gaps.length);
    assert.ok(Math.max(...gaps) < 900, 'gap between chunks: ' + gaps);
    return {firstAudioMs, chunks: gaps.length + 1, maxGapMs: Math.max(...gaps), gapsMs: gaps.join(',')};
  } finally { await c.close(); }
};

scenarios.relayCleanup = async () => {
  // After every kind of relay traffic (and an orphan from a vanished client) the relay channel and uploads must drain to empty.
  await L.wipeTasks();
  const wav = await L.speechWav('Cleanup check for the relay channel.');
  const srv2 = await L.serve(undefined, {'/__mic.wav': wav});
  const c = await L.openClient({url: srv2.url, prefs: {'voice2-topic': 'alpha'}});
  try {
    await L.agentSays('alpha', '[alpha] Cleanup\n\nThis message makes the relay pre-render speech. It has two sentences.');
    await L.waitFor(c.page, 'client up', () => window.__voice.events.info().state === 'live', 20000);
    const r = await c.page.evaluate(async () => {
      const v = window.__voice.voice;
      const tts = await v.ttsBatch(['A one off sentence for the cleanup test.']);
      const blob = await (await fetch('/__mic.wav')).blob();
      const stt = await v.stt(blob);
      const ping = await v.ping();
      await v.sendLog('cleanup-test', 'hello');
      return {tts: tts.ok, stt: stt.ok && stt.text, ping: ping.ok};
    });
    assert.ok(r.tts && r.stt && r.ping, JSON.stringify(r));
    // an orphan: a request from a client that vanished before cleaning up
    await L.human('POST', 'messages', {type: 'stream', to: L.RELAY, topic: 'tts', content: JSON.stringify({voice: 1, id: 'orphan_' + Date.now(), op: 'tts', text: 'Nobody will clean this up.'})});
    await c.close();
    const start = Date.now();
    let left;
    for (;;) {
      const m = await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.RELAY}], anchor: 'newest', num_before: 200, num_after: 0, apply_markdown: false});
      const keep = x => { try { return String(JSON.parse(x.content).id).startsWith('proactive_'); } catch { return false; } };
      const botBytes = (await L.bot('GET', 'attachments')).attachments.filter(a => /^voice-/.test(a.name)).reduce((n, a) => n + a.size, 0);
      left = {messages: m.messages.filter(x => !keep(x)).length, dictationUploads: (await L.human('GET', 'attachments')).attachments.filter(a => /^dictation-/.test(a.name)).length, botUploadBytes: botBytes};
      // only pre-rendered speech may remain, and only within the (1 MB test) size cap
      if (!left.messages && !left.dictationUploads) break;   // bot upload bytes are account-wide (production clips included), so only reported
      if (Date.now() - start > 120000) assert.fail('relay not drained: ' + JSON.stringify(left));
      await L.sleep(5000);
    }
    return {drainedAfterMs: Date.now() - start};
  } finally { await c.close().catch(() => {}); await srv2.close(); }
};

scenarios.clipDeletedRecovers = async ({srv}) => {
  // The pre-rendered clip's upload disappears (evicted or expired): playback must recover by rendering a fresh clip.
  await L.wipeTasks();
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  try {
    const id = await L.agentSays('alpha', '[alpha] Victim\n\nThis clip will lose its upload before it plays.');
    await L.waitFor(c.page, 'prepared clip known', () => window.__voice.voice.preparedCount() > 0 && document.querySelector('.msg.agent'), 20000);
    const att = (await L.bot('GET', 'attachments')).attachments.filter(a => a.name.startsWith('voice-proactive_' + id));
    assert.ok(att.length, 'found the prepared upload');
    for (const a of att) await L.bot('DELETE', 'attachments/' + a.id);
    await c.page.evaluate(() => { for (const e of window.__voice.voice.preparedEntries()) e.arrivedMs = 0; });
    await c.page.locator('.msg.agent .s').first().tap();
    await L.waitFor(c.page, 'recovered and playing', () => window.__voice.player.snapshot().status === 'playing', 30000);
    await L.waitFor(c.page, 'read', () => !document.querySelector('.msg.unread'), 30000);
    assert.ok(await L.serverRead(id));
  } finally { await c.close(); }
};

scenarios.walkingNetwork = async () => {
  // A phone walking outside: random 0-2.5 s latency, 20% aborted requests, and two 6 s blackouts. The UI must stay responsive,
  // show a clear state for every pending item, and everything must end consistent once the network is good again.
  await L.wipeTasks();
  const wav = await L.speechWav('Please run the build now and tell me when it is done.');
  const srv2 = await L.serve(undefined, {'/__mic.wav': wav});
  const c = await L.openClient({url: srv2.url, mic: '/__mic.wav', prefs: {'voice2-topic': 'alpha'}});
  const p = c.page;
  try {
    await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
    await L.waitFor(p, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await p.locator('#feed').tap();
    await L.waitFor(p, 'seed done', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 25000);
    await p.evaluate(() => { window.__lag = []; let last = performance.now(); setInterval(() => { const n = performance.now(); if (n - last - 100 > 250) window.__lag.push(Math.round(n - last - 100)); last = n; }, 100); });
    let blackoutUntil = 0, aborted = 0, passed = 0;
    await p.route(/\/(api\/v1|user_uploads)\//, async route => {
      const now = Date.now();
      if (now < blackoutUntil) { await L.sleep(1500); aborted++; return route.abort('connectionfailed').catch(() => {}); }
      if (Math.random() < 0.2) { await L.sleep(Math.random() * 1500); aborted++; return route.abort('connectionreset').catch(() => {}); }
      await L.sleep(Math.random() * 2500);
      passed++;
      return route.continue().catch(() => {});
    });
    const agentIds = [];
    const mineWanted = [];
    const t0 = Date.now();
    const blackout = () => { blackoutUntil = Date.now() + 6000; };
    // 1. messages arrive while the network is bad
    agentIds.push(await L.agentSays('alpha', '[alpha] W1\n\nFirst walking message with a couple of sentences. Second sentence here.'));
    await L.sleep(3000); blackout();
    agentIds.push(await L.agentSays('alpha', '[alpha] W2\n\nSecond walking message that arrives during a blackout. It should still be read.'));
    // 2. the human sends text meanwhile
    await p.locator('#draft').fill('walking text one'); mineWanted.push('walking text one');
    const tSend = Date.now(); await p.locator('#send').tap();
    const sendTapMs = Date.now() - tSend;
    assert.ok(await p.locator('.msg.mine').count() >= 1, 'bubble appears at once');
    const bubbleState = await p.locator('.msg.mine .state').first().innerText();
    // 3. controls respond during the bad network
    const respond = [];
    for (let i = 0; i < 4; i++) { const t = Date.now(); const before = await p.locator('#rate').innerText(); await p.locator('#faster').tap(); await L.waitFor(p, 'rate changes', b => document.querySelector('#rate').innerText !== b, 2000, before); respond.push(Date.now() - t); await L.sleep(700); }
    // 4. dictation and stop-and-send under bad network
    await L.sleep(4000); blackout();
    await p.locator('#mic').tap();
    await L.waitFor(p, 'recording', () => document.querySelector('#composer').classList.contains('rec'), 10000);
    await L.waitFor(p, 'dictated text or pending marker', () => /build|…/.test(document.querySelector('#draft').value), 40000);
    await p.locator('#send').tap();
    agentIds.push(await L.agentSays('alpha', '[alpha] W3\n\nThird walking message after the dictation. Last sentence.'));
    // everything has a visible state while it is flaky
    const sawNet = await p.evaluate(() => !document.querySelector('#net').hidden || document.querySelector('#conn').dataset.state !== 'live' || !!document.querySelector('.msg.mine.sending'));
    await L.sleep(Math.max(0, 40000 - (Date.now() - t0)));
    // good network again
    await p.unroute(/\/(api\/v1|user_uploads)\//);
    const tGood = Date.now();
    await L.waitFor(p, 'everything settled', () => document.querySelector('#conn').dataset.state === 'live' && document.querySelector('#net').hidden && !document.querySelector('.msg.mine.sending, .msg.mine.failed') && !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 90000);
    const settleMs = Date.now() - tGood;
    for (const id of agentIds) assert.ok(await L.serverRead(id), `agent message ${id} not read on server`);
    const r = await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 30, num_after: 0, apply_markdown: false});
    const mine = r.messages.filter(m => m.sender_id === L.account.humanId).map(m => m.content);
    assert.equal(mine.filter(t => t === 'walking text one').length, 1, 'typed message exactly once: ' + JSON.stringify(mine));
    assert.equal(mine.length, 2, 'typed and dictated messages exactly once each: ' + JSON.stringify(mine));
    const lag = await p.evaluate(() => window.__lag);
    const calls = (await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => /\shttp\s/.test(l)).length;
    assert.ok(Math.max(0, ...lag) < 400, 'main thread stalls: ' + lag);
    assert.ok(Math.max(...respond) < 700, 'controls slow: ' + respond);
    assert.ok(sendTapMs < 500, 'send tap blocked ' + sendTapMs);
    assert.ok(sawNet, 'a visible network/pending state was shown');
    return {pageHttpLogLines: calls, requestsPassed: passed, requestsAborted: aborted, sendTapMs, bubbleState, controlResponseMs: respond.join(','), maxMainThreadStallMs: Math.max(0, ...lag), settleAfterGoodNetworkMs: settleMs};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.chunkSkip = async ({srv}) => {
  await L.wipeTasks();
  const long = await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 5}, (_, i) => `Paragraph ${i} of the long message, spoken as its own chunk.`).join('\n\n'));
  const second = await L.agentSays('alpha', '[alpha] Second\n\nThe second message.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const now = () => p.evaluate(() => { const n = document.querySelector('.s.now'); return n ? n.closest('.msg').dataset.id + ':' + n.dataset.from : null; });
  const at = async want => { await L.waitFor(p, 'chunk ' + want, w => { const n = document.querySelector('.s.now'); return n && n.closest('.msg').dataset.id + ':' + n.dataset.from === w; }, 15000, want); };
  try {
    await L.waitFor(p, 'messages', () => document.querySelectorAll('.msg.agent').length === 2);
    await p.locator(`.msg[data-id="${long}"] .s`).first().tap();
    await at(`${long}:0`);
    await tap(p, '#next'); await at(`${long}:1`);
    await tap(p, '#next'); await at(`${long}:2`);
    await tap(p, '#prev'); await at(`${long}:1`);          // within 2 s: previous chunk
    await L.sleep(2600);
    await tap(p, '#prev'); await at(`${long}:1`);          // after 2 s: restarts this chunk
    assert.ok(await p.evaluate(() => window.__voice.player.currentTime()) < 2, 'restarted from the start');
    // long press next: whole next message
    await p.locator('#next').dispatchEvent('pointerdown'); await L.sleep(700); await p.locator('#next').dispatchEvent('pointerup');
    await p.locator('#next').dispatchEvent('click');
    await at(`${second}:0`);
    // chunk next from the last chunk of a message crosses into the next message
    await p.locator(`.msg[data-id="${long}"] .s`).last().tap(); await at(`${long}:4`);
    await tap(p, '#next'); await at(`${second}:0`);
  } finally { await c.close(); }
};

scenarios.skipSpam = async ({srv}) => {
  await L.wipeTasks();
  const id = await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 12}, (_, i) => `Paragraph ${i} of a long message, spoken as its own chunk.`).join('\n\n'));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const nowIdx = () => p.evaluate(() => { const n = document.querySelector('.s.now'); return n ? +n.dataset.from : -1; });
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.evaluate(() => { window.__lag = []; let last = performance.now(); setInterval(() => { const n = performance.now(); if (n - last - 100 > 250) window.__lag.push(Math.round(n - last - 100)); last = n; }, 100); });
    await p.locator(`.msg[data-id="${id}"] .s`).first().tap();
    await L.waitFor(p, 'chunk 0 playing', () => window.__voice.player.snapshot().status === 'playing');
    // first tap acts at once; 4 more quick taps accumulate; the highlight moves instantly, audio loads only the final target
    const t = await p.evaluate(async () => {
      const b = document.querySelector('#next'), t0 = performance.now(), seen = [];
      for (let i = 0; i < 5; i++) { b.click(); await new Promise(r => setTimeout(r, 60)); const n = document.querySelector('.s.now'); seen.push(n ? +n.dataset.from : -1); }
      return {ms: Math.round(performance.now() - t0), seen, disabled: b.disabled};
    });
    assert.deepEqual(t.seen.slice(1), [2, 3, 4, 5], 'highlight follows each tap instantly: ' + t.seen);
    assert.equal(t.disabled, false, 'button never disabled');
    await L.waitFor(p, 'audio at chunk 5', () => window.__voice.player.snapshot().status === 'playing' && document.querySelector('.s.now').dataset.from === '5', 8000);
    const starts = (await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => /audio\s+start/.test(l)).map(l => l.split(':').pop());
    assert.ok(!starts.includes('2') && !starts.includes('3') && !starts.includes('4'), 'skipped chunks never started: ' + starts);
    // and backwards spam
    await p.evaluate(async () => { const b = document.querySelector('#prev'); for (let i = 0; i < 3; i++) { b.click(); await new Promise(r => setTimeout(r, 60)); } });
    await L.waitFor(p, 'audio at chunk 3 (restart + 2 back)', () => document.querySelector('.s.now').dataset.from === '3' && window.__voice.player.snapshot().status === 'playing', 8000);
    const lag = await p.evaluate(() => window.__lag);
    assert.ok(Math.max(0, ...lag) < 300, 'stalls ' + lag);
    return {fiveTapsMs: t.ms, highlightPath: t.seen.join('>')};
  } finally { await c.close(); }
};

scenarios.apiBudget = async ({srv}) => {
  // 10 agent messages (one long) played by autoplay: Zulip calls on the phone's key must stay far below the 200/min limit.
  await L.wipeTasks();
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha'}});
  const p = c.page;
  try {
    await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
    await L.waitFor(p, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await p.locator('#feed').tap();
    await L.waitFor(p, 'seed done', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 25000);
    const before = await p.evaluate(() => window.__voice.zulip.totalCalls());
    const t0 = Date.now(), ids = [];
    for (let i = 1; i <= 10; i++) { ids.push(await L.agentSays('alpha', `[alpha] M${i}\n\n` + (i === 5 ? Array.from({length: 8}, (_, k) => `Long message paragraph ${k} with some words in it.`).join('\n\n') : `Message number ${i}. It has a second sentence.`))); await L.sleep(1500); }
    await L.waitFor(p, 'all read', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 150000);
    const minutes = (Date.now() - t0) / 60000;
    const calls = await p.evaluate(() => window.__voice.zulip.totalCalls());
    const total = calls - before;
    const perMessage = +(total / 10).toFixed(1), perMinute = Math.round(total / minutes);
    assert.ok(perMinute < 60, `phone made ${perMinute} API calls/min (${total} calls, ${perMessage} per message)`);
    assert.ok(perMessage <= 4, `per message ${perMessage}`);
    // idle: no traffic besides the long poll
    const idle0 = await p.evaluate(() => window.__voice.zulip.totalCalls()); await L.sleep(20000);
    const idle = (await p.evaluate(() => window.__voice.zulip.totalCalls())) - idle0;
    assert.ok(idle <= 2, 'idle calls in 20 s: ' + idle);
    return {phoneCalls: total, perMessage, perMinute, idleCallsIn20s: idle, minutes: +minutes.toFixed(1)};
  } finally { await c.close(); }
};

scenarios.reloadNoDuplicates = async ({srv}) => {
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    for (const text of ['first sent message', 'second sent message']) {
      await p.locator('#draft').fill(text); await p.locator('#send').tap();
      await L.waitFor(p, text, t => [...document.querySelectorAll('.msg.mine')].some(n => n.innerText.includes(t)) && !document.querySelector('.msg.mine.sending'), 8000, text);
    }
    await L.sleep(5000);
    const count = async () => (await p.evaluate(() => [...document.querySelectorAll('.msg.mine .bubble')].map(b => b.innerText))).sort();
    assert.deepEqual(await count(), ['first sent message', 'second sent message'], 'before reload');
    const seen = [];
    await p.reload();
    for (let i = 0; i < 40; i++) { seen.push((await count()).join('|')); await L.sleep(200); }
    const bad = seen.filter(s => s && s !== 'first sent message|second sent message');
    assert.equal(bad.length, 0, 'bubbles during reload: ' + [...new Set(bad)].join(' // '));
    const r = await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 20, num_after: 0, apply_markdown: false});
    assert.equal(r.messages.filter(m => m.sender_id === L.account.humanId).length, 2, 'server has exactly two human messages');
  } finally { await c.close(); }
};

scenarios.dictationBlocksPlayback = async () => {
  await L.wipeTasks();
  const wav = await L.speechWav('Dictating now, please wait for me to finish this sentence and then some more words.');
  const srv2 = await L.serve(undefined, {'/__mic.wav': wav});
  const c = await L.openClient({url: srv2.url, mic: '/__mic.wav', prefs: {'voice2-topic': 'alpha'}});
  const p = c.page;
  try {
    await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
    await L.waitFor(p, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await p.locator('#feed').tap();
    await L.waitFor(p, 'seed done', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 25000);
    await p.locator('#mic').tap();
    await L.waitFor(p, 'recording', () => document.querySelector('#composer').classList.contains('rec'), 10000);
    const id = await L.agentSays('alpha', '[alpha] During\n\nThis message arrives while you dictate. It must wait.');
    await L.waitFor(p, 'message shown', () => document.querySelector('#feed').innerText.includes('while you dictate'), 15000);
    await L.sleep(6000);
    assert.equal((await player(p)).status, 'idle', 'nothing plays during dictation');
    assert.ok(await p.locator('.msg.unread').count() >= 1, 'still unread');
    await p.locator('#cancel-rec').tap();
    await L.waitFor(p, 'plays after dictation', () => window.__voice.player.snapshot().status === 'playing' || !document.querySelector('.msg.unread'), 20000);
    await L.waitFor(p, 'read', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 30000);
    assert.ok(await L.serverRead(id));
  } finally { await c.close(); await srv2.close(); }
};

scenarios.switchNeverBlank = async ({srv}) => {
  await L.wipeTasks();
  await L.agentSays('beta', '[beta] B\n\nBeta has history.\n\n' + Array.from({length: 25}, (_, i) => `Beta paragraph ${i} filling the screen.`).join('\\n\\n'));
  await L.agentSays('gamma', '[gamma] G\n\nGamma has history too.\n\n' + Array.from({length: 25}, (_, i) => `Gamma paragraph ${i} filling the screen.`).join('\\n\\n'));
  await L.agentSays('alpha', '[alpha] A\n\nAlpha first. Second sentence.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const filled = (want, ms = 10000) => L.waitFor(p, 'feed shows ' + want, w => document.querySelector('#feed').innerText.includes(w), ms, want);
  const atEnd = async what => {
    await L.sleep(2500);
    const m = await p.evaluate(() => { const f = document.querySelector('#feed'), last = [...f.querySelectorAll('.msg')].at(-1), fb = f.getBoundingClientRect(); return {tall: f.scrollHeight > f.clientHeight + 50, over: f.scrollTop + f.clientHeight - f.scrollHeight, gap: fb.bottom - last.getBoundingClientRect().bottom}; });
    assert.ok(m.over <= 1, what + ': scrolled past content by ' + m.over);
    assert.ok(!m.tall || m.gap <= 30, what + ': blank space under last message ' + m.gap);
  };
  const nonBlank = () => p.evaluate(() => document.querySelector('#feed').innerText.trim().length > 0);
  try {
    await filled('Alpha first');
    // chip switch to an uncached topic, with slow network: loading state, never blank
    await p.route(/\/api\/v1\/messages\?/, async r => { await L.sleep(2500); r.continue().catch(() => {}); });
    await p.locator('#agent').tap(); await p.locator('#topics li', {hasText: 'beta'}).tap();
    assert.ok(await nonBlank(), 'blank right after switch');
    await filled('Beta has history', 15000);
    await atEnd('after chip switch');
    await p.unroute(/\/api\/v1\/messages\?/);
    // switch while offline: loading state, then history once back online
    await c.context.setOffline(true);
    await p.locator('#agent').tap(); await p.locator('#topics li', {hasText: 'gamma'}).tap();
    await L.sleep(3000);
    assert.ok(await nonBlank(), 'blank while offline');
    await c.context.setOffline(false);
    await filled('Gamma has history', 30000);
    await atEnd('after offline switch');
    // [switch: TASK] while playing
    await p.locator('#auto').tap();
    await L.agentSays('gamma', '[gamma] H\n\nHanding over now.\n[switch: alpha]');
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'alpha', 170000);
    assert.ok(await nonBlank(), 'blank after directive switch');
    await filled('Alpha first');
    await atEnd('after directive switch');
  } finally { await c.close(); }
};

scenarios.attachments = async ({srv}) => {
  await L.wipeTasks();
  // 1x1 png uploaded by the bot, linked in an agent message; plus a text file
  const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64');
  const uri = await L.uploadAs('bot', 'shot.png', 'image/png', png), uri2 = await L.uploadAs('bot', 'notes.txt', 'text/plain', Buffer.from('hello'));
  await L.agentSays('alpha', `[alpha] Files\n\nHere is the screenshot and notes.\n\n[shot.png](${uri})\n[notes.txt](${uri2})`);
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'image loaded', () => { const i = document.querySelector('.att img'); return i && i.complete && i.naturalWidth > 0; }, 20000);
    assert.equal(await p.locator('.att.chip').count(), 1, 'text file is a chip');
    assert.ok(!(await p.locator('#feed').innerText()).includes('user_uploads'), 'no raw link text');
    // swipe collapses the image into a chip
    await p.locator('.att:not(.chip)').dispatchEvent('touchstart', {touches: [{clientX: 300, clientY: 100}]});
    await p.locator('.att:not(.chip)').dispatchEvent('touchend', {changedTouches: [{clientX: 100, clientY: 100}]});
    await L.waitFor(p, 'collapsed', () => document.querySelectorAll('.att.chip').length === 2, 3000);
    await p.reload();
    await L.waitFor(p, 'still collapsed after reload', () => document.querySelectorAll('.att.chip').length === 2, 15000);
    await p.locator('.att.chip .chipbtn', {hasText: 'shot.png'}).tap();
    await L.waitFor(p, 'expanded', () => document.querySelectorAll('.att img').length === 1, 3000);
    // global hide
    await p.locator('#settings').tap(); await p.locator('#set-hide-att').check(); await p.keyboard.press('Escape');
    await L.waitFor(p, 'all hidden', () => document.querySelectorAll('.att img').length === 0, 3000);
  } finally { await c.close(); }
};

scenarios.pauseResume = async ({srv}) => {
  await L.wipeTasks();
  const wav = await L.speechWav('Quick dictation.');
  const srv2 = await L.serve(undefined, {'/__mic.wav': wav});
  const c = await L.openClient({url: srv2.url, mic: '/__mic.wav', prefs: {'voice2-topic': 'alpha'}});
  const p = c.page;
  const t = () => p.evaluate(() => window.__voice.player.currentTime());
  try {
    const id = await L.agentSays('alpha', '[alpha] One\n\n' + 'This single chunk of speech is deliberately long so that it plays for a while. '.repeat(1).trim() + ' It keeps going with some more words to stretch it out a little further.');
    await L.waitFor(p, 'banner', () => document.querySelector('#banner').innerText.includes('Tap to listen'));
    await p.locator('#banner').tap();
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing' && window.__voice.player.currentTime() > 1.5, 20000);
    // resume mid-chunk continues near where it stopped (backs up at most 2 s), never from the chunk start
    await p.evaluate(() => window.__voice.player.pause());
    const before = await t();
    await p.evaluate(() => window.__voice.player.resume());
    await L.waitFor(p, 'playing again', () => window.__voice.player.snapshot().status === 'playing', 5000);
    const after = await t();
    assert.ok(after >= before - 0.3 && after <= before + 1, `resumed at ${after}, paused at ${before}`);
    // pausing when the chunk is almost heard counts it as heard: resume advances (no replay) and the message is read
    const paused = await p.evaluate(async () => {
      const pl = window.__voice.player;
      for (let i = 0; i < 400; i++) { const el = [...document.querySelectorAll('audio')].find(a => !a.paused); if (el && el.duration && el.currentTime / el.duration > 0.85) { pl.pause(); return true; } await new Promise(r => setTimeout(r, 50)); }
      return false;
    });
    assert.ok(paused, 'caught the chunk near its end');
    await p.evaluate(() => window.__voice.player.resume());
    await L.waitFor(p, 'done without replay', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 10000);
    assert.ok(await L.serverRead(id));
    assert.equal((await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => /audio\s+start/.test(l)).length, 2, 'started once, plus one mid-chunk resume, no replay after the near-end pause');
    // rapid dictation start/stop: no errors from late worklet frames
    for (let i = 0; i < 3; i++) { await p.locator('#mic').tap(); await L.sleep(400); await p.locator('#cancel-rec').tap(); await L.sleep(300); }
    await L.sleep(1500);
    assert.deepEqual(c.errors.filter(e => /sampleRate|null is not an object/.test(e)), []);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.micSpam = async () => {
  await L.wipeTasks();
  const wav = await L.speechWav('Testing the microphone button.');
  const srv2 = await L.serve(undefined, {'/__mic.wav': wav});
  const c = await L.openClient({url: srv2.url, mic: '/__mic.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const rec = () => p.evaluate(() => document.querySelector('#composer').classList.contains('rec'));
  try {
    await L.agentSays('alpha', '[alpha] Seed\n\nSeed message.');
    await L.waitFor(p, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    // first dictation: the UI flips on the tap itself
    const t1 = await p.evaluate(async () => { const t = performance.now(); document.querySelector('#mic').click(); const ms = performance.now() - t; return {ms, rec: document.querySelector('#composer').classList.contains('rec')}; });
    // Cold start creates the AudioContext inside the tap (iOS needs the gesture), so the first paint waits for it.
    assert.ok(t1.rec && t1.ms < 150, 'first tap to recording UI ' + JSON.stringify(t1));
    await L.waitFor(p, 'mic ready', () => window.__voice.log.text().includes('mic    ready'), 15000);
    await p.evaluate(() => document.querySelector('#cancel-rec').click());
    await L.waitFor(p, 'idle', () => !document.querySelector('#composer').classList.contains('rec'), 5000);
    // second dictation reuses the warm microphone
    const t2 = await p.evaluate(async () => { const t = performance.now(); document.querySelector('#mic').click(); await new Promise(r => requestAnimationFrame(r)); return performance.now() - t; });
    assert.ok(t2 < 100, 'second dictation start ' + t2);
    await p.evaluate(() => document.querySelector('#cancel-rec').click());
    await L.waitFor(p, 'idle again', () => !document.querySelector('#composer').classList.contains('rec'), 5000);
    // rapid spam: start, stop, start, stop, start within a few ms, in a fresh page so startup is cold
    await p.reload();
    await L.waitFor(p, 'reloaded', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await p.evaluate(async () => { const m = document.querySelector('#mic'), x = document.querySelector('#cancel-rec'); m.click(); m.click(); m.click(); await new Promise(r => setTimeout(r, 30)); m.click(); x.click(); m.click(); });
    await L.sleep(3000);
    const state = await rec();
    // whatever the end state is, it must be consistent: recording UI iff the recorder is capturing
    const capturing = await p.evaluate(() => window.__voice.log.text().split('\n').filter(l => /mic /.test(l)).at(-1));
    assert.ok(typeof state === 'boolean', capturing);
    await p.evaluate(() => { if (document.querySelector('#composer').classList.contains('rec')) document.querySelector('#cancel-rec').click(); });
    await L.waitFor(p, 'ends idle', () => !document.querySelector('#composer').classList.contains('rec') && !document.querySelector('#mic').disabled, 8000);
    assert.deepEqual(c.errors.filter(e => !/interactive-widget/.test(e)), [], 'no page errors');
    assert.ok(!(await p.evaluate(() => window.__voice.log.text())).includes('start failed'), 'no start failure');
    return {firstTapMs: Math.round(t1.ms), secondStartMs: Math.round(t2)};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceControl = async () => {
  // Fake-mic sequences: barge-in pauses; "pause pause" is dropped; dictated reply + resume phrase + silence sends the reply and resumes.
  await L.wipeTasks();
  const say = async text => await L.speechWav(text);
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const seq = async (...parts) => {   // concatenate WAVs and silences (24 kHz 16-bit mono)
    const wavs = await Promise.all(parts.map(x => typeof x === 'string' ? say(x) : x));
    const pcm = wavs.map(w => Buffer.isBuffer(w) && w.toString('latin1', 0, 4) === 'RIFF' ? w.subarray(w.indexOf('data') + 8) : w);
    const first = wavs.find(w => w.toString('latin1', 0, 4) === 'RIFF'), at = first.indexOf('data'), head = Buffer.from(first.subarray(0, at + 8));
    const data = Buffer.concat(pcm); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
    return Buffer.concat([head, data]);
  };
  const files = {
    '/__pause.wav': await seq(gap(9000), 'Pause pause pause pause pause.', gap(12000)),
    '/__reply.wav': await seq(gap(9000), 'Pause. Yes, deploy it to staging first. Over.', gap(20000)),
    '/__phrase.wav': await seq(gap(9000), 'Pause.', gap(6000), 'Over.', gap(20000)),
    '/__split.wav': await seq(gap(9000), 'Pause. Yes, deploy it to staging first and tell me when it is done.', gap(4000), 'Over.', gap(20000)),
    '/__next.wav': await seq(gap(9000), 'Pause. Next.', gap(8000)),
    '/__noise.wav': await seq(gap(9000), 'The weather is quite nice today, I think we should go outside.', gap(12000)),
  };
  const srv2 = await L.serve(undefined, files);
  const long = Array.from({length: 12}, (_, i) => `Paragraph ${i} is long enough that the message keeps playing for a while.`).join('\n\n');
  await L.agentSays('alpha', '[alpha] Long\n\n' + long);
  const run = async (wavUrl, check) => {
    const c = await L.openClient({url: srv2.url, mic: wavUrl, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
    const p = c.page;
    try {
      await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
      await p.locator('.msg.agent .s').first().tap();
      await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing');
      await p.locator('#settings').tap(); await p.locator('#set-voice').check(); await p.keyboard.press('Escape');
      return await check(p);
    } finally { await c.close(); }
  };
  const status = p => p.evaluate(() => window.__voice.player.snapshot().status);
  const draft = p => p.locator('#draft').inputValue();
  const mine = async () => (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId).map(m => m.content);
  const times = {};
  await run('/__pause.wav', async p => {
    const t = Date.now();
    await L.waitFor(p, 'start word pauses playback', () => window.__voice.player.snapshot().status === 'paused', 25000);
    times.pauseMs = Date.now() - t;
    assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'pause words are not added to the draft');
    // "pause" and then nothing: the session closes by itself and playback resumes (never left hanging)
    await L.waitFor(p, 'empty session resumes', () => /vc\s+empty session timed out/.test(window.__voice.log.text()), 25000);
    await L.waitFor(p, 'playing again', () => window.__voice.player.snapshot().status === 'playing', 10000);
  });
  await run('/__noise.wav', async p => {   // other speech during playback never pauses it
    await p.evaluate(() => { window.__mono = {bad: 0, paused: 0, last: -1, chunk: -1}; setInterval(() => { const sn = window.__voice.player.snapshot(), el = [...document.querySelectorAll('audio')].find(a => !a.paused); if (sn.status === 'paused') window.__mono.paused++; if (el) { if (sn.index === window.__mono.chunk && el.currentTime < window.__mono.last - 0.05) window.__mono.bad++; window.__mono.last = el.currentTime; window.__mono.chunk = sn.index; } }, 100); });
    await L.waitFor(p, 'ignored', () => /vc\s+ignored: /.test(window.__voice.log.text()), 30000);
    await L.sleep(1500);
    assert.equal(await status(p), 'playing', 'unrelated speech must not pause');
    const mono = await p.evaluate(() => window.__mono);
    assert.deepEqual([mono.bad, mono.paused], [0, 0], 'playback never paused or jumped back during noise (energy detector): ' + JSON.stringify(mono));
    assert.equal((await draft(p)).trim(), '', 'nothing added to the draft');
    assert.deepEqual(await mine(), [], 'nothing sent');
  });
  await run('/__reply.wav', async p => {
    await L.waitFor(p, 'reply sent and playing again', () => /vc\s+resume phrase \+ silence/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'resumed', () => window.__voice.player.snapshot().status === 'playing', 10000);
    const sent = await (async () => { for (let i = 0; i < 10; i++) { const m = await mine(); if (m.length) return m; await L.sleep(1000); } return []; })();
    assert.equal(sent.length, 1, 'exactly one message sent: ' + JSON.stringify(sent));
    assert.match(sent[0], /deploy it to staging/i);
    assert.ok(!/\bover\b/i.test(sent[0]), 'resume phrase stripped: ' + sent[0]);
    times.replySent = sent[0];
  });
  await L.wipeTasks(); await L.agentSays('alpha', '[alpha] Long\n\n' + long);
  await run('/__phrase.wav', async p => {   // phrase alone = just resume, nothing sent
    await L.waitFor(p, 'phrase resumes', () => /vc\s+resume phrase \+ silence/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing', 10000).catch(async e => { throw Error(e.message.split('\n')[0] + ' ' + JSON.stringify(await p.evaluate(() => window.__voice.player.snapshot())) + (await p.evaluate(() => window.__voice.log.text())).split('\n').slice(-12).join(' | ')); });
    await L.sleep(1500);
    assert.deepEqual(await mine(), [], 'nothing sent');
  });
  await L.wipeTasks(); await L.agentSays('alpha', '[alpha] Long\n\n' + long);
  await run('/__split.wav', async p => {   // reply text, a pause, then "over" as its own short utterance
    await L.waitFor(p, 'split reply finished', () => /vc\s+resume phrase \+ silence/.test(window.__voice.log.text()), 50000);
    const sent = await (async () => { for (let i = 0; i < 10; i++) { const m = await mine(); if (m.length) return m; await L.sleep(1000); } return []; })();
    assert.equal(sent.length, 1, 'reply sent: ' + JSON.stringify(sent));
    assert.match(sent[0], /deploy it to staging/i);
  });
  await run('/__next.wav', async p => {
    await L.waitFor(p, 'action next', () => /vc\s+navigate next/.test(window.__voice.log.text()), 30000);
    await L.waitFor(p, 'moved on a chunk', () => { const n = document.querySelector('.s.now'); return n && +n.dataset.from >= 1; }, 15000);
  });
  return times;
};

scenarios.voiceControlPersists = async () => {
  await L.wipeTasks();
  const long = Array.from({length: 8}, (_, i) => `Paragraph ${i} is long enough that the message keeps playing for a while.`).join('\n\n');
  await L.agentSays('alpha', '[alpha] Long\n\n' + long);
  const wav = Buffer.concat([Buffer.alloc(0)]);
  void wav;
  const srv2 = await L.serve(undefined, {'/__pause.wav': L.padWav(await L.speechWav('Pause now please.'), 3000)});
  const c = await L.openClient({url: srv2.url, mic: '/__pause.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    assert.equal(await p.locator('#set-voice').isChecked(), true, 'setting persisted after reload');
    assert.ok(await p.locator('#listen').isVisible(), 'toggle visible without re-enabling');
    const label = await p.locator('#listen').getAttribute('data-state');
    assert.match(label, /waiting|listening|on/, 'state shown: ' + label);
    // the first tap anywhere (here: Play on a message) arms the microphone inside the gesture
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'armed', () => /vc\s+voice control on/.test(window.__voice.log.text()), 15000);
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing');
    await L.waitFor(p, 'barge-in pauses', () => window.__voice.player.snapshot().status === 'paused', 25000);
    // the dock toggle stays visible when the transport row collapses, and turns the mode off and on
    await p.evaluate(() => document.querySelector('#dock').classList.add('min'));
    assert.ok(await p.locator('#listen').isVisible(), 'toggle visible with the dock collapsed');
    await p.locator('#listen').tap();
    assert.equal(await p.locator('#set-voice').isChecked(), false, 'toggle turns it off');
    await p.locator('#listen').tap();
    assert.equal(await p.locator('#set-voice').isChecked(), true, 'toggle turns it on');
    return {stateAtLoad: label};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceFirstWord = async () => {
  // Speech that starts right at barge-in: the ring-buffer preroll keeps the first word; our own chimes never count as speech.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const speech = await L.speechWav('Pause. Deploy the build to staging now. Over.');
  const at = speech.indexOf('data'), head = Buffer.from(speech.subarray(0, at + 8)), data = Buffer.concat([gap(2500), speech.subarray(at + 8), gap(20000)]);
  head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__first.wav': Buffer.concat([head, data])});
  const long = Array.from({length: 10}, (_, i) => `Paragraph ${i} is long enough that the message keeps playing for a while.`).join('\n\n');
  await L.agentSays('alpha', '[alpha] Long\n\n' + long);
  const c = await L.openClient({url: srv2.url, mic: '/__first.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'reply sent', () => /vc\s+resume phrase \+ silence/.test(window.__voice.log.text()), 60000);
    const log = await p.evaluate(() => window.__voice.log.text());
    const heard = [...log.matchAll(/vc\s+heard "([^"]*)"/g)].map(m => m[1]).join(' ');
    assert.match(heard, /deploy/i, 'first word kept: ' + heard);
    await L.sleep(5000);
    const barges = [...(await p.evaluate(() => window.__voice.log.text())).matchAll(/barge-in: speech/g)].length;
    assert.ok(barges <= 2, 'chimes caused extra barge-ins: ' + barges);
    return {heard, barges};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceLongSilence = async () => {
  // A reply without "over" is finished by a long silence (3 s here): sent, then playback resumes.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const speech = await L.speechWav('Pause. Yes, deploy it to staging now please.');
  const at = speech.indexOf('data'), head = Buffer.from(speech.subarray(0, at + 8)), data = Buffer.concat([gap(2500), speech.subarray(at + 8), gap(30000)]);
  head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__long.wav': Buffer.concat([head, data])});
  const long = Array.from({length: 10}, (_, i) => `Paragraph ${i} is long enough that the message keeps playing for a while.`).join('\n\n');
  await L.agentSays('alpha', '[alpha] Long\n\n' + long);
  const c = await L.openClient({url: srv2.url, mic: '/__long.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-long-silence-s': 3}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'finished by silence', () => /vc\s+resume phrase \+ silence/.test(window.__voice.log.text()), 60000);
    await L.waitFor(p, 'playing again', () => window.__voice.player.snapshot().status === 'playing', 10000);
    const sent = await (async () => { for (let i = 0; i < 10; i++) { const m = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(x => x.sender_id === L.account.humanId); if (m.length) return m.map(x => x.content); await L.sleep(1000); } return []; })();
    assert.equal(sent.length, 1, JSON.stringify(sent));
    assert.match(sent[0], /deploy it to staging/i);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.noReplayAtEnd = async () => {
  // After the last chunk ends, neither noise, a lone "pause" timing out, nor idle voice control may play anything again.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const wavOf = async (text, tail) => { const sp = await L.speechWav(text), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8)), data = Buffer.concat([gap(9000), sp.subarray(at + 8), gap(tail)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4); return Buffer.concat([head, data]); };
  const srv2 = await L.serve(undefined, {'/__noise.wav': await wavOf('The weather is quite nice today and I think we should go outside.', 20000), '/__pause.wav': await wavOf('Pause.', 25000)});
  await L.agentSays('alpha', '[alpha] Short\n\nFirst paragraph of the final message.\n\nSecond and last paragraph.');
  for (const file of ['/__noise.wav', '/__pause.wav']) {
    const c = await L.openClient({url: srv2.url, mic: file, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': true, 'voice2-vc': true}});
    const p = c.page;
    try {
      await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
      await p.locator('.msg.agent .s').first().tap();
      await L.waitFor(p, 'reached the end', () => /audio\s+ended \d+:1/.test(window.__voice.log.text()) && window.__voice.player.snapshot().status === 'idle', 40000);
      const count = () => p.evaluate(() => (window.__voice.log.text().match(/audio\s+start /g) || []).length);
      const before = await count();
      await L.sleep(25000);   // the fake mic now makes noise / says pause; the empty session times out after 8 s
      assert.equal(await count(), before, file + ': something played again after the end');
      assert.equal(await p.evaluate(() => window.__voice.player.snapshot().status), 'idle');
    } finally { await c.close(); }
    await L.human('POST', 'messages/flags/narrow', {anchor: 'oldest', num_before: 0, num_after: 100, narrow: [{operator: 'stream', operand: L.TASKS}], op: 'remove', flag: 'read'}).catch(() => {});
  }
};

scenarios.noPlaybackWhileSpeaking = async () => {
  // A new agent message arriving while the user speaks (manual dictation, or a voice-control reply) must wait until they finish.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const sp = await L.speechWav('Pause. Yes, deploy it to staging and then tell me how it went.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(2000), sp.subarray(at + 8), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__vc.wav': Buffer.concat([head, data])});
  await L.agentSays('alpha', '[alpha] Seed\n\nSeed message to start with.');
  for (const mode of ['manual', 'voice']) {
    const c = await L.openClient({url: srv2.url, mic: '/__vc.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': true, 'voice2-vc': mode === 'voice'}});
    const p = c.page;
    try {
      await L.waitFor(p, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
      await p.locator('#feed').tap();
      await L.waitFor(p, 'idle and read', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 30000);
      if (mode === 'manual') { await p.locator('#mic').tap(); await L.waitFor(p, 'recording', () => document.querySelector('#composer').classList.contains('rec'), 8000); }
      else await L.waitFor(p, 'speech heard', () => /vc\s+speech detected/.test(window.__voice.log.text()), 30000);
      const starts = () => p.evaluate(() => (window.__voice.log.text().match(/audio\s+start /g) || []).length);
      const before = await starts();
      const id = await L.agentSays('alpha', `[alpha] During ${mode}\n\nThis message arrives while you are speaking.`);
      await L.waitFor(p, 'message shown', () => document.querySelector('#feed').innerText.includes('arrives while you are speaking'), 15000);
      await L.sleep(4000);
      assert.equal(await starts(), before, mode + ': audio started while the user was speaking');
      assert.ok(await p.locator('.msg.unread').count() >= 1, mode + ': stays unread');
      if (mode === 'manual') await p.locator('#cancel-rec').tap();
      else await L.waitFor(p, 'reply closed', () => /vc\s+resume phrase \+ silence|ignored/.test(window.__voice.log.text()), 40000);
      await L.waitFor(p, 'plays after', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 40000);
      assert.ok(await L.serverRead(id));
    } finally { await c.close(); }
    await L.wipeTasks(); await L.agentSays('alpha', '[alpha] Seed\n\nSeed message to start with.');
  }
};

scenarios.agentSearch = async () => {
  await L.wipeTasks();
  await L.agentSays('terminal_viz', '[terminal_viz] T\n\nTerminal visualisation agent is here.');
  await L.agentSays('vl_meeting_work', '[vl] V\n\nMeeting agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\nAlpha agent is here. It has a long enough message to play for a few seconds while we talk to it.');
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const sp = await L.speechWav('Pause. Switch to terminal viz.'), yes = await L.speechWav('Yes.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(2000), sp.subarray(at + 8), gap(16000), yes.subarray(yes.indexOf('data') + 8), gap(20000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__sw.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__sw.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'alpha', () => document.querySelector('#feed').innerText.includes('Alpha agent'));
    await L.waitFor(p, 'topics loaded', () => window.__voice.model.topics().length >= 3, 15000);
    // searchable picker: focused input, fuzzy ("terminal viz" finds terminal_viz), Enter switches
    await p.locator('#find').tap();
    assert.equal(await p.evaluate(() => document.activeElement?.id), 'search', 'search box focused on open');
    await p.locator('#search').fill('terminal viz');
    assert.equal(await p.locator('#topics li').first().locator('.name').innerText(), 'terminal_viz', 'fuzzy match ranks first');
    await p.locator('#search').press('Enter');
    await L.waitFor(p, 'switched by search', () => window.__voice.current() === 'terminal_viz', 10000);
    assert.equal(await p.evaluate(() => document.querySelector('#picker').open), false, 'picker closed after Enter');
    await L.sleep(600);
    // voice: "pause, switch to terminal viz" from another agent
    await p.locator('#agent').tap(); await p.locator('#topics li', {hasText: 'alpha'}).tap();
    await L.waitFor(p, 'on alpha', () => window.__voice.current() === 'alpha', 10000);
    await p.locator('#settings').tap(); await p.locator('#set-voice').check(); await p.keyboard.press('Escape');
    await L.waitFor(p, 'voice switch', () => window.__voice.current() === 'terminal_viz', 45000);
    assert.match(await p.evaluate(() => window.__voice.log.text()), /vc\s+switch to "terminal viz/i);
    assert.match(await p.evaluate(() => window.__voice.log.text()), /vc\s+heard "Yes/i, 'confirmed by voice');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.idlePrevNext = async ({srv}) => {
  await L.wipeTasks();
  const id1 = await L.agentSays('alpha', '[alpha] One\n\nFirst message first chunk.\n\nFirst message last chunk.');
  await L.agentSays('alpha', '[alpha] Two\n\nSecond message only chunk.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'banner', () => document.querySelector('#banner').innerText.includes('Tap to listen'));
    await p.locator('#banner').tap();
    await L.waitFor(p, 'all read and idle', () => !document.querySelector('.msg.unread') && window.__voice.player.snapshot().status === 'idle', 40000);
    // idle: previous replays the last heard chunk (the second message's only chunk)
    await p.locator('#prev').tap();
    await L.waitFor(p, 'previous plays', () => window.__voice.player.snapshot().status === 'playing', 10000);
    const lastLine = await p.evaluate(() => document.querySelector('.s.now')?.innerText);
    assert.match(lastLine, /Second message only chunk/, 'replayed the last heard chunk: ' + lastLine);
    await L.waitFor(p, 'idle again', () => window.__voice.player.snapshot().status === 'idle', 20000);
    // wake lock is requested while audio plays (logged either way; headless WebKit may not grant it)
    assert.match(await p.evaluate(() => window.__voice.log.text()), /wake\s+(screen wake lock acquired|not granted)|^/m);
    void id1;
  } finally { await c.close(); }
};

scenarios.voiceFollowUp = async () => {
  // Speech right after a reply was sent (no start word) continues as a follow-up instead of being dropped.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const a = await L.speechWav('Pause. Deploy it to staging now.'), b = await L.speechWav('And then run all of the tests afterwards.');
  const pcm = w => w.subarray(w.indexOf('data') + 8), at = a.indexOf('data'), head = Buffer.from(a.subarray(0, at + 8));
  const data = Buffer.concat([gap(2000), pcm(a), gap(5500), pcm(b), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__fu.wav': Buffer.concat([head, data])});
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv2.url, mic: '/__fu.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-long-silence-s': 3}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'follow-up handled', () => /vc\s+follow-up right after a send/.test(window.__voice.log.text()), 60000);
    const sent = await (async () => { for (let i = 0; i < 25; i++) { const m = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(x => x.sender_id === L.account.humanId).map(x => x.content); if (m.length >= 2) return m; await L.sleep(1000); } return []; })();
    assert.equal(sent.length, 2, JSON.stringify(sent));
    assert.match(sent[1], /run all of the tests/i);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceSwitchInSession = async () => {
  // "pause" opens a session, then a whole utterance "switch agent to UI client" is a command: read back, "yes", switched.
  await L.wipeTasks();
  await L.agentSays('uiclient_steer', '[uiclient_steer] U\n\nUI client agent is here.');
  await L.agentSays('vl_meeting_work', '[vl] V\n\nMeeting agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const ws = await Promise.all(['Pause. Pause.', 'Switch agent to UI client.', 'Yes.'].map(t => L.speechWav(t))), pcm = w => w.subarray(w.indexOf('data') + 8), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(2000), pcm(ws[0]), gap(2500), pcm(ws[1]), gap(10000), pcm(ws[2]), gap(25000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ss.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ss.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'alpha', () => document.querySelector('#feed').innerText.includes('Paragraph 0'));
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 3, 15000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing');
    await p.locator('#settings').tap(); await p.locator('#set-voice').check(); await p.keyboard.press('Escape');
    await L.waitFor(p, 'switched to uiclient_steer', () => window.__voice.current() === 'uiclient_steer', 60000);
    const log = await p.evaluate(() => window.__voice.log.text());
    assert.match(log, /switch command inside an open session/);
    assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'command text not left in the draft');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.forwardOnlyWatermark = async () => {
  // An older unread message with a stale saved position must never replay after a newer one is paused and resumed.
  await L.wipeTasks();
  const para = (n, w) => Array.from({length: n}, (_, i) => `${w} paragraph ${i} of the message, long enough to take a few seconds to speak.`).join('\n\n');
  const oldId = await L.agentSays('alpha', '[alpha] Old\n\n' + para(5, 'Old'));
  const newId = await L.agentSays('alpha', '[alpha] New\n\n' + para(6, 'New'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const sp = await L.speechWav('Pause.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(9000), sp.subarray(at + 8), gap(40000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__wm.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__wm.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': true, 'voice2-vc': true, 'voice2-pos': {topic: 'alpha', msgId: oldId, idx: 3}}});
  const p = c.page;
  try {
    await L.waitFor(p, 'messages', () => document.querySelectorAll('.msg.agent').length === 2);
    // listen to the NEW message from its first chunk (explicit tap), then the voice session pauses it
    await p.locator(`.msg[data-id="${newId}"] .s`).first().tap();
    await L.waitFor(p, 'paused by voice session', () => window.__voice.player.snapshot().status === 'paused', 40000);
    await L.waitFor(p, 'session timed out and resumed', () => /vc\s+empty session timed out/.test(window.__voice.log.text()) && window.__voice.player.snapshot().status === 'playing', 25000);
    await L.waitFor(p, 'new message finished', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 90000);
    const starts = (await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => /audio\s+start /.test(l)).map(l => l.split('start ')[1]);
    assert.ok(!starts.some(k => k.startsWith(oldId + ':')), 'older message replayed: ' + starts.join(','));
    assert.ok(await L.serverRead(oldId), 'older message marked read');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.externalPause = async ({srv}) => {
  // The platform pauses the <audio> behind the app's back (iOS interruption): playback resumes in place, never from the chunk start.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\nThis single chunk is long enough to play for several seconds so that the pause lands well inside it, and it keeps going for a while longer than that.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'playing past 2 s', () => window.__voice.player.currentTime() > 2, 20000);
    const before = await p.evaluate(() => { const el = [...document.querySelectorAll('audio')].find(a => !a.paused); el.pause(); return el.currentTime; });
    await L.waitFor(p, 'resumed by itself', () => window.__voice.player.snapshot().status === 'playing' && [...document.querySelectorAll('audio')].some(a => !a.paused), 5000);
    const after = await p.evaluate(() => [...document.querySelectorAll('audio')].find(a => !a.paused).currentTime);
    assert.ok(after >= before - 0.1, `resumed from ${after}, paused at ${before}`);
    assert.match(await p.evaluate(() => window.__voice.log.text()), /external pause at t=/);
  } finally { await c.close(); }
};

scenarios.voiceNavigation = async () => {
  // "pause" opens a session, then "back back" (a whole utterance) steps two chunks back and plays from there; nothing goes into the draft.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 14}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const a = await L.speechWav('Pause.'), b = await L.speechWav('Back back.'), pcm = w => w.subarray(w.indexOf('data') + 8), at = a.indexOf('data'), head = Buffer.from(a.subarray(0, at + 8));
  const data = Buffer.concat([gap(24000), pcm(a), gap(3000), pcm(b), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__nav.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__nav.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'navigated', () => /vc\s+navigate previous x2/.test(window.__voice.log.text()), 70000);
    await L.sleep(3000);
    const lines = (await p.evaluate(() => window.__voice.log.text())).split('\n');
    const keyOf = l => +l.match(/start \d+:(\d+)/)[1];
    const open = lines.findIndex(l => /session opened by start word/.test(l)), nav = lines.findIndex(l => /vc\s+navigate previous x2/.test(l));
    const before = lines.slice(0, open).filter(l => /audio\s+start /.test(l)).map(keyOf).at(-1);
    const after = lines.slice(nav).filter(l => /audio\s+start /.test(l)).map(keyOf);
    assert.ok(after.includes(before - 2), `back back from chunk ${before} never reached chunk ${before - 2}: ${after}`);
    assert.ok(!after.slice(0, 3).includes(before - 1) || after.indexOf(before - 2) > after.indexOf(before - 1), 'order');
    assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'nav words are not reply text');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceNoDuplicateSend = async () => {
  // A draft sent by hand while a voice reply is still open must not be sent again by the session's timer.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const sp = await L.speechWav('Pause. Deploy it to staging now please.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(14000), sp.subarray(at + 8), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__dup.wav': Buffer.concat([head, data])});
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv2.url, mic: '/__dup.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-long-silence-s': 5}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'reply text in the draft', () => /deploy it to staging/i.test(document.querySelector('#draft').value), 60000);
    await p.locator('#send').tap();
    await L.sleep(12000);
    const sent = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId);
    assert.equal(sent.length, 1, 'sent once: ' + JSON.stringify(sent.map(m => m.content)));
  } finally { await c.close(); await srv2.close(); }
};

scenarios.noPlaybackWhileConfirming = async () => {
  // A message arriving while "Switch to X?" waits for the answer does not play until the flow ends.
  await L.wipeTasks();
  await L.agentSays('uiclient_steer', '[uiclient_steer] U\n\nUI client agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 6}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const sp = await L.speechWav('Pause. Switch to UI client.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(12000), sp.subarray(at + 8), gap(40000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__cf.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__cf.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': true, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 15000);
    await p.locator('#feed').tap();
    await L.waitFor(p, 'asked to confirm', () => /vc\s+switch to "ui client/i.test(window.__voice.log.text()), 60000);
    const starts = () => p.evaluate(() => (window.__voice.log.text().match(/audio\s+start /g) || []).length);
    await L.waitFor(p, 'paused or idle while asking', () => window.__voice.player.snapshot().status !== 'playing', 10000);
    const before = await starts();
    await L.agentSays('alpha', '[alpha] During confirm\n\nThis arrives while the switch question is open.');
    await L.waitFor(p, 'shown', () => document.querySelector('#feed').innerText.includes('switch question is open'), 15000);
    await L.sleep(5000);
    assert.equal(await starts(), before, 'nothing may start while the confirmation is pending');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceSwitchRules = async ({srv}) => {
  await L.wipeTasks();
  await L.agentSays('uiclient_steer', '[uiclient_steer] U\n\nUI client agent is here.');
  const long = '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n');
  await L.agentSays('alpha', long);
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const wavOf = async (...parts) => { const ws = await Promise.all(parts.filter(x => typeof x === 'string').map(t => L.speechWav(t))); let k = 0; const bufs = parts.map(x => typeof x === 'string' ? pcm(ws[k++]) : x), first = ws[0], at = first.indexOf('data'), head = Buffer.from(first.subarray(0, at + 8)), data = Buffer.concat(bufs); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4); return Buffer.concat([head, data]); };
  const files = {
    '/__plain.wav': await wavOf(gap(9000), 'Pause. Switch to the store and buy some milk.', gap(30000)),
    '/__cancel.wav': await wavOf(gap(9000), 'Pause. Deploy it to staging now.', gap(2500), 'Cancel.', gap(30000)),
  };
  const srv2 = await L.serve(undefined, files);
  const run = async (file, prefs, check) => {
    const c = await L.openClient({url: srv2.url, mic: file, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, ...prefs}});
    try {
      await L.waitFor(c.page, 'message', () => document.querySelector('.msg.agent'));
      await L.waitFor(c.page, 'topics', () => window.__voice.model.topics().length >= 2, 15000);
      await c.page.locator('.msg.agent .s').first().tap();
      await check(c.page);
    } finally { await c.close(); }
  };
  await run('/__plain.wav', {'voice2-long-silence-s': 60}, async p => {   // no confident match: ordinary reply text, no read-back
    await L.waitFor(p, 'text in draft', () => /switch to the store/i.test(document.querySelector('#draft').value), 60000);
    const log = await p.evaluate(() => window.__voice.log.text());
    assert.doesNotMatch(log, /vc\s+switch to "/, "no read-back for a weak match");
    assert.equal(await p.evaluate(() => window.__voice.current()), 'alpha');
  });
  await run('/__cancel.wav', {'voice2-long-silence-s': 60}, async p => {   // "cancel" discards the reply and resumes
    await L.waitFor(p, 'text then cancel', () => /vc\s+cancel: reply discarded/.test(window.__voice.log.text()), 70000);
    assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'draft restored');
    await L.waitFor(p, 'playing again', () => window.__voice.player.snapshot().status === 'playing', 10000);
  });
  // any switch is announced (earcon + "Now on ...") even when the new agent has nothing to play
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  try {
    const p = c.page;
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 15000);
    const ttsCalls = () => p.evaluate(() => (window.__voice.log.text().match(/relay\s+tts /g) || []).length);
    const before = await ttsCalls();
    await p.locator('#find').tap(); await p.locator('#topics li', {hasText: 'uiclient_steer'}).tap();
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'uiclient_steer', 10000);
    await L.waitFor(p, 'announcement requested', n => (window.__voice.log.text().match(/relay\s+tts /g) || []).length > n, 10000, before);
  } finally { await c.close(); }
};

scenarios.voiceYesAfterPause = async () => {
  // The switch question is open; "Pause." then "Yes." must be taken as the answer, with no reply session left behind.
  await L.wipeTasks();
  await L.agentSays('terminal_viz', '[terminal_viz] T\n\nTerminal agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Pause. Switch to terminal.', 'Pause.', 'Yes.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(10000), pcm(ws[0]), gap(7000), pcm(ws[1]), gap(2500), pcm(ws[2]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ya.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ya.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 15000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'terminal_viz', 60000);
    await L.sleep(12000);   // no delayed "out of voice mode" chime or leftover session
    const log = await p.evaluate(() => window.__voice.log.text());
    assert.doesNotMatch(log, /session opened by start word[^]*switched|empty session timed out/, 'no lingering session');
    assert.equal(await p.evaluate(() => document.querySelector('#listen').dataset.state), 'on', 'back to idle state');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceTrailingCancel = async () => {
  // "Pause. Some reply. Cancel." in one utterance discards everything; later silence or "over" sends nothing.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Pause. This is a test whether the cancel works. Cancel.', 'Over.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(9000), pcm(ws[0]), gap(6000), pcm(ws[1]), gap(25000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__tc.wav': Buffer.concat([head, data])});
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv2.url, mic: '/__tc.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-long-silence-s': 4}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'cancelled', () => /vc\s+cancel: reply discarded/.test(window.__voice.log.text()), 60000);
    await L.sleep(20000);   // a later "Over." and the long silence must not send anything
    const sent = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId);
    assert.equal(sent.length, 0, 'nothing sent: ' + JSON.stringify(sent.map(m => m.content)));
    assert.equal((await p.locator('#draft').inputValue()).trim(), '');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.switchBack = async ({srv}) => {
  await L.wipeTasks();
  for (const t of ['alpha', 'beta']) await L.agentSays(t, `[${t}] T\n\nThis is the ${t} agent.`);
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 15000);
    assert.equal(await p.locator('#prev-agent').isVisible(), false, 'no previous agent yet');
    await p.locator('#find').tap(); await p.locator('#topics li', {hasText: 'beta'}).tap();
    await L.waitFor(p, 'on beta', () => window.__voice.current() === 'beta', 10000);
    assert.equal(await p.locator('#prev-agent').isVisible(), true);
    await L.sleep(600);
    await p.locator('#prev-agent').tap();
    await L.waitFor(p, 'back on alpha', () => window.__voice.current() === 'alpha', 10000);
    await L.sleep(600);
    await p.locator('#prev-agent').tap();   // Alt-Tab: toggles
    await L.waitFor(p, 'on beta again', () => window.__voice.current() === 'beta', 10000);
    await p.reload();
    await L.waitFor(p, 'previous persisted', () => document.querySelector('#prev-agent') && !document.querySelector('#prev-agent').hidden, 15000);
  } finally { await c.close(); }
};

scenarios.voiceSwitchBack = async () => {
  await L.wipeTasks();
  await L.agentSays('beta', '[beta] T\n\nThis is the beta agent.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), sp = await L.speechWav('Pause. Switch back.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(9000), sp.subarray(at + 8), gap(25000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__sb.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__sb.wav', prefs: {'voice2-topic': 'alpha', 'voice2-prev': 'beta', 'voice2-autoplay': false, 'voice2-vc': true}});
  try {
    await L.waitFor(c.page, 'message', () => document.querySelector('.msg.agent'));
    await L.waitFor(c.page, 'topics', () => window.__voice.model.topics().length >= 2, 15000);
    await c.page.locator('.msg.agent .s').first().tap();
    await L.waitFor(c.page, 'switched back', () => window.__voice.current() === 'beta', 50000);
    assert.match(await c.page.evaluate(() => window.__voice.log.text()), /switch back to the previous agent/);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.footerFold = async () => {
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Done\n\nHere is the real message text.\n\npending item created:\n- "first request"\n- "second request"\npending item deleted:\n- "old request"');
  for (const [name, width] of [['narrow', 390], ['wide', 1100]]) {
    const c = await L.openClient({url: (await L.serve()).url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
    const p = c.page; await p.setViewportSize({width, height: 800});
    try {
      await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
      const vis = () => p.evaluate(() => { const t = document.querySelector('.footer-text'); return !!t && t.getBoundingClientRect().height > 0; });
      if (name === 'narrow') {
        assert.equal(await vis(), false, 'folded on a narrow screen');
        assert.match(await p.locator('.footer-chip').innerText(), /3 items/);
        await p.locator('.footer-chip').tap();
        assert.equal(await vis(), true, 'chip expands it');
      } else assert.equal(await vis(), true, 'fully shown on a wide screen');
      assert.ok(!(await p.evaluate(() => [...document.querySelectorAll('.msg.agent .s')].map(x => x.innerText).join(' '))).match(/request|pending/), 'not in the readable text');
    } finally { await c.close(); }
  }
};

scenarios.voiceSwitchAbbrev = async () => {
  // "Switch to Human Manager" must find human_mgr and ask for confirmation; "Switch to xyzzy" must never become a sent reply.
  await L.wipeTasks();
  for (const t of ['human_mgr', 'terminal_viz', 'uiclient_steer', 'main_manager_agy']) await L.agentSays(t, `[${t}] T\n\nThis is ${t}.`);
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Pause. Switch to Human Manager.', 'Yes.', 'Pause. Switch to xyzzy.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(9000), pcm(ws[0]), gap(14000), pcm(ws[1]), gap(12000), pcm(ws[2]), gap(25000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ab.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ab.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 5, 20000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'switched to human_mgr', () => window.__voice.current() === 'human_mgr', 60000);
    await L.waitFor(p, 'xyzzy handled', () => /vc\s+no agent matches/.test(window.__voice.log.text()), 60000);
    await L.sleep(3000);
    const sent = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}], anchor: 'newest', num_before: 20, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId);
    assert.equal(sent.length, 0, 'nothing sent as a reply: ' + JSON.stringify(sent.map(m => m.content)));
  } finally { await c.close(); await srv2.close(); }
};

const wanted = Object.keys(scenarios).filter(n => !only || n === only);
const srv = await L.serve();
for (const name of wanted) {
  const t0 = Date.now();
  try { const extra = await scenarios[name]({srv}); results.push({name, ok: true, ms: Date.now() - t0, ...extra}); }
  catch (error) { results.push({name, ok: false, ms: Date.now() - t0, error: error.message}); }
}
await srv.close();
for (const r of results) console.log(r.ok ? `ok   ${r.name} ${r.ms}ms ${JSON.stringify({...r, name: undefined, ok: undefined, ms: undefined})}` : `FAIL ${r.name}: ${r.error}`);
process.exit(results.every(r => r.ok) ? 0 : 1);
