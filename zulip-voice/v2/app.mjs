/** Wires the model, Zulip events, voice service, player, recorder and DOM. Behavior is described in `../docs/v2/design.md`. */
import {createLog} from './log.mjs';
import {createZulip, createEvents} from './zulip.mjs';
import {createVoice} from './voice.mjs';
import {createPlayer} from './player.mjs';
import {createRecorder} from './recorder.mjs';
import {createModel, parsed, itemsOf} from './model.mjs';
import {agentEl, mineEl, setUnread, topicRow, ago} from './view.mjs';

const BUILD = 'v2.0';
const ACCOUNT_KEY = 'zulip-voice-account';
const $ = id => document.getElementById(id);
const store = {
  get(key, fallback) { try { const v = localStorage.getItem(key); return v === null ? fallback : JSON.parse(v); } catch { return fallback; } },
  set(key, value) { try { localStorage.setItem(key, JSON.stringify(value)); } catch { /* private mode */ } },
};
const log = createLog();
addEventListener('error', e => log('err', `${e.message} @${e.filename?.split('/').pop()}:${e.lineno}`));
addEventListener('unhandledrejection', e => log('err', `unhandled: ${e.reason?.message ?? e.reason}`));

// ---- account ----
const DEFAULT_SITE = 'https://shagent.zulipchat.com';

async function login() {
  const dialog = $('login');
  dialog.showModal();
  return new Promise(resolve => {
    $('login-form').onsubmit = async event => {
      event.preventDefault();
      const values = {};
      let section = '';
      for (const line of event.target.elements.zuliprc.value.split(/\r?\n/)) {
        const text = line.trim();
        if (text.startsWith('[')) { section = text; continue; }
        const match = text.match(/^(site|email|key)\s*=\s*(.+)$/);
        if (section === '[api]' && match) values[match[1]] = match[2].trim();
      }
      const fail = text => { $('login-error').textContent = text; };
      if (!values.site || !values.email || !values.key) return fail('Need [api] with site, email and key.');
      const site = new URL(values.site).origin;
      const zulip = createZulip({site, email: values.email, key: values.key}, log);
      const me = await zulip.call('GET', 'users/me');
      if (!me.ok) return fail(me.error);
      if (me.data.is_bot) return fail('Use the human account, not a bot.');
      const next = {site, email: values.email, key: values.key, humanId: me.data.user_id};
      store.set(ACCOUNT_KEY, next);
      dialog.close();
      resolve(next);
    };
  });
}

/** Fills in channel and agent ids missing from the stored account. */
async function completeAccount(cfg, zulip) {
  if (cfg.streamId && cfg.botId && cfg.relayStreamId) return cfg;
  const subs = await zulip.call('GET', 'users/me/subscriptions');
  if (!subs.ok) return cfg;
  const find = name => subs.data.subscriptions.find(s => s.name === name)?.stream_id;
  cfg.streamId ||= find('AMH');
  cfg.relayStreamId ||= find('voice-relay');
  if (!cfg.botId && cfg.streamId) {
    const recent = await zulip.call('GET', 'messages', {narrow: [{operator: 'stream', operand: cfg.streamId}], anchor: 'newest', num_before: 30, num_after: 0, apply_markdown: false});
    cfg.botId = recent.data?.messages.findLast(m => m.sender_id !== cfg.humanId)?.sender_id;
  }
  store.set(ACCOUNT_KEY, cfg);
  return cfg;
}

/** Keeps the private relay channel from ever notifying the human. */
async function ensureRelayMuted(zulip, relayStreamId) {
  const subs = await zulip.call('GET', 'users/me/subscriptions');
  const sub = subs.data?.subscriptions.find(s => s.stream_id === relayStreamId);
  if (!sub) { log('app', 'relay channel not subscribed'); return false; }
  // 🧑 "I'm still getting push&email notification for the ping channel and email for stt"
  // Posting auto-follows a topic, and followed topics notify even in a muted channel; muting each relay topic stops that.
  if (!store.get('voice2-relay-topics-muted', false)) {
    const muted = await Promise.all(['ping', 'stt', 'tts', 'log', 'ack'].map(topic => zulip.call('POST', 'user_topics', {stream_id: relayStreamId, topic, visibility_policy: 1})));
    if (muted.every(r => r.ok)) store.set('voice2-relay-topics-muted', true);
  }
  const props = ['push_notifications', 'desktop_notifications', 'audible_notifications', 'email_notifications', 'wildcard_mentions_notify'];
  if (sub.is_muted && props.every(p => sub[p] === false)) return true;
  log('app', 'muting relay channel notifications');
  const set = await zulip.call('POST', 'users/me/subscriptions/properties', {subscription_data: [{stream_id: relayStreamId, property: 'is_muted', value: true}, ...props.map(property => ({stream_id: relayStreamId, property, value: false}))]});
  return set.ok;
}

// ---- boot ----
let cfg = store.get(ACCOUNT_KEY, null) ?? await login();
if (new URL(cfg.site).protocol !== 'https:') throw Error('Zulip must use HTTPS');
const zulip = createZulip(cfg, log);
$('app').hidden = false;
cfg = await completeAccount(cfg, zulip);

