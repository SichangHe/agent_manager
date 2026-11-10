/** Wires the model, Zulip events, voice service, player, recorder and DOM. Behavior is described in `../docs/v2/design.md`. */
import {createLog} from './log.mjs';
import {createLatency} from './latency.mjs';
import {createKeywordSpotter} from './kws.mjs';
import {createZulip, createEvents} from './zulip.mjs';
import {createVoice} from './voice.mjs';
import {createPlayer} from './player.mjs';
import {createRecorder, EARCON_NOTES, EARCON_NOTE_S} from './recorder.mjs';
import {looksLikeClippedEndWord, soundsLikeWord, soundsLike, removeControlWords, stripTrailingControl, leadingControlCount, DOMAIN_PROMPT} from './text.mjs';
import {createModel, parsed, itemsOf} from './model.mjs';
import {hideAllAttachments, agentEl, mineEl, setUnread, topicRow, ago} from './view.mjs';

const BUILD = 'v2.0';
const ACCOUNT_KEY = 'zulip-voice-account';
const $ = id => document.getElementById(id);
const store = {
  get(key, fallback) { try { const v = localStorage.getItem(key); return v === null ? fallback : JSON.parse(v); } catch { return fallback; } },
  set(key, value) { try { localStorage.setItem(key, JSON.stringify(value)); } catch { /* private mode */ } },
};
const log = createLog();
const lat = createLatency(log);
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
  if (!store.get('voice2-relay-topics-muted-2', false)) {
    const muted = await Promise.all(['ping', 'stt', 'tts', 'log', 'ack', 'send', 'result'].map(topic => zulip.call('POST', 'user_topics', {stream_id: relayStreamId, topic, visibility_policy: 1})));
    if (muted.every(r => r.ok)) store.set('voice2-relay-topics-muted-2', true);
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
/** Directive messages already acted on (persisted): each `[switch:]` fires at most once ever, so returning to the topic never replays it. */
const actedKey = 'voice2-acted';
const acted = new Set(store.get(actedKey, []));
const hasActed = id => id !== null && acted.has(id);
function markActed(id) { if (id === null) return; acted.add(id); store.set(actedKey, [...acted].slice(-400)); }
const locals = new Map();
const els = new Map();
const feed = $('feed');

// ---- channels: AMH plus the other subscribed channels the user includes (Settings). A non-AMH topic's key is "channel · topic". ----
const channelNames = new Map(Object.entries(store.get('voice2-subs', {})).map(([id, n]) => [Number(id), n]));   // stream id -> name, from the subscriptions (cached)
const keyInfo = new Map();   // topic key -> {streamId, topic}
const isOurChannel = id => id !== cfg.streamId && id !== cfg.relayStreamId && channelNames.has(id) && includedChannels().includes(id);
/** Subscribed channels offered by default: everything except AMH, the relay and test channels. */
const mutedChannels = new Set(store.get('voice2-muted-subs', []));
const defaultChannels = () => [...channelNames].filter(([id, n]) => !mutedChannels.has(id) && id !== cfg.streamId && id !== cfg.relayStreamId && !/^(voice-|vtest)/i.test(n)).map(([id]) => id);
const includedChannels = () => store.get('voice2-channels', null) ?? defaultChannels();   // no UI: only tests pin it   // every subscribed, non-muted channel except the relay and test ones: all topics in one recency-sorted list
const keyFor = (streamId, topic) => { if (streamId === cfg.streamId) return topic; const key = `${channelNames.get(streamId)} · ${topic}`; keyInfo.set(key, {streamId, topic}); return key; };
/** A message with its topic replaced by the key (the model, caches and the UI only know keys). */
const ing = m => m.stream_id === cfg.streamId || m.t0 !== undefined ? m : {...m, t0: m.subject, subject: keyFor(m.stream_id, m.subject)};
const streamOf = key => keyInfo.get(key)?.streamId ?? cfg.streamId, topicOf = key => keyInfo.get(key)?.topic ?? key;

const cache = store.get('voice2-cache', null);
if (cache) { for (const list of Object.values(cache.msgs ?? {})) for (const m of list) if (m.t0 !== undefined) keyInfo.set(m.subject, {streamId: m.stream_id, topic: m.t0}); model.restore(cache); current ??= cache.current; }

const player = createPlayer({
  log, voice,
  onItemStart(item) { log('audio', `start ${item.key}`); lat.finish('play:' + item.msgId, 'ready+playing'); if (burst?.pending) return; store.set('voice2-pos', {topic: current, msgId: item.msgId, idx: item.idx}); store.set('voice2-last', {topic: current, msgId: item.msgId, idx: item.idx}); if (after(item, watermark())) store.set(wmKey(current), {msgId: item.msgId, idx: item.idx}); pausedItem = {topic: current, msgId: item.msgId, idx: item.idx}; const before = model.unreadAgentMessages(current).filter(m => m.id < item.msgId).map(m => m.id); if (before.length) markRead(before); nowMsgId = item.msgId; nowIdx = item.idx; highlight(); $('jump').hidden = follow; if (follow) scrollToNow(); setMediaSession(item); },
  onItemEnd(item) {
    if (!item.last) return;
    store.set('voice2-pos', null); if (pausedItem?.msgId === item.msgId) pausedItem = null;
    const played = model.find(item.msgId), wasUnread = played && !played.flags?.includes('read');
    if (wasUnread && !pendingSwitch && parsed(played).switchTo && !hasActed(item.msgId)) pendingSwitch = {to: parsed(played).switchTo, msgId: item.msgId};   // also for messages played from the backlog after a reload
    markRead([item.msgId]);
    if (pendingSwitch?.msgId === item.msgId) runSwitch();
  },
  onIdle() { store.set('voice2-pos', null); nowMsgId = null; highlight(); $('jump').hidden = true; if (pendingSwitch && (pendingSwitch.msgId === null || model.find(pendingSwitch.msgId)?.flags?.includes('read'))) runSwitch(); else if (pendingSwitch) pendingSwitch = null; /* an unplayed message's directive does not fire */ maybeAutoplay(); },
  onStatus: renderTransport, onInterrupt: reason => interrupted(reason),
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
  if (!messages.length) { const d = document.createElement('div'); d.className = 'empty'; d.id = 'empty'; d.textContent = !current ? 'Choose an agent' : loading === current ? 'Loading…' : 'No messages yet'; feed.append(d); }
  for (const m of messages) appendMessage(m);
  for (const l of locals.get(current) ?? []) feed.append(localEl(l));
  pinBottom();
  $('agent-name').textContent = current ?? 'Choose agent';
  highlight();
}

/** Keeps the feed at its end while layout settles (fonts, images, late renders), until the user scrolls. Never leaves scrollTop past the content. */
let pinUntilMs = 0, pinTimer = 0;
function pinBottom() {
  const end = () => { feed.scrollTop = Math.max(0, feed.scrollHeight - feed.clientHeight); };
  end();
  pinUntilMs = Date.now() + 2000;
  cancelAnimationFrame(pinTimer);
  const tick = () => { if (Date.now() < pinUntilMs && Date.now() - userScrollMs > 300) { end(); pinTimer = requestAnimationFrame(tick); } };
  pinTimer = requestAnimationFrame(tick);
}

/** Signed temporary link for an upload (one call, cached for 50 s). */
const attCache = new Map();
async function attUrl(uri) {
  const hit = attCache.get(uri);
  if (hit && Date.now() - hit.at < 50000) return hit.r;
  const z = await zulip.call('GET', uri.slice(1));
  const r = z.ok ? {ok: true, url: new URL(z.data.url, cfg.site).href} : {ok: false};
  if (r.ok) attCache.set(uri, {at: Date.now(), r});
  return r;
}

function appendMessage(m) {
  feed.querySelector('#empty')?.remove();
  if (els.has(m.id)) return;
  const node = model.isAgent(m) ? agentEl(m, attUrl) : mineEl(m.content, 'sent', {}, attUrl);
  node.dataset.id = m.id;
  els.set(m.id, node);
  if (model.isMine(m)) {   // the server copy replaces the local bubble in the same tick: never two copies on screen
    const local = (locals.get(m.subject) ?? []).find(l => l.localId === m.local_message_id || (l.text === m.content && l.state !== 'failed'));
    if (local) { lat.finish('send:' + local.localId, 'echo-visible'); local.node?.replaceWith(node); locals.set(m.subject, locals.get(m.subject).filter(x => x !== local)); return; }
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
const topicQuery = key => [{operator: 'stream', operand: streamOf(key)}, {operator: 'topic', operand: topicOf(key)}];

/** Loads recent messages of `topic`; returns the newly seen ones. */
async function loadTopic(topic) {
  const r = await zulip.call('GET', 'messages', {narrow: topicQuery(topic), anchor: 'newest', num_before: 40, num_after: 0, apply_markdown: false});
  if (!r.ok) return [];
  const fresh = r.data.messages.map(ing).filter(m => model.add(m));
  model.markLoaded(topic);
  if (topic === current) { if (fresh.length === r.data.messages.length) hasEarlier = !r.data.found_oldest; for (const m of fresh) appendMessage(m); }
  return fresh;
}

async function loadEarlier() {
  const oldest = model.messages(current)[0];
  const r = await zulip.call('GET', 'messages', {narrow: topicQuery(current), anchor: oldest?.id ?? 'newest', num_before: 40, num_after: 0, include_anchor: false, apply_markdown: false});
  if (!r.ok) return toast(r.error);
  for (const m of r.data.messages) model.add(ing(m));
  hasEarlier = !r.data.found_oldest;
  const before = feed.scrollHeight - feed.scrollTop;
  renderFeed();
  feed.scrollTop = feed.scrollHeight - before;
}

/** Latest messages of the whole channel, so the agent list can show a snippet per agent. */
async function loadRecent() {
  const r = await zulip.call('GET', 'messages', {narrow: [{operator: 'stream', operand: cfg.streamId}], anchor: 'newest', num_before: 800, num_after: 0, apply_markdown: false});   // one call buckets every topic, so a switch never waits for the network
  if (r.ok) for (const m of r.data.messages) model.add(m);
  for (const id of includedChannels()) {   // one call per extra channel buckets all its topics
    if (!channelNames.has(id)) continue;
    const x = await zulip.call('GET', 'messages', {narrow: [{operator: 'stream', operand: id}], anchor: 'newest', num_before: 300, num_after: 0, apply_markdown: false});
    if (x.ok) for (const m of x.data.messages) model.add(ing(m));
  }
}

async function loadChannels() {
  const r = await zulip.call('GET', 'users/me/subscriptions');
  if (!r.ok) return;
  channelNames.clear();
  mutedChannels.clear();
  for (const sub of r.data.subscriptions) { channelNames.set(sub.stream_id, sub.name); if (sub.is_muted) mutedChannels.add(sub.stream_id); }
  store.set('voice2-subs', Object.fromEntries(channelNames)); store.set('voice2-muted-subs', [...mutedChannels]);
}
async function loadTopics(unreadByTopic) {
  const r = await zulip.call('GET', `users/me/${cfg.streamId}/topics`);
  const list = r.ok ? [...r.data.topics] : [];
  for (const id of includedChannels()) {
    if (!channelNames.has(id)) continue;
    const x = await zulip.call('GET', `users/me/${id}/topics`);
    if (x.ok) for (const t of x.data.topics) list.push({name: keyFor(id, t.name), max_id: t.max_id});
  }
  if (r.ok) model.setTopics(list, unreadByTopic ?? new Map(model.topics().map(t => [t.name, t.unread])));
}

async function onInit(state, isResync) {
  if (cfg.relayStreamId) voice.backfill();
  const unread = new Map();
  await loadChannels();
  for (const s of state.unread_msgs?.streams ?? []) if (s.stream_id === cfg.streamId || isOurChannel(s.stream_id)) unread.set(keyFor(s.stream_id, s.topic), s.unread_message_ids.length);
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
    if (m.type === 'stream' && (m.stream_id === cfg.streamId || isOurChannel(m.stream_id))) onTaskMessage(ing(m));
  } else if (event.type === 'delete_message') {
    for (const id of event.message_ids ?? [event.message_id]) onMessageDeleted(id);
  } else if (event.type === 'update_message') {
    if (event.content !== undefined) onMessageEdited(event.message_id, event.content);
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
/** One gate: manual dictation, an open voice-control reply, or an utterance being captured or transcribed. No playback starts while it is true. */
let pendingChoice = null;   // a switch waiting for yes/no/number: `{names, until, confirm}`
let utterancePending = false, utteranceTimer = 0;   // set on speech detection, cleared when its clip is handled (or after 15 s if no clip comes)
const userSpeaking = () => !!dict || !!vc || utterancePending || Date.now() < announcingUntil || (!!pendingChoice && Date.now() < pendingChoice.until);   // also while a switch read-back waits for yes/no/number
const utteranceDone = () => { utterancePending = false; clearTimeout(utteranceTimer); if (!userSpeaking()) maybeAutoplay(); };

function onNewAgentMessage(m) {
  if (takeReplacement(m)) return;
  lat.begin('play:' + m.id, 'playback', performance.now() - Math.max(0, Date.now() - m.timestamp * 1000));   // starts at the server's timestamp
  lat.stage('play:' + m.id, 'to-client');
  const items = itemsOf(m);
  const p = parsed(m);
  if (p.switchTo && !hasActed(m.id) && !m.flags?.includes('read')) pendingSwitch = {to: p.switchTo, msgId: items.length ? m.id : null, sourceId: m.id};
  if (!items.length) markRead([m.id]);
  else if (userSpeaking()) { /* stays unread; read after the user finishes */ }
  else if (autoplay || player.position().status !== 'idle') {
    if (!player.isUnlocked()) showBanner('Tap to listen', 'tap', () => { player.unlock(); maybeAutoplay(); });
    else player.append(items);
  }
  if (pendingSwitch && !items.length && player.position().status === 'idle') runSwitch();
}


// ---- edited and deleted messages (amh replaces a message by deleting it and posting a new one) ----
let replacementWait = null;   // `{topic, until, timer}` while the message being read was deleted and its replacement may still arrive
const reading = id => nowMsgId === id && ['playing', 'loading', 'paused'].includes(player.position().status);
function forgetReadState(id) {
  const pos = store.get('voice2-pos', null); if (pos?.msgId === id) store.set('voice2-pos', null);
  const last = store.get('voice2-last', null); if (last?.msgId === id) store.set('voice2-last', null);
  if (pausedItem?.msgId === id) pausedItem = null;
  readQueue.delete(id); store.set('voice2-readq', [...readQueue]);
}
function replaceNode(m) {
  const node = els.get(m.id); if (!node) return;
  const fresh = model.isAgent(m) ? agentEl(m, attUrl) : mineEl(m.content, 'sent', {}, attUrl);
  fresh.dataset.id = m.id; node.replaceWith(fresh); els.set(m.id, fresh);
}
function onMessageEdited(id, content) {
  if (!model.find(id)) return;
  const wasReading = reading(id), m = model.edit(id, content);
  log('app', `message ${id} edited${wasReading ? ' while being read' : ''}`);
  replaceNode(m); highlight();
  if (!model.isAgent(m)) return;
  if (wasReading) { player.stop(); player.earcon(EARCON_NOTES.updated, EARCON_NOTE_S); setTimeout(() => playFrom(m, 0), 700); }   // the changed text is read from its start
  else if (!m.flags?.includes('read') && player.position().status !== 'idle') { player.removeFuture(id); player.append(itemsOf(m)); }
  renderUnread(); persist();
}
function onMessageDeleted(id) {
  const m = model.find(id); if (!m) return;
  const wasReading = reading(id), topic = m.subject;
  model.remove(topic, id); els.get(id)?.remove(); els.delete(id); forgetReadState(id);
  log('app', `message ${id} deleted${wasReading ? ' while being read' : ''}`);
  player.removeFuture(id);
  if (wasReading && model.isAgent(m)) {
    player.stop(); player.earcon(EARCON_NOTES.updated, EARCON_NOTE_S);
    const recent = x => model.isAgent(x) && x.id > id && !x.flags?.includes('read') && Math.abs(Date.now() - x.timestamp * 1000) <= REPLACEMENT_WINDOW_MS;
    const found = model.agentMessages(topic).find(recent);
    if (found) setTimeout(() => playFrom(found, 0), 700);
    else {   // the replacement is usually posted a moment after the delete; if none comes, reading carries on with whatever is queued
      const timer = setTimeout(() => { replacementWait = null; maybeAutoplay(); }, 3000);
      replacementWait = {topic, until: Date.now() + REPLACEMENT_WINDOW_MS, timer};
    }
  }
  renderUnread(); persist();
}
const REPLACEMENT_WINDOW_MS = 10000;
/** A new agent message that replaces the one just deleted (same topic, within the window): read it from its start. */
function takeReplacement(m) {
  if (!replacementWait || replacementWait.topic !== m.subject || Date.now() > replacementWait.until) return false;
  clearTimeout(replacementWait.timer); replacementWait = null;
  log('app', `replacement ${m.id} read from its start`);
  setTimeout(() => playFrom(m, 0), 700);
  return true;
}

// ---- reading ----
const msgIndex = id => model.agentMessages(current).findIndex(m => m.id === id);

function playFrom(m, idx = 0) {
  setFollow(true);
  player.unlock();
  const later = model.agentMessages(current).filter(x => x.id > m.id);
  player.play([...itemsOf(m, idx), ...later.flatMap(x => itemsOf(x))], 0);
}

/** Forward-only watermark per topic: the newest (message, chunk) ever started. Autoplay never queues anything at or before it. */
const wmKey = topic => 'voice2-wm:' + topic;
const after = (a, b) => !b || a.msgId > b.msgId || (a.msgId === b.msgId && a.idx > b.idx);   // is `a` strictly after `b`?
const watermark = () => store.get(wmKey(current), null);
const pastWatermark = it => after(it, watermark());
/** What was paused (by a voice session or otherwise) and is not finished: resumes it mid-chunk, else falls back to autoplay. */
let pausedItem = null;
let lastVoiceReply = {key: '', ms: 0};
function resumeAfterPause() {
  player.unlock();
  const st = player.position().status;
  if (st === 'paused') { player.resume(); return; }
  const m = pausedItem && model.find(pausedItem.msgId);
  if (st === 'idle' && m && !m.flags?.includes('read') && pausedItem.topic === current) {
    log('app', `resume paused item ${pausedItem.msgId}:${pausedItem.idx}`);
    const later = model.agentMessages(current).filter(x => x.id > m.id && !x.flags?.includes('read')).flatMap(x => itemsOf(x));
    player.play([...itemsOf(m, pausedItem.idx), ...later], 0);
    return;
  }
  maybeAutoplay();
}

function maybeAutoplay() {
  if (replacementWait) return;
  if (!autoplay || !current || userSpeaking() || player.position().status !== 'idle') return;   // never start speech while the user speaks
  // Forward only: anything older than the newest read message counts as read and is never played again.
  const newestRead = model.agentMessages(current).filter(m => m.flags?.includes('read')).at(-1);
  if (newestRead) markRead(model.unreadAgentMessages(current).filter(m => m.id < newestRead.id).map(m => m.id));
  const unread = model.unreadAgentMessages(current);
  const speakable = unread.filter(m => itemsOf(m).some(pastWatermark));
  markRead(unread.filter(m => !itemsOf(m).length).map(m => m.id));
  let pos = store.get('voice2-pos', null);
  if (pos && (model.find(pos.msgId)?.flags?.includes('read') || (pos.topic === current && !pastWatermark({msgId: pos.msgId, idx: pos.idx - 1})))) { store.set('voice2-pos', null); pos = null; }   // finished, or older than the watermark   // a finished message is never resumed
  const resumable = pos?.topic === current && model.find(pos.msgId) && itemsOf(model.find(pos.msgId), pos.idx).length;
  if (!speakable.length && !resumable) { hideBanner(); return; }
  if (!player.isUnlocked()) { showBanner(`Tap to listen (${speakable.length} unread)`, 'tap', () => { player.unlock(); maybeAutoplay(); }); return; }
  hideBanner();
  const at = pos?.topic === current ? model.find(pos.msgId) : null;
  if (at && itemsOf(at, pos.idx).length) {   // resume exactly where listening stopped, then continue forward
    player.play([...itemsOf(at, pos.idx), ...model.agentMessages(current).filter(m => m.id > at.id && !m.flags?.includes('read')).flatMap(m => itemsOf(m))], 0);
    return;
  }
  player.play(speakable.flatMap(m => itemsOf(m).filter(pastWatermark)), 0);
}

/** Skipped past the last chunk: stop, mark everything read, play nothing. */
function endOfQueue() {
  clearTimeout(burst?.timer); burst = null;
  player.stop(); nowMsgId = null; highlight();
  markRead(model.agentMessages(current).map(m => m.id));
  store.set('voice2-pos', null);
  toast('End of messages');
}
const RESTART_AFTER_S = 2;
const SKIP_DEBOUNCE_MS = 350;
let burst = null;   // taps within SKIP_DEBOUNCE_MS of each other: `{msgId, idx, pending, timer}`
/**
 * Steps one spoken chunk (one whole message when `whole`). The first tap plays at once; further taps in quick succession move the
 * highlighted target instantly and only the final target is loaded after the debounce. Previous restarts the chunk if it played >2 s.
 */
function jump(direction, whole = false, noRestart = false) {
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
  } else if (at < 0) {   // idle: previous replays the last heard chunk, next goes to the first unheard one
    const heard = store.get('voice2-last', null);
    const hi = heard?.topic === current ? flat.findIndex(it => it.msgId === heard.msgId && it.idx === heard.idx) : -1;
    if (direction < 0) target = hi >= 0 ? flat[hi] : flat.at(-1);
    else target = flat.find(it => !model.find(it.msgId)?.flags?.includes('read')) ?? (hi >= 0 ? flat[hi + 1] : flat[0]);
  }
  else if (direction < 0 && !burst && !noRestart && player.currentTime() > RESTART_AFTER_S) target = flat[at];
  else target = flat[at + direction];
  if (!target && direction > 0) { endOfQueue(); return; }   // skipping past the last chunk ends reading: nothing plays, nothing wraps around
  if (!target) return toast('No earlier messages');
  const play = () => playFrom(agents.find(m => m.id === target.msgId), target.idx);
  if (burst) clearTimeout(burst.timer);
  const pending = !!burst;
  burst = {msgId: target.msgId, idx: target.idx, pending};
  if (pending) { nowMsgId = target.msgId; nowIdx = target.idx; highlight(); setFollow(true); scrollToNow(); } else play();
  burst.timer = setTimeout(() => { const b = burst; burst = null; if (b.pending) play(); }, SKIP_DEBOUNCE_MS);
}


// ---- waiting and interruptions: the user always knows whether audio is coming ----
let lastInterruptMs = 0, waitingSinceMs = 0, lastPlayTapMs = 0, lastErrorTapMs = 0;
const WAITING_SOUND_AFTER_MS = 5000;
/** Playback stopped unexpectedly (load error, stall, play rejected): one distinct falling tone, at most every 20 s. */
function interrupted(reason) {
  log('audio', `interrupted: ${reason}`);
  if (userSpeaking() || Date.now() - lastInterruptMs < 20000) return;
  lastInterruptMs = Date.now();
  player.earcon(EARCON_NOTES.interrupted, EARCON_NOTE_S);
}
/** Every second: is the user waiting for audio (loading or retrying, queue not empty)? After 5 s, a soft ambient pad fades in; it fades out when audio plays or the wait ends. */
setInterval(() => {
  const {status} = player.position(), waiting = (status === 'loading' || status === 'error' || status === 'blocked') && !userSpeaking() && !vc && !dict;
  if (!waiting) { waitingSinceMs = 0; if (player.ambientOn()) { log('audio', 'waiting sound off'); player.ambient(false); } return; }
  waitingSinceMs ||= Date.now();
  if (Date.now() - waitingSinceMs >= WAITING_SOUND_AFTER_MS && !player.ambientOn()) { log('audio', `waiting sound on (${status} for ${Math.round((Date.now() - waitingSinceMs) / 1000)} s)`); player.ambient(true); }
}, 1000);

function togglePlay() {
  const {status} = player.position();
  player.unlock();
  if (status === 'loading') {   // idempotent: a quick second tap never restarts or queues; a later tap cancels
    if (Date.now() - lastPlayTapMs < 1500) { toast(navigator.onLine ? 'Loading audio…' : 'Waiting for the connection…'); lastPlayTapMs = Date.now(); return; }
    player.pause(); return;
  }
  lastPlayTapMs = Date.now();
  if (status === 'playing') player.pause();
  else if (status === 'paused') player.resume();
  else if (status === 'error' || status === 'blocked') { if (Date.now() - lastErrorTapMs < 1500) { toast(navigator.onLine ? 'Still trying to load audio…' : 'Waiting for the connection…'); return; } lastErrorTapMs = Date.now(); player.retry(); }
  else {
    const agents = model.agentMessages(current);
    const start = model.unreadAgentMessages(current).find(m => itemsOf(m).length) ?? agents.findLast(m => itemsOf(m).length);
    if (start) playFrom(start, 0); else toast('Nothing to read');
  }
}

function renderTransport({status, error}) {
  $('play').dataset.state = status;
  try { navigator.mediaSession.playbackState = status === 'playing' || status === 'loading' ? 'playing' : status === 'paused' ? 'paused' : 'none'; } catch { /* unsupported */ }
  $('play').setAttribute('aria-label', status === 'playing' ? 'Pause' : 'Play');
  if (status === 'error') showBanner(`Waiting for audio (${error}). Retrying by itself…`, 'warn', () => player.retry());
  else if (status === 'blocked') showBanner('Tap to resume audio', 'tap', () => player.unlock());
  else if (bannerKind === 'err') hideBanner();
  $('dock').classList.toggle('min', status === 'idle' && scrolledAway);
}

/** Lock screen, Control Center and headphone buttons: metadata names the agent and message; play/pause/next/previous map to the app's own controls. */
function setMediaSession(item) {
  if (!('mediaSession' in navigator)) return;
  const m = item ? model.find(item.msgId) : null, title = m ? (parsed(m).title.replace(/^\[[^\]]*\]\s*/, '') || current) : current;
  navigator.mediaSession.metadata = new MediaMetadata({title: title ?? 'Voice', artist: current ?? 'Agent', album: 'Zulip voice'});
  const handlers = {play: () => { player.unlock(); if (player.position().status === 'paused') resumeAfterPause(); else togglePlay(); }, pause: () => player.pause(), nexttrack: () => jump(1, false, true), previoustrack: () => jump(-1, false, true), seekforward: () => jump(1), seekbackward: () => jump(-1)};
  for (const [name, fn] of Object.entries(handlers)) { try { navigator.mediaSession.setActionHandler(name, fn); } catch { /* unsupported action */ } }
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
let previousTopic = store.get('voice2-prev', null);
function renderPrevButton() {
  const b = $('prev-agent');
  b.hidden = !previousTopic || previousTopic === current;
  b.title = b.hidden ? '' : `Back to ${previousTopic}`;
  $('prev-agent-name').textContent = previousTopic ?? '';
}
function switchBack() {
  if (!previousTopic || previousTopic === current) { toast('No previous agent'); return false; }
  switchTopic(previousTopic);
  return true;
}

function switchTopic(topic) {
  lat.begin('switchfeed', 'switch-render');
  lat.finish('switch', 'switched');
  if (topic === current) { $('picker').close(); return; }
  player.stop();
  nowMsgId = null; pendingSwitch = null; hideBanner();
  saveDraft();
  if (current && current !== topic) { previousTopic = current; store.set('voice2-prev', current); }   // Alt-Tab style: the topic we just left
  current = topic;
  store.set('voice2-topic', topic);
  renderPrevButton();
  $('draft').value = store.get('voice2-draft:' + topic, '');
  autosize();
  hasEarlier = false;
  renderFeed();
  $('picker').close();
  loading = model.messages(topic).length ? null : topic;
  lat.finish('switchfeed', model.messages(topic).length ? 'from-cache' : 'EMPTY-cache');
  if (model.messages(topic).length) maybeAutoplay();
  loadTopicUntilLoaded(topic);
  renderUnread();
  announceSwitch(topic);
}

let announcingUntil = 0;
/** Earcon plus "Now on <agent>", so a switch to an agent with nothing to play is still noticeable; unread messages play afterwards. */
function announceSwitch(topic) {
  if (!voiceControl) return;   // silent (visual only) unless voice control is on
  recorder.beep('switch');
  announcingUntil = Date.now() + 4000;
  recorder.muteFor(3500);
  player.say(`Now on ${topic}`).then(ms => { recorder.muteFor(ms); announcingUntil = Date.now() + ms + 300; setTimeout(() => { if (current === topic) maybeAutoplay(); }, ms + 350); });
}

let loading = null;
/** Loads `topic`'s history, retrying with backoff (offline, 429) until it arrives; the feed shows "Loading…" meanwhile, never blank. */
async function loadTopicUntilLoaded(topic) {
  for (let attempt = 0; current === topic; attempt++) {
    const r = await zulip.call('GET', 'messages', {narrow: topicQuery(topic), anchor: 'newest', num_before: 40, num_after: 0, apply_markdown: false});
    if (r.ok) {
      for (const m of r.data.messages) model.add(m);
      model.markLoaded(topic);
      if (current !== topic) return;
      hasEarlier = !r.data.found_oldest && r.data.messages.length >= 40;
      const shown = els.size; loading = null; renderUnread();
      if (!shown || model.messages(topic).length > shown) renderFeed();   // a cached topic is only redrawn when the fetch found something new
      maybeAutoplay();
      return;
    }
    log('app', `loading ${topic} failed (${r.error}); retry ${attempt + 1}`);
    await new Promise(res => setTimeout(res, Math.min(1000 * 2 ** attempt, 8000)));
  }
}

function runSwitch() {
  const {to, sourceId, msgId} = pendingSwitch;
  pendingSwitch = null;
  const id = msgId ?? sourceId ?? null;
  if (hasActed(id)) return;
  if (id !== null) { markRead([id]); markActed(id); }   // read first, then act: the message is never unread when the switch happens
  const target = model.hasTopic(to) ? to : null;
  if (!target) return toast(`Agent “${to}” not found`);
  const from = current;
  switchTopic(target);
  toast(`Switched to ${target}`, 'Back', () => switchTopic(from));
}

/** Topics ranked by how well they match `query` ("terminal viz" finds terminal_viz); most recent first when the query is empty. */
const ABBREVIATIONS = {mgr: 'manager', manager: 'mgr', viz: 'visualization', impl: 'implementation', cfg: 'config', config: 'cfg', dev: 'developer', ops: 'operations', eval: 'evaluation', exp: 'experiment', doc: 'documentation', docs: 'documentation', repo: 'repository', infra: 'infrastructure', ui: 'uiclient', pr: 'pullrequest', ci: 'continuous', ml: 'machinelearning', db: 'database'};
/** 0..1: how well every spoken word matches some word of the topic name ("human manager" ~ human_mgr), allowing abbreviations, prefixes and initials. */
function tokenScore(query, name) {
  const qs = normalize(query).split(' ').filter(Boolean), ns = normalize(name.replace(/[_-]/g, ' ')).split(' ').filter(Boolean);
  if (!qs.length || !ns.length) return 0;
  const same = (a, b) => a === b || ABBREVIATIONS[a] === b || ABBREVIATIONS[b] === a || (a.length >= 3 && b.startsWith(a)) || (b.length >= 3 && a.startsWith(b)) || (ABBREVIATIONS[b] && ABBREVIATIONS[b].startsWith(a) && a.length >= 3) || (ABBREVIATIONS[a] && ABBREVIATIONS[a].startsWith(b) && b.length >= 3) || (a.length >= 4 && editDistance(a, b) <= 1);
  const matched = qs.filter(q => ns.some(n => same(q, n))).length / qs.length;
  const cover = ns.filter(n => qs.some(q => same(q, n))).length / ns.length;
  return matched === 1 ? 0.7 + 0.3 * cover : matched * 0.7;
}
function rankTopics(query) {
  const q = normalize(query), qFlat = q.replace(/ /g, '');
  const topics = model.topics();
  if (!qFlat) return topics.map(t => ({t, score: 1}));
  return topics.map(t => {
    const name = normalize(t.name.replace(/[_-]/g, ' ')), flat = name.replace(/ /g, '');
    const score = Math.max(tokenScore(query, t.name), 0) > 0 && tokenScore(query, t.name) >= 0.85 ? tokenScore(query, t.name) : flat === qFlat ? 1 : flat.includes(qFlat) ? 0.9 : q.split(' ').every(w => name.includes(w)) ? 0.8 : 1 - editDistance(flat.slice(0, qFlat.length + 2), qFlat) / Math.max(qFlat.length, 1);
    return {t, score};
  }).filter(x => x.score >= 0.4).sort((a, b) => b.score - a.score || b.t.maxId - a.t.maxId);
}

function openPicker() {
  const query = $('search').value;
  const list = $('topics');
  list.replaceChildren();
  for (const {t} of rankTopics(query).slice(0, 80)) {
    const last = model.lastMessage(t.name);
    const snip = last ? `${ago(last.timestamp)} · ${(model.isAgent(last) ? parsed(last).body || parsed(last).title : 'You: ' + last.content).replace(/\s+/g, ' ').slice(0, 90)}` : '';
    list.append(topicRow(t, snip, t.name === current, () => switchTopic(t.name)));
  }
  if (!$('picker').open) { $('picker').showModal(); $('search').focus(); }
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

/** The UI flips to recording on the tap itself; microphone setup finishes in the background and a failure reverts it. */
async function startDictation() {
  if (dict) return;
  player.unlock();
  if (voiceState.ok === false) toast('Voice service is not responding; dictation may fail');
  const wasPlaying = ['playing', 'loading'].includes(player.position().status);
  const mine = dict = {base: draftEl.value, parts: [], stopping: false, sendAfter: false, cancelled: false, wasPlaying};
  if (wasPlaying) player.pause();
  $('composer').classList.add('rec'); $('cancel-rec').hidden = false; $('level').hidden = false;
  draftEl.readOnly = true; $('send').disabled = false; $('mic').setAttribute('aria-label', 'Stop dictation');
  $('send').setAttribute('aria-label', 'Finish and send');
  const r = await recorder.start();
  if (r.ok || dict !== mine) return;   // started, or the user already stopped it
  mine.stopping = true; mine.cancelled = true;
  if (r.error) toast(r.error);
  maybeFinish();
}

function onClip(blob, meta = {}) {
  if (!dict || dict.cancelled) return;
  const part = {state: 'pending', text: '', blob, speechMs: meta.speechMs};
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
      r = await voice.stt(part.blob, (part.speechMs ?? 9e9) >= 1500 ? DOMAIN_PROMPT : undefined);   // domain words bias reply dictation only; start-word clips keep their own prompt
      if (r.ok) break;
      log('stt', `clip attempt ${attempt + 1} failed: ${r.error}`);
      for (let waited = 0; waited < 20000 && events.info().state !== 'live'; waited += 500) await new Promise(res => setTimeout(res, 500));
      await new Promise(res => setTimeout(res, Math.min(1000 * (attempt + 1), 8000)));
    }
    return r;
  })().then(r => {
    if (owner.sentAway) return;
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
  if (send && !cancel) { sendDictationNow(dict); return; }   // no waiting for transcripts
  maybeFinish();
}
/** Send now: finished transcripts go as text, unfinished clips as uploads for the service to transcribe; the UI is free at once. */
function sendDictationNow(finished) {
  finished.sentAway = true;
  const items = [];
  const prefix = finished.base.trim();
  if (prefix) items.push({text: prefix});
  for (const p of finished.parts) items.push(p.state === 'done' ? {text: p.text} : {blob: p.blob});
  const preview = [prefix, ...finished.parts.map(p => p.state === 'done' ? p.text : '…')].filter(Boolean).join(' ');
  $('composer').classList.remove('rec'); $('cancel-rec').hidden = true; $('level').hidden = true;
  draftEl.readOnly = false; $('mic').setAttribute('aria-label', 'Dictate'); $('send').setAttribute('aria-label', 'Send');
  dict = null; draftEl.value = ''; autosize(); saveDraft();
  if (!items.some(i => i.blob || i.text.trim())) { if (finished.wasPlaying) resumeAfterPause(); return; }
  const local = {localId: `l${Date.now()}_${localCounter++}`, sendId: 'send_' + Array.from(crypto.getRandomValues(new Uint8Array(9)), b => b.toString(16).padStart(2, '0')).join(''), topic: current, text: preview || '…', items, state: 'sending', sentAtMs: Date.now()};
  lat.begin('send:' + local.localId, 'send-voice');
  locals.set(current, [...(locals.get(current) ?? []), local]);
  feed.querySelector('#empty')?.remove(); feed.append(localEl(local)); feed.scrollTop = feed.scrollHeight;
  log('send', `voice send ${local.sendId}: ${items.length} parts, ${items.filter(i => i.blob).length} still to transcribe`);
  deliver(local);
  if (finished.wasPlaying) resumeAfterPause(); else maybeAutoplay();
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
  if (finished.wasPlaying) resumeAfterPause(); else maybeAutoplay();
}

const kwsSpotter = createKeywordSpotter({log});
const kwsHitMs = {};
let sttInFlight = 0, lastAttentionHitMs = 0;
/** The on-device keyword spotter heard a keyword: "attention" opens a reply session at once (pausing playback, no upload); "tomato" inside a reply ends it. */
function onKwsHit(word) {
  if (!voiceControl || !kwsEnabled() || dict || Date.now() - (kwsHitMs[word] ?? 0) < 1500) return;   // the same word twice in a row is one hit; a different word may follow at once ("attention katana")
  kwsHitMs[word] = Date.now();
  log('vc', `on-device keyword: ${word}`);
  if (word === 'ATTENTION' && startWord() === 'attention' && !vc && !(pendingChoice && Date.now() < pendingChoice.until)) {
    if (!lat.has('sw')) lat.begin('sw', 'startword');
    lat.stage('sw', 'kws-local');
    lastAttentionHitMs = Date.now();
    openSession('', {deferChime: true});
  } else if (word === 'BAMBOO' && cancelWord() === 'bamboo') {
    if (vc) cancelSession(); else if (pendingChoice) cancelChoice();
  } else if (word === 'KATANA' && vc && vc.mode !== 'command' && commandWord() === 'katana') {
    enterCommandMode('');
  } else if (word === 'TOMATO' && vc?.mode === 'command') {
    leaveCommandMode('end word on device');
  } else if (word === 'TOMATO' && vc && normalize(store.get('voice2-resume-phrase', RESUME_DEFAULT)) === 'tomato') {
    sendVoiceReplyNow('end word on device');
  }
}
const kwsEnabled = () => store.get('voice2-kws', null) ?? true;   // on-device recognition is the default; the toggle in Settings turns it off
const recorder = createRecorder({log, onSpeechOnset: () => { if (!kwsSpotter.ready()) return; for (const w of kwsSpotter.restart(0.6)) onKwsHit(w); if (vc?.mode === 'command' || choiceOpen()) for (const tag of kwsSpotter.restartCommands(0.6)) onCommandHit(tag); }, onKwsAudio: samples => { for (const w of kwsSpotter.feed(samples)) onKwsHit(w); if (vc?.mode === 'command' || choiceOpen()) for (const tag of kwsSpotter.feedCommands(samples)) onCommandHit(tag); }, onClip, onLevel: drawLevel, onBarge, onCommandClip, onEarcon: kind => player.earcon(EARCON_NOTES[kind], EARCON_NOTE_S)});
let voiceControl = store.get('voice2-vc', false), voiceListening = false;   // a saved "on" needs one tap after load before the microphone may open

const RESUME_DEFAULT = 'tomato';   // the human's pick from the end-word study: 99% on-device detection, 0 false accepts in 4.9 h of speech
if (['over', 'kangaroo'].includes(store.get('voice2-resume-phrase', null))) store.set('voice2-resume-phrase', RESUME_DEFAULT);   // the old default migrates; a customised phrase stays
const RESUME_SILENCE_MS = 1000;   // plus the recorder's 0.7 s end-of-utterance silence: about 1.7 s of quiet after the phrase
const normalize = t => t.toLowerCase().replace(/[^a-z0-9 ]+/g, ' ').replace(/\s+/g, ' ').trim();
function editDistance(a, b) {
  const d = Array.from({length: a.length + 1}, (_, i) => [i, ...Array(b.length).fill(0)]);
  for (let j = 1; j <= b.length; j++) d[0][j] = j;
  for (let i = 1; i <= a.length; i++) for (let j = 1; j <= b.length; j++) d[i][j] = Math.min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1));
  return d[a.length][b.length];
}
/** `text` without a trailing end phrase ("over", also "over over", "over and out"; any custom phrase matches fuzzily), or null when it does not end with one. */
function stripResumePhrase(text) {
  const words = text.trim().split(/\s+/).filter(Boolean), norm = words.map(w => normalize(w));
  const phrase = normalize(store.get('voice2-resume-phrase', RESUME_DEFAULT));
  if (phrase === 'tomato' || phrase === 'over') {
    let n = norm.length;
    if (phrase === 'over' && n >= 3 && norm[n - 1] === 'out' && norm[n - 2] === 'and' && norm[n - 3] === 'over') n -= 2;
    const kept = phrase === 'over' ? (() => { let m = n; while (m > 0 && norm[m - 1] === 'over') m--; return m; })() : stripTrailingControl(words.slice(0, n), 'tomato').length;   // "kangaroo" is matched by sound (Cangaroo, Canguro, kanga roo ...)
    return kept < norm.length ? words.slice(0, kept).join(' ') : null;
  }
  for (const q of [phrase]) {
    const k = q.split(' ').length;
    if (q && words.length >= k && editDistance(normalize(words.slice(-k).join(' ')), q) <= Math.floor(q.length / 6)) return words.slice(0, -k).join(' ');
  }
  return null;
}
const START_DEFAULT = 'attention';   // chosen from the trigger-word study: long, no clipped final consonant, 98-100% detection, 0 false accepts
const LONG_SILENCE_DEFAULT_S = 20;   // "over" is the main end signal; a short silence cut replies off while the user was still thinking
const FOLLOW_UP_GRACE_MS = 6000;
let lastReplyEndMs = 0;
const START_ALIASES = {attention: ['attention', 'attension', 'attentions', 'attenshun', 'a tension', 'attending'], pause: ['pause', 'paws', 'pose', 'pours', 'paused', 'pawz', 'pulse', 'pulls', 'polls']};
/** A saved start word of "pause" (the old default) moves to the new default; a customised word stays. */
if (store.get('voice2-start-word', null) === 'pause') store.set('voice2-start-word', START_DEFAULT);
const startWord = () => normalize(store.get('voice2-start-word', START_DEFAULT));
const EMPTY_SESSION_MS = 8000;
let vc = null;   // an open voice-control session: `{base, text, timer, emptyTimer}`
const setVcState = (state, label) => { $('listen').dataset.state = state; $('listen-label').textContent = label; $('listen').title = 'Voice control: ' + label; $('listen').setAttribute('aria-label', 'Voice control: ' + label); };
const vcIdle = () => { const playing = ['playing', 'loading'].includes(player.position().status); setVcState(playing ? 'listening' : 'on', playing ? 'listening' : 'paused'); };

/** `text` after a leading start word (default "attention", fuzzy, repeats collapsed), or null when it does not start with one. */
function afterStartWord(text) {
  const word = startWord();
  const accepted = new Set([word, ...(START_ALIASES[word] ?? [])]);
  const words = text.trim().split(/\s+/).filter(Boolean);
  if (word === 'attention') { const c = leadingControlCount('attention', words); if (c) return words.slice(c).join(' '); }
  const isStart = w => { const n = normalize(w); return n && (accepted.has(n) || editDistance(n, word) <= Math.floor(word.length / 4)); };
  if (!words.length || !isStart(words[0])) return null;
  let i = 0;
  while (i < words.length && isStart(words[i])) i++;
  return words.slice(i).join(' ');
}

/** Speech while idle or playing only records; the transcript decides (it must start with the start word). Playback volume is left alone. */
let bargePaused = false;
function resumeBarge() {
  if (!bargePaused) return;
  bargePaused = false;
  if (player.position().status === 'paused') { log('vc', 'not a command: resuming in place'); player.resume(); }
  vcIdle();
}
function onBarge() {
  if (dict) return;
  if (!vc) lat.begin('sw', 'startword');   // speech onset
  const st = player.position().status;
  log('vc', `speech detected while ${st}${vc ? ' (session open)' : ''}`);
  utterancePending = true; clearTimeout(utteranceTimer); utteranceTimer = setTimeout(utteranceDone, 6000);
  if (vc) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); vc.timer = vc.emptyTimer = 0; }
  setVcState('heard', 'hearing you');
}
async function onCommandClip(blob, meta = {}) {
  if (dict) return;
  if (vc) { meta.part = {blob, text: vc.mode === 'command' ? '' : null, fromCommand: vc.mode === 'command'}; (vc.parts ??= []).push(meta.part); }   // speech in command mode is never reply text   // kept so a spotted end word can send the audio itself
  try { await handleCommandClip(blob, meta); } finally { utterancePending = false; clearTimeout(utteranceTimer); if (!userSpeaking()) maybeAutoplay(); }
}
const START_MIN_SPEECH_MS = 250, START_MAX_NO_SPEECH = 0.6, START_MIN_LOGPROB = -1.0;
const UPLOADS_PER_MIN = 8, EMPTY_RUN_BACKOFF = 3, BACKOFF_MS = 60000;
let startWordUploads = [], emptyRun = 0, backoffUntil = 0;
/** Every start-word clip costs a Zulip upload plus messages: cap them, and back off in a noisy room instead of exhausting the rate limit. */
function noteStartResult(useful) {
  emptyRun = useful ? 0 : emptyRun + 1;
  if (emptyRun >= EMPTY_RUN_BACKOFF && Date.now() > backoffUntil) { backoffUntil = Date.now() + BACKOFF_MS; recorder.setBackoff(true); log('vc', `noisy room: ${emptyRun} useless clips in a row, stricter detection for 60 s`); setTimeout(() => { recorder.setBackoff(false); emptyRun = 0; }, BACKOFF_MS); }
}
async function handleCommandClip(blob, meta) {
  if (Date.now() - lastLocalCommandMs < (meta.ms ?? 0) + 800 && lastLocalCommandMs) { log('vc', 'clip already handled on-device: not sent to the server'); if (vc?.parts?.length) vc.parts.at(-1).text ||= ''; if (vc?.mode === 'command') armCommandTimer(); return; }
  log('vc', `clip ${Math.round(meta.ms ?? 0)}ms (${Math.round(meta.speechMs ?? 0)}ms speech) ${vc ? 'in reply' : pendingChoice ? 'for the question' : 'for the start word'}`);
  lat.stage('sw', 'clip-end');
  if (!vc && !pendingChoice) {
    startWordUploads = startWordUploads.filter(t => Date.now() - t < 60000);
    if (startWordUploads.length >= UPLOADS_PER_MIN || zulip.rateLimitedForMs() > 0) { log('vc', `dropped a clip without uploading (${startWordUploads.length} uploads this minute${zulip.rateLimitedForMs() ? ', Zulip rate limit' : ''})`); recorder.setReplyMode(false); vcIdle(); return; }
    startWordUploads.push(Date.now());
  }
  const listening = !!vc || (!!pendingChoice && Date.now() < pendingChoice.until);
  // The "Pause. Over." hint only helps inside an open reply or a question; for start-word detection it can make Whisper hear "Pause." in noise.
  sttInFlight++;
  const r = await voice.stt(blob, vc && vc.mode !== 'command' && (meta.speechMs ?? 0) >= 1500 ? DOMAIN_PROMPT : undefined, !listening)   // never a prompt that names the control words: the model echoes it back on noise.finally(() => { sttInFlight--; });   // start-word clips also get keyword spotting on the server
  if (!r.ok) {   // a transient failure is silent: inside a reply the clip stays as audio and goes out with the reply; the user is told what to do only once
    log('vc', `stt failed: ${r.error}`);
    if (vc && !vc.toldOffline) { vc.toldOffline = true; sayAndMute("Connection lost. Keep talking; I'll send when it's back."); }
    if (vc) armSessionTimer();
    resumeBarge(); vcIdle(); return;
  }
  let heard = r.text.trim();
  { const w = heard.split(/\s+/).filter(Boolean); if (w.length >= 2 && removeControlWords(removeControlWords(w, 'tomato'), 'attention').length === 0 && w.some(x => soundsLike('tomato', [x])) && w.some(x => soundsLike('attention', [x]))) { log('vc', `dropped "${heard}": only control words, an echo of noise`); heard = ''; } }
  if (meta.part) { if (meta.part.sentAway) return; meta.part.text = heard; }   // already sent from the audio: nothing more to do with this transcript
  lat.stage('sw', 'stt-roundtrip', `relay q${r.timings?.queueMs ?? '?'} w${r.timings?.workMs ?? '?'} lag${r.timings?.eventLagMs ?? '?'}`);
  log('vc', `heard "${heard}" (speech ${Math.round(meta.speechMs ?? 0)}ms, no_speech ${r.noSpeechProb?.toFixed(2)}, logprob ${r.avgLogprob?.toFixed(2)})`);
  if (!listening && normalize(heard) && ((meta.speechMs ?? 1e9) < START_MIN_SPEECH_MS || r.noSpeechProb > START_MAX_NO_SPEECH || r.avgLogprob < START_MIN_LOGPROB)) {
    log('vc', 'rejected as noise: too short or low confidence'); recorder.setReplyMode(false); resumeBarge(); vcIdle(); return;
  }
  if (!normalize(heard)) { if (!vc) { noteStartResult(false); recorder.setReplyMode(false); resumeBarge(); } else armSessionTimer(); vcIdle(); return; }   // noise must not cancel the end-of-reply timer for good
  if (lastLocalNav && Date.now() - lastLocalNav.ms < 10000) {   // "skip forward three": the device already skipped once, add the rest
    const nav = navCommand(normalize(afterStartWord(heard) ?? heard).replace(/^katana\s+/, ''));
    if (nav && nav.dir === lastLocalNav.dir && nav.count > 1) { log('vc', `count from the transcript: ${nav.count - 1} more`); navigate({...nav, count: nav.count - 1}); lastLocalNav = null; vcIdle(); return; }
    lastLocalNav = null;
  }
  if (pendingChoice && Date.now() < pendingChoice.until) {   // while a switch question is open, answers are matched first, even inside or after "pause"
    const w = (afterStartWord(heard) !== null ? normalize(afterStartWord(heard)) : normalize(heard));
    const NUMBERS = ['one first 1', 'two second 2', 'three third 3'];
    let pick = null;
    if (pendingChoice.confirm) { if (/^(positive|yes|yeah|yep|sure|ok|okay|correct)$/.test(w)) pick = pendingChoice.names[0]; }
    else { const i = NUMBERS.findIndex(g => g.split(' ').includes(w.replace(/^number /, ''))); if (i >= 0) pick = pendingChoice.names[i]; }
    const closeSession = () => { if (vc) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); draftEl.value = vc.base; autosize(); vc = null; } recorder.setReplyMode(false); };
    if (pick) { pendingChoice = null; lat.stage('switch', 'answer-heard'); closeSession(); switchTopic(pick); vcIdle(); return; }   // the switch tone and "Now on X" follow; no voice state lingers
    if (/^(negative|no|nope|cancel|cancel that|none|never mind|scratch that|stop)$/.test(w) || CANCEL_RE.test(w)) { pendingChoice = null; closeSession(); recorder.beep('cancel'); vcIdle(); maybeAutoplay(); return; }
    if (!vc) { log('vc', `ignored during the switch question: "${heard}"`); vcIdle(); return; }   // "pause" must not open a reply while we wait for the answer
  }
  if (!vc) {
    // With server keyword spotting on (r.kws is a list): it must have heard PAUSE, and Whisper must confirm the standalone word, so "paused"/"paws" never open a session.
    const kwsOn = Array.isArray(r.kws) && startWord() === 'attention' && Date.now() - lastReplyEndMs >= FOLLOW_UP_GRACE_MS;   // the server spots "attention" only   // a follow-up right after a send needs no start word
    if (kwsOn && !r.kws.includes('ATTENTION')) { noteStartResult(false); log('vc', `ignored: "${heard}" (keyword spotter did not hear the start word)`); resumeBarge(); recorder.setReplyMode(false); vcIdle(); return; }
    let rest = afterStartWord(heard);
    // Whisper sometimes drops the leading start word but keeps the command after it: with a spotter hit, a bare command still counts.
    const commandLike = t => { const n = normalize(t); return CANCEL_RE.test(n) || n === 'send'; };   // commands live in command mode, behind the command word
    if (kwsOn && rest === null && commandLike(heard)) rest = heard;
    if (kwsOn && rest !== null && afterStartWord(heard) === null && !commandLike(heard)) { log('vc', `ignored: "${heard}" (transcript is not the start word)`); rest = null; resumeBarge(); recorder.setReplyMode(false); vcIdle(); return; }
    if (rest === null && Date.now() - lastReplyEndMs < FOLLOW_UP_GRACE_MS && commandLike(heard)) rest = heard;   // a command right after a send is still a command, never reply text
    if (rest === null && Date.now() - lastReplyEndMs < FOLLOW_UP_GRACE_MS) { log('vc', `follow-up right after a send: "${heard}"`); openSession(''); addToSession(heard); return; }
    if (rest === null) { noteStartResult(false); resumeBarge(); log('vc', `ignored: "${heard}" (no start word)`); toast(`Ignored (no start word): ${heard.slice(0, 60)}`); recorder.setReplyMode(false); vcIdle(); return; }
    noteStartResult(true);
    openSession(rest);
    return;
  }
  const body = normalize(afterStartWord(heard) ?? heard);   // a clip that begins with the start word (the on-device spotter already opened the session) still holds what follows it
  if (vc.mode === 'command') {   // COMMAND mode: the closed list only; the end word returns to input, cancel discards
    const words = body.split(' ').filter(Boolean);
    if (stripResumePhrase(body) !== null && !words.some(w => CANCEL_RE.test(w))) { leaveCommandMode('end word'); return; }
    runCommands(body); return;
  }
  if (CANCEL_RE.test(body)) { cancelSession(); return; }   // discards this reply and carries on
  addToSession(heard);
}

