/** Debug timeline: ring buffer of `{t, cat, msg}` persisted so a crashed or reloaded page's previous session can still be copied. */
const KEY = 'voice2-log';
const MAX_LINES = 800;

export function createLog(storage = globalThis.localStorage, now = Date.now) {
  let lines = [];
  try { lines = JSON.parse(storage.getItem(KEY) || '[]'); } catch { lines = []; }
  const listeners = new Set();
  const t0 = now();
  let dirty = false;
  const save = () => {
    if (!dirty) return;
    dirty = false;
    try { storage.setItem(KEY, JSON.stringify(lines)); } catch { /* storage full or unavailable */ }
  };
  setInterval(save, 2000);
  globalThis.addEventListener?.('pagehide', save);
  const log = (cat, msg) => {
    const line = {t: now(), cat, msg: String(msg).slice(0, 300)};
    lines.push(line);
    if (lines.length > MAX_LINES) lines.splice(0, lines.length - MAX_LINES);
    dirty = true;
    for (const listener of listeners) listener(line);
  };
  log('app', `session start ua=${globalThis.navigator?.userAgent ?? '?'}`);
  return Object.assign(log, {
    lines: () => lines,
    subscribe: fn => { listeners.add(fn); return () => listeners.delete(fn); },
    clear: () => { lines.length = 0; dirty = true; save(); },
    t0,
    text: () => lines.map(l => `${new Date(l.t).toISOString().slice(11, 23)} ${l.cat.padEnd(6)} ${l.msg}`).join('\n'),
  });
}