const model = createModel(cfg);
const voice = createVoice({zulip, cfg, log});
let current = store.get('voice2-topic', null);
let autoplay = store.get('voice2-autoplay', true);
let rate = store.get('voice2-rate', 1);
let follow = true, nowMsgId = null, nowIdx = -1, hasEarlier = false, voiceState = {ok: null, text: 'checking'};
let pendingSwitch = null;
const locals = new Map();
const els = new Map();
const feed = $('feed');

const cache = store.get('voice2-cache', null);
if (cache) { model.restore(cache); current ??= cache.current; }

const player = createPlayer({
  log, voice,
  onItemStart(item) { log('audio', `start ${item.key}`); if (burst?.pending) return; store.set('voice2-pos', {topic: current, msgId: item.msgId, idx: item.idx}); const before = model.unreadAgentMessages(current).filter(m => m.id < item.msgId).map(m => m.id); if (before.length) markRead(before); nowMsgId = item.msgId; nowIdx = item.idx; highlight(); $('jump').hidden = follow; if (follow) scrollToNow(); setMediaSession(); },
  onItemEnd(item) {
    if (!item.last) return;
    store.set('voice2-pos', null);
    markRead([item.msgId]);
    if (pendingSwitch?.msgId === item.msgId) runSwitch();
  },
  onIdle() { nowMsgId = null; highlight(); $('jump').hidden = true; if (pendingSwitch) runSwitch(); maybeAutoplay(); },
  onStatus: renderTransport,
});
player.setRate(rate);

// ---- feed rendering ----
const atBottom = () => feed.scrollHeight - feed.scrollTop - feed.clientHeight < 140;

function renderFeed() {
  feed.replaceChildren();
  els.clear();
  hasEarlier = hasEarlier && !!current;
  if (hasEarlier) { const b = document.createElement('button'); b.className = 'notice'; b.id = 'earlier'; b.textContent = 'Load earlier messages'; b.onclick = loadEarlier; feed.append(b); }
  const messages = model.messages(current);
  if (!messages.length) { const d = document.createElement('div'); d.className = 'empty'; d.id = 'empty'; d.textContent = current ? 'No messages yet' : 'Choose an agent'; feed.append(d); }
  for (const m of messages) appendMessage(m);
  for (const l of locals.get(current) ?? []) feed.append(localEl(l));
  feed.scrollTop = feed.scrollHeight;
  $('agent-name').textContent = current ?? 'Choose agent';
  highlight();
}

function appendMessage(m) {
  feed.querySelector('#empty')?.remove();
  if (els.has(m.id)) return;
  const node = model.isAgent(m) ? agentEl(m) : mineEl(m.content, 'sent', {});
  node.dataset.id = m.id;
  els.set(m.id, node);
  if (model.isMine(m)) {   // the server copy replaces the local bubble in the same tick: never two copies on screen
    const local = (locals.get(m.subject) ?? []).find(l => l.localId === m.local_message_id || (l.text === m.content && l.state !== 'failed'));
    if (local) { local.node?.replaceWith(node); locals.set(m.subject, locals.get(m.subject).filter(x => x !== local)); return; }
  }
  // keep message order by id even when older messages arrive late (history or reconnect), so nothing looks re-sent
  const later = [...feed.querySelectorAll('.msg[data-id]')].find(n => +n.dataset.id > m.id);
  feed.insertBefore(node, later ?? feed.querySelector('[data-local]'));
}

function localEl(l) {
  const node = mineEl(l.text, l.state, {retry: () => deliver(l), discard: () => { locals.set(current, locals.get(current).filter(x => x !== l)); renderFeed(); }});
  node.dataset.local = l.localId;
  l.node = node;
  return node;
}

function highlight() {
  for (const n of feed.querySelectorAll('.s.now')) n.classList.remove('now');
  if (nowMsgId === null) return;
  const node = els.get(nowMsgId)?.querySelectorAll('.s');
  for (const s of node ?? []) if (+s.dataset.from <= nowIdx && nowIdx <= +s.dataset.to) s.classList.add('now');
}

function scrollToNow(force = false) {
  const target = feed.querySelector('.s.now');
  if (!target) return;
  const box = feed.getBoundingClientRect(), r = target.getBoundingClientRect();
  if (!force && r.top >= box.top + 60 && r.bottom <= box.bottom - 20) return;
  feed.scrollTo({top: feed.scrollTop + r.top - box.top - box.height * 0.3, behavior: force || Math.abs(r.top - box.top) > box.height * 3 ? 'auto' : 'smooth'});
}

function renderUnread() {
  for (const [id, node] of els) { const m = model.messages(current).find(x => x.id === id); if (m && model.isAgent(m)) setUnread(node, !m.flags?.includes('read')); }
  const other = model.topics().filter(t => t.name !== current).reduce((n, t) => n + t.unread, 0);
  $('agent-unread').hidden = !other;
  $('agent-unread').textContent = other;
}

// ---- data loading ----
const topicQuery = topic => [{operator: 'stream', operand: cfg.streamId}, {operator: 'topic', operand: topic}];