// ---- On-device command phrases (command mode only). The study (research-kws/trig/cmdstudy.py) found only these reliable: "switch to <agent>" (78-98% hit, no confusions, 0 false accepts in 4.9 h),
// "cancel" (98%), "stop" (82%). next/back/skip/again/play/yes/no and number chains are confused with each other, so the server transcript stays their path. ----
const LOCAL_CMD_THRESHOLD = 0.3;
const SPOKEN_EXPANSIONS = {mgr: 'manager', impl: 'implementation', cfg: 'config', dev: 'developer', eval: 'evaluation', exp: 'experiment', ops: 'operations'};
/** How an agent name is said: underscores and dashes become spaces, abbreviations are spelled out ("human_mgr" -> "human manager"). */
const spokenName = key => normalize(topicOf(key).replace(/[_-]/g, ' ')).split(' ').filter(Boolean).map(w => SPOKEN_EXPANSIONS[w] ?? w).join(' ');
function commandPhrases() {
  const names = model.topics().slice(0, 40).map(t => spokenName(t.name)).filter(Boolean);
  const unique = [...new Set(names)].filter(n => !names.some(o => o !== n && o.startsWith(n + ' ')));   // "alpha" is left out while "alpha beta" exists: it would fire first
  return [{tag: 'CANCEL', text: 'cancel', threshold: LOCAL_CMD_THRESHOLD}, {tag: 'STOP', text: 'stop', threshold: LOCAL_CMD_THRESHOLD},
    {tag: 'POSITIVE', text: 'positive', threshold: LOCAL_CMD_THRESHOLD}, {tag: 'NEGATIVE', text: 'negative', threshold: LOCAL_CMD_THRESHOLD},
    {tag: 'SKIP_FORWARD', text: 'skip forward', threshold: LOCAL_CMD_THRESHOLD}, {tag: 'SKIP_BACKWARD', text: 'skip backward', threshold: LOCAL_CMD_THRESHOLD},
    ...unique.map(n => ({tag: 'SWITCH_TO_' + n.toUpperCase().replace(/[^A-Z0-9]+/g, '_'), text: 'switch to ' + n, threshold: LOCAL_CMD_THRESHOLD, say: 'switch to ' + n}))];
}
let lastLocalCommandMs = 0, lastLocalTag = '', lastLocalTagMs = 0, localPhrases = new Map();
let lastLocalNav = null;   // {dir, ms}: a "skip forward/backward" heard on the device; the transcript may add a count
const choiceOpen = () => !!pendingChoice && Date.now() < pendingChoice.until;
function onCommandHit(tag) {
  const inCommand = vc?.mode === 'command';
  if ((!inCommand && !choiceOpen()) || (tag === lastLocalTag && Date.now() - lastLocalTagMs < 1000)) return;
  lastLocalTag = tag; lastLocalTagMs = Date.now();
  log('vc', `on-device command: ${localPhrases.get(tag) ?? tag.toLowerCase().replace(/_/g, ' ')} (${Math.round(Date.now() - (lastCommandWindowMs || Date.now()))}ms into the window)`);
  if (choiceOpen() && (tag === 'POSITIVE' || tag === 'NEGATIVE')) {   // the switch question: answered at once
    lastLocalCommandMs = Date.now(); lat.stage('switch', 'answer-heard');
    const names = pendingChoice.names, confirm = pendingChoice.confirm; pendingChoice = null;
    if (tag === 'POSITIVE' && confirm) { if (vc) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); draftEl.value = vc.base; autosize(); vc = null; recorder.setReplyMode(false); } switchTopic(names[0]); vcIdle(); }
    else { recorder.beep('cancel'); if (vc) { clearTimeout(vc.timer); draftEl.value = vc.base; vc = null; recorder.setReplyMode(false); } vcIdle(); maybeAutoplay(); }
    return;
  }
  if (!inCommand) return;
  if (tag === 'SKIP_FORWARD' || tag === 'SKIP_BACKWARD') {   // runs once now; a spoken count ("skip forward three") is added when the transcript arrives
    lastLocalNav = {dir: tag === 'SKIP_FORWARD' ? 1 : -1, ms: Date.now()};
    runCommands(tag === 'SKIP_FORWARD' ? 'skip forward' : 'skip backward'); return;   // does not set lastLocalCommandMs: the clip still goes to the server for the count
  }
  lastLocalCommandMs = Date.now();
  runCommands(localPhrases.get(tag) ?? tag.toLowerCase().replace(/_/g, ' '));
}
let lastCommandWindowMs = 0, prebuildTimer = 0;
/** Rebuilds the command spotter ahead of time (about 0.7 s of work) whenever the agent list changes, so opening command mode never waits for it. */
function prebuildCommands() {
  clearTimeout(prebuildTimer);
  prebuildTimer = setTimeout(() => { if (kwsSpotter.ready() && kwsEnabled() && !vc) { const phrases = commandPhrases(); localPhrases = new Map(phrases.map(p => [p.tag, p.say ?? p.text])); kwsSpotter.setCommands(phrases); } }, 2500);
}
function armLocalCommands() {
  lastCommandWindowMs = Date.now();
  if (!kwsSpotter.ready() || !kwsEnabled()) return;
  const phrases = commandPhrases(); localPhrases = new Map(phrases.map(p => [p.tag, p.say ?? p.text]));
  kwsSpotter.setCommands(phrases); kwsSpotter.resetCommands();
}

