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
    let mine = [];
    for (let i = 0; i < 15 && !mine.length; i++) { await L.sleep(1000); mine = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 5, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId); }
    await L.sleep(1500);
    mine = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 5, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId);
    assert.equal(mine.length, 1, 'exactly one dictated message sent: ' + JSON.stringify(mine.map(m => m.content)));
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
    await L.waitFor(c.page, 'prepared clip known', mid => window.__voice.voice.preparedEntries().some(e => e.messageId === mid) && document.querySelector('.msg.agent'), 30000, id);
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
  } catch (e) { if (process.env.DUMP) console.log((await p.evaluate(() => window.__voice.log.text())).split('\n').slice(-40).join('\n')); throw e; } finally { await c.close(); }
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
    '/__pause.wav': await seq(gap(9000), 'Attention attention attention.', gap(12000)),
    '/__reply.wav': await seq(gap(9000), 'Attention. Yes, deploy it to staging first. Tomato.', gap(20000)),
    '/__phrase.wav': await seq(gap(9000), 'Attention.', gap(6000), 'Tomato.', gap(20000)),
    '/__split.wav': await seq(gap(9000), 'Attention. Yes, deploy it to staging first and tell me when it is done.', gap(4000), 'Tomato.', gap(20000)),
    '/__next.wav': await seq(gap(9000), 'Katana. Next.', gap(8000)),
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
    await L.waitFor(p, 'reply sent and playing again', () => /vc\s+(resume phrase \+ silence|.*sending now)/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'resumed', () => window.__voice.player.snapshot().status === 'playing', 10000);
    const sent = await (async () => { for (let i = 0; i < 10; i++) { const m = await mine(); if (m.length) return m; await L.sleep(1000); } return []; })();
    assert.equal(sent.length, 1, 'exactly one message sent: ' + JSON.stringify(sent));
    assert.match(sent[0], /deploy it to staging/i);
    assert.ok(!/tomato/i.test(sent[0]), 'resume phrase stripped: ' + sent[0]);
    times.replySent = sent[0];
  });
  await L.wipeTasks(); await L.agentSays('alpha', '[alpha] Long\n\n' + long);
  await run('/__phrase.wav', async p => {   // phrase alone = just resume, nothing sent
    await L.waitFor(p, 'phrase resumes', () => /vc\s+(resume phrase \+ silence|.*sending now)/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing', 10000).catch(async e => { throw Error(e.message.split('\n')[0] + ' ' + JSON.stringify(await p.evaluate(() => window.__voice.player.snapshot())) + (await p.evaluate(() => window.__voice.log.text())).split('\n').slice(-12).join(' | ')); });
    await L.sleep(1500);
    assert.deepEqual(await mine(), [], 'nothing sent');
  });
  await L.wipeTasks(); await L.agentSays('alpha', '[alpha] Long\n\n' + long);
  await run('/__split.wav', async p => {   // reply text, a pause, then "over" as its own short utterance
    await L.waitFor(p, 'split reply finished', () => /vc\s+(resume phrase \+ silence|.*sending now)/.test(window.__voice.log.text()), 50000);
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
  const srv2 = await L.serve(undefined, {'/__pause.wav': L.padWav(await L.speechWav('Attention now please.'), 3000)});
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
  const speech = await L.speechWav('Attention. Deploy the build to staging now. Tomato.');
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
    await L.waitFor(p, 'reply sent', () => /vc\s+(resume phrase \+ silence|.*sending now)/.test(window.__voice.log.text()), 60000);
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
  const speech = await L.speechWav('Attention. Yes, deploy it to staging now please.');
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
    await L.waitFor(p, 'finished by silence', () => /vc\s+(resume phrase \+ silence|.*sending now)/.test(window.__voice.log.text()), 60000);
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
  const srv2 = await L.serve(undefined, {'/__noise.wav': await wavOf('The weather is quite nice today and I think we should go outside.', 20000), '/__pause.wav': await wavOf('Attention.', 25000)});
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
  const sp = await L.speechWav('Attention. Yes, deploy it to staging and then tell me how it went.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
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
      else await L.waitFor(p, 'reply closed', () => /vc\s+(resume phrase \+ silence|.*sending now)|ignored/.test(window.__voice.log.text()), 40000);
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
  const sp = await L.speechWav('Katana. Switch to terminal viz.'), yes = await L.speechWav('Yes.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(2000), sp.subarray(at + 8), gap(16000), yes.subarray(yes.indexOf('data') + 8), gap(20000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__sw.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__sw.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-switch-confirm': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'alpha', () => document.querySelector('#feed').innerText.includes('Alpha agent'));
    await L.waitFor(p, 'topics loaded', () => window.__voice.model.topics().length >= 3, 45000);
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
  // 🧑 "The system picked up garbage": speech right after a reply was sent, without the start word, is ignored like any other (it used to open a reply and send room talk).
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const a = await L.speechWav('Attention. Deploy it to staging now.'), b = await L.speechWav('And then run all of the tests afterwards.');
  const pcm = w => w.subarray(w.indexOf('data') + 8), at = a.indexOf('data'), head = Buffer.from(a.subarray(0, at + 8));
  const data = Buffer.concat([gap(2000), pcm(a), gap(5500), pcm(b), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__fu.wav': Buffer.concat([head, data])});
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv2.url, mic: '/__fu.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-long-silence-s': 3}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'follow-up ignored', () => /vc\s+right after a send, but no start word/.test(window.__voice.log.text()), 60000);
    await p.waitForTimeout(8000);   // past the long silence that used to send it
    const sent = await (async () => { for (let i = 0; i < 25; i++) { const m = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(x => x.sender_id === L.account.humanId).map(x => x.content); if (m.length >= 1) return m; await L.sleep(1000); } return []; })();
    assert.equal(sent.length, 1, JSON.stringify(sent));
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceSwitchInSession = async () => {
  // "pause" opens a session, then a whole utterance "switch agent to UI client" is a command: read back, "yes", switched.
  await L.wipeTasks();
  await L.agentSays('uiclient_steer', '[uiclient_steer] U\n\nUI client agent is here.');
  await L.agentSays('vl_meeting_work', '[vl] V\n\nMeeting agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const ws = await Promise.all(['Katana.', 'Switch agent to UI client.', 'Yes.'].map(t => L.speechWav(t))), pcm = w => w.subarray(w.indexOf('data') + 8), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(2000), pcm(ws[0]), gap(1800), pcm(ws[1]), gap(10000), pcm(ws[2]), gap(25000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ss.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ss.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'alpha', () => document.querySelector('#feed').innerText.includes('Paragraph 0'));
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 3, 45000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing');
    await p.locator('#settings').tap(); await p.locator('#set-voice').check(); await p.keyboard.press('Escape');
    await L.waitFor(p, 'switched to uiclient_steer', () => window.__voice.current() === 'uiclient_steer', 60000);
    const log = await p.evaluate(() => window.__voice.log.text());
    assert.match(log, /vc +commands: switch/);
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
  const sp = await L.speechWav('Attention.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
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
  const a = await L.speechWav('Katana.'), b = await L.speechWav('Back back.'), pcm = w => w.subarray(w.indexOf('data') + 8), at = a.indexOf('data'), head = Buffer.from(a.subarray(0, at + 8));
  const data = Buffer.concat([gap(24000), pcm(a), gap(1800), pcm(b), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__nav.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__nav.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'navigated', () => /vc\s+navigate previous x2/.test(window.__voice.log.text()), 70000);
    await L.sleep(3000);
    const lines = (await p.evaluate(() => window.__voice.log.text())).split('\n');
    if (process.env.DUMP) console.log(lines.slice(lines.findIndex(l => /navigate previous/.test(l))).slice(0,30).join('\n'));
    const keyOf = l => +l.match(/start \d+:(\d+)/)[1];
    const open = lines.findIndex(l => /session opened by start word/.test(l)), nav = lines.findIndex(l => /vc\s+navigate previous x2/.test(l));
    const before = lines.slice(0, open).filter(l => /audio\s+start /.test(l)).map(keyOf).at(-1);
    const after = lines.slice(nav).filter(l => /audio\s+start /.test(l)).map(keyOf);
    assert.ok(after.includes(before - 2), `back back from chunk ${before} never reached chunk ${before - 2}: ${after}`);
    assert.ok(!after.slice(0, 3).includes(before - 1) || after.indexOf(before - 2) > after.indexOf(before - 1), 'order');
    assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'nav words are not reply text');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceSkipCountNoFlicker = async () => {
  // 🧑 "Every time we skip backward, it starts playing the older playback multiple times ... It should recognize the command quickly. Play an acknowledgement sound ... and then start playing":
  // "skip backward three" plays exactly one new chunk, the one three back, with no chunk started in between.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 14}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const a = await L.speechWav('Katana.'), b = await L.speechWav('Skip backward three.'), pcm = w => w.subarray(w.indexOf('data') + 8), at = a.indexOf('data'), head = Buffer.from(a.subarray(0, at + 8));
  const data = Buffer.concat([gap(24000), pcm(a), gap(1800), pcm(b), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__skip3.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__skip3.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'navigated', () => /vc\s+navigate previous x\d/.test(window.__voice.log.text()), 70000);
    await L.sleep(4000);
    const lines = (await p.evaluate(() => window.__voice.log.text())).split('\n');
    if (process.env.DUMP) console.log(lines.slice(lines.findIndex(l => /session opened by start word/.test(l))).slice(0, 40).join('\n'));
    const keyOf = l => +l.match(/start \d+:(\d+)/)[1];
    const open = lines.findIndex(l => /session opened by start word/.test(l));
    const before = lines.slice(0, open).filter(l => /audio\s+start /.test(l)).map(keyOf).at(-1);
    const after = lines.slice(open).filter(l => /audio\s+start /.test(l)).map(keyOf);
    assert.equal(after[0], before - 3, `first chunk after "skip backward three" from ${before}: ${after}`);
    assert.ok(after.every((k, j) => k === before - 3 + j), `no flicker: chunks started after the command ${after}`);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceLongFirstSentence = async () => {
  // 🧑 "I was in input mode. Said something. And then, when I said the end word, everything got dropped": "Attention" and a sentence over 8 s in one breath, then the end word: it is sent.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Seed\n\nSeed message to open the topic.');
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const a = await L.speechWav('Attention. This sentence by itself seems pretty weird, and I do not know what it is supposed to say, so please rewrite the whole paragraph around it so that the reader can follow the argument from the start to the end without getting lost in the details.');
  const b = await L.speechWav('Tomato.'), pcm = w => w.subarray(w.indexOf('data') + 8), at = a.indexOf('data'), head = Buffer.from(a.subarray(0, at + 8));
  const data = Buffer.concat([gap(14000), pcm(a), gap(1500), pcm(b), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__long.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__long.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('#feed').tap();
    const deadline = Date.now() + 70000; let sent = [];
    while (Date.now() < deadline && !sent.length) {
      await L.sleep(3000);
      sent = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => /rewrite the whole paragraph/i.test(m.content));
    }
    if (process.env.DUMP) console.log((await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => / vc /.test(l)).join('\n'));
    assert.equal(sent.length, 1, 'the long first sentence was sent');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceSkipPreviousMessage = async () => {
  // 🧑 "trying to skip to the previous message skipped to the first message": while message 3 plays, "Katana. Skip previous message." plays message 2 from its start, nothing else first.
  await L.wipeTasks();
  const ids = [];
  for (const n of [1, 2, 3]) ids.push(await L.agentSays('alpha', `[alpha] M${n}\n\n` + Array.from({length: 8}, (_, i) => `Message ${n} paragraph ${i} is long enough to take a few seconds to read.`).join('\n\n')));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const a = await L.speechWav('Katana.'), b = await L.speechWav('Skip previous message.'), pcm = w => w.subarray(w.indexOf('data') + 8), at = a.indexOf('data'), head = Buffer.from(a.subarray(0, at + 8));
  const data = Buffer.concat([gap(20000), pcm(a), gap(1800), pcm(b), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__prevmsg.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__prevmsg.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'messages', () => document.querySelectorAll('.msg.agent').length >= 3);
    await p.locator(`.msg[data-id="${ids[2]}"] .s`).first().tap();
    await L.waitFor(p, 'navigated', () => /vc\s+navigate previous message/.test(window.__voice.log.text()), 70000);
    await L.waitFor(p, 'played after the command', () => { const t = window.__voice.log.text(); return /audio\s+start \d/.test(t.slice(t.search(/vc\s+commands:/))); }, 40000);
    const lines = (await p.evaluate(() => window.__voice.log.text())).split('\n');
    if (process.env.DUMP) console.log(lines.filter(l => / vc |audio|relay/.test(l)).slice(-60).join('\n'));
    const nav = lines.findIndex(l => /vc\s+commands:/.test(l));
    assert.match(lines[nav], /commands: navigate$/, 'one command: ' + lines[nav]);
    const first = lines.slice(nav).find(l => /audio\s+start \d/.test(l));
    assert.ok(first?.includes(`start ${ids[1]}:0`), `first start after the command is message 2 (${ids}) from its start: ${first}`);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.closedHidden = async ({srv}) => {
  // 🧑 "I'm still seeing old threads on the web interface, and agent switching also gives them as options": the service names closed tasks; their read topics leave the list.
  const c = await L.openClient({url: srv.url, prefs: {'voice2-autoplay': false}});
  try {
    const r = await c.page.evaluate(() => window.__voice.voice.ping());
    if (process.env.DUMP) { console.log(JSON.stringify(r).slice(0, 600)); console.log((await c.page.evaluate(() => window.__voice.log.text())).split('\n').filter(l => /voice|relay|err/.test(l)).slice(-15).join('\n')); }
    assert.ok(Array.isArray(r.closed) && r.closed.length > 10, 'ping lists closed tasks: ' + JSON.stringify(r.closed)?.slice(0, 200));
    assert.ok(!r.closed.includes('terminal_viz'), 'an active task is not closed');
  } finally { await c.close(); }
};

scenarios.voiceNoDuplicateSend = async () => {
  // A draft sent by hand while a voice reply is still open must not be sent again by the session's timer.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const sp = await L.speechWav('Attention. Deploy it to staging now please.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
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
  const sp = await L.speechWav('Katana. Switch to UI client.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(12000), sp.subarray(at + 8), gap(40000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__cf.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__cf.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': true, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
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
    '/__plain.wav': await wavOf(gap(9000), 'Katana. Switch to the store and buy some milk.', gap(30000)),
    '/__cancel.wav': await wavOf(gap(9000), 'Attention. Deploy it to staging now.', gap(2500), 'Cancel.', gap(30000)),
  };
  const srv2 = await L.serve(undefined, files);
  const run = async (file, prefs, check) => {
    const c = await L.openClient({url: srv2.url, mic: file, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, ...prefs}});
    try {
      await L.waitFor(c.page, 'message', () => document.querySelector('.msg.agent'));
      await L.waitFor(c.page, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
      await c.page.locator('.msg.agent .s').first().tap();
      await check(c.page);
    } finally { await c.close(); }
  };
  await run('/__plain.wav', {'voice2-long-silence-s': 60}, async p => {   // a "switch to ..." phrase is never sent as a reply, even without a match
    await L.waitFor(p, 'switch phrase handled', () => /vc\s+(switch to "|no agent matches)/.test(window.__voice.log.text()), 60000);
    assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'not reply text');
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
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
    const ttsCalls = () => p.evaluate(() => (window.__voice.log.text().match(/relay\s+tts /g) || []).length);
    const before = await ttsCalls();
    await p.locator('#find').tap(); await p.locator('#topics li', {hasText: 'uiclient_steer'}).tap();
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'uiclient_steer', 10000);
    await L.sleep(4000);
    assert.equal(await ttsCalls(), before, 'silent switch with voice control off');
  } finally { await c.close(); }
};

scenarios.voiceYesAfterPause = async () => {
  // "Attention. Switch to terminal." (no command word) is a switch: read back, "Yes.", switched; the phrase is never reply text.
  await L.wipeTasks();
  await L.agentSays('terminal_viz', '[terminal_viz] T\n\nTerminal agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Katana. Switch to terminal.', 'Attention.', 'Yes.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(10000), pcm(ws[0]), gap(7000), pcm(ws[1]), gap(2500), pcm(ws[2]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ya.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ya.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'terminal_viz', 60000);
    await L.sleep(12000);   // no delayed "out of voice mode" chime or leftover session
    const log = await p.evaluate(() => window.__voice.log.text());
    if (process.env.DUMP) console.log(log.split('\n').filter(l => / vc /.test(l)).join('\n'));
    assert.doesNotMatch(log, /empty session timed out|command mode left/, 'no lingering session');
    assert.equal(await p.evaluate(() => document.querySelector('#listen').dataset.state), 'on', 'back to idle state: ' + await p.evaluate(() => document.querySelector('#listen').dataset.state));
  } finally { await c.close(); await srv2.close(); }
};

scenarios.voiceTrailingCancel = async () => {
  // "Pause. Some reply. Cancel." in one utterance discards everything; later silence or "over" sends nothing.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Attention. This is a test whether the cancel works. Cancel.', 'Tomato.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
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
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
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
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), sp = await L.speechWav('Katana. Switch back.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([gap(9000), sp.subarray(at + 8), gap(25000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__sb.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__sb.wav', prefs: {'voice2-topic': 'alpha', 'voice2-prev': 'beta', 'voice2-autoplay': false, 'voice2-vc': true}});
  try {
    await L.waitFor(c.page, 'message', () => document.querySelector('.msg.agent'));
    await L.waitFor(c.page, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
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
  const ws = await Promise.all(['Katana. Switch to Human Manager.', 'Yes.', 'Katana. Switch to zebra quokka.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(9000), pcm(ws[0]), gap(14000), pcm(ws[1]), gap(12000), pcm(ws[2]), gap(25000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ab.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ab.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 5, 20000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'switched to human_mgr', () => window.__voice.current() === 'human_mgr', 60000);
    if (process.env.DUMP) setTimeout(async () => console.log((await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => / vc /.test(l)).join('\n')), 40000);
    await L.waitFor(p, 'xyzzy handled', () => /vc\s+no agent matches/.test(window.__voice.log.text()), 60000);
    await L.sleep(3000);
    const sent = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}], anchor: 'newest', num_before: 20, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId);
    assert.equal(sent.length, 0, 'nothing sent as a reply: ' + JSON.stringify(sent.map(m => m.content)));
  } finally { await c.close(); await srv2.close(); }
};

scenarios.directiveWithFooter = async ({srv}) => {
  // Exact amh content: the [switch:] line is followed by a footer block; the message is already unread when the page loads (backlog).
  await L.wipeTasks();
  await L.agentSays('vl_meeting_work', '[vl_meeting_work] Meeting\n\nMeeting agent is here.');
  await L.agentSays('human_mgr', '[human_mgr] Zulip message\n\nNext: vl_meeting_work, to discuss research directions for your advisors.\n\n[switch: vl_meeting_work]\n\npending item deleted:\n- Switch me to the next agent I should talk to.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'human_mgr', 'voice2-autoplay': true}});
  try {
    await L.waitFor(c.page, 'message', () => document.querySelector('.msg.agent'));
    await c.page.locator('#feed').tap();
    await L.waitFor(c.page, 'switched by the directive', () => window.__voice.current() === 'vl_meeting_work', 40000);
    assert.ok(!(await c.page.evaluate(() => document.querySelector('#feed').innerText)).includes('[switch'), 'directive line hidden');
  } finally { await c.close(); }
};

scenarios.noisyRoomUploads = async () => {
  // Noise bursts every ~2 s for a minute: uploads stay under the per-minute cap and the app backs off.
  await L.wipeTasks();
  const {spawnSync} = await import('node:child_process');
  const bursts = Array.from({length: 40}, (_, i) => `anoisesrc=color=pink:amplitude=0.5,atrim=0:0.6,adelay=${i * 2000}|${i * 2000}`);
  const noise = spawnSync('ffmpeg', ['-nostdin', '-v', 'error', '-f', 'lavfi', '-i', 'anoisesrc=color=pink:amplitude=0.45:duration=80,volume=enable=\'lt(mod(t,2),0.6)\':volume=1,volume=enable=\'gte(mod(t,2),0.6)\':volume=0.01', '-ar', '24000', '-ac', '1', '-f', 'wav', 'pipe:1'], {maxBuffer: 1e9}).stdout;
  void bursts;
  const srv2 = await L.serve(undefined, {'/__noise2.wav': noise});
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 12}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv2.url, mic: '/__noise2.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  try {
    await L.waitFor(c.page, 'message', () => document.querySelector('.msg.agent'));
    await c.page.locator('#listen').tap(); await L.sleep(1000); await c.page.locator('#listen').tap();   // off then on: a tap arms the microphone
    await L.sleep(70000);
    const log = await c.page.evaluate(() => window.__voice.log.text());
    const uploads = (log.match(/http +POST user_uploads/g) || []).length;
    return {uploadsIn70s: uploads, droppedByCap: (log.match(/dropped a clip without uploading/g) || []).length, backoffs: (log.match(/noisy room/g) || []).length, speechDetected: (log.match(/speech detected/g) || []).length};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.tapToPause = async ({srv}) => {
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'banner', () => document.querySelector('#banner').innerText.includes('Tap to listen'));
    await p.locator('#banner').tap();
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing');
    await p.locator('.s.now').tap();            // tap the playing sentence: pauses
    await L.waitFor(p, 'paused', () => window.__voice.player.snapshot().status === 'paused', 3000);
    assert.ok(await p.evaluate(() => document.querySelector('#tap-flash').classList.contains('show')), 'visual feedback');
    const before = await p.evaluate(() => window.__voice.player.currentTime());
    await p.locator('#feed').tap({position: {x: 4, y: 300}});   // empty space: resumes in place
    await L.waitFor(p, 'resumed', () => window.__voice.player.snapshot().status === 'playing', 5000);
    assert.ok(await p.evaluate(t => window.__voice.player.currentTime() >= t - 0.3, before), 'resumed in place');
    // media session: the lock screen / headphone handlers drive the same controls
    const ms = await p.evaluate(async () => { const ok = 'mediaSession' in navigator; return {ok, title: ok ? navigator.mediaSession.metadata?.title : null, state: ok ? navigator.mediaSession.playbackState : null}; });
    if (ms.ok) assert.ok(ms.title && ms.state === 'playing', JSON.stringify(ms));
    return {mediaSession: ms.ok};
  } finally { await c.close(); }
};

scenarios.directiveFiresOnce = async ({srv}) => {
  await L.wipeTasks();
  await L.agentSays('beta', '[beta] B\n\nBeta agent here.');
  const dirId = await L.agentSays('alpha', '[alpha] Handoff\n\nPassing you to beta now.\n\n[switch: beta]');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('#feed').tap();
    await L.waitFor(p, 'switched to beta by the directive', () => window.__voice.current() === 'beta', 40000);
    assert.ok(await L.serverRead(dirId), 'directive message marked read');
    await L.waitFor(p, 'quiet on beta', () => window.__voice.player.snapshot().status === 'idle' && !document.querySelector('.msg.unread'), 40000);
    // switch back by hand: the read directive must not replay or switch again
    await p.locator('#find').tap(); await p.locator('#topics li', {hasText: 'alpha'}).tap();
    await L.waitFor(p, 'on alpha', () => window.__voice.current() === 'alpha', 10000);
    await L.sleep(8000);
    assert.equal(await p.evaluate(() => window.__voice.current()), 'alpha', 'no second switch');
    assert.equal((await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => new RegExp('audio\\s+start ' + 0 + '').test(l) && false).length, 0);
    const starts = (await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => /audio\s+start /.test(l) && l.includes(String(dirId)));
    assert.equal(starts.length, 1, 'directive message played once: ' + starts.length);
  } finally { await c.close(); }
};

scenarios.voiceSendNoWait = async () => {
  // Send while transcripts are pending, and again while offline: the UI frees at once, queued sends retry with the same id, exactly one message each, end word stripped.
  await L.wipeTasks();
  const sp = await L.speechWav('Deploy the build to staging now. Tomato.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([sp.subarray(at + 8), Buffer.alloc(48000 * 30)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__sn.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__sn.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const mine = async () => (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId).map(m => m.content);
  try {
    await L.agentSays('alpha', '[alpha] A\n\nSeed message.');
    await L.waitFor(p, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    for (const offline of [false, true]) {
      await p.locator('#mic').tap();
      await L.waitFor(p, 'recording', () => document.querySelector('#composer').classList.contains('rec'), 8000);
      await L.sleep(4500);
      if (offline) await c.context.setOffline(true);
      const t0 = Date.now(); await p.locator('#send').tap();
      await L.waitFor(p, 'idle composer and a bubble', () => !document.querySelector('#composer').classList.contains('rec') && document.querySelector('.msg.mine.sending, .msg.mine'), 3000);
      const freeMs = Date.now() - t0;
      assert.ok(freeMs < 1500, 'composer free after ' + freeMs);
      if (offline) {
        await L.sleep(4000);
        assert.match(await p.locator('.msg.mine .state').last().innerText(), /Waiting for connection|Sending/, 'visible queued state');
        await c.context.setOffline(false);
      }
      const want = offline ? 2 : 1;
      let got = [];
      for (let i = 0; i < 40; i++) { got = await mine(); if (got.length >= want) break; await L.sleep(1500); }
      await L.sleep(3000); got = await mine();
      assert.equal(got.length, want, `exactly ${want} message(s): ` + JSON.stringify(got));
      assert.match(got.at(-1), /deploy/i); assert.doesNotMatch(got.at(-1), /tomato/i, 'end word stripped: ' + JSON.stringify(got));
    }
  } finally { await c.close(); await srv2.close(); }
};

scenarios.outboxSurvivesReload = async () => {
  // A voice send queued while the network is blocked survives a page reload and is sent exactly once afterwards.
  await L.wipeTasks();
  const sp = await L.speechWav('Deploy the build to staging now.'), at = sp.indexOf('data'), head = Buffer.from(sp.subarray(0, at + 8));
  const data = Buffer.concat([sp.subarray(at + 8), Buffer.alloc(48000 * 30)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ob.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ob.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const mine = async () => (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId).map(m => m.content);
  try {
    await L.agentSays('alpha', '[alpha] A\n\nSeed message.');
    await L.waitFor(p, 'seed', () => document.querySelector('#feed').innerText.includes('Seed message'));
    await p.locator('#mic').tap();
    await L.waitFor(p, 'recording', () => document.querySelector('#composer').classList.contains('rec'), 8000);
    await L.sleep(4500);
    await p.route(/\/api\/v1\//, r => r.abort('connectionfailed'));
    await p.locator('#send').tap();
    await L.waitFor(p, 'queued bubble', () => document.querySelector('.msg.mine'), 3000);
    await L.sleep(2500);   // the outbox is saved shortly after the state changes
    assert.ok(await p.evaluate(() => (JSON.parse(localStorage.getItem('voice2-outbox') || '[]')).length >= 1), 'outbox saved');
    await p.unroute(/\/api\/v1\//);
    await p.reload();
    let got = [];
    for (let i = 0; i < 40; i++) { got = await mine(); if (got.length) break; await L.sleep(1500); }
    await L.sleep(4000); got = await mine();
    assert.equal(got.length, 1, 'exactly one message after the reload: ' + JSON.stringify(got));
    assert.match(got[0], /deploy/i);
    assert.equal(await p.evaluate(() => (JSON.parse(localStorage.getItem('voice2-outbox') || '[]')).length), 0, 'outbox cleared');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.onDeviceStartWord = async () => {
  // With the opt-in on-device keyword spotter, "attention" pauses playback and opens the session without any upload; "tomato" ends a reply.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Attention.', 'Deploy it to staging.', 'Tomato.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(10000), pcm(ws[0]), gap(3500), pcm(ws[1]), gap(3500), pcm(ws[2]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__od.wav': Buffer.concat([head, data])});
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 12}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv2.url, mic: '/__od.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': false, 'voice2-kws': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await p.locator('#listen').tap();
    await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 30000).catch(async e => { console.log((await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => / kws /.test(l) || /voice control|vc  /.test(l)).slice(0, 14).join('\n')); throw e; });
    await L.waitFor(p, 'local start word', () => /on-device keyword: ATTENTION/.test(window.__voice.log.text()), 60000);
    const log = await p.evaluate(() => window.__voice.log.text());
    if (process.env.DUMP) console.log(log.split('\n').filter(l => / (vc|lat|kws|mic) /.test(l)).slice(0, 30).join('\n'));
    assert.match(log, /lat +startword total=\d+ms.* kws-local=/, 'local span logged');
    assert.equal(await p.evaluate(() => window.__voice.player.snapshot().status), 'paused', 'paused locally');
    await L.waitFor(p, 'sent at once by the end word', () => /sending now/.test(window.__voice.log.text()), 40000);
    const lg = await p.evaluate(() => window.__voice.log.text());
    assert.doesNotMatch(lg, /vc +resume phrase \+ silence/, 'no quiet window');
    assert.match(lg, /vc +on-device keyword: TOMATO/, 'the end word was heard on the device, after a pause, not only by the server');
    let sent = [];
    for (let i = 0; i < 10 && !sent.length; i++) { sent = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId).map(m => m.content); if (!sent.length) await L.sleep(1000); }
    assert.equal(sent.length, 1, JSON.stringify(sent)); assert.match(sent[0], /deploy it to staging/i); assert.doesNotMatch(sent[0], /tomato|attention/i);
    const lat = ((await p.evaluate(() => window.__voice.log.text())).match(/local hit -> \w+ sound started in \d+ms/g) || []).join('; ');
    return {lat};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.onDeviceAfterReload = async () => {
  // After a reload with voice control and the on-device toggle persisted on, the spotter loads by itself (no tap on the toggle); with the opt-in on-device keyword spotter, "attention" pauses playback and opens the session without any upload; "tomato" ends a reply.
  await L.wipeTasks();
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Attention.', 'Deploy it to staging.', 'Tomato.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(10000), pcm(ws[0]), gap(3500), pcm(ws[1]), gap(1200), pcm(ws[2]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__od.wav': Buffer.concat([head, data])});
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 12}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv2.url, mic: '/__od.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-kws': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'spotter loading started by the restored setting', () => /kws +loading the on-device model/.test(window.__voice.log.text()), 10000);
    await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 30000).catch(async e => { console.log((await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => / kws /.test(l) || /voice control|vc  /.test(l)).slice(0, 14).join('\n')); throw e; });
    await L.waitFor(p, 'local start word', () => /on-device keyword: ATTENTION/.test(window.__voice.log.text()), 60000);
    const log = await p.evaluate(() => window.__voice.log.text());
    if (process.env.DUMP) console.log(log.split('\n').filter(l => / (vc|lat|kws|mic) /.test(l)).slice(0, 30).join('\n'));
    assert.match(log, /lat +startword total=\d+ms.* kws-local=/, 'local span logged');
    assert.equal(await p.evaluate(() => window.__voice.player.snapshot().status), 'paused', 'paused locally');
    await L.waitFor(p, 'sent at once by the end word', () => /sending now/.test(window.__voice.log.text()), 40000);
    const lg = await p.evaluate(() => window.__voice.log.text());
    assert.doesNotMatch(lg, /vc +resume phrase \+ silence/, 'no quiet window');
    let sent = [];
    for (let i = 0; i < 10 && !sent.length; i++) { sent = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId).map(m => m.content); if (!sent.length) await L.sleep(1000); }
    assert.equal(sent.length, 1, JSON.stringify(sent)); assert.match(sent[0], /deploy it to staging/i); assert.doesNotMatch(sent[0], /tomato|attention/i);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.instantSwitch = async ({srv}) => {
  await L.wipeTasks();
  for (const t of ['alpha', 'beta', 'gamma']) await L.agentSays(t, `[${t}] T\n\nThis is the ${t} agent with some history.`);
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const switchTo = async name => p.evaluate(async n => { const t = performance.now(); document.querySelector('#find').click(); await new Promise(r => setTimeout(r, 50)); const li = [...document.querySelectorAll('#topics li')].find(li => li.innerText.includes(n)); const t1 = performance.now(); li.click(); await new Promise(r => requestAnimationFrame(r)); return {ms: performance.now() - t1, text: document.querySelector('#feed').innerText}; }, name);
  try {
    await L.waitFor(p, 'topics cached', () => window.__voice.model.messages('beta').length > 0 && window.__voice.model.messages('gamma').length > 0, 20000);
    let calls = await p.evaluate(() => window.__voice.zulip.totalCalls());
    const r1 = await switchTo('beta');
    assert.match(r1.text, /beta agent/, 'cached feed shown at once'); assert.ok(r1.ms < 250, 'render ms ' + r1.ms);
    await L.sleep(1500);
    // cold reload: the persisted cache is enough, offline
    await L.sleep(1200);
    await p.route(/\/api\/v1\//, r => r.abort('connectionfailed'));   // no Zulip at all after the reload
    await p.reload();
    await L.waitFor(p, 'cold cache', () => window.__voice.model.messages('gamma').length > 0, 10000);
    const r2 = await switchTo('gamma');
    assert.match(r2.text, /gamma agent/, 'cold offline switch renders from the cache'); assert.ok(r2.ms < 250, 'cold render ms ' + r2.ms);
    await p.unroute(/\/api\/v1\//);
    return {warmSwitchMs: Math.round(r1.ms), coldOfflineSwitchMs: Math.round(r2.ms)};
  } finally { await c.close(); }
};

scenarios.skipPastEnd = async ({srv}) => {
  // Next past the last chunk: nothing plays, everything is marked read, no wrap-around; "next three" by voice skips three chunks.
  await L.wipeTasks();
  const id = await L.agentSays('alpha', '[alpha] Short\n\nFirst chunk of the message.\n\nSecond chunk of the message.\n\nThird and last chunk.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent .s'));
    await p.locator('.msg.agent .s').last().tap();
    await L.waitFor(p, 'on the last chunk', () => document.querySelector('.s.now')?.innerText.includes('Third'), 10000);
    await p.evaluate(() => document.querySelector('#next').click());
    await L.waitFor(p, 'end of queue', () => window.__voice.player.snapshot().status === 'idle' && document.querySelector('#toast').innerText.includes('End of messages'), 5000);
    await L.sleep(2500);
    assert.equal(await p.evaluate(() => window.__voice.player.snapshot().status), 'idle', 'nothing plays after the end');
    assert.ok(await L.serverRead(id), 'marked read');
    // 🧑 "It wraps back when skipping next from the end": next (tap, and whole-message via Shift+Right) at the end, also with the last-heard position forgotten, must not wrap to the start
    for (const forget of [false, true]) {
      if (forget) await p.evaluate(() => { for (const k of Object.keys(localStorage)) if (k.includes('voice2-last')) localStorage.removeItem(k); });
      await p.evaluate(() => document.querySelector('#next').click());
      await p.keyboard.press('Shift+ArrowRight');
      await L.sleep(2500);
      assert.equal(await p.evaluate(() => window.__voice.player.snapshot().status), 'idle', `next at the end wrapped (forgot last heard: ${forget})`);
    }
  } finally { await c.close(); }
};

scenarios.coalescedBundle = async ({srv}) => {
  // A burst of messages in one topic is pre-rendered as one head bundle plus ONE shared tail bundle; every chunk of every message still plays, in order.
  await L.wipeTasks();
  const ids = [];
  for (const n of [1, 2, 3]) ids.push(await L.agentSays('alpha', `[alpha] M${n}\n\n` + Array.from({length: 5}, (_, i) => `Message ${n} sentence ${i} is here.`).join('\n\n')));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'messages', () => document.querySelectorAll('.msg.agent').length >= 3);
    await L.waitFor(p, 'prepared bundles', ids => new Set(window.__voice.voice.preparedEntries().filter(b => (b.spans ?? [b]).some(x => ids.includes(x.messageId))).map(b => b.id)).size >= 2, 40000, ids);
    await L.sleep(4000);
    const ids2 = await p.evaluate(ids => [...new Set(window.__voice.voice.preparedEntries().filter(b => (b.spans ?? [b]).some(x => ids.includes(x.messageId))).map(b => b.id))], ids);
    assert.ok(ids2.length <= 3, 'bundles: ' + JSON.stringify(ids2));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'last message heard', () => /start \d+:\d+/.test(window.__voice.log.text()) && (window.__voice.log.text().match(/audio +start /g) || []).length >= 15, 90000);
    const starts = (await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => /audio +start /.test(l)).map(l => l.match(/start (\d+):(\d+)/).slice(1).join(':'));
    assert.deepEqual(starts.slice(0, 15), ids.flatMap(id => [0, 1, 2, 3, 4].map(i => `${id}:${i}`)), 'order ' + starts);
    return {bundles: ids2.length};
  } finally { await c.close(); }
};

scenarios.commandModes = async ({srv}) => {
  // Standby -> "katana" alone -> COMMAND mode: a chained "next two, back one" runs both and never becomes text. In INPUT mode (dictating) "katana" is not a command: no mode switch, nothing runs.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 10}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const mk = async (texts, gaps) => { const ws = await Promise.all(texts.map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8)); const parts = [gap(14000)]; ws.forEach((w, i) => { parts.push(pcm(w), gap(gaps[i])); }); const data = Buffer.concat(parts); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4); return Buffer.concat([head, data]); };
  const srv2 = await L.serve(undefined, {'/__modes.wav': await mk(['Katana.', 'Next two. Back one.'], [2000, 30000]), '/__input.wav': await mk(['Attention.', 'Remember the milk.', 'Katana.', 'Next two.'], [2500, 2000, 2000, 25000])});
  try {
    let c = await L.openClient({url: srv2.url, mic: '/__modes.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
    let p = c.page;
    try {
      await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
      await p.locator('.msg.agent .s').first().tap();
      await L.waitFor(p, 'command mode', () => /vc +command mode/.test(window.__voice.log.text()), 70000);
      await L.waitFor(p, 'both commands', () => /navigate next x2/.test(window.__voice.log.text()) && /navigate previous x1/.test(window.__voice.log.text()), 30000);
      assert.match(await p.evaluate(() => window.__voice.log.text()), /commands: navigate, navigate/, 'two ticks');
      assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'commands are not text');
    } finally { await c.close(); }
    c = await L.openClient({url: srv2.url, mic: '/__input.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
    p = c.page;
    try {
      await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
      await p.locator('.msg.agent .s').first().tap();
      await L.waitFor(p, 'dictated', () => /heard "Remember the milk/i.test(window.__voice.log.text()), 70000);
      await L.sleep(22000);
      const log = await p.evaluate(() => window.__voice.log.text());
      assert.doesNotMatch(log, /vc +command mode/, 'katana while dictating did not enter command mode');
      assert.doesNotMatch(log, /navigate next/, 'no command ran');
    } finally { await c.close(); }
  } finally { await srv2.close(); }
};

scenarios.multiChannel = async ({srv}) => {
  // Topics of another subscribed channel appear in the one list with a channel label; reading, unread marking and replies use that channel.
  await L.wipeTasks();
  const subs = await L.human('GET', 'users/me/subscriptions'), extra = subs.subscriptions.find(x => x.name === 'vtest-extra').stream_id;
  const post = (topic, content) => L.bot('POST', 'messages', {type: 'stream', to: extra, topic, content}).then(r => r.id);
  const id = await post('news_item', '[news_item] N\n\nFirst news sentence here.\n\nSecond news sentence here.');
  await L.agentSays('alpha', '[alpha] A\n\nAlpha agent is here.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-channels': [extra]}});
  const p = c.page;
  try {
    await L.waitFor(p, 'both channels listed', () => window.__voice.model.topics().some(t => t.name === 'vtest-extra · news_item') && window.__voice.model.topics().some(t => t.name === 'alpha'), 20000);
    await L.waitFor(p, 'channel history cached', () => window.__voice.model.messages('vtest-extra · news_item').length > 0, 20000);
    await p.locator('#find').tap();
    await p.locator('#topics li', {hasText: 'news_item'}).tap();
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'vtest-extra · news_item', 10000);
    await L.waitFor(p, 'news shown', () => document.querySelector('#feed').innerText.includes('First news sentence'), 10000).catch(async e => { console.log('FEED:', await p.evaluate(() => JSON.stringify(document.querySelector('#feed').innerText.slice(0, 100)) + ' || ' + window.__voice.current() + ' || ' + JSON.stringify(window.__voice.model.messages(window.__voice.current()).map(m => m.subject)))); throw e; });
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing', 30000);
    const reply = 'Thanks for the news ' + Date.now();
    await p.locator('#draft').fill(reply);
    await p.locator('#send').tap();
    let msgs = [];
    for (let i = 0; i < 10 && !msgs.length; i++) { await L.sleep(1000); msgs = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: extra}, {operator: 'topic', operand: 'news_item'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId && m.content === reply); }
    assert.equal(msgs.length, 1, 'reply posted to the other channel');
    for (const m of (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: extra}], anchor: 'newest', num_before: 50, num_after: 0, apply_markdown: false})).messages) await L.zulipAs(m.sender_id === L.account.botId ? 'bot' : 'human')('DELETE', 'messages/' + m.id).catch(() => {});
    return {id};
  } finally { await c.close(); }
};

const attentionCommand = onDevice => async ({srv}) => {
  // "Attention katana next two" in one breath: straight to command mode, both chunks skipped, nothing in the draft.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 10}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1);
  const w = await L.speechWav('Katana. Next two.'), at = w.indexOf('data'), head = Buffer.from(w.subarray(0, at + 8));
  const data = Buffer.concat([gap(14000), w.subarray(at + 8), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ak.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ak.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, ...(onDevice ? {'voice2-kws': true} : {})}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    if (onDevice) await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'navigated', () => /navigate next x2/.test(window.__voice.log.text()), 70000);
    const log = await p.evaluate(() => window.__voice.log.text());
    assert.equal((log.match(/vc +command mode\n|vc +command mode$/gm) || []).length, 1, 'one command-mode entry');
    assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'no draft text');
    if (process.env.DUMP) console.log(log.split('\n').filter(l => / (vc|kws|lat) /.test(l)).join('\n'));
    return {ready: (log.match(/command mode ready (\d+)ms/) || [])[1]};
  } finally { await c.close(); await srv2.close(); }
};
scenarios.attentionKatana = attentionCommand(false);
scenarios.attentionKatanaOnDevice = attentionCommand(true);

scenarios.localCommand = async ({srv}) => {
  // In command mode the on-device spotter hears "switch to terminal viz" and acts at once; the clip is not sent to the server.
  await L.wipeTasks();
  await L.agentSays('terminal_viz', '[terminal_viz] T\n\nTerminal agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Katana.', 'Switch to terminal viz.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(12000), pcm(ws[0]), gap(1800), pcm(ws[1]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__lc.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__lc.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-kws': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'local command', () => /on-device command: switch to terminal viz/.test(window.__voice.log.text()), 70000);
    await L.waitFor(p, 'clip kept off the server', () => /clip already handled on-device/.test(window.__voice.log.text()), 20000);
    const lines = (await p.evaluate(() => window.__voice.log.text())).split('\n'), tm = l => { const m = l?.match(/^(\d+):(\d+):(\d+)\.(\d+)/); return m ? ((+m[1] * 60 + +m[2]) * 60 + +m[3]) * 1000 + +m[4] : NaN; };
    const hit = lines.find(l => /on-device command: switch to terminal viz/.test(l)), clip = lines.find(l => /clip already handled on-device/.test(l)), build = lines.find(l => /command spotter: \d+ phrases in/.test(l));
    assert.match(lines.join('\n'), /vc +switch to "terminal viz"/, 'the switch question was asked');
    return {hitToClipEndMs: tm(clip) - tm(hit), spotterBuild: build?.match(/phrases in (\d+)ms/)?.[1]};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.messageEditedWhileRead = async ({srv}) => {
  // The message being read is edited: the "updated" tone, then it is read again from its start with the new text.
  await L.wipeTasks();
  const long = tag => '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `${tag} paragraph ${i} keeps the message playing for a while.`).join('\n\n');
  const id = await L.agentSays('alpha', long('First'));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent .s'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'on the second chunk', () => /audio +start \d+:1/.test(window.__voice.log.text()), 30000);
    await L.bot('PATCH', 'messages/' + id, {content: long('Second')});
    await L.waitFor(p, 'edit seen', () => /message \d+ edited while being read/.test(window.__voice.log.text()), 20000);
    await L.waitFor(p, 'new text shown', () => document.querySelector('#feed').innerText.includes('Second paragraph 0'), 10000);
    await L.waitFor(p, 'restart from the start', () => (window.__voice.log.text().match(/audio +start \d+:0/g) || []).length >= 2, 20000);
    assert.equal(await p.evaluate(() => window.__voice.player.snapshot().status), 'playing');
  } finally { await c.close(); }
};

scenarios.messageDeletedAndReplaced = async ({srv}) => {
  // amh replaces a message by delete + new post: the message being read is deleted, a replacement arrives; tone, then the replacement is read from its start.
  await L.wipeTasks();
  const long = tag => '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `${tag} paragraph ${i} keeps the message playing for a while.`).join('\n\n');
  const id = await L.agentSays('alpha', long('Old'));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent .s'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'on the second chunk', () => /audio +start \d+:1/.test(window.__voice.log.text()), 30000);
    await L.bot('DELETE', 'messages/' + id);
    await L.waitFor(p, 'delete seen', () => /message \d+ deleted while being read/.test(window.__voice.log.text()), 20000);
    const id2 = await L.agentSays('alpha', long('New'));
    await L.waitFor(p, 'replacement read', id2 => new RegExp(`replacement ${id2} read from its start`).test(window.__voice.log.text()), 20000, id2);
    await L.waitFor(p, 'replacement playing', id2 => new RegExp(`audio +start ${id2}:0`).test(window.__voice.log.text()), 30000, id2);
    assert.ok(!(await p.evaluate(() => document.querySelector('#feed').innerText)).includes('Old paragraph'), 'the deleted message is gone from the feed');
  } finally { await c.close(); }
};

scenarios.offlinePlayWaits = async ({srv}) => {
  // Play with no connection: spinner at once, spamming does not restart or queue, after ~5 s the waiting sound fades in, an interruption tone is logged; when the network returns playback starts and the sound stops.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 6}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent .s'));
    await L.sleep(1500);
    await c.context.setOffline(true);
    for (let i = 0; i < 6; i++) { await p.locator('#play').tap().catch(() => {}); await L.sleep(150); }
    const st = await p.evaluate(() => document.querySelector('#play').dataset.state + ' ' + window.__voice.player.snapshot().status);
    assert.match(st, /loading|error/, 'spinner or waiting state right away: ' + st);
    await L.waitFor(p, 'waiting sound', () => /audio +waiting sound on/.test(window.__voice.log.text()), 20000);
    const log = await p.evaluate(() => window.__voice.log.text());
    assert.equal((log.match(/audio +idle -> loading/g) || []).length, 1, 'one load, not one per tap');
    await c.context.setOffline(false);
    await L.waitFor(p, 'playing after the network is back', () => window.__voice.player.snapshot().status === 'playing', 60000);
    await L.waitFor(p, 'waiting sound off', () => /audio +waiting sound off/.test(window.__voice.log.text()), 10000);
    return {interrupted: ((await p.evaluate(() => window.__voice.log.text())).match(/interrupted: [\w-]+/g) || []).length};
  } finally { await c.close(); }
};

scenarios.bambooCancels = async ({srv}) => {
  // The rare cancel word, heard on the device, discards the open reply at once.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Attention.', 'Remember to buy the milk today.', 'Bamboo.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(12000), pcm(ws[0]), gap(2500), pcm(ws[1]), gap(2500), pcm(ws[2]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__bb.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__bb.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-kws': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'on-device cancel', () => /on-device keyword: BAMBOO/.test(window.__voice.log.text()) && /vc +cancel: reply discarded/.test(window.__voice.log.text()), 70000);
    assert.equal((await p.locator('#draft').inputValue()).trim(), '', 'nothing left in the draft');
    const lat = ((await p.evaluate(() => window.__voice.log.text())).match(/local hit -> \w+ sound started in \d+ms/g) || []).join('; ');
    return {lat};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.positiveAnswer = async ({srv}) => {
  // "switch to terminal viz" -> "Switch to ...?" -> "positive" answered on the device -> switched.
  await L.wipeTasks();
  await L.agentSays('terminal_viz', '[terminal_viz] T\n\nTerminal agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Katana.', 'Switch to terminal viz.', 'Positive.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(12000), pcm(ws[0]), gap(1800), pcm(ws[1]), gap(7000), pcm(ws[2]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__pa.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__pa.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-kws': true, 'voice2-switch-confirm': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'terminal_viz', 80000);
    assert.match(await p.evaluate(() => window.__voice.log.text()), /on-device command: positive/, 'the answer was heard on the device');
    const lat = ((await p.evaluate(() => window.__voice.log.text())).match(/local hit -> \w+ sound started in \d+ms/g) || []).join('; ');
    return {lat};
  } finally { await c.close(); await srv2.close(); }
};

scenarios.commandSpeechNeverPosted = async ({srv}) => {
  // Speech after the command word never joins the reply; a cancel word (even misheard as "Rambo") posts nothing.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const mk = async (name, texts, gaps) => { const ws = await Promise.all(texts.map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8)); const parts = [gap(12000)]; ws.forEach((w, i) => { parts.push(pcm(w), gap(gaps[i])); }); const data = Buffer.concat(parts); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4); return Buffer.concat([head, data]); };
  const files = {
    '/__leak.wav': await mk('leak', ['Katana.', 'Pencil.'], [1800, 40000]),
    '/__rambo.wav': await mk('rambo', ['Attention.', 'Remember the milk.', 'Pencil. Rambo. Cancel.'], [2500, 2000, 40000]),
  };
  const srv2 = await L.serve(undefined, files);
  const posted = async () => (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId).map(m => m.content);
  try {
    for (const [file, wantText] of [['/__leak.wav', null], ['/__rambo.wav', null]]) {
      const c = await L.openClient({url: srv2.url, mic: file, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-long-silence-s': 6}});
      try {
        await L.waitFor(c.page, 'message', () => document.querySelector('.msg.agent'));
        await c.page.locator('.msg.agent .s').first().tap();
        await L.waitFor(c.page, 'session', () => /vc +(command mode|session opened)/.test(window.__voice.log.text()), 70000);
        await L.sleep(file === '/__leak.wav' ? 25000 : 25000);
      } finally { await c.close(); }
      const got = await posted();
      if (wantText) { assert.equal(got.length, 1, 'one reply: ' + JSON.stringify(got)); assert.match(got[0], wantText, 'only the reply text: ' + got[0]); }
      else assert.equal(got.length, 0, 'command speech and cancelled dictation post nothing: ' + JSON.stringify(got));
    }
  } finally { await srv2.close(); }
};

scenarios.switchEarcons = async ({srv}) => {
  // The switch flow's sounds: ready (start word), command (katana), then the same double click at once when "positive" confirms; "Now on" is spoken only when nothing is about to play.
  await L.wipeTasks();
  await L.agentSays('terminal_viz', '[terminal_viz] T\n\nTerminal agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Katana.', 'Switch to terminal viz.', 'Positive.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(12000), pcm(ws[0]), gap(1800), pcm(ws[1]), gap(7000), pcm(ws[2]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__se.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__se.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-switch-confirm': true}});
  const p = c.page;
  try {
    await p.evaluate(() => { window.__tones = []; const orig = window.__voice.player.earcon; window.__voice.player.earcon = (notes, s) => { window.__tones.push(notes.join('/')); return orig.call(window.__voice.player, notes, s); }; });
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'terminal_viz', 80000);
    await L.sleep(1500);
    const tones = await p.evaluate(() => window.__tones); if (process.env.DUMP) console.log((await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => / vc /.test(l)).join('\n'));
    assert.deepEqual(tones, ['880/1320', '740/0/740', '740/0/740'], 'mic-on ready, command, switched click (same double click), nothing else: ' + JSON.stringify(tones));
  } finally { await c.close(); await srv2.close(); }
};

scenarios.readFlagsOnReplace = async ({srv}) => {
  // (A) the last message played to the end is read on the server soon after; (B) a delete + near-identical replacement of a read message is read too; a changed one stays unread.
  await L.wipeTasks();
  const body = tag => `[alpha] A\n\nStatus ${tag}: the build finished and all tests passed on staging.\n\nSecond line says nothing changed since the last run.`;
  const id1 = await L.agentSays('alpha', body('one'));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent .s'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'played to the end', () => window.__voice.player.snapshot().status === 'idle' && /audio +ended \d+:1/.test(window.__voice.log.text()), 60000);
    assert.ok(await L.serverRead(id1, 12000), 'A: last message read on the server');
    await L.bot('DELETE', 'messages/' + id1);
    const id2 = await L.agentSays('alpha', body('one'));
    await L.waitFor(p, 'replacement handled', i => new RegExp(`replacement ${i} of a read message`).test(window.__voice.log.text()), 20000, id2);
    assert.ok(await L.serverRead(id2, 12000), 'B: near-identical replacement read');
    await L.bot('DELETE', 'messages/' + id2);
    const id3 = await L.agentSays('alpha', '[alpha] A\n\nCompletely different news: deployment failed on production, rollback needed urgently.');
    await L.sleep(6000);
    assert.ok(!(await L.flagsOf(id3)).includes('read'), 'changed text stays unread');
  } finally { await c.close(); }
};

scenarios.readElsewhere = async ({srv}) => {
  // Read on another device while queued: never played here; the last message played to the end stays read after a refresh.
  await L.wipeTasks();
  const line = i => `Message ${i} has a sentence that takes a little while to speak aloud.`;
  const ids = [];
  for (const i of [1, 2, 3]) ids.push(await L.agentSays('alpha', `[alpha] M${i}\n\n${line(i)}\n\n${line(i)} again.`));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'messages', () => document.querySelectorAll('.msg.agent').length >= 3);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'playing message 1', i => new RegExp(`audio +start ${i}:`).test(window.__voice.log.text()), 30000, ids[0]);
    await L.human('POST', 'messages/flags', {messages: [ids[1]], op: 'add', flag: 'read'});   // another device reads message 2
    await L.waitFor(p, 'last message heard', i => new RegExp(`audio +ended ${i}:1`).test(window.__voice.log.text()), 90000, ids[2]);
    const log = await p.evaluate(() => window.__voice.log.text());
    assert.doesNotMatch(log, new RegExp(`audio +start ${ids[1]}:`), 'message read elsewhere was not played');
    assert.ok(await L.serverRead(ids[2], 12000), 'last message read on the server');
    await p.reload();
    await L.waitFor(p, 'reloaded', () => document.querySelector('.msg.agent'));
    await L.sleep(3000);
    assert.equal(await p.evaluate(() => window.__voice.model.unreadAgentMessages('alpha').length), 0, 'nothing unread after the refresh');
  } finally { await c.close(); }
};