/** Loads recent messages of `topic`; returns the newly seen ones. */
async function loadTopic(topic) {
  const r = await zulip.call('GET', 'messages', {narrow: topicQuery(topic), anchor: 'newest', num_before: 40, num_after: 0, apply_markdown: false});
  if (!r.ok) return [];
  const fresh = r.data.messages.filter(m => model.add(m));
  model.markLoaded(topic);
  if (topic === current) { if (fresh.length === r.data.messages.length) hasEarlier = !r.data.found_oldest; for (const m of fresh) appendMessage(m); }
  return fresh;
}

async function loadEarlier() {
  const oldest = model.messages(current)[0];
  const r = await zulip.call('GET', 'messages', {narrow: topicQuery(current), anchor: oldest?.id ?? 'newest', num_before: 40, num_after: 0, include_anchor: false, apply_markdown: false});
  if (!r.ok) return toast(r.error);
  for (const m of r.data.messages) model.add(m);
  hasEarlier = !r.data.found_oldest;
  const before = feed.scrollHeight - feed.scrollTop;
  renderFeed();
  feed.scrollTop = feed.scrollHeight - before;
}

/** Latest messages of the whole channel, so the agent list can show a snippet per agent. */
async function loadRecent() {
  const r = await zulip.call('GET', 'messages', {narrow: [{operator: 'stream', operand: cfg.streamId}], anchor: 'newest', num_before: 60, num_after: 0, apply_markdown: false});
  if (r.ok) for (const m of r.data.messages) model.add(m);
}

async function loadTopics(unreadByTopic) {
  const r = await zulip.call('GET', `users/me/${cfg.streamId}/topics`);
  if (r.ok) model.setTopics(r.data.topics, unreadByTopic ?? new Map(model.topics().map(t => [t.name, t.unread])));
}

async function onInit(state, isResync) {
  if (cfg.relayStreamId) voice.backfill();
  const unread = new Map();
  for (const s of state.unread_msgs?.streams ?? []) if (s.stream_id === cfg.streamId) unread.set(s.topic, s.unread_message_ids.length);
  const topicsLoaded = loadTopics(unread);
  if (!current) { await topicsLoaded; current = model.topics()[0]?.name ?? null; }
  if (current) {
    const [fresh] = await Promise.all([loadTopic(current), topicsLoaded]);
    for (const m of fresh) if (isResync && model.isAgent(m)) onNewAgentMessage(m);
    if (!isResync) renderFeedKeepingScroll();
  }
  await loadRecent();
  if (current) catchUpReadMark(unread.get(current) ?? 0);
  renderUnread();
  maybeAutoplay();
  persist();
}

/** Forward-only read state on load: everything older than the newest read agent message is read (one call, also covers backlog beyond the loaded history). */
async function catchUpReadMark(serverUnread) {
  const agents = model.agentMessages(current);
  const newestRead = agents.filter(m => m.flags?.includes('read')).at(-1);
  if (!newestRead) return;
  const earlier = agents.filter(m => m.id < newestRead.id && !m.flags?.includes('read'));
  if (!earlier.length && serverUnread <= model.unreadAgentMessages(current).length) return;
  model.markRead(earlier.map(m => m.id));
  renderUnread();
  const r = await zulip.call('POST', 'messages/flags/narrow', {anchor: newestRead.id, num_before: 1000, num_after: 0, narrow: topicQuery(current), op: 'add', flag: 'read'});
  log('read', `watermark at ${newestRead.id}: ${r.ok ? 'ok' : r.error}`);
}

function renderFeedKeepingScroll() { const stick = atBottom(); if (!els.size || !feed.querySelector('.msg')) renderFeed(); if (stick) feed.scrollTop = feed.scrollHeight; $('agent-name').textContent = current ?? 'Choose agent'; }

// ---- events ----
function onEvent(event) {
  if (event.type === 'message') {
    const m = event.message;
    voice.onMessage(m);
    if (m.type === 'stream' && m.stream_id === cfg.streamId) onTaskMessage(m);
  } else if (event.type === 'update_message_flags' && event.flag === 'read' && event.op === 'add') {
    model.markRead(event.messages);
    renderUnread();
  }
}

function onTaskMessage(m) {
  if (!model.add(m)) return;
  const stick = atBottom();
  if (m.subject === current) {
    appendMessage(m);
    if (stick || model.isMine(m)) feed.scrollTop = feed.scrollHeight;
    if (model.isAgent(m)) onNewAgentMessage(m);
  }
  renderUnread();
  persist();
}

/** A new agent message in the open topic: queue it for autoplay and honour its `[switch: TASK]` line. */
function onNewAgentMessage(m) {
  const items = itemsOf(m);
  const p = parsed(m);
  if (p.switchTo) pendingSwitch = {to: p.switchTo, msgId: items.length ? m.id : null};
  if (!items.length) markRead([m.id]);
  else if (autoplay || player.position().status !== 'idle') {
    if (!player.isUnlocked()) showBanner('Tap to listen', 'tap', () => { player.unlock(); maybeAutoplay(); });
    else player.append(items);
  }
  if (pendingSwitch && !items.length && player.position().status === 'idle') runSwitch();
}

// ---- reading ----
const msgIndex = id => model.agentMessages(current).findIndex(m => m.id === id);

function playFrom(m, idx = 0) {
  setFollow(true);
  player.unlock();
  const later = model.agentMessages(current).filter(x => x.id > m.id);
  player.play([...itemsOf(m, idx), ...later.flatMap(x => itemsOf(x))], 0);
}