// ---- Command mode: start word -> INPUT mode (dictation) -> command word -> COMMAND mode (a closed list, chainable, never text) ----
const CMD_DEFAULT = 'katana', COMMAND_SILENCE_MS = 4000, START_CHIME_DELAY_MS = 600, ATTENTION_COMMAND_WINDOW_MS = 1500;
const commandWord = () => normalize(store.get('voice2-cmd-word', CMD_DEFAULT)) || CMD_DEFAULT;
const isCommandWord = w => { const n = normalize(w); return !!n && (n === commandWord() || soundsLikeWord(commandWord(), [n])); };
/** Index of the first command word in `words`, or -1. */
const commandWordAt = words => words.findIndex(isCommandWord);
function enterCommandMode(rest) {
  if (!vc) return;
  clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); clearTimeout(vc.readyTimer);
  const first = vc.mode !== 'command';
  vc.mode = 'command'; vc.sawCommand = true;
  if (first) { armLocalCommands(); recorder.beep('command'); log('vc', 'command mode'); log('lat', `command mode ready ${Math.round(Date.now() - (vc.openedAt ?? Date.now()))}ms after the session opened`); }
  setVcState('command', 'command mode');
  toast('Command mode: ' + 'back, next, switch to…, cancel');
  armCommandTimer();
  if (rest) runCommands(rest);
}
function armCommandTimer() { clearTimeout(vc.timer); vc.timer = setTimeout(() => leaveCommandMode('silence'), COMMAND_SILENCE_MS); }
/** Back to INPUT mode with the draft kept. */
function leaveCommandMode(why) {
  if (!vc || vc.mode !== 'command') return;
  clearTimeout(vc.timer); vc.mode = 'input'; log('vc', `command mode left (${why}); back to input`);
  recorder.beep('ready'); setVcState('dictating', 'say your reply');
  armSessionTimer();
}
/** Words after the command word, executed in order: "next three back one switch to human manager". Unknown words are ignored; nothing here becomes text. */
function runCommands(text) {
  let tokens = normalize(text).split(' ').filter(Boolean).filter(t => !isCommandWord(t));
  const done = [];
  const tick = name => { done.push(name); recorder.beep('tick'); };
  let navigated = false, left = false;
  const finishSession = (discard = false) => { if (!vc) return; clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); draftEl.value = discard ? vc.base : vc.base + (vc.text ? (vc.base ? ' ' : '') + vc.text : ''); autosize(); saveDraft(); vc = null; recorder.setReplyMode(false); };
  while (tokens.length && vc) {
    const t = tokens[0], two = tokens.slice(0, 2).join(' ');
    if (SWITCH_BACK_RE.test(tokens.slice(0, 3).join(' ')) || SWITCH_BACK_RE.test(two)) { tokens.splice(0, SWITCH_BACK_RE.test(two) ? 2 : 3); log('vc', 'switch back to the previous agent'); tick('switch back'); finishSession(true); bargePaused = false; switchBack(); left = true; break; }
    if (/^(switch|go)$/.test(t) && /^(to|agent|agents)$/.test(tokens[1] ?? '')) {   // the name is free-form: it takes the rest of the utterance and ends the chain
      const name = tokens.slice(/^(agent|agents)$/.test(tokens[1]) && tokens[2] === 'to' ? 3 : 2).join(' ');
      tick('switch'); finishSession(true); recorder.setReplyMode(false); voiceSwitch(name); left = true; break;
    }
    if (CANCEL_RE.test(tokens.slice(0, 2).join(' ')) || CANCEL_RE.test(t)) { cancelSession(); return done; }
    const navTokens = []; let used = 0;   // longest prefix that is one navigation command ("next three", "previous message", "back")
    for (const take of [3, 2, 1]) { const nav = navCommand(tokens.slice(0, take).join(' ')); if (nav) { used = take; navTokens.push(nav); break; } }
    if (used) { tokens.splice(0, used); navigate(navTokens[0], true); tick('navigate'); navigated = true; continue; }
    if (/^(stop|pause|wait)$/.test(t)) { tokens.shift(); player.pause(); tick('pause'); continue; }
    if (/^(play|resume|continue)$/.test(t)) { tokens.shift(); tick('play'); navigated = true; resumeAfterPause(); continue; }
    log('vc', `command mode ignored "${t}"`); tokens.shift();
  }
  if (done.length) log('vc', `commands: ${done.join(', ')}`);
  if (navigated && vc && !left) { finishSession(); vcIdle(); left = true; }
  if (vc && !left) armCommandTimer();
  return done;
}

