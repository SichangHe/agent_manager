/** Read flags: marked locally at once, queued (persisted) and flushed to Zulip; survives reloads and offline periods. */
const KEY = 'voice2-readq';

export function createReads({zulip, model, store, onChange}) {
  const queue = new Set(store.get(KEY, []));
  let flushing = false;
  const save = () => store.set(KEY, [...queue]);

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
    for (const id of ids) queue.add(id);
    save();
    onChange();
    flush();
  }
  /** Marks only `id` (no forward fill). */
  function markOne(id) { model.markRead([id]); queue.add(id); save(); flush(); onChange(); }
  async function flush() {
    if (flushing || !queue.size) return;
    flushing = true;
    const ids = [...queue];
    const r = await zulip.call('POST', 'messages/flags', {messages: ids, op: 'add', flag: 'read'});
    if (r.ok) { for (const id of ids) queue.delete(id); save(); }
    flushing = false;
    if (queue.size && r.ok) flush();   // ids added while this call was in flight (the last message ends moments after the previous one) go out now, not 15 s later
  }
  /** No longer ours to send (read elsewhere, or deleted). */
  function forget(ids) { for (const id of ids) queue.delete(id); save(); }
  setInterval(flush, 15000);
  document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible') flush(); });
  return {markRead, markOne, flush, forget, has: id => queue.has(id), pending: () => [...queue], size: () => queue.size};
}