function maybeAutoplay() {
  if (!autoplay || !current || player.position().status !== 'idle') return;
  const unread = model.unreadAgentMessages(current);
  const speakable = unread.filter(m => itemsOf(m).length);
  markRead(unread.filter(m => !itemsOf(m).length).map(m => m.id));
  const pos = store.get('voice2-pos', null);
  const resumable = pos?.topic === current && model.find(pos.msgId) && itemsOf(model.find(pos.msgId), pos.idx).length;
  if (!speakable.length && !resumable) { hideBanner(); return; }
  if (!player.isUnlocked()) { showBanner(`Tap to listen (${speakable.length} unread)`, 'tap', () => { player.unlock(); maybeAutoplay(); }); return; }
  hideBanner();
  const at = pos?.topic === current ? model.find(pos.msgId) : null;
  if (at && itemsOf(at, pos.idx).length) {   // resume exactly where listening stopped, then continue forward
    player.play([...itemsOf(at, pos.idx), ...model.agentMessages(current).filter(m => m.id > at.id && !m.flags?.includes('read')).flatMap(m => itemsOf(m))], 0);
    return;
  }
  player.play(speakable.flatMap(m => itemsOf(m)), 0);
}

const RESTART_AFTER_S = 2;
const SKIP_DEBOUNCE_MS = 350;
let burst = null;   // taps within SKIP_DEBOUNCE_MS of each other: `{msgId, idx, pending, timer}`
/**
 * Steps one spoken chunk (one whole message when `whole`). The first tap plays at once; further taps in quick succession move the
 * highlighted target instantly and only the final target is loaded after the debounce. Previous restarts the chunk if it played >2 s.
 */
function jump(direction, whole = false) {
  const agents = model.agentMessages(current);
  const flat = agents.flatMap(m => itemsOf(m));
  if (!flat.length) return toast('No agent messages here');
  const from = burst ?? (nowMsgId === null ? null : {msgId: nowMsgId, idx: nowIdx});
  const at = from ? flat.findIndex(it => it.msgId === from.msgId && it.idx === from.idx) : -1;
  let target;
  if (whole) {
    const mi = from ? agents.findIndex(m => m.id === from.msgId) : -1;
    const m = agents[from ? mi + direction + (direction < 0 && from.idx > 0 && !burst ? 1 : 0) : (direction > 0 ? 0 : agents.length - 1)];
    target = m && itemsOf(m)[0];
  } else if (at < 0) target = direction > 0 ? flat[0] : flat.at(-1);
  else if (direction < 0 && !burst && player.currentTime() > RESTART_AFTER_S) target = flat[at];
  else target = flat[at + direction];
  if (!target) return toast(direction > 0 ? 'No later messages' : 'No earlier messages');
  const play = () => playFrom(agents.find(m => m.id === target.msgId), target.idx);
  if (burst) clearTimeout(burst.timer);
  const pending = !!burst;
  burst = {msgId: target.msgId, idx: target.idx, pending};
  if (pending) { nowMsgId = target.msgId; nowIdx = target.idx; highlight(); setFollow(true); scrollToNow(); } else play();
  burst.timer = setTimeout(() => { const b = burst; burst = null; if (b.pending) play(); }, SKIP_DEBOUNCE_MS);
}

function togglePlay() {
  const {status} = player.position();
  player.unlock();
  if (status === 'playing' || status === 'loading') player.pause();
  else if (status === 'paused') player.resume();
  else if (status === 'error' || status === 'blocked') player.retry();
  else {
    const agents = model.agentMessages(current);
    const start = model.unreadAgentMessages(current).find(m => itemsOf(m).length) ?? agents.findLast(m => itemsOf(m).length);
    if (start) playFrom(start, 0); else toast('Nothing to read');
  }
}

function renderTransport({status, error}) {
  $('play').dataset.state = status;
  $('play').setAttribute('aria-label', status === 'playing' ? 'Pause' : 'Play');
  if (status === 'error') showBanner(`Waiting for audio (${error}). Retrying by itself…`, 'warn', () => player.retry());
  else if (status === 'blocked') showBanner('Tap to resume audio', 'tap', () => player.unlock());
  else if (bannerKind === 'err') hideBanner();
  $('dock').classList.toggle('min', status === 'idle' && scrolledAway);
}

function setMediaSession() {
  if (!('mediaSession' in navigator)) return;
  navigator.mediaSession.metadata = new MediaMetadata({title: current ?? 'Voice', artist: 'Agent'});
  navigator.mediaSession.setActionHandler('play', () => togglePlay());
  navigator.mediaSession.setActionHandler('pause', () => player.pause());
  navigator.mediaSession.setActionHandler('nexttrack', () => jump(1));
  navigator.mediaSession.setActionHandler('previoustrack', () => jump(-1));
}