/**
 * A whole utterance that is only navigation: "back", "previous", "back back", "previous message", "next", "again".
 * `{dir: -1|0|1, whole, count}` (dir 0 = replay the current chunk), or null.
 */
function navCommand(words) {
  const tokens = words.split(' ').filter(Boolean);
  const whole = tokens.includes('message') || tokens.includes('messages');
  const rest = tokens.filter(t => t !== 'message' && t !== 'messages');
  const NUMBERS = {to: 2, too: 2, won: 1, for: 4, one: 1, two: 2, three: 3, four: 4, five: 5, six: 6, seven: 7, eight: 8, nine: 9, ten: 10, '1': 1, '2': 2, '3': 3, '4': 4, '5': 5, '6': 6, '7': 7, '8': 8, '9': 9, '10': 10};
  { const dirWord = {forward: 1, forwards: 1, ahead: 1, backward: -1, backwards: -1}[rest[1]];   // "skip forward [two]" / "skip backward [three]": no count means one
    if (rest[0] === 'skip' && dirWord && rest.length <= 3) { const c = rest.length === 3 ? NUMBERS[rest[2]] : 1; if (c) return {dir: dirWord, whole, count: c}; } }
  const n = NUMBERS[rest.at(-1)];
  if (rest.length === 2 && n && !NUMBERS[rest[0]]) {   // "next three", "skip five", "back two"
    const d = ['back', 'previous', 'last'].includes(rest[0]) ? -1 : ['next', 'skip', 'forward'].includes(rest[0]) ? 1 : null;
    if (d) return {dir: d, whole, count: n};
  }
  if (!rest.length || rest.length > 5) return null;
  const kind = t => ['back', 'previous', 'last'].includes(t) ? -1 : ['next', 'skip', 'forward'].includes(t) ? 1 : ['again', 'repeat'].includes(t) ? 0 : null;
  const dirs = rest.map(kind);
  if (dirs.some(d => d === null) || new Set(dirs).size > 1) return null;
  return {dir: dirs[0], whole, count: dirs[0] === 0 ? 1 : rest.length};
}
function navigate({dir, whole, count}) {
  log('vc', `navigate ${dir > 0 ? 'next' : dir < 0 ? 'previous' : 'again'}${whole ? ' message' : ''} x${count}`);
  player.unlock();
  if (dir === 0) {
    const m = nowMsgId !== null ? model.find(nowMsgId) : null, last = store.get('voice2-last', null);
    const target = m ? {m, idx: nowIdx} : last && model.find(last.msgId) ? {m: model.find(last.msgId), idx: last.idx} : null;
    if (target) playFrom(target.m, target.idx); else jump(-1);
    return;
  }
  for (let i = 0; i < count; i++) jump(dir, whole, true);   // by voice, "back" always means the previous chunk
}

