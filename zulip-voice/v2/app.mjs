/** Wires the model, Zulip events, voice service, player, voice control and DOM: feed, playback order, read state, switching, sending. Behavior is described in `design.md`. */
import {createLog} from './log.mjs';
import {createLatency} from './latency.mjs';
import {createZulip, createEvents} from './zulip.mjs';
import {createVoice} from './voice.mjs';
import {createPlayer} from './player.mjs';
import {EARCON_NOTES, EARCON_NOTE_S} from './recorder.mjs';
import {createModel, parsed, itemsOf} from './model.mjs';
import {hideAllAttachments, agentEl, mineEl, setUnread, topicRow, ago} from './view.mjs';
import {store} from './store.mjs';
import {ACCOUNT_KEY, login, completeAccount, ensureRelayMuted} from './account.mjs';
import {$, toast, showBanner, hideBanner, bannerKind, flash, drawLevel} from './chrome.mjs';
import {createReads} from './reads.mjs';
import {createVoiceControl} from './voicecontrol.mjs';
import {createDictation} from './dictation.mjs';
import {rankTopics, similarText} from './grammar.mjs';

const BUILD = 'v2.0';
const log = createLog();
const lat = createLatency(log);
addEventListener('error', e => log('err', `${e.message} @${e.filename?.split('/').pop()}:${e.lineno}`));
addEventListener('unhandledrejection', e => log('err', `unhandled: ${e.reason?.message ?? e.reason} @ ${String(e.reason?.stack ?? '').split('\n').slice(0, 3).join(' | ')}`));   // the stack says where, for the next report

// ---- boot ----
let cfg = store.get(ACCOUNT_KEY, null) ?? await login(store, log);
if (new URL(cfg.site).protocol !== 'https:') throw Error('Zulip must use HTTPS');
const zulip = createZulip(cfg, log);
$('app').hidden = false;
cfg = await completeAccount(cfg, zulip, store);

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
const reads = createReads({zulip, model, store, onChange: () => renderUnread()});
const markRead = reads.markRead;

// ---- channels: AMH plus the other subscribed channels the user includes (Settings). A non-AMH topic's key is "channel · topic". ----
const channelNames = new Map(Object.entries(store.get('voice2-subs', {})).map(([id, n]) => [Number(id), n]));   // stream id -> name, from the subscriptions (cached)
const keyInfo = new Map();   // topic key -> {streamId, topic}
const isOurChannel = id => id !== cfg.streamId && id !== cfg.relayStreamId && channelNames.has(id) && includedChannels().includes(id);
/** Subscribed channels offered by default: everything except AMH, the relay and test channels. */
const mutedChannels = new Set(store.get('voice2-muted-subs', []));
const defaultChannels = () => [...channelNames].filter(([id, n]) => !mutedChannels.has(id) && id !== cfg.streamId && id !== cfg.relayStreamId && !/^(voice-|vtest)/i.test(n)).map(([id]) => id);
const includedChannels = () => store.get('voice2-channels', null) ?? defaultChannels();   // no UI: only tests pin it; default: every subscribed, non-muted channel except the relay and test ones: all topics in one recency-sorted list
const keyFor = (streamId, topic) => { if (streamId === cfg.streamId) return topic; const key = `${channelNames.get(streamId)} · ${topic}`; keyInfo.set(key, {streamId, topic}); return key; };
/** A message with its topic replaced by the key (the model, caches and the UI only know keys). */
const ing = m => m.stream_id === cfg.streamId || m.t0 !== undefined ? m : {...m, t0: m.subject, subject: keyFor(m.stream_id, m.subject)};
const streamOf = key => keyInfo.get(key)?.streamId ?? cfg.streamId, topicOf = key => keyInfo.get(key)?.topic ?? key;

const cache = store.get('voice2-cache', null);
if (cache) {   // an older build cached channel-topic messages twice (also without their "channel · topic" key): drop the unkeyed copies and the rows they made
  const keyed = new Set();
  for (const list of Object.values(cache.msgs ?? {})) for (const m of list) if (m.t0 !== undefined) keyed.add(m.id);
  for (const [name, list] of Object.entries(cache.msgs ?? {})) {
    cache.msgs[name] = list.filter(m => m.t0 !== undefined || !keyed.has(m.id));
    if (!cache.msgs[name].length) { delete cache.msgs[name]; cache.topics = (cache.topics ?? []).filter(t => t.name !== name); }
  }
  for (const list of Object.values(cache.msgs ?? {})) for (const m of list) if (m.t0 !== undefined) keyInfo.set(m.subject, {streamId: m.stream_id, topic: m.t0}); model.restore(cache); current ??= cache.current; }

