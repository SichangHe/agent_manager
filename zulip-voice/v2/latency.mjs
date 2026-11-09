/** Per-event latency spans: `begin` an event, mark `stage`s, `finish` it; one summary line per event goes to the log and a ring feeds the debug sheet's median/p90. */
const KEEP = 60;

export function createLatency(log) {
  const open = new Map();
  const ring = new Map();
  const quantile = (xs, q) => { const s = [...xs].sort((a, b) => a - b); return s.length ? s[Math.min(s.length - 1, Math.floor(q * s.length))] : null; };
  return {
    /** `startMs` lets an event start in the past (e.g. a server timestamp translated to the page clock). */
    begin(key, label, startMs = performance.now()) { open.set(key, {label, t0: startMs, last: startMs, stages: []}); },
    stage(key, name, extra) { const e = open.get(key); if (!e) return; const now = performance.now(); e.stages.push(`${name}=${Math.round(now - e.last)}${extra ? `(${extra})` : ''}`); e.last = now; },
    finish(key, name = 'done') {
      const e = open.get(key); if (!e) return;
      this.stage(key, name);
      open.delete(key);
      const total = Math.round(performance.now() - e.t0);
      const list = ring.get(e.label) ?? []; list.push(total); if (list.length > KEEP) list.shift(); ring.set(e.label, list);
      log('lat', `${e.label} total=${total}ms ${e.stages.join(' ')}`);
    },
    drop(key) { open.delete(key); },
    /** Lines like `startword n=8 median=2100ms p90=2900ms`. */
    summary() { return [...ring].map(([label, xs]) => `${label.padEnd(10)} n=${String(xs.length).padStart(2)} median=${quantile(xs, 0.5)}ms p90=${quantile(xs, 0.9)}ms`); },
  };
}