const SWITCH_RE = /^(?:switch agents? to|switch agents?|switch to|go to agents?)\s+(.+)$/;   // never bare "open" or "go to": those are ordinary words
const SWITCH_BACK_RE = /^(switch back|go back|previous agent|(go|switch) back to (the )?previous agent|switch to (the )?previous agent)$/;
const CANCEL_WORD_DEFAULT = 'bamboo';   // the rare word the human picked (study: 90% hit, 0 false accepts in 4.9 h); "cancel" and friends keep working as server-path aliases
const cancelWord = () => normalize(store.get('voice2-cancel-word', CANCEL_WORD_DEFAULT)) || CANCEL_WORD_DEFAULT;
const hasCancelWord = text => { const w = cancelWord(); return normalize(text).split(' ').some(t => t === w || soundsLikeWord(w, [t])); };   // the rare word counts anywhere in an utterance
const CANCEL_RE = {test: t => /^(?:cancel|cancel that|never mind|scratch that|forget it)$/.test(t) || (t.split(' ').every(x => x === cancelWord() || soundsLikeWord(cancelWord(), [x])))};
const SWITCH_MIN_SCORE = 0.8;
/** "switch to <name>": one clear match switches (with a chime); otherwise show the top 3 and accept first/second/third. */
function sayAndMute(text) { recorder.muteFor(2500 + text.length * 70); player.say(text).then(ms => recorder.muteFor(ms)); }
function cancelChoice() { if (pendingChoice) { pendingChoice = null; toast('Switch cancelled'); maybeAutoplay(); } }
const ranksConfident = spoken => (rankTopics(spoken)[0]?.score ?? 0) >= SWITCH_MIN_SCORE;
function voiceSwitch(spoken) {
  lat.begin('switch', 'switch');
  const ranked = rankTopics(spoken);
  log('vc', `switch to "${spoken}" -> ${ranked.slice(0, 3).map(x => `${x.t.name}:${x.score.toFixed(2)}`).join(', ') || 'no match'}`);
  if (!ranked.length) { log('vc', 'no agent matches'); toast(`No agent called “${spoken}”`); sayAndMute(`No agent called ${spoken}`); return false; }
  const clear = ranked[0].score >= SWITCH_MIN_SCORE && (ranked.length === 1 || ranked[0].score - ranked[1].score >= 0.15);
  pendingChoice = {names: clear ? [ranked[0].t.name] : ranked.slice(0, 3).map(x => x.t.name), until: Date.now() + 20000, confirm: clear};
  setTimeout(() => { if (pendingChoice && Date.now() >= pendingChoice.until) cancelChoice(); }, 20500);
  const readable = n => n.replace(/[_-]/g, ' ');
  const prompt = clear ? `Switch to ${readable(ranked[0].t.name)}?` : pendingChoice.names.map((n, i) => `${i + 1}, ${readable(n)}.`).join(' ');
  toast(prompt);
  sayAndMute(prompt);
  armLocalCommands();
  lat.stage('switch', 'question-shown');
  return true;
}