const player = createPlayer({
  log, voice,
  onItemStart(item) { log('audio', `start ${item.key}`); lat.finish('play:' + item.msgId, 'ready+playing'); if (burst?.pending) return; store.set('voice2-pos', {topic: current, msgId: item.msgId, idx: item.idx}); store.set('voice2-last', {topic: current, msgId: item.msgId, idx: item.idx}); if (after(item, watermark())) store.set(wmKey(current), {msgId: item.msgId, idx: item.idx}); pausedItem = {topic: current, msgId: item.msgId, idx: item.idx}; const before = model.unreadAgentMessages(current).filter(m => m.id < item.msgId).map(m => m.id); if (before.length) markRead(before); nowMsgId = item.msgId; nowIdx = item.idx; highlight(); if (follow) scrollToNow(); updateJump(); setMediaSession(item); },
  onItemEnd(item) {
    if (!item.last) { if (burst) log('read', `chunk ${item.key} ended mid-skip (not last)`); return; }
    store.set('voice2-pos', null); if (pausedItem?.msgId === item.msgId) pausedItem = null;
    const played = model.find(item.msgId), wasUnread = played && !played.flags?.includes('read');
    if (wasUnread && !pendingSwitch && parsed(played).switchTo && !hasActed(item.msgId)) pendingSwitch = {to: parsed(played).switchTo, msgId: item.msgId};   // also for messages played from the backlog after a reload
    log('read', `message ${item.msgId} finished${wasUnread ? '' : ' (was already read)'}`);
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
addEventListener('att-log', e => log('att', e.detail));
async function attUrl(uri, fresh = false) {
  const hit = attCache.get(uri);
  if (!fresh && hit && Date.now() - hit.at < 50000) return hit.r;
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
  updateJump();
}
/** The "jump to playing" pill shows exactly when the playing chunk is outside the visible feed, recomputed on every chunk change, scroll and layout change. */
function updateJump() {
  const n = nowMsgId === null ? null : feed.querySelector('.s.now');
  if (!n) { $('jump').hidden = true; return; }
  const box = feed.getBoundingClientRect(), r = n.getBoundingClientRect();
  $('jump').hidden = r.bottom > box.top && r.top < box.bottom;
}

function scrollToNow(force = false) {
  const target = feed.querySelector('.s.now');
  if (!target) return;
  pinUntilMs = 0; cancelAnimationFrame(pinTimer);   // the playing place wins over "stay at the bottom" (after a switch the pin held the feed at the end for 2 s)
  force = force || Date.now() - lastSwitchMs < 5000;   // right after a switch: jump, do not glide
  const box = feed.getBoundingClientRect(), r = target.getBoundingClientRect();
  if (!force && r.top >= box.top + 60 && r.bottom <= box.bottom - 20) return;
  feed.scrollTo({top: feed.scrollTop + r.top - box.top - box.height * 0.3, behavior: force || Math.abs(r.top - box.top) > box.height * 3 ? 'auto' : 'smooth'});
}

feed.addEventListener('click', e => {
  const b = e.target.closest?.('button.mark-read'); if (!b) return;
  const id = +b.dataset.markRead; log('read', `marked read by hand: ${id}`);
  const playing = nowMsgId === id && player.position().status !== 'idle'; if (playing) player.stop();
  markRead([id]); renderUnread(); toast('Marked read');
});
function renderUnread() {
  for (const [id, node] of els) { const m = model.messages(current).find(x => x.id === id); if (m && model.isAgent(m)) setUnread(node, !m.flags?.includes('read')); }
  const other = model.topics().filter(t => t.name !== current).reduce((n, t) => n + t.unread, 0);
  $('agent-unread').hidden = !other;
  $('agent-unread').textContent = other;
  renderNextButton();
}


// ---- data loading ----
const topicQuery = key => [{operator: 'stream', operand: streamOf(key)}, {operator: 'topic', operand: topicOf(key)}];

/** Loads recent messages of `topic`; returns the newly seen ones. */
async function loadTopic(topic) {
  const r = await zulip.call('GET', 'messages', {narrow: topicQuery(topic), anchor: 'newest', num_before: 40, num_after: 0, apply_markdown: false});
  if (!r.ok) return [];
  const fresh = r.data.messages.map(ing).filter(m => model.add(m));
  model.markRead(reads.pending());   // our reads the server has not confirmed yet stay read
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
  if (r.ok) for (const m of r.data.messages) model.add(ing(m));
  for (const id of includedChannels()) {   // one call per extra channel buckets all its topics
    if (!channelNames.has(id)) continue;
    const x = await zulip.call('GET', 'messages', {narrow: [{operator: 'stream', operand: id}], anchor: 'newest', num_before: 300, num_after: 0, apply_markdown: false});
    if (x.ok) for (const m of x.data.messages) model.add(ing(m));
  }
  model.markRead(reads.pending());
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
  { const unreadIds = new Set(); for (const st of state.unread_msgs?.streams ?? []) for (const id of st.unread_message_ids ?? []) unreadIds.add(id);
    const changed = model.reconcileUnread(unreadIds, m => model.isAgent(m) && !reads.has(m.id));   // cached copies the server no longer lists as unread were read elsewhere
    if (changed.length) log('read', `${changed.length} cached messages were already read on the server`); }
  reads.flush();   // our own unconfirmed reads go out first
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
  } else if (event.type === 'update_message_flags' && event.flag === 'read') {
    if (event.op === 'add') {   // read on another device: never played here (a chunk already playing finishes, the rest of that message is dropped), no tone
      model.markRead(event.messages);
      reads.forget(event.messages);
      for (const id of event.messages) player.removeFuture(id);
    } else { model.markUnread(event.messages); }
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
  if (takeReplacement(m)) return;
  if (inheritReadFromDeleted(m)) return;
  lat.begin('play:' + m.id, 'playback', performance.now() - Math.max(0, Date.now() - m.timestamp * 1000));   // starts at the server's timestamp
  lat.stage('play:' + m.id, 'to-client');
  const items = itemsOf(m);
  const p = parsed(m);
  if (p.switchTo && !hasActed(m.id) && !m.flags?.includes('read')) pendingSwitch = {to: p.switchTo, msgId: items.length ? m.id : null, sourceId: m.id};
  if (!items.length) markRead([m.id]);
  else if (vctl.userSpeaking()) { /* stays unread; read after the user finishes */ }
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
  reads.forget([id]);
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
  if (model.isAgent(m)) { recentlyDeleted.push({topic, content: m.content, read: !!m.flags?.includes('read'), at: Date.now()}); if (recentlyDeleted.length > 10) recentlyDeleted.shift(); }
  model.remove(topic, id);
  { const d = recentlyDeleted.at(-1); if (d && d.read && d.topic === topic) for (const x of model.unreadAgentMessages(topic)) if (x.id > id && Math.abs(Date.now() - x.timestamp * 1000) <= REPLACEMENT_WINDOW_MS && inheritReadFromDeleted(x)) break; }   // the replacement was already here
  els.get(id)?.remove(); els.delete(id); forgetReadState(id);
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

// ---- amh replaces a message by delete + new post: a replacement of an already-read message with (nearly) the same text is read too ----
const recentlyDeleted = [];   // {topic, content, read, at}
/** True (and the message is marked read) when `m` replaces a read predecessor deleted within the window with near-identical text. */
function inheritReadFromDeleted(m) {
  const now = Date.now(), pred = recentlyDeleted.find(d => d.topic === m.subject && d.read && Math.abs(now - d.at) <= REPLACEMENT_WINDOW_MS && similarText(d.content, m.content));
  if (!pred) return false;
  log('app', `replacement ${m.id} of a read message with the same text: marked read`);
  const i = recentlyDeleted.indexOf(pred); recentlyDeleted.splice(i, 1);
  reads.markOne(m.id);
  return true;
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
function resumeAfterPause() {
  { const wait = vctl.soundLeftMs?.() ?? 0; if (wait > 0 && wait < 3000) { clearTimeout(resumeAfterPause.t); resumeAfterPause.t = setTimeout(resumeAfterPause, wait + 60); return; } }   // let a chime finish first
  player.unlock(); setFollow(true); if (nowMsgId !== null) scrollToNow();
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
  { const wait = vctl.soundLeftMs?.() ?? 0; if (wait > 0 && wait < 3000) { clearTimeout(maybeAutoplay.t); maybeAutoplay.t = setTimeout(maybeAutoplay, wait + 60); return; } }   // a chime (e.g. cancel) is still playing: reading starts after it, never over it
  if (replacementWait) return;
  if (!autoplay || !current || vctl.userSpeaking() || player.position().status !== 'idle') return;   // never start speech while the user speaks
  // Forward only: anything older than the newest read message counts as read and is never played again.
  const newestRead = model.agentMessages(current).filter(m => m.flags?.includes('read')).at(-1);
  if (newestRead) markRead(model.unreadAgentMessages(current).filter(m => m.id < newestRead.id).map(m => m.id));
  const unread = model.unreadAgentMessages(current);
  const speakable = unread.filter(m => itemsOf(m).some(pastWatermark));
  markRead(unread.filter(m => !itemsOf(m).length).map(m => m.id));
  { const stale = unread.filter(m => itemsOf(m).length && !speakable.includes(m) && m.id !== store.get('voice2-pos', null)?.msgId); if (stale.length) { log('read', `${stale.length} unread message(s) already fully started (watermark): marking read`); markRead(stale.map(m => m.id)); } }   // the last chunk started (watermark) but the player stopped or the page reloaded before it ended: it is never played again, so it must not stay unread (blue dot)
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
  setFollow(true);
  clearTimeout(burst?.timer); burst = null;
  player.stop(); nowMsgId = null; highlight();
  log('read', `end of messages: marking ${model.unreadAgentMessages(current).length} unread as read`);
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
  if (vctl.userSpeaking() || Date.now() - lastInterruptMs < 20000) return;
  lastInterruptMs = Date.now();
  player.earcon(EARCON_NOTES.interrupted, EARCON_NOTE_S);
}
/** Every second: is the user waiting for audio (loading or retrying, queue not empty)? After 5 s, a soft ambient pad fades in; it fades out when audio plays or the wait ends. */
setInterval(() => {
  const {status} = player.position(), waiting = (status === 'loading' || status === 'error' || status === 'blocked') && !vctl.userSpeaking() && !vctl.sessionOpen() && !dictation.active();
  if (!waiting) { waitingSinceMs = 0; if (player.ambientOn()) { log('audio', 'waiting sound off'); player.ambient(false); } return; }
  waitingSinceMs ||= Date.now();
  if (player.ambientKeep()) log('audio', 'waiting sound had stopped by itself: restarted');
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
  else if (bannerKind() === 'err') hideBanner();
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

// ---- switching agents ----
let previousTopic = store.get('voice2-prev', null);
function renderPrevButton() {
  const b = $('prev-agent');
  b.hidden = !previousTopic || previousTopic === current;
  b.title = b.hidden ? '' : `Back to ${previousTopic}`;
  $('prev-agent-name').textContent = previousTopic ?? '';
}
/** Next thread with unread messages, in the picker's order (after the current one, wrapping round). */
function switchNextUnread() {
  const order = rank('').map(x => x.t), at = order.findIndex(t => t.name === current);
  const next = [...order.slice(at + 1), ...order.slice(0, Math.max(0, at))].find(t => t.name !== current && t.unread > 0);
  if (!next) {   // nothing unread anywhere: still move on, to the next agent in the list order, and say so
    const plain = [...order.slice(at + 1), ...order.slice(0, Math.max(0, at))].find(t => t.name !== current);
    if (!plain) { toast('No other agent'); return false; }
    toast('No unread messages elsewhere: next agent in the list'); switchTopic(plain.name); return true;
  }
  switchTopic(next.name);
  return true;
}
function renderNextButton() { const n = model.topics().filter(t => t.name !== current && t.unread > 0).length; $('next-unread').hidden = !n; $('next-unread-count').textContent = n; }
function switchBack() {
  if (!previousTopic || previousTopic === current) { toast('No previous agent'); return false; }
  switchTopic(previousTopic);
  return true;
}

let lastSwitchMs = 0;
function switchTopic(topic, confirmed = false) {
  lastSwitchMs = Date.now();
  setFollow(true);
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
  vctl.announceSwitch(topic, confirmed);   // before any playback starts: the click or "Now on X" is sequenced ahead of the messages, never over them
  if (model.messages(topic).length) maybeAutoplay();
  loadTopicUntilLoaded(topic);
  renderUnread();
}

let loading = null;
/** Loads `topic`'s history, retrying with backoff (offline, 429) until it arrives; the feed shows "Loading…" meanwhile, never blank. */
async function loadTopicUntilLoaded(topic) {
  for (let attempt = 0; current === topic; attempt++) {
    const r = await zulip.call('GET', 'messages', {narrow: topicQuery(topic), anchor: 'newest', num_before: 40, num_after: 0, apply_markdown: false});
    if (r.ok) {
      for (const m of r.data.messages) model.add(ing(m));   // channel topics are keyed "channel · topic": adding the raw message made a second, unkeyed row
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

const rank = query => rankTopics(model.topics(), query);

function openPicker() {
  const query = $('search').value;
  const list = $('topics');
  list.replaceChildren();
  for (const {t} of rank(query).slice(0, 80)) {
    const last = model.lastMessage(t.name);
    const snip = last ? `${ago(last.timestamp)} · ${(model.isAgent(last) ? parsed(last).body || parsed(last).title : 'You: ' + last.content).replace(/\s+/g, ' ').slice(0, 90)}` : '';
    list.append(topicRow(t, snip, t.name === current, () => switchTopic(t.name)));
  }
  if (!$('picker').open) { $('picker').showModal(); fitPicker(); $('search').focus(); }
}

// ---- attachments: upload to Zulip, then the link goes into the draft like any message text ----
$('attach').onclick = () => $('attach-input').click();
$('attach-input').onchange = async e => {
  const files = [...e.target.files]; e.target.value = '';
  if (!files.length) return;
  toast(`Uploading ${files.length} file${files.length > 1 ? 's' : ''}…`);
  const links = [];
  for (const file of files) {
    const form = new FormData(); form.append('file', file, file.name || `photo-${Date.now()}`);
    const up = await zulip.call('POST', 'user_uploads', form, {timeoutMs: 120000});
    log('att', `upload ${file.name} ${file.size}B: ${up.ok ? 'ok' : up.error}`);
    if (up.ok) links.push(`[${(file.name || 'file').replace(/[\[\]]/g, '')}](${up.data.uri})`);
    else { cue('error'); toast(`Upload failed: ${file.name} (${up.error})`); }
  }
  if (!links.length) return;
  draftEl.value = [draftEl.value.trimEnd(), ...links].filter(Boolean).join('\n'); autosize(); saveDraft();
  toast(links.length > 1 ? 'Files attached: tap send' : 'File attached: tap send');
};

// ---- composer: typing and sending ----
const draftEl = $('draft');
let localCounter = 0;
function autosize() { draftEl.style.height = 'auto'; $('composer').classList.toggle('tall', dictation.active() || draftEl.value.length > 28 || draftEl.value.includes('\n')); draftEl.style.height = Math.min(draftEl.scrollHeight, innerHeight * 0.4) + 'px'; $('send').disabled = !dictation.active() && !draftEl.value.trim(); }
function saveDraft() { if (current) store.set('voice2-draft:' + current, draftEl.value); }
draftEl.addEventListener('input', () => { autosize(); saveDraft(); });
draftEl.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey && !matchMedia('(pointer: coarse)').matches) { e.preventDefault(); $('composer').requestSubmit(); } });
draftEl.addEventListener('focus', () => setTimeout(() => { if (atBottom()) feed.scrollTop = feed.scrollHeight; }, 300));

/** Navigation by voice: chunk or whole-message steps, or replay of the current chunk (dir 0). */
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

const vctl = createVoiceControl({
  store, log, lat, player, zulip, voice, model, draft: draftEl, autosize, saveDraft, navigate, send, sendLocal, switchTopic, switchBack, switchNextUnread, sendLog, maybeAutoplay, resumeAfterPause,
  current: () => current, topicOf, dictating: () => dictation.active(), onClip: (...a) => dictation.onClip(...a), onLevel: drawLevel,
});
const dictation = createDictation({
  log, player, voice, recorder: () => vctl.recorder, draft: draftEl, autosize, saveDraft, send, sendLocal, resumeAfterPause, maybeAutoplay,
  voiceDown: () => voiceState.ok === false, isLive: () => events.info().state === 'live',
});
$('composer').onsubmit = event => {
  event.preventDefault();
  if (dictation.active()) { dictation.stop({send: true}); return; }
  send();
};

/** A new outgoing message for the open topic; `items` (text and audio parts) makes it a voice reply that the service assembles. */
function newLocal(text, items) {
  const local = {localId: `l${Date.now()}_${localCounter++}`, topic: current, text, state: 'sending', sentAtMs: Date.now()};
  if (items) Object.assign(local, {items, sendId: 'send_' + Array.from(crypto.getRandomValues(new Uint8Array(9)), b => b.toString(16).padStart(2, '0')).join('')});
  return local;
}
/** Shows `local` at the end of the feed, saves it to the outbox and delivers it. */
function enqueue(local) {
  lat.begin('send:' + local.localId, local.items ? 'send-voice' : 'send-text');
  locals.set(current, [...(locals.get(current) ?? []), local]);
  saveOutbox();
  feed.querySelector('#empty')?.remove();
  feed.append(localEl(local));
  feed.scrollTop = feed.scrollHeight;
  deliver(local);
  return local;
}
function sendLocal(text, items) { return enqueue(newLocal(text, items)); }
function send() {
  const text = draftEl.value.trim();
  if (!text || !current) return;
  vctl.closeForManualSend();
  draftEl.value = '';
  saveDraft(); autosize();
  enqueue(newLocal(text));
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
      const v = await voice.sendVoice({sendId: l.sendId, topic: topicOf(l.topic), streamId: streamOf(l.topic), items: l.items, cancelWord: vctl.cancelWord(), commandWord: vctl.commandWord()});
      if (v.ok) { lat.finish('send:' + l.localId, 'server-posted'); l.state = 'sent'; removeLocal(l); saveOutbox(); if (v.empty) { cue('empty'); toast(v.discarded ? 'Reply cancelled' : 'Nothing was recognised in that recording'); } return; }
      l.attempts = (l.attempts ?? 0) + 1;
      if (l.attempts <= 12) { l.state = 'queued'; refreshLocal(l); setTimeout(() => { if (l.state === 'queued') deliver(l); }, Math.min(1500 * 2 ** l.attempts, 15000)); }
      else { l.state = 'failed'; refreshLocal(l); cue('error'); toast(v.error); }
      return;
    }
    const r = await zulip.call('POST', 'messages', {type: 'stream', to: streamOf(l.topic), topic: topicOf(l.topic), content: l.text, queue_id: events.queueId() ?? '', local_id: l.localId});
    if (r.ok) { lat.stage('send:' + l.localId, 'post'); l.state = 'sent'; l.sentId = r.data.id; saveOutbox(); setTimeout(() => { if ((locals.get(l.topic) ?? []).includes(l)) removeLocal(l); }, 4000); l.node?.querySelector('.state')?.replaceChildren(); l.node?.classList.remove('sending'); return; }
    l.uncertain = r.uncertain;
    l.attempts = (l.attempts ?? 0) + 1;
    if (l.attempts <= 8) { l.state = 'queued'; refreshLocal(l); setTimeout(() => { if (l.state === 'queued') deliver(l); }, Math.min(1500 * 2 ** l.attempts, 15000)); }
    else { l.state = 'failed'; refreshLocal(l); cue('error'); toast(r.error); }
  } finally { l.inflight = false; }
}

// ---- controls ----
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
const setFollow = on => { follow = on; updateJump(); };
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
let jumpFrame = 0;
const scheduleJump = () => { if (!jumpFrame) jumpFrame = requestAnimationFrame(() => { jumpFrame = 0; updateJump(); }); };
feed.addEventListener('scroll', scheduleJump, {passive: true});
addEventListener('resize', scheduleJump);
new MutationObserver(scheduleJump).observe(feed, {childList: true, subtree: true});
/** Tapping the feed: while audio plays, a tap on empty space or on the playing sentence pauses (another sentence seeks); while paused, a tap on empty space resumes and a tap on a sentence reads from there; idle, a tap on a message reads it. */
feed.addEventListener('click', event => {
  if (event.target.closest('a, button, pre, .att, .footer') || String(getSelection()).length) return;
  const status = player.position().status;
  const node = event.target.closest('.msg.agent');
  const span = event.target.closest('.s');
  const other = span && node && !span.classList.contains('now');   // a different sentence still seeks
  if ((status === 'playing' || status === 'loading') && !other) { player.unlock(); player.pause(); flash('❚❚'); return; }
  if (span && node) { const m = model.messages(current).find(x => x.id === +node.dataset.id); clearTimeout(burst?.timer); burst = null; playFrom(m, +span.dataset.from); return; }
  if (status === 'paused') { player.unlock(); resumeAfterPause(); flash('▶'); return; }
  if (node) { const m = model.messages(current).find(x => x.id === +node.dataset.id); playFrom(m, 0); }
});
// iOS only treats touchend/click as audio-unlocking gestures (not pointerdown); autoplay starts after the tap's own handlers ran.
for (const type of ['pointerup', 'touchend', 'click']) addEventListener(type, e => { if (e.target.closest?.('#listen')) return; if (!player.isUnlocked()) { player.unlock(); setTimeout(maybeAutoplay); } vctl.onGesture(); }, {capture: true});

let pickerClosedMs = 0;
$('picker').addEventListener('close', () => { pickerClosedMs = Date.now(); });
/** The keyboard shrinks the visual viewport (and iOS may scroll it): keep the picker inside it, so no gap opens between the picker and the keyboard. */
function fitPicker() {
  const p = $('picker'), v = window.visualViewport;
  if (!p.open || !v) return;
  p.style.position = 'fixed'; p.style.top = v.offsetTop + 'px'; p.style.margin = '0 auto'; p.style.maxHeight = Math.max(160, v.height - 8) + 'px';
  p.firstElementChild.style.maxHeight = p.style.maxHeight;
}
window.visualViewport?.addEventListener('resize', fitPicker);
window.visualViewport?.addEventListener('scroll', fitPicker);
$('picker').addEventListener('toggle', fitPicker);
/** Sticky: while the picker is open the page behind it cannot scroll (iOS otherwise drags the whole menu with the page), and only the topic list scrolls. */
$('picker').addEventListener('toggle', () => document.documentElement.classList.toggle('picker-open', $('picker').open));
$('picker').addEventListener('close', () => { document.documentElement.classList.remove('picker-open'); scrollTo(0, 0); });
$('picker').addEventListener('touchmove', e => { if (!e.target.closest('#topics')) e.preventDefault(); }, {passive: false});   // drags on the search row or backdrop do nothing
$('picker').addEventListener('click', e => { if (e.target === $('picker')) $('picker').close(); });
window.visualViewport?.addEventListener('scroll', () => { if ($('picker').open && visualViewport.offsetTop) scrollTo(0, 0); });
// Enter in the search box closes the dialog and focus returns to the button, whose key-up would reopen it: ignore that click.
$('agent').onclick = $('find').onclick = () => { if (Date.now() - pickerClosedMs < 400) return; $('search').value = ''; openPicker(); };
$('search').oninput = openPicker;
$('search').onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); const first = rank($('search').value)[0]; if (first) switchTopic(first.t.name); } };
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
let lastState = 'connecting', lastAutoLogMs = 0, wasLimited = false;
/** Every problem has its own sound (600-1600 Hz, played on the app's audio element): empty = nothing was sent, error = gave up, limited = Zulip rate limit. */
function cue(kind) { player.earcon(EARCON_NOTES[kind], EARCON_NOTE_S); }
function renderConn(info) {
  // After an outage the timeline is the evidence; send it once (at most every 2 min).
  if (info.state === 'live' && lastState !== 'live' && lastState !== 'connecting' && Date.now() - lastAutoLogMs > 120000 && cfg.relayStreamId) { lastAutoLogMs = Date.now(); sendLog('reconnected after ' + lastState); }
  lastState = info.state;
  const limited = !!zulip.rateLimitedForMs(); if (limited && !wasLimited) cue('limited'); wasLimited = limited;
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
    vctl.debugLine(),
    (st => `stt: n=${st.n} p50=${st.p50}ms p90=${st.p90}ms server-work p50=${st.workP50}ms${st.n >= 3 && st.p50 > 1500 ? '  WARNING: speech-to-text is slow (>1.5 s), the server GPU is probably busy' : ''}`)(voice.sttStats()),
    `voice: ${voiceState.text}; prepared=${voice.preparedCount()} pending=${voice.pendingCount()}`,
    `player: ${JSON.stringify(player.snapshot())}`,
    `api: ${zulip.callsLastMinute()} calls in the last minute${zulip.rateLimitedForMs() ? '; RATE LIMITED for ' + Math.ceil(zulip.rateLimitedForMs() / 1000) + 's' : ''}`,
    ...lat.summary().map(l => 'latency ' + l),
    `view: topic=${current} msgs=${model.messages(current).length} unread=${model.unreadAgentMessages(current).length} autoplay=${autoplay} readQueue=${reads.size()}`,
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
$('open-keys').onclick = () => { $('settings-sheet').close(); $('keys-sheet').showModal(); };
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
  if (r.ok && r.priorities) { model.setPriorities(r.priorities); renderUnread(); }   // human-manager order of the agent list
  log('voice', voiceState.text);
  return voiceState;
}

function persist() { vctl.prebuildCommands(); clearTimeout(persist.t); persist.t = setTimeout(() => store.set('voice2-cache', model.snapshot(current)), 800); }

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
  const want = vctl.enabled() || ['playing', 'loading'].includes(player.position().status) || (autoplay && model.unreadAgentMessages(current).length > 0);
  if (!navigator.wakeLock) return;
  if (!want || document.visibilityState !== 'visible') { if (wakeLock) { wakeLock.release().catch(() => {}); wakeLock = null; log('wake', 'released'); } return; }
  if (wakeLock) return;
  try { wakeLock = await navigator.wakeLock.request('screen'); log('wake', 'screen wake lock acquired'); wakeLock.addEventListener('release', () => { wakeLock = null; }); }
  catch (error) { log('wake', `not granted: ${error.name}`); }
}
setInterval(syncWakeLock, 2000);
setTimeout(checkVoice, 4000);   // first priorities, then every minute while the page is visible (one relay round trip): an urgent task drops back to its place once the human has read its message
setInterval(() => { if (document.visibilityState === 'visible' && (voice.pendingCount?.() ?? 0) === 0) checkVoice(); }, 60000);
document.addEventListener('visibilitychange', syncWakeLock);

// ---- desktop keyboard shortcuts (ignored while typing; Esc leaves the text box) ----
function stepAgent(direction) {
  const names = rank('').map(r => r.t.name);
  if (!names.length) return;
  const at = names.indexOf(current);
  const next = names[(at + direction + names.length) % names.length];
  if (next && next !== current) { switchTopic(next); toast(next); }
}
addEventListener('keydown', e => {
  if (e.ctrlKey || e.metaKey || e.isComposing) return;
  const typing = e.target.closest?.('input, textarea, select, [contenteditable="true"]');
  if (e.key === 'Escape') {
    if (dictation.active()) { e.preventDefault(); $('cancel-rec').click(); return; }
    if (typing && !document.querySelector('dialog[open]')) { e.target.blur(); return; }
    return;
  }
  if (typing || document.querySelector('dialog[open]:not(#keys-sheet)')) return;
  const key = e.altKey ? 'Alt+' + e.key : e.shiftKey && e.key.startsWith('Arrow') ? 'Shift+' + e.key : e.key;
  const actions = {
    ' ': () => togglePlay(),
    ArrowLeft: () => jump(-1), ArrowRight: () => jump(1),
    'Shift+ArrowLeft': () => jump(-1, true), 'Shift+ArrowRight': () => jump(1, true),
    '[': () => setRate(rate - 0.1), ']': () => setRate(rate + 0.1),
    a: () => setAutoplay(!autoplay),
    m: () => $('mic').click(),
    i: () => draftEl.focus(),
    '/': () => $('find').click(),
    'Alt+ArrowUp': () => stepAgent(-1), 'Alt+ArrowDown': () => stepAgent(1),
    v: () => $('listen').click(),
    '?': () => { const d = $('keys-sheet'); if (d.open) d.close(); else d.showModal(); },
  };
  const action = actions[key];
  if (!action) return;
  e.preventDefault();
  player.unlock();
  action();
});

// ---- update notice: a page never updates itself; tell the user when a newer build is published ----
let updateShown = false;
async function checkForUpdate() {
  const mine = BUILD.replace(/^v2\.0-?/, '');
  if (!mine || updateShown) return;   // unbuilt page: nothing to compare
  try {
    const r = await fetch('version.txt?t=' + Date.now(), {cache: 'no-store'});
    const latest = r.ok ? (await r.text()).trim() : '';
    if (/^[0-9a-f]{10}$/.test(latest) && latest !== mine) { updateShown = true; log('app', `update available: ${mine} -> ${latest}`); showBanner('New version available. Tap to reload.', 'tap', () => location.reload()); }
  } catch { /* offline: try again later */ }
}
setTimeout(checkForUpdate, 8000); setInterval(checkForUpdate, 300000);
document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible') checkForUpdate(); });

// ---- start ----
renderRate(); renderAuto(); renderPrevButton(); $('prev-agent').onclick = switchBack; $('next-unread').onclick = switchNextUnread;
// A saved Voice control setting re-arms after reload: try at once, else on the first tap anywhere.
vctl.start();
if (current) { draftEl.value = store.get('voice2-draft:' + current, ''); renderFeed(); renderUnread(); }
autosize();
renderConn(events.info());
events.start();
restoreOutbox();
if (cfg.relayStreamId) {
  ensureRelayMuted(zulip, cfg.relayStreamId, store, log);
  setTimeout(checkVoice, 1500);
} else voiceState = {ok: false, text: 'voice-relay channel not found'};
window.__voice = {model, player, events, voice, zulip, log, cfg: () => ({...cfg, key: '…'}), current: () => current};