// ---- read flags ----
let readQueue = new Set(store.get('voice2-readq', []));
let flushing = false;
/** Read state only moves forward: reading message N also reads every earlier unread agent message of that topic. */
function markRead(ids) {
  if (!ids.length) return;
  const all = new Set(ids);
  for (const id of ids) {
    const m = model.find(id);
    if (m) for (const earlier of model.unreadAgentMessages(m.subject)) if (earlier.id < id) all.add(earlier.id);
  }
  ids = [...all];
  model.markRead(ids);
  for (const id of ids) readQueue.add(id);
  store.set('voice2-readq', [...readQueue]);
  renderUnread();
  flushRead();
}
async function flushRead() {
  if (flushing || !readQueue.size) return;
  flushing = true;
  const ids = [...readQueue];
  const r = await zulip.call('POST', 'messages/flags', {messages: ids, op: 'add', flag: 'read'});
  if (r.ok) { for (const id of ids) readQueue.delete(id); store.set('voice2-readq', [...readQueue]); }
  flushing = false;
}
setInterval(flushRead, 15000);

// ---- switching agents ----
function switchTopic(topic) {
  if (topic === current) { $('picker').close(); return; }
  player.stop();
  nowMsgId = null; pendingSwitch = null; hideBanner();
  saveDraft();
  current = topic;
  store.set('voice2-topic', topic);
  $('draft').value = store.get('voice2-draft:' + topic, '');
  autosize();
  hasEarlier = false;
  renderFeed();
  $('picker').close();
  loadTopic(topic).then(() => { if (current === topic) { renderFeed(); maybeAutoplay(); } });
  renderUnread();
}

function runSwitch() {
  const {to} = pendingSwitch;
  pendingSwitch = null;
  const target = model.hasTopic(to) ? to : null;
  if (!target) return toast(`Agent “${to}” not found`);
  const from = current;
  switchTopic(target);
  toast(`Switched to ${target}`, 'Back', () => switchTopic(from));
}

function openPicker() {
  const query = $('search').value.trim().toLowerCase();
  const list = $('topics');
  list.replaceChildren();
  for (const t of model.topics().filter(x => x.name.toLowerCase().includes(query)).slice(0, 80)) {
    const last = model.lastMessage(t.name);
    const snip = last ? `${ago(last.timestamp)} · ${(model.isAgent(last) ? parsed(last).body || parsed(last).title : 'You: ' + last.content).replace(/\s+/g, ' ').slice(0, 90)}` : '';
    list.append(topicRow(t, snip, t.name === current, () => switchTopic(t.name)));
  }
  if (!$('picker').open) $('picker').showModal();
}

// ---- composer: typing, dictation, sending ----
const draftEl = $('draft');
let dict = null;
let localCounter = 0;
function autosize() { draftEl.style.height = 'auto'; $('composer').classList.toggle('tall', !!dict || draftEl.value.length > 28 || draftEl.value.includes('\n')); draftEl.style.height = Math.min(draftEl.scrollHeight, innerHeight * 0.4) + 'px'; $('send').disabled = !dict && !draftEl.value.trim(); }
function saveDraft() { if (current) store.set('voice2-draft:' + current, draftEl.value); }
draftEl.addEventListener('input', () => { autosize(); saveDraft(); });
draftEl.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey && !matchMedia('(pointer: coarse)').matches) { e.preventDefault(); $('composer').requestSubmit(); } });
draftEl.addEventListener('focus', () => setTimeout(() => { if (atBottom()) feed.scrollTop = feed.scrollHeight; }, 300));

function joinText(base, parts) { return [base.trim(), ...parts].filter(Boolean).join(' '); }
function syncDictation() {
  const done = dict.parts.filter(p => p.state === 'done').map(p => p.text);
  const pending = dict.parts.some(p => p.state === 'pending');
  draftEl.value = joinText(dict.base, done) + (pending ? ' …' : '');
  autosize();
  draftEl.scrollTop = draftEl.scrollHeight;
}

async function startDictation() {
  player.unlock();
  if (voiceState.ok === false) toast('Voice service is not responding; dictation may fail');
  const wasPlaying = ['playing', 'loading'].includes(player.position().status);
  dict = {base: draftEl.value, parts: [], stopping: false, sendAfter: false, cancelled: false, wasPlaying};
  const r = await recorder.start();
  if (!r.ok) { dict = null; return toast(r.error); }
  if (wasPlaying) player.pause();
  $('composer').classList.add('rec'); $('cancel-rec').hidden = false; $('level').hidden = false;
  draftEl.readOnly = true; $('send').disabled = false; $('mic').setAttribute('aria-label', 'Stop dictation');
  $('send').setAttribute('aria-label', 'Finish and send');
}

function onClip(blob) {
  if (!dict || dict.cancelled) return;
  const part = {state: 'pending', text: '', blob};
  dict.parts.push(part);
  transcribe(part, dict);
  syncDictation();
}

/** Transcribes `part`, retrying with backoff while the network is bad; the clip stays pending (shown as `…`) meanwhile. */
function transcribe(part, owner) {
  part.state = 'pending';
  (async () => {
    let r;
    for (let attempt = 0; attempt < 12; attempt++) {
      r = await voice.stt(part.blob);
      if (r.ok) break;
      log('stt', `clip attempt ${attempt + 1} failed: ${r.error}`);
      for (let waited = 0; waited < 20000 && events.info().state !== 'live'; waited += 500) await new Promise(res => setTimeout(res, 500));
      await new Promise(res => setTimeout(res, Math.min(1000 * (attempt + 1), 8000)));
    }
    return r;
  })().then(r => {
    part.state = r.ok ? 'done' : 'failed';
    part.text = r.ok ? r.text : '';
    if (!r.ok) log('stt', `clip failed: ${r.error}`);
    if (owner === dict) { syncDictation(); maybeFinish(); } else if (r.ok) { draftEl.value = joinText(draftEl.value, [r.text]); autosize(); saveDraft(); }
    showFailedClips(owner);
  });
}