function openSession(rest, {deferChime = false} = {}) {
  bargePaused = false;
  if (['playing', 'loading'].includes(player.position().status)) player.pause();
  lat.finish('sw', 'pause-applied');
 
  vc = {base: draftEl.value, text: '', timer: 0, emptyTimer: 0, openedAt: Date.now()};
  recorder.setReplyMode(true);
  const direct = !!rest && isCommandWord(normalize(rest).split(' ')[0]);   // "attention katana ...": straight to command mode, one chime only
  if (direct) { /* the command chime follows */ }
  else if (deferChime) vc.readyTimer = setTimeout(() => { if (vc && vc.mode !== 'command') recorder.beep('ready'); }, START_CHIME_DELAY_MS);   // gives a following command word the chance to replace it
  else recorder.beep('ready');
  log('vc', `session opened by start word; rest "${rest}"`);
  const words = normalize(rest);
  if (CANCEL_RE.test(words)) { log('vc', 'cancel right after the start word'); vc = null; recorder.setReplyMode(false); recorder.beep('cancel'); resumeAfterPause(); vcIdle(); return; }
  if (rest) addToSession(rest);
  else { setVcState('dictating', 'say your reply'); vc.emptyTimer = setTimeout(closeEmptySession, EMPTY_SESSION_MS); }
}
/** True when the last sentence of `text` is only a cancel phrase ("Some reply. Cancel."). */
function endsWithCancel(text) {
  const last = text.split(/[.!?]+/).map(x => normalize(x)).filter(Boolean).at(-1) ?? '';
  return CANCEL_RE.test(last);
}
/** Discards the open reply (restoring the draft), plays the cancel tone and carries on reading. */
function cancelSession() {
  clearTimeout(vc.timer); clearTimeout(vc.emptyTimer);
  log('vc', 'cancel: reply discarded');
  draftEl.value = vc.base; autosize(); saveDraft();
  vc = null; recorder.setReplyMode(false); recorder.beep('cancel');
  resumeAfterPause(); vcIdle();
}
const withoutStrayEndWords = t => removeControlWords(t.split(/\s+/).filter(Boolean)).join(' ');   // "Tomato." said mid-reply is a command, never text
function addToSession(heard) {
  if (hasCancelWord(heard)) { cancelSession(); return; }   // the rare cancel word fires wherever it is said
  { const words = (afterStartWord(heard) ?? heard).split(/\s+/).filter(Boolean), at = commandWordAt(words);   // the command word switches to COMMAND mode: what follows it is commands, what precedes it is reply text
    if (at >= 0 && vc) { const before = words.slice(0, at).join(' '); if (before) { vc.text = [vc.text, withoutStrayEndWords(before)].filter(Boolean).join(' '); draftEl.value = [vc.base.trim(), vc.text].filter(Boolean).join(' '); autosize(); } enterCommandMode(words.slice(at + 1).join(' ')); return; } }
  if (endsWithCancel(heard)) { cancelSession(); return; }   // "... cancel" at the end of any utterance cancels the whole reply
  clearTimeout(vc.emptyTimer); vc.emptyTimer = 0;
  if (stripResumePhrase(heard) !== null && !vc.sending) { vc.text = [vc.text, (afterStartWord(heard) ?? heard).trim()].filter(Boolean).join(' '); draftEl.value = [vc.base.trim(), vc.text].filter(Boolean).join(' '); autosize(); sendVoiceReplyNow('end word in the transcript'); return; }   // no quiet window
  vc.text = [vc.text, withoutStrayEndWords((afterStartWord(heard) ?? heard).trim())].filter(Boolean).join(' ');
  draftEl.value = [vc.base.trim(), vc.text].filter(Boolean).join(' '); autosize();
  setVcState('dictating', 'dictating reply');
  armSessionTimer();
}
/** (Re)starts the end-of-reply timer: "over" + short silence, else the long silence; an empty session gets the empty timeout. */
function armSessionTimer() {
  if (!vc || vc.ending) return;
  clearTimeout(vc.timer); clearTimeout(vc.emptyTimer);
  const silenceMs = stripResumePhrase(vc.text) !== null ? RESUME_SILENCE_MS : Number(store.get('voice2-long-silence-s', LONG_SILENCE_DEFAULT_S)) * 1000;
  if (vc.text.trim() && silenceMs > 0) vc.timer = setTimeout(finishVcReply, silenceMs);
  else if (!vc.text.trim()) vc.emptyTimer = setTimeout(closeEmptySession, EMPTY_SESSION_MS);
}
/** A pause with nothing said after it never hangs: close it and carry on. */
function closeEmptySession() {
  if (!vc || vc.text.trim()) return;
  if (!vc.sawCommand && (vc.parts ?? []).some(p => p.text === null && !p.sentAway)) { sendVoiceReplyNow('clips could not be transcribed here'); return; }   // offline: the audio is sent and transcribed by the server later
  log('vc', 'empty session timed out: resuming');
  vc = null; recorder.setReplyMode(false); recorder.beep('done');
  player.unlock();
  resumeAfterPause();
  vcIdle();
}

