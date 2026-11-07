/** Conversation state (pure data, no DOM, no network): messages per topic, unread, play queues. */
import {parseAgentMessage, messageChunks} from './text.mjs';

const parsedCache = new WeakMap();

/** `{title, body, switchTo, blocks, chunks}` of an agent message (cached per message object). */
export function parsed(message) {
  let p = parsedCache.get(message);
  if (!p) {
    const {title, body, switchTo} = parseAgentMessage(message.content);
    p = {title, body, switchTo, ...messageChunks(body)};
    parsedCache.set(message, p);
  }
  return p;
}

/** Player items for the spoken chunks of `message`, starting at chunk `fromIdx`. `last` marks the item that completes the message. */
export function itemsOf(message, fromIdx = 0) {
  const spoken = parsed(message).chunks.filter(c => c.speech);
  return spoken.filter(c => c.idx >= fromIdx).map(c => ({key: `${message.id}:${c.idx}`, text: c.speech, msgId: message.id, idx: c.idx, last: c.idx === spoken.length - 1}));
}

export function createModel({botId, humanId}) {
  const byTopic = new Map();
  const topics = new Map();
  const loaded = new Set();
  const isAgent = m => m.sender_id === botId;
  const topicInfo = name => { let t = topics.get(name); if (!t) { t = {name, maxId: 0, unread: 0}; topics.set(name, t); } return t; };

  return {
    isAgent,
    isMine: m => m.sender_id === humanId,
    /** Adds a message; returns true when it was new. */
    add(message) {
      let map = byTopic.get(message.subject);
      if (!map) { map = new Map(); byTopic.set(message.subject, map); }
      if (map.has(message.id)) return false;
      map.set(message.id, message);
      const t = topicInfo(message.subject);
      t.maxId = Math.max(t.maxId, message.id);
      return true;
    },
    find(id) { for (const map of byTopic.values()) if (map.has(id)) return map.get(id); },
    remove(topic, id) { byTopic.get(topic)?.delete(id); },
    messages: topic => [...(byTopic.get(topic)?.values() ?? [])].sort((a, b) => a.id - b.id),
    agentMessages(topic) { return this.messages(topic).filter(m => isAgent(m) && !m.local); },
    unreadAgentMessages(topic) { return this.agentMessages(topic).filter(m => !m.flags?.includes('read')); },
    markRead(ids) {
      for (const map of byTopic.values()) for (const id of ids) {
        const m = map.get(id);
        if (m && !m.flags?.includes('read')) m.flags = [...(m.flags ?? []), 'read'];
      }
    },
    /** Topics whose full recent history is in memory count unread from it; the others keep the registration's count. */
    markLoaded(topic) { loaded.add(topic); },
    /** Replaces topic metadata from the topics endpoint and the registration's unread list. */
    setTopics(list, unreadByTopic) {
      for (const {name, max_id} of list) { const t = topicInfo(name); t.maxId = Math.max(t.maxId, max_id); }
      for (const t of topics.values()) t.unread = unreadByTopic.get(t.name) ?? 0;
    },
    topics() {
      for (const t of topics.values()) if (loaded.has(t.name)) t.unread = this.unreadAgentMessages(t.name).length;
      return [...topics.values()].sort((a, b) => b.maxId - a.maxId);
    },
    hasTopic: name => topics.has(name),
    lastMessage(topic) { return this.messages(topic).at(-1); },
    /** Compact copy for the offline cache. */
    snapshot(current) {
      const msgs = {};
      for (const name of [current]) msgs[name] = this.messages(name).filter(m => !m.local).slice(-40).map(({id, sender_id, subject, content, timestamp, flags}) => ({id, sender_id, subject, content, timestamp, flags}));
      return {topics: this.topics().slice(0, 150), msgs, current};
    },
    restore(snapshot) {
      for (const t of snapshot.topics ?? []) topics.set(t.name, t);
      for (const list of Object.values(snapshot.msgs ?? {})) for (const m of list) this.add(m);
    },
  };
}