function showFailedClips(owner) {
  const failed = owner.parts.filter(p => p.state === 'failed');
  if (failed.length) showBanner(`${failed.length} dictation clip${failed.length > 1 ? 's' : ''} not transcribed. Tap to retry`, 'warn', () => { for (const p of failed) transcribe(p, owner); hideBanner(); });
}

function stopDictation({send, cancel}) {
  if (!dict || dict.stopping) return;
  dict.stopping = true; dict.sendAfter = send; dict.cancelled = !!cancel;
  recorder.stop();
  if (cancel) draftEl.value = dict.base;
  maybeFinish();
}

function maybeFinish() {
  if (!dict?.stopping || dict.parts.some(p => p.state === 'pending')) return;
  const finished = dict;
  $('composer').classList.remove('rec'); $('cancel-rec').hidden = true; $('level').hidden = true;
  draftEl.readOnly = false; $('mic').setAttribute('aria-label', 'Dictate'); $('send').setAttribute('aria-label', 'Send');
  dict = null;
  autosize(); saveDraft();
  if (finished.parts.some(p => p.state === 'failed')) { dict = null; showFailedClips(finished); finished.parts = finished.parts.filter(p => p.state === 'failed'); }
  // Never send with a clip missing: a failed clip keeps the draft and offers a retry.
  if (finished.sendAfter && draftEl.value.trim() && !finished.parts.some(p => p.state === 'failed')) send();
  if (finished.wasPlaying) player.resume();
}

const recorder = createRecorder({log, onClip, onLevel: drawLevel});
function drawLevel(level) {
  const canvas = $('level'), ctx = canvas.getContext('2d');
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue('--danger') || '#c00';
  const bars = 10, w = canvas.width / bars;
  for (let i = 0; i < bars; i++) { const h = Math.max(3, canvas.height * Math.min(1, level * (0.5 + Math.sin(i * 1.7 + performance.now() / 120) * 0.5 + 0.4))); ctx.fillRect(i * w + 2, (canvas.height - h) / 2, w - 4, h); }
}

$('mic').onclick = () => { if (dict) stopDictation({send: false}); else startDictation(); };
$('cancel-rec').onclick = () => stopDictation({cancel: true});
$('composer').onsubmit = event => {
  event.preventDefault();
  if (dict) { stopDictation({send: true}); return; }
  send();
};

function send() {
  const text = draftEl.value.trim();
  if (!text || !current) return;
  draftEl.value = '';
  saveDraft(); autosize();
  const local = {localId: `l${Date.now()}_${localCounter++}`, topic: current, text, state: 'sending', sentAtMs: Date.now()};
  locals.set(current, [...(locals.get(current) ?? []), local]);
  feed.querySelector('#empty')?.remove();
  feed.append(localEl(local));
  feed.scrollTop = feed.scrollHeight;
  deliver(local);
}

function removeLocal(l) { locals.set(l.topic, (locals.get(l.topic) ?? []).filter(x => x !== l)); l.node?.remove(); }
function refreshLocal(l) { if (l.topic !== current) return; const old = l.node; const fresh = localEl(l); old?.replaceWith(fresh); }

/** Sends `l`; failures requeue it with backoff (and at once when the connection returns) so it never needs a manual retry unless 8 attempts failed. */
async function deliver(l) {
  if (l.inflight) return;
  l.inflight = true; l.state = 'sending'; refreshLocal(l);
  try {
    if (l.uncertain) {
      const found = await zulip.call('GET', 'messages', {narrow: topicQuery(l.topic), anchor: 'newest', num_before: 5, num_after: 0, apply_markdown: false});
      if (found.ok && found.data.messages.some(m => m.sender_id === cfg.humanId && m.content === l.text && m.timestamp * 1000 > l.sentAtMs - 5000)) { log('send', 'uncertain send was delivered'); removeLocal(l); for (const m of found.data.messages) if (model.add(m) && l.topic === current) appendMessage(m); return; }
    }
    const r = await zulip.call('POST', 'messages', {type: 'stream', to: cfg.streamId, topic: l.topic, content: l.text, queue_id: events.queueId() ?? '', local_id: l.localId});
    if (r.ok) { l.state = 'sent'; l.sentId = r.data.id; setTimeout(() => { if ((locals.get(l.topic) ?? []).includes(l)) removeLocal(l); }, 4000); l.node?.querySelector('.state')?.replaceChildren(); l.node?.classList.remove('sending'); return; }
    l.uncertain = r.uncertain;
    l.attempts = (l.attempts ?? 0) + 1;
    if (l.attempts <= 8) { l.state = 'queued'; refreshLocal(l); setTimeout(() => { if (l.state === 'queued') deliver(l); }, Math.min(1500 * 2 ** l.attempts, 15000)); }
    else { l.state = 'failed'; refreshLocal(l); toast(r.error); }
  } finally { l.inflight = false; }
}