/** The resume phrase was followed by silence: drop it, send the reply if there is one, then resume reading. */
/** The end word was heard: no quiet window, no waiting for transcripts. The clip in progress is cut here and the service transcribes whatever is not yet text. */
const END_WAIT_MS = 2500;   // clips already being transcribed finish sooner than a fresh upload + transcription of the same audio would
async function sendVoiceReplyNow(why) {
  const reply = vc; if (!reply || reply.ending) return;
  reply.ending = true;
  recorder.flush();   // hands the last clip to onCommandClip, which records it in reply.parts
  clearTimeout(reply.timer); clearTimeout(reply.emptyTimer);
  const waitStart = performance.now();
  while (performance.now() - waitStart < END_WAIT_MS && (reply.parts ?? []).some(p => p.text === null)) await new Promise(r => setTimeout(r, 40));
  await new Promise(r => setTimeout(r, 0));   // lets the session take the text of the clip that just finished
  if (vc !== reply) return;
  clearTimeout(reply.timer); clearTimeout(reply.emptyTimer);   // transcripts that finished during the wait re-armed them
  if (performance.now() - waitStart > 50) log('vc', `waited ${Math.round(performance.now() - waitStart)}ms for clips already in transcription`);
  if (why.includes('on device')) { const w = reply.text.trim().split(/\s+/); if (w.length && looksLikeClippedEndWord(w.at(-1))) { log('vc', `dropped the clipped end word "${w.at(-1)}"`); reply.text = w.slice(0, -1).join(' '); } }
  const items = [];
  if (reply.base.trim()) items.push({text: reply.base.trim()});
  if (reply.text.trim()) items.push({text: reply.text.trim()});   // everything already transcribed, in arrival order
  for (const p of reply.parts ?? []) { p.sentAway = true; if (p.text === null && !reply.sawCommand) items.push({blob: p.blob}); }   // after a command word the audio may hold commands: only text already transcribed is trusted   // clips still waiting for text go as audio
  vc = null; recorder.setReplyMode(false); lastReplyEndMs = Date.now();
  log('vc', `${why}: sending now (${items.filter(i => i.blob).length} clips for the server to transcribe)`);
  recorder.beep('done');
  if (!items.some(i => i.blob || (i.text && i.text.trim()))) { resumeAfterPause(); vcIdle(); return; }
  const local = {localId: `l${Date.now()}_${localCounter++}`, sendId: 'send_' + Array.from(crypto.getRandomValues(new Uint8Array(9)), b => b.toString(16).padStart(2, '0')).join(''), topic: current, text: reply.text || '…', items, state: 'sending', sentAtMs: Date.now()};
  lat.begin('send:' + local.localId, 'send-voice');
  locals.set(current, [...(locals.get(current) ?? []), local]); saveOutbox();
  feed.querySelector('#empty')?.remove(); feed.append(localEl(local)); feed.scrollTop = feed.scrollHeight;
  draftEl.value = ''; autosize(); saveDraft();
  setVcState('sending', 'sending');
  deliver(local);
  player.unlock(); resumeAfterPause(); vcIdle();
}

function finishVcReply() {
  if (!vc || vc.ending) return;   // a timer re-armed while a send was waiting for clips must not fire on a closed session
  if (!vc.sawCommand && (vc?.parts ?? []).some(p => p.text === null && !p.sentAway)) { sendVoiceReplyNow('clips still untranscribed'); return; }
  const reply = vc, stripped = stripResumePhrase(reply.text) ?? reply.text;
  vc = null; recorder.setReplyMode(false); lastReplyEndMs = Date.now();
  draftEl.value = [reply.base.trim(), stripped].filter(Boolean).join(' '); autosize(); saveDraft();
  log('vc', `resume phrase + silence: reply "${stripped}"`);
  recorder.beep('done');
  const key = normalize(stripped);
  const duplicate = key && lastVoiceReply.key === key && Date.now() - lastVoiceReply.ms < 120000;   // never send the same reply twice
  if (duplicate) log('vc', 'duplicate reply suppressed');
  if (stripped && !duplicate) {
    lastVoiceReply = {key, ms: Date.now()}; setVcState('sending', 'sending'); send(); }
  player.unlock();
  resumeAfterPause();
  vcIdle();
}
async function loadKws(retry = true) {
  const r = await kwsSpotter.load();
  if (r.ok) prebuildCommands();
  if (!r.ok && retry && voiceControl && kwsEnabled()) { setTimeout(() => { if (voiceControl && kwsEnabled() && !kwsSpotter.ready()) loadKws(false); }, 8000); }
  if (!r.ok && voiceControl) { toast('On-device start word failed to load; using the server (slower)'); setTimeout(() => $('set-kws').checked = kwsEnabled(), 0); }
}
async function setVoiceControl(on) {
  voiceControl = on; voiceListening = on; store.set('voice2-vc', on);
  $('set-voice').checked = on; if (on) vcIdle(); else setVcState('off', 'off');
  if (!on) { recorder.listen(false); kwsSpotter.unload(); return; }
  if (kwsEnabled()) loadKws(); else log('kws', 'on-device start word is OFF in Settings: start words take the server path (slower)');   // opt-in beta, lazy: the model (about 21 MB over the wire, once) loads only when voice control is switched on
  const r = await recorder.listen(true);
  if (r.ok && !r.running) { voiceListening = false; setVcState('waiting', 'tap'); log('vc', 'armed, waiting for a tap'); return; }
  if (!r.ok) { toast(r.error); player.say('Microphone stopped. Tap the microphone button to restart.'); voiceControl = false; store.set('voice2-vc', false); $('set-voice').checked = false; setVcState('off', 'off'); }
  log('vc', `voice control ${r.ok ? 'on' : 'failed'}`);
  if (r.ok && r.running) recorder.beep('ready');
}
$('set-phrase').value = store.get('voice2-resume-phrase', RESUME_DEFAULT);
$('set-phrase').onchange = e => store.set('voice2-resume-phrase', e.target.value.trim() || RESUME_DEFAULT);
$('set-start').value = store.get('voice2-start-word', START_DEFAULT);
$('set-cmd').value = store.get('voice2-cmd-word', CMD_DEFAULT);
$('set-cmd').onchange = e => store.set('voice2-cmd-word', e.target.value.trim().toLowerCase() || CMD_DEFAULT);
$('set-start').onchange = e => store.set('voice2-start-word', e.target.value.trim().toLowerCase() || START_DEFAULT);
$('set-silence').value = store.get('voice2-long-silence-s', LONG_SILENCE_DEFAULT_S);
$('set-silence').onchange = e => store.set('voice2-long-silence-s', Math.max(0, Number(e.target.value) || 0));
$('set-kws').checked = kwsEnabled();
$('set-kws').onchange = e => { store.set('voice2-kws', e.target.checked); if (e.target.checked && voiceControl) loadKws(); else if (!e.target.checked) kwsSpotter.unload(); };
$('set-voice').onchange = e => setVoiceControl(e.target.checked);
$('listen').onclick = e => { e.stopPropagation(); setVoiceControl(!voiceControl); toast(voiceControl ? 'Voice control on: say attention, then your reply, and finish with "tomato"' : 'Voice control off'); };
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
  if (vc) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); vc = null; recorder.setReplyMode(false); log('vc', 'draft sent by hand: voice session closed'); }   // a later timer must not send it again
  draftEl.value = '';
  saveDraft(); autosize();
  const local = {localId: `l${Date.now()}_${localCounter++}`, topic: current, text, state: 'sending', sentAtMs: Date.now()};
  lat.begin('send:' + local.localId, local.items ? 'send-voice' : 'send-text');
  locals.set(current, [...(locals.get(current) ?? []), local]);
  saveOutbox();
  feed.querySelector('#empty')?.remove();
  feed.append(localEl(local));
  feed.scrollTop = feed.scrollHeight;
  deliver(local);
}

