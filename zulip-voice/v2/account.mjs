/** The stored Zulip account: login from a pasted zuliprc, discovery of channel and agent ids, muting the relay channel. */
import {t} from './i18n.mjs';
import {createZulip} from './zulip.mjs';
import {$} from './chrome.mjs';

export const ACCOUNT_KEY = 'zulip-voice-account';

export async function login(store, log) {
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
      if (!values.site || !values.email || !values.key) return fail(t('login.need'));
      let site; try { site = new URL(values.site).origin; } catch { return fail(t('login.badsite')); }
      const zulip = createZulip({site, email: values.email, key: values.key}, log);
      const me = await zulip.call('GET', 'users/me');
      if (!me.ok) return fail(me.error);
      if (me.data.is_bot) return fail(t('login.bot'));
      const next = {site, email: values.email, key: values.key, humanId: me.data.user_id};
      store.set(ACCOUNT_KEY, next);
      dialog.close();
      resolve(next);
    };
  });
}

/** Fills in channel and agent ids missing from the stored account. */
export async function completeAccount(cfg, zulip, store) {
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
export async function ensureRelayMuted(zulip, relayStreamId, store, log) {
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