// ---- UI chrome ----
let toastTimer = 0;
function toast(text, action, onAction) {
  const t = $('toast');
  t.replaceChildren(text);
  if (action) { const b = document.createElement('button'); b.textContent = action; b.onclick = () => { t.hidden = true; onAction(); }; t.append(b); }
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, action ? 7000 : 3200);
}
let bannerAction = null;
let bannerKind = '';
function showBanner(text, kind, onTap) { const b = $('banner'); b.textContent = text; b.className = kind; bannerKind = kind; b.hidden = false; bannerAction = onTap; }
function hideBanner() { $('banner').hidden = true; bannerAction = null; }
$('banner').onclick = () => bannerAction?.();

function renderRate() { $('rate').textContent = rate.toFixed(1) + '×'; $('slower').disabled = rate <= 0.5; $('faster').disabled = rate >= 3; $('set-rate').value = rate; $('set-rate-text').textContent = rate.toFixed(1) + '×'; }
function setRate(value) { rate = Math.round(Math.min(3, Math.max(0.5, value)) * 10) / 10; player.setRate(rate); store.set('voice2-rate', rate); renderRate(); }
function renderAuto() { $('auto').setAttribute('aria-pressed', String(autoplay)); $('set-auto').checked = autoplay; }
function setAutoplay(value) { autoplay = value; store.set('voice2-autoplay', value); renderAuto(); if (value) maybeAutoplay(); else if (player.position().status === 'idle') hideBanner(); }

let lastScroll = 0, scrolledAway = false;
feed.addEventListener('scroll', () => {
  const top = feed.scrollTop, delta = top - lastScroll;
  if (Math.abs(delta) > 6) { scrolledAway = delta > 0 && top > 80 && !atBottom(); $('bar').classList.toggle('away', scrolledAway); $('dock').classList.toggle('min', scrolledAway && player.position().status === 'idle'); lastScroll = top; }
}, {passive: true});
/** A manual scroll while audio plays suspends following and offers a pill to resume. */
const setFollow = on => { follow = on; $('jump').hidden = on || nowMsgId === null; };
feed.addEventListener('touchmove', () => { if (nowMsgId !== null) setFollow(false); }, {passive: true});
feed.addEventListener('wheel', () => { if (nowMsgId !== null) setFollow(false); }, {passive: true});
let userScrollMs = 0, scrollPastTimer = 0;
for (const type of ['touchmove', 'wheel']) feed.addEventListener(type, () => { userScrollMs = Date.now(); }, {passive: true});
feed.addEventListener('scroll', () => {
  clearTimeout(scrollPastTimer);
  if (Date.now() - userScrollMs > 1500) return;
  scrollPastTimer = setTimeout(() => {   // an agent message fully above the viewport was scrolled past by hand: it counts as read
    const top = feed.getBoundingClientRect().top;
    const past = [...feed.querySelectorAll('.msg.agent.unread')].filter(n => n.getBoundingClientRect().bottom < top).map(n => +n.dataset.id);
    if (past.length) markRead(past);
  }, 700);
}, {passive: true});
$('jump').onclick = () => { setFollow(true); scrollToNow(true); };
feed.addEventListener('click', event => {
  const node = event.target.closest('.msg.agent');
  if (!node || event.target.closest('a')) return;
  const m = model.messages(current).find(x => x.id === +node.dataset.id);
  const span = event.target.closest('.s');
  playFrom(m, span ? +span.dataset.from : 0);
});
// iOS only treats touchend/click as audio-unlocking gestures (not pointerdown); autoplay starts after the tap's own handlers ran.
for (const type of ['touchend', 'click']) addEventListener(type, () => { if (!player.isUnlocked()) { player.unlock(); setTimeout(maybeAutoplay); } }, {capture: true});

$('agent').onclick = () => { $('search').value = ''; openPicker(); };
$('search').oninput = openPicker;
$('play').onclick = togglePlay;
/** Tap steps one chunk; holding for 0.5 s steps a whole message. */
for (const [id, direction] of [['prev', -1], ['next', 1]]) {
  let timer = 0, held = false;
  const button = $(id);
  button.addEventListener('pointerdown', () => { held = false; timer = setTimeout(() => { held = true; jump(direction, true); }, 500); });
  for (const type of ['pointerup', 'pointerleave', 'pointercancel']) button.addEventListener(type, () => clearTimeout(timer));
  button.onclick = () => { if (!held) jump(direction); held = false; };
}
$('slower').onclick = () => setRate(rate - 0.1);
$('faster').onclick = () => setRate(rate + 0.1);
$('auto').onclick = () => setAutoplay(!autoplay);
$('set-auto').onchange = e => setAutoplay(e.target.checked);
$('set-rate').oninput = e => setRate(+e.target.value);
$('settings').onclick = () => { $('account-text').textContent = `${cfg.email} · ${cfg.site} · ${BUILD}`; $('settings-sheet').showModal(); };
$('reload').onclick = () => location.reload();
$('sign-out').onclick = () => { if (confirm('Sign out of this account?')) { try { localStorage.removeItem(ACCOUNT_KEY); } catch { /* ignore */ } location.reload(); } };
for (const dialog of document.querySelectorAll('dialog')) {
  dialog.addEventListener('click', e => { if (e.target === dialog || e.target.closest('[data-close]')) dialog.close(); });
}