/** Unsent messages survive a reload: queued/sending/failed locals are saved (voice clips as base64 WAV) and re-sent with the same id. */
const OUTBOX_KEY = 'voice2-outbox';
const blobToB64 = blob => new Promise(res => { const r = new FileReader(); r.onload = () => res(String(r.result).split(',')[1]); r.readAsDataURL(blob); });
const b64ToBlob = b64 => new Blob([Uint8Array.from(atob(b64), c => c.charCodeAt(0))], {type: 'audio/wav'});
let outboxTimer = 0;
function saveOutbox() {
  clearTimeout(outboxTimer);
  outboxTimer = setTimeout(async () => {
    const list = [];
    for (const l of [...locals.values()].flat()) {
      if (l.state === 'sent') continue;
      list.push({sendId: l.sendId ?? null, localId: l.localId, topic: l.topic, text: l.text, sentAtMs: l.sentAtMs, items: l.items ? await Promise.all(l.items.map(async i => i.blob ? {b64: await blobToB64(i.blob), uri: i.uri} : {text: i.text})) : null});
    }
    store.set(OUTBOX_KEY, list.slice(-10));
  }, 300);
}
async function restoreOutbox() {
  for (const e of store.get(OUTBOX_KEY, [])) {
    const items = e.items?.map(i => i.b64 ? {blob: b64ToBlob(i.b64), uri: i.uri} : {text: i.text});
    const l = {localId: e.localId, sendId: e.sendId ?? undefined, topic: e.topic, text: e.text, items, state: 'queued', sentAtMs: e.sentAtMs};
    if (!l.items) delete l.items;
    locals.set(e.topic, [...(locals.get(e.topic) ?? []), l]);
    log('send', `restored unsent ${e.sendId ?? e.localId} from the previous session`);
    deliver(l);
  }
  if (current) renderFeed();
}

function removeLocal(l) { locals.set(l.topic, (locals.get(l.topic) ?? []).filter(x => x !== l)); l.node?.remove(); saveOutbox(); }
function refreshLocal(l) { if (l.topic !== current) return; const old = l.node; const fresh = localEl(l); old?.replaceWith(fresh); }

/** Sends `l`; failures requeue it with backoff (and at once when the connection returns) so it never needs a manual retry unless 8 attempts failed. */
async function deliver(l) {
  if (l.inflight) return;
  l.inflight = true; l.state = 'sending'; refreshLocal(l); saveOutbox();
  try {
    if (l.uncertain && !l.items) {
      const found = await zulip.call('GET', 'messages', {narrow: topicQuery(l.topic), anchor: 'newest', num_before: 5, num_after: 0, apply_markdown: false});
      if (found.ok && found.data.messages.some(m => m.sender_id === cfg.humanId && m.content === l.text && m.timestamp * 1000 > l.sentAtMs - 5000)) { log('send', 'uncertain send was delivered'); removeLocal(l); for (const m of found.data.messages) if (model.add(m) && l.topic === current) appendMessage(m); return; }
    }
    if (l.items) {   // a voice reply: the service joins the transcripts and posts it, so nothing here waits for text
      const v = await voice.sendVoice({sendId: l.sendId, topic: topicOf(l.topic), streamId: streamOf(l.topic), items: l.items, cancelWord: cancelWord(), commandWord: commandWord()});
      if (v.ok) { lat.finish('send:' + l.localId, 'server-posted'); l.state = 'sent'; removeLocal(l); saveOutbox(); if (v.empty) toast(v.discarded ? 'Reply cancelled' : 'Nothing was recognised in that recording'); return; }
      l.attempts = (l.attempts ?? 0) + 1;
      if (l.attempts <= 12) { l.state = 'queued'; refreshLocal(l); setTimeout(() => { if (l.state === 'queued') deliver(l); }, Math.min(1500 * 2 ** l.attempts, 15000)); }
      else { l.state = 'failed'; refreshLocal(l); toast(v.error); }
      return;
    }
    const r = await zulip.call('POST', 'messages', {type: 'stream', to: streamOf(l.topic), topic: topicOf(l.topic), content: l.text, queue_id: events.queueId() ?? '', local_id: l.localId});
    if (r.ok) { lat.stage('send:' + l.localId, 'post'); l.state = 'sent'; l.sentId = r.data.id; saveOutbox(); setTimeout(() => { if ((locals.get(l.topic) ?? []).includes(l)) removeLocal(l); }, 4000); l.node?.querySelector('.state')?.replaceChildren(); l.node?.classList.remove('sending'); return; }
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
/** Tapping the feed: while audio plays, a tap on empty space or on the playing sentence pauses (another sentence seeks); while paused, a tap on empty space resumes and a tap on a sentence reads from there; idle, a tap on a message reads it. */
function flash(text) { const el = $('tap-flash'); el.textContent = text; el.classList.remove('show'); void el.offsetWidth; el.classList.add('show'); }
feed.addEventListener('click', event => {
  if (event.target.closest('a, button, pre, .att, .footer') || String(getSelection()).length) return;
  const status = player.position().status;
  const node = event.target.closest('.msg.agent');
  const span = event.target.closest('.s');
  const other = span && node && !span.classList.contains('now');   // a different sentence still seeks
  if ((status === 'playing' || status === 'loading') && !other) { player.unlock(); player.pause(); flash('❚❚'); return; }
  if (span && node) { const m = model.messages(current).find(x => x.id === +node.dataset.id); playFrom(m, +span.dataset.from); return; }
  if (status === 'paused') { player.unlock(); resumeAfterPause(); flash('▶'); return; }
  if (node) { const m = model.messages(current).find(x => x.id === +node.dataset.id); playFrom(m, 0); }
});
// iOS only treats touchend/click as audio-unlocking gestures (not pointerdown); autoplay starts after the tap's own handlers ran.
for (const type of ['pointerup', 'touchend', 'click']) addEventListener(type, e => { if (e.target.closest?.('#listen')) return; if (!player.isUnlocked()) { player.unlock(); setTimeout(maybeAutoplay); } if (voiceControl && !recorder.active() && !voiceListening) setVoiceControl(true); }, {capture: true});

let pickerClosedMs = 0;
$('picker').addEventListener('close', () => { pickerClosedMs = Date.now(); });
// Enter in the search box closes the dialog and focus returns to the button, whose key-up would reopen it: ignore that click.
$('agent').onclick = $('find').onclick = () => { if (Date.now() - pickerClosedMs < 400) return; $('search').value = ''; openPicker(); };
$('search').oninput = openPicker;
$('search').onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); const first = rankTopics($('search').value)[0]; if (first) switchTopic(first.t.name); } };
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
$('set-hide-att').checked = hideAllAttachments.on;
$('set-hide-att').onchange = e => { hideAllAttachments.on = e.target.checked; if (e.target.checked) try { localStorage.removeItem('voice2-att-expanded'); } catch { /* ignore */ } store.set('voice2-hide-att', e.target.checked ? '1' : '0'); try { localStorage.setItem('voice2-hide-att', e.target.checked ? '1' : '0'); } catch { /* ignore */ } renderFeed(); };
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
    `ua: ${navigator.userAgent}`,
    `page: ${document.visibilityState}, navigator.onLine=${navigator.onLine}`,
    `events: ${info.state} queue=${info.queueId} lastOk=${info.lastOkAgoMs ?? '-'}ms fails=${info.failures} poll=${info.pollAgeMs ?? '-'}ms`,
    `on-device start word: setting ${kwsEnabled() ? 'on' : 'OFF'}, ${kwsSpotter.state().status}${kwsSpotter.state().detail ? ' (' + kwsSpotter.state().detail + ')' : ''}`,
    (st => `stt: n=${st.n} p50=${st.p50}ms p90=${st.p90}ms server-work p50=${st.workP50}ms${st.n >= 3 && st.p50 > 1500 ? '  WARNING: speech-to-text is slow (>1.5 s), the server GPU is probably busy' : ''}`)(voice.sttStats()),
    `voice: ${voiceState.text}; prepared=${voice.preparedCount()} pending=${voice.pendingCount()}`,
    `player: ${JSON.stringify(player.snapshot())}`,
    `api: ${zulip.callsLastMinute()} calls in the last minute${zulip.rateLimitedForMs() ? '; RATE LIMITED for ' + Math.ceil(zulip.rateLimitedForMs() / 1000) + 's' : ''}`,
    ...lat.summary().map(l => 'latency ' + l),
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

function persist() { prebuildCommands(); clearTimeout(persist.t); persist.t = setTimeout(() => store.set('voice2-cache', model.snapshot(current)), 800); }

const events = createEvents({zulip, log, eventTypes: ['message', 'update_message_flags', 'update_message', 'delete_message'], onInit, onEvent, onState: renderConn});

// Blank-feed watchdog and uncaught errors ship the debug log to the server.
let lastErrLogMs = 0;
const reportProblem = why => { if (!cfg.relayStreamId || Date.now() - lastErrLogMs < 60000) return; lastErrLogMs = Date.now(); log('app', 'PROBLEM ' + why); sendLog('problem: ' + why.slice(0, 40)); };
addEventListener('error', e => reportProblem(e.message));
addEventListener('unhandledrejection', e => reportProblem(String(e.reason?.message ?? e.reason)));
setInterval(() => { if (current && !feed.querySelector('.msg, #empty, .notice')) { reportProblem('blank feed'); renderFeed(); } }, 3000);

// ---- screen wake lock: keep the screen on while voice control is on or audio plays ----
let wakeLock = null;
async function syncWakeLock() {
  const want = voiceControl || ['playing', 'loading'].includes(player.position().status) || (autoplay && model.unreadAgentMessages(current).length > 0);
  if (!navigator.wakeLock) return;
  if (!want || document.visibilityState !== 'visible') { if (wakeLock) { wakeLock.release().catch(() => {}); wakeLock = null; log('wake', 'released'); } return; }
  if (wakeLock) return;
  try { wakeLock = await navigator.wakeLock.request('screen'); log('wake', 'screen wake lock acquired'); wakeLock.addEventListener('release', () => { wakeLock = null; }); }
  catch (error) { log('wake', `not granted: ${error.name}`); }
}
setInterval(syncWakeLock, 2000);
document.addEventListener('visibilitychange', syncWakeLock);

// ---- start ----
renderRate(); renderAuto(); renderPrevButton(); $('prev-agent').onclick = switchBack;
// A saved Voice control setting re-arms after reload: try at once, else on the first tap anywhere.
if (voiceControl) { $('set-voice').checked = true; setVcState('waiting', 'tap'); setVoiceControl(true); }
if (current) { draftEl.value = store.get('voice2-draft:' + current, ''); renderFeed(); renderUnread(); }
autosize();
renderConn(events.info());
events.start();
restoreOutbox();
if (cfg.relayStreamId) {
  ensureRelayMuted(zulip, cfg.relayStreamId);
  setTimeout(checkVoice, 1500);
} else voiceState = {ok: false, text: 'voice-relay channel not found'};
window.__voice = {model, player, events, voice, zulip, log, cfg: () => ({...cfg, key: '…'}), current: () => current};