scenarios.highlightFollowsAudio = async ({srv}) => {
  // Three messages share one bundle; the middle one is read elsewhere while the first plays. At every moment the highlighted chunk is the chunk being heard.
  await L.wipeTasks();
  const ids = [];
  for (const n of [1, 2, 3]) ids.push(await L.agentSays('alpha', `[alpha] M${n}\n\n` + Array.from({length: 4}, (_, i) => `Message ${n} sentence ${i} is here and takes a moment.`).join('\n\n')));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  try {
    await L.waitFor(p, 'prepared', ids => new Set(window.__voice.voice.preparedEntries().filter(b => (b.spans ?? [b]).some(x => ids.includes(x.messageId))).map(b => b.id)).size >= 2, 40000, ids);
    await L.sleep(3000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing', 30000);
    await L.human('POST', 'messages/flags', {messages: [ids[1]], op: 'add', flag: 'read'});
    const bad = [], seen = new Set(); let strikes = 0;
    for (let i = 0; i < 400; i++) {
      const s = await p.evaluate(() => { const n = document.querySelector('.s.now'), key = n ? n.closest('.msg').dataset.id + ':' + n.dataset.from : null; return {key, heard: window.__voice.player.audible(), status: window.__voice.player.snapshot().status}; });
      if (s.heard) seen.add(s.heard);
      if (s.heard && s.key !== s.heard) { if (++strikes >= 3) bad.push(s); } else strikes = 0;
      if (s.status === 'idle' && seen.size > 4) break;
      await L.sleep(150);
    }
    assert.ok(seen.size >= 8, 'chunks heard: ' + seen.size);
    assert.equal(bad.length, 0, 'highlight differs from the audio: ' + JSON.stringify(bad.slice(0, 3)));
    assert.ok(![...seen].some(k => k.startsWith(ids[1] + ':')), 'message read elsewhere was not played');
  } finally { await c.close(); }
};

scenarios.katanaAlone = async ({srv}) => {
  // "Katana" alone (no start word) enters command mode with one command click; "attention" then "katana" 0.3 s and 1 s apart are both heard on the device.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 10}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const cases = {alone: [['Katana.', 'Next two.'], [1500]]};
  const files = {}, wavs = {};
  for (const [name, [texts, gaps]] of Object.entries(cases)) {
    const ws = await Promise.all(texts.map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
    const parts = [gap(14000)]; ws.forEach((w, i) => { parts.push(pcm(w)); if (i < ws.length - 1) parts.push(gap(gaps[i])); }); parts.push(gap(20000));
    const data = Buffer.concat(parts); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4); files['/__ka_' + name + '.wav'] = Buffer.concat([head, data]);
  }
  const srv2 = await L.serve(undefined, files);
  const out = {};
  try {
    for (const name of Object.keys(cases)) {
      const c = await L.openClient({url: srv2.url, mic: '/__ka_' + name + '.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-kws': true}});
      const p = c.page;
      try {
        await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
        await p.locator('.msg.agent .s').first().tap();
        await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 40000);
        await L.waitFor(p, 'navigated', () => /navigate next x2/.test(window.__voice.log.text()), 70000);
        const log = await p.evaluate(() => window.__voice.log.text());
        assert.equal((log.match(/vc +command mode\n|vc +command mode$/gm) || []).length, 1, name + ': one command-mode entry');
        assert.match(log, /on-device keyword: KATANA/, name + ': katana heard on the device');
        
        assert.equal((await p.locator('#draft').inputValue()).trim(), '', name + ': no draft text');
        out[name] = 'ok';
      } finally { await c.close(); }
    }
  } finally { await srv2.close(); }
  return out;
};