// ---- connection and debug ----
let lastState = 'connecting', lastAutoLogMs = 0;
function renderConn(info) {
  // After an outage the timeline is the evidence; send it once (at most every 2 min).
  if (info.state === 'live' && lastState !== 'live' && lastState !== 'connecting' && Date.now() - lastAutoLogMs > 120000 && cfg.relayStreamId) { lastAutoLogMs = Date.now(); sendLog('reconnected after ' + lastState); }
  lastState = info.state;
  const down = info.state === 'retrying' || info.state === 'offline';
  $('net').hidden = !down && !zulip.rateLimitedForMs();
  $('net').textContent = zulip.rateLimitedForMs() ? `Zulip is rate limiting this account; resuming in ${Math.ceil(zulip.rateLimitedForMs() / 1000)}s.` : 'No connection. Messages and audio will resume by themselves.';
  if (info.state === 'live') for (const list of locals.values()) for (const l of list) if (l.state === 'queued' || l.state === 'failed') { l.attempts = 0; deliver(l); }
  $('conn').dataset.state = info.state;
  $('conn-text').textContent = info.state === 'live' ? '' : info.state === 'offline' ? 'Offline' : 'Reconnecting…';
  if (document.querySelector('#debug-sheet[open]')) renderDebug();
}

function debugStatus() {
  const info = events.info();
  return [
    `build ${BUILD}  ${new Date().toISOString()}`,
    `page: ${document.visibilityState}, navigator.onLine=${navigator.onLine}`,
    `events: ${info.state} queue=${info.queueId} lastOk=${info.lastOkAgoMs ?? '-'}ms fails=${info.failures} poll=${info.pollAgeMs ?? '-'}ms`,
    `voice: ${voiceState.text}; prepared=${voice.preparedCount()} pending=${voice.pendingCount()}`,
    `player: ${JSON.stringify(player.snapshot())}`,
    `api: ${zulip.callsLastMinute()} calls in the last minute${zulip.rateLimitedForMs() ? '; RATE LIMITED for ' + Math.ceil(zulip.rateLimitedForMs() / 1000) + 's' : ''}`,
    `view: topic=${current} msgs=${model.messages(current).length} unread=${model.unreadAgentMessages(current).length} autoplay=${autoplay} readQueue=${readQueue.size}`,
  ].join('\n');
}
function renderDebug() {
  $('debug-status').textContent = debugStatus();
  const logEl = $('debug-log');
  const stick = logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight < 40;
  logEl.textContent = log.text();
  if (stick) logEl.scrollTop = logEl.scrollHeight;
}
$('conn').onclick = () => { $('debug-sheet').showModal(); renderDebug(); };
$('open-debug').onclick = () => { $('settings-sheet').close(); $('conn').click(); };
setInterval(() => { if (document.querySelector('#debug-sheet[open]')) renderDebug(); }, 1000);
$('dbg-copy').onclick = async () => {
  const text = debugStatus() + '\n\n' + log.text();
  try { await navigator.clipboard.writeText(text); toast('Copied'); }
  catch { const range = document.createRange(); range.selectNodeContents($('debug-log')); getSelection().removeAllRanges(); getSelection().addRange(range); toast('Select-all done; use Copy in the menu'); }
};
async function sendLog(label) { const r = await voice.sendLog(label, debugStatus() + '\n' + log.text()); log('app', `log sent (${label}): ${r.ok ? 'ok' : r.error}`); return r; }
$('dbg-send').onclick = async () => toast((await sendLog('manual')).ok ? 'Sent to server' : 'Send failed');
$('dbg-clear').onclick = () => { log.clear(); renderDebug(); };
$('dbg-kick').onclick = () => events.kick('manual');
$('dbg-ping').onclick = async () => { const r = await checkVoice(); toast(r.text); renderDebug(); };

async function checkVoice() {
  const r = await voice.ping();
  voiceState = r.ok ? {ok: true, text: `ok rtt=${r.totalMs}ms stt=${r.workers?.stt} tts=${r.workers?.tts} active=${r.active} up=${r.uptimeS}s`} : {ok: false, text: `NOT responding: ${r.error}`};
  log('voice', voiceState.text);
  return voiceState;
}

function persist() { clearTimeout(persist.t); persist.t = setTimeout(() => store.set('voice2-cache', model.snapshot(current)), 800); }

const events = createEvents({zulip, log, eventTypes: ['message', 'update_message_flags'], onInit, onEvent, onState: renderConn});

// ---- start ----
renderRate(); renderAuto();
if (current) { draftEl.value = store.get('voice2-draft:' + current, ''); renderFeed(); renderUnread(); }
autosize();
renderConn(events.info());
events.start();
if (cfg.relayStreamId) {
  ensureRelayMuted(zulip, cfg.relayStreamId);
  setTimeout(checkVoice, 1500);
} else voiceState = {ok: false, text: 'voice-relay channel not found'};
window.__voice = {model, player, events, voice, zulip, log, cfg: () => ({...cfg, key: '…'}), current: () => current};