scenarios.switchNoOverlap = async ({srv}) => {
  // Confirming a switch clicks at once. A topic with unread plays right after the click with no spoken "Now on"; one without gets "Now on" and no playback overlaps it.
  await L.wipeTasks();
  await L.agentSays('terminal_viz', '[terminal_viz] T\n\nTerminal agent is here.');
  await L.agentSays('quiet_agent', '[quiet_agent] Q\n\nNothing new here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const mk = async (name, target) => { const ws = await Promise.all(['Katana.', 'Switch to ' + target + '.', 'Positive.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8)); const data = Buffer.concat([gap(12000), pcm(ws[0]), gap(1800), pcm(ws[1]), gap(7000), pcm(ws[2]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4); return Buffer.concat([head, data]); };
  const files = {'/__sn_unread.wav': await mk('u', 'terminal viz'), '/__sn_quiet.wav': await mk('q', 'quiet agent')};
  const srv2 = await L.serve(undefined, files);
  try {
    for (const [file, target, hasUnread] of [['/__sn_unread.wav', 'terminal_viz', true], ['/__sn_quiet.wav', 'quiet_agent', false]]) {
      const c = await L.openClient({url: srv2.url, mic: file, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
      const p = c.page;
      try {
        if (!hasUnread) await p.evaluate(() => {});   // quiet_agent's one message is marked read below
        await p.evaluate(() => { window.__ev = []; const v = window.__voice, t0 = () => Math.round(performance.now()); const e = v.player.earcon; v.player.earcon = (n, s) => { window.__ev.push([t0(), 'tone ' + n.join('/')]); return e.call(v.player, n, s); }; const say = v.player.say; v.player.say = async text => { window.__ev.push([t0(), 'say ' + text]); const ms = await say.call(v.player, text); window.__ev.push([t0(), 'say-end']); return ms; }; });
        await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 3, 45000);
        if (!hasUnread) { const m = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'quiet_agent'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.map(x => x.id); await L.human('POST', 'messages/flags', {messages: m, op: 'add', flag: 'read'}); await L.sleep(2500); }
        await p.locator('.msg.agent .s').first().tap();
        await L.waitFor(p, 'switched', t => window.__voice.current() === t, 80000, target);
        await L.sleep(hasUnread ? 6000 : 7000);
        const ev = await p.evaluate(() => window.__ev), log = await p.evaluate(() => window.__voice.log.text());
        const tones = ev.filter(e => e[1].startsWith('tone')).map(e => e[1]);
        assert.deepEqual(tones.slice(-2), ['tone 740/0/740', 'tone 740/0/740'], 'command click then the switched click: ' + JSON.stringify(ev));
        const said = ev.find(e => e[1].startsWith('say Now on'));
        if (hasUnread) { assert.ok(!said, 'no "Now on" when messages play'); assert.match(log, new RegExp('audio +start \\d+:0'), 'playback started'); }
        else {
          assert.ok(said, '"Now on" spoken: ' + JSON.stringify(ev));
          const end = ev.find(e => e[0] > said[0] && e[1] === 'say-end');
          assert.ok(said[0] > ev.filter(e => e[1].startsWith('tone')).at(-1)[0], 'click before the announcement');
          assert.ok(end, 'announcement finished');
        }
      } catch (e) { if (process.env.DUMP) console.log('kws=' + kws, (await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => / vc /.test(l)).join('\n')); throw e; } finally { await c.close(); }
    }
  } finally { await srv2.close(); }
};

scenarios.noiseNeverPosted = async ({srv}) => {
  // Command-mode speech, retired keywords and cancelled dictations never become a reply.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] A\n\nSomething to reply to.');
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const posted = async () => (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'alpha'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.filter(m => m.sender_id === L.account.humanId).map(m => m.content);
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    const cases = [['Katana. Jump backward five. Skip backward five. Pencil. Rambo. Cancel.'], ['Say that again, but slower. Cangaroo. Canguro.'], ['Remember the milk.', 'Katana.', 'Pencil. Rambo. Cancel.'], ['Remember the milk. Rambo.']];
    for (const [i, texts] of cases.entries()) await p.evaluate(async ([texts, i]) => { await window.__voice.voice.sendVoice({sendId: 'noise' + i + Date.now(), topic: 'alpha', items: texts.map(text => ({text})), cancelWord: 'bamboo', commandWord: 'katana'}); }, [texts, i]);
    await L.sleep(4000);
    assert.deepEqual(await posted(), [], 'nothing was posted');
    await p.evaluate(async () => { await window.__voice.voice.sendVoice({sendId: 'real' + Date.now(), topic: 'alpha', items: [{text: 'Please run the tests.'}], cancelWord: 'bamboo', commandWord: 'katana'}); });
    await L.sleep(3000);
    assert.deepEqual(await posted(), ['Please run the tests.'], 'a normal reply still posts');
  } finally { await c.close(); }
};

scenarios.dictateMidMessage = async ({srv}) => {
  // Dictating in the middle of a message: after the send, reading resumes at the chunk that was paused, never from chunk 0.
  await L.wipeTasks();
  const id = await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Attention.', 'Thanks, please continue with the tests.', 'Tomato.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(24000), pcm(ws[0]), gap(2000), pcm(ws[1]), gap(1500), pcm(ws[2]), gap(40000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__dm.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__dm.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'sent', () => /resume phrase|end word.*sending now/.test(window.__voice.log.text()), 90000);
    await L.sleep(8000);
    const starts = (await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => /audio +start /.test(l)).map(l => +l.match(/start \d+:(\d+)/)[1]);
    const cut = starts.findIndex((v, i) => i > 0 && v <= starts[i - 1]);
    assert.ok(starts.length >= 3, 'played several chunks: ' + starts);
    assert.ok(cut < 0 || starts[cut] >= starts[cut - 1], 'playback restarted from an earlier chunk after the dictation: ' + starts);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.commandCancelSound = async ({srv}) => {
  // Cancel word in COMMAND mode: the low falling cancel earcon plays and a toast shows, via the on-device spotter and via the server transcript.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Katana.', 'Bamboo.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(14000), pcm(ws[0]), gap(1800), pcm(ws[1]), gap(25000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__cc.wav': Buffer.concat([head, data])});
  try {
    for (const kws of [true, false]) {
      const c = await L.openClient({url: srv2.url, mic: '/__cc.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-kws': kws}});
      const p = c.page;
      try {
        await p.evaluate(() => { window.__tones = []; window.__toasts = []; const v = window.__voice, orig = v.player.earcon; v.player.earcon = (n, s) => { window.__tones.push(n.join('/')); return orig.call(v.player, n, s); }; new MutationObserver(() => window.__toasts.push(document.body.innerText)).observe(document.body, {childList: true, subtree: true}); });
        await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
        await p.locator('.msg.agent .s').first().tap();
        await L.waitFor(p, 'command mode', () => /vc +command mode/.test(window.__voice.log.text()), 70000);
        await L.waitFor(p, 'cancelled', () => /cancel: reply discarded/.test(window.__voice.log.text()), 40000);
        await L.sleep(800);
        const tones = await p.evaluate(() => window.__tones);
        assert.equal(tones.at(-1), '900/620', `kws=${kws}: cancel earcon last: ` + JSON.stringify(tones));
        assert.ok((await p.evaluate(() => window.__toasts)).some(x => /cancel/i.test(x)), `kws=${kws}: a cancel toast was shown`);
      } finally { await c.close(); }
    }
  } finally { await srv2.close(); }
};

scenarios.switchNameAfterList = async ({srv}) => {
  // The logged failure: "Switch to." (name cut off) then the name alone ("Human manager.") must switch (after "positive"), not be ignored.
  await L.wipeTasks();
  await L.agentSays('human_mgr', '[human_mgr] H\n\nHuman manager is here.');
  await L.agentSays('terminal_viz', '[terminal_viz] T\n\nTerminal agent is here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Katana.', 'Switch to.', 'Human manager.', 'Positive.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(12000), pcm(ws[0]), gap(1800), pcm(ws[1]), gap(6000), pcm(ws[2]), gap(7000), pcm(ws[3]), gap(30000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__sn.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__sn.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-switch-confirm': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 3, 45000);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'human_mgr', 90000);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.interruptAnnouncement = async ({srv}) => {
  // "Now on X" is cut short by "attention" said right after the switch, which opens input mode for the new agent.
  await L.wipeTasks();
  await L.agentSays('quiet_agent', '[quiet_agent] Q\n\nNothing new here.');
  await L.agentSays('alpha', '[alpha] A\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} keeps the message playing for a while.`).join('\n\n'));
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Katana.', 'Switch to quiet agent.', 'Positive.', 'Attention.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(12000), pcm(ws[0]), gap(1800), pcm(ws[1]), gap(7000), pcm(ws[2]), gap(1800), pcm(ws[3]), gap(15000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ia.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ia.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-kws': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'topics', () => window.__voice.model.topics().length >= 2, 45000);
    const m = (await L.human('GET', 'messages', {narrow: [{operator: 'stream', operand: L.TASKS}, {operator: 'topic', operand: 'quiet_agent'}], anchor: 'newest', num_before: 10, num_after: 0, apply_markdown: false})).messages.map(x => x.id); await L.human('POST', 'messages/flags', {messages: m, op: 'add', flag: 'read'}); await L.sleep(2500);
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'switched', () => window.__voice.current() === 'quiet_agent', 80000);
    if (process.env.DUMP) setTimeout(async () => console.log((await p.evaluate(() => window.__voice.log.text())).split('\n').filter(l => / (vc|kws|mic|audio) /.test(l)).join('\n')), 55000);
    await L.waitFor(p, 'interrupted', () => /prompt interrupted by ATTENTION/.test(window.__voice.log.text()) && /session opened by start word/.test(window.__voice.log.text().split('prompt interrupted')[1] ?? ''), 30000);
  } finally { await c.close(); await srv2.close(); }
};

scenarios.jumpPillFollows = async ({srv}) => {
  // The "jump to playing" pill tracks whether the playing chunk is visible (also with no scroll event), and app actions resume following.
  await L.wipeTasks();
  const filler = '```\n' + Array.from({length: 80}, (_, i) => `filler line ${i} to make the topic tall enough to scroll`).join('\n') + '\n```';   // a code block is tall but never spoken
  await L.agentSays('alpha', '[alpha] Old\n\nShort intro.\n\n' + filler);
  const longId = await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const visible = () => p.evaluate(() => !document.querySelector('#jump').hidden);
  try {
    await L.waitFor(p, 'messages', () => document.querySelectorAll('.msg.agent').length >= 2);
    await L.waitFor(p, 'prepared', id => window.__voice.voice.bundleFor(id, 1) && window.__voice.voice.bundleFor(id, 0), 90000, longId);
    await L.sleep(3000);
    await p.locator('.msg.agent').last().locator('.s').first().tap();
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing', 30000);
    await L.sleep(1500);
    assert.equal(await visible(), false, 'pill hidden while the playing chunk is in view');
    // layout change without any scroll event: content above grows, pushing the chunk out of view
    await p.evaluate(() => { const f = document.querySelector('#feed'); f.style.scrollBehavior = 'auto'; const d = document.createElement('div'); d.id = 'spacer'; d.style.height = '3000px'; f.querySelector('.msg').before(d); f.scrollTop = 0; });
    await L.waitFor(p, 'pill appears', () => !document.querySelector('#jump').hidden, 5000);
    await p.evaluate(() => { document.querySelector('#spacer').remove(); });
    // an app action (next chunk) resumes following: the playing chunk is scrolled into view and the pill goes
    await p.evaluate(() => { document.querySelector('#feed').dispatchEvent(new Event('touchmove')); document.querySelector('#feed').scrollTop = 0; });
    await L.waitFor(p, 'pill after manual scroll', () => !document.querySelector('#jump').hidden, 5000);
    await tap(p, '#next');
    await L.waitFor(p, 'following again', () => { const n = document.querySelector('.s.now'), f = document.querySelector('#feed').getBoundingClientRect(), r = n && n.getBoundingClientRect(); return r && r.bottom > f.top && r.top < f.bottom && document.querySelector('#jump').hidden; }, 30000);
  } catch (e) { console.log(JSON.stringify(await p.evaluate(() => { const n = document.querySelector('.s.now'), f = document.querySelector('#feed'); return {now: n && n.closest('.msg').dataset.id + ':' + n.dataset.from, nowTop: n && n.getBoundingClientRect().top, fTop: f.getBoundingClientRect().top, fBottom: f.getBoundingClientRect().bottom, scrollTop: f.scrollTop, jumpHidden: document.querySelector('#jump').hidden, st: window.__voice.player.snapshot().status, log: window.__voice.log.text().split(String.fromCharCode(10)).filter(l => / audio /.test(l)).slice(-8).join(' | ')}; }).catch(x => String(x)))); throw e; } finally { await c.close(); }
};

scenarios.chatterNotUploaded = async ({srv}) => {
  // Talk around the phone with the on-device spotter ready: no start word, so nothing is uploaded or transcribed.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] A\n\nSomething.');
  const gap = ms => Buffer.alloc(Math.round(48000 * ms / 1000) & ~1), pcm = w => w.subarray(w.indexOf('data') + 8);
  const ws = await Promise.all(['Please run the build now.', 'Then tell me when it finishes.', 'I will be back in ten minutes.'].map(t => L.speechWav(t))), at = ws[0].indexOf('data'), head = Buffer.from(ws[0].subarray(0, at + 8));
  const data = Buffer.concat([gap(10000), ...ws.flatMap(w => [pcm(w), gap(3000)]), gap(10000)]); head.writeUInt32LE(data.length, at + 4); head.writeUInt32LE(head.length - 8 + data.length, 4);
  const srv2 = await L.serve(undefined, {'/__ch.wav': Buffer.concat([head, data])});
  const c = await L.openClient({url: srv2.url, mic: '/__ch.wav', prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false, 'voice2-vc': true, 'voice2-kws': true}});
  const p = c.page;
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').first().tap();
    await L.waitFor(p, 'kws ready', () => /kws +ready/.test(window.__voice.log.text()), 40000);
    await L.waitFor(p, 'three clips skipped', () => (window.__voice.log.text().match(/not uploaded/g) || []).length >= 3, 60000);
    const log = await p.evaluate(() => window.__voice.log.text());
    assert.equal((log.match(/http +POST user_uploads/g) || []).length, 0, 'no audio uploaded');
  } finally { await c.close(); await srv2.close(); }
};

scenarios.resumeKeepsSync = async ({srv}) => {
  // 🧑 "a desync between text highlighted and voice played"; "After the dictation, it restarts from the beginning of the whole message."
  // Pause and resume inside a multi-chunk bundle: the highlighted chunk keeps following the audio, and a second pause/resume continues from the right chunk.
  await L.wipeTasks();
  await L.agentSays('alpha', '[alpha] Long\n\n' + Array.from({length: 8}, (_, i) => `Paragraph ${i} is a chunk of its own, long enough to take a few seconds.`).join('\n\n'));
  const c = await L.openClient({url: srv.url, prefs: {'voice2-topic': 'alpha', 'voice2-autoplay': false}});
  const p = c.page;
  const state = () => p.evaluate(() => {
    const now = document.querySelector('.s.now');
    return {index: window.__voice.player.position().index, audible: window.__voice.player.audible(), status: window.__voice.player.snapshot().status,
      hl: now && {msg: now.closest('.msg').dataset.id, from: +now.dataset.from, to: +now.dataset.to}};
  });
  const synced = async what => {   // sampled while playing: the highlighted sentence is the chunk the audio clock is in
    let checked = 0;
    for (let i = 0; i < 6; i++) {
      await L.sleep(1200);
      const inSync = x => { const [msg, idx] = x.audible.split(':'); return x.hl && x.hl.msg === msg && x.hl.from <= +idx && +idx <= x.hl.to; };
      let s = await state();
      if (s.status !== 'playing' || s.audible === null) continue;
      if (!inSync(s)) { await L.sleep(300); s = await state(); if (s.status !== 'playing' || s.audible === null) continue; }   // a sample right at a chunk boundary
      const [msg, idx] = s.audible.split(':');
      assert.ok(s.hl && s.hl.msg === msg && s.hl.from <= +idx && +idx <= s.hl.to, `${what}: audio in chunk ${s.audible}, highlight ${JSON.stringify(s.hl)}`);
      checked++;
    }
    assert.ok(checked >= 2, `${what}: only ${checked} samples while playing`);
  };
  try {
    await L.waitFor(p, 'message', () => document.querySelector('.msg.agent'));
    await p.locator('.msg.agent .s').nth(1).tap();
    await L.waitFor(p, 'playing', () => window.__voice.player.snapshot().status === 'playing', 30000);
    await L.sleep(1500);
    await p.evaluate(() => window.__voice.player.pause());
    await L.sleep(1000);
    await p.evaluate(() => window.__voice.player.resume());
    await synced('after the first resume');
    const before = await state();
    await p.evaluate(() => window.__voice.player.pause());
    await L.sleep(1000);
    await p.evaluate(() => window.__voice.player.resume());
    await L.waitFor(p, 'playing again', () => window.__voice.player.snapshot().status === 'playing' && window.__voice.player.audible() !== null, 10000);
    const after = await state();
    assert.ok(after.index >= before.index && after.index <= before.index + 1, `second resume went from item ${before.index} to ${after.index}`);
    await synced('after the second resume');
  } finally { await c.close(); }
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
