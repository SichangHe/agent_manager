/** Zulip REST client and a self-healing event-queue loop. All failures are returned, never thrown. */

const sleepMs = ms => new Promise(resolve => setTimeout(resolve, ms));

/** `call` resolves to `{ok: true, data}` or `{ok: false, kind: 'network'|'timeout'|'aborted'|'http', status, code, error, uncertain}`. */
export function createZulip({site, email, key}, log, fetchFn = (...a) => fetch(...a)) {
  const authorization = 'Basic ' + btoa(String.fromCharCode(...new TextEncoder().encode(email + ':' + key)));
  let rateLimitedUntilMs = 0;
  let totalCalls = 0;
  const callTimes = [];   // start times of API calls in the last minute, for the budget display and tests
  async function call(method, path, fields = {}, {timeoutMs = 15000, signal} = {}) {
    const started = performance.now();
    if (path !== 'events') { totalCalls++; callTimes.push(Date.now()); while (callTimes[0] < Date.now() - 60000) callTimes.shift(); }
    const label = `${method} ${path.split('?')[0]}`;
    if (Date.now() < rateLimitedUntilMs) return {ok: false, kind: 'http', status: 429, code: 'RATE_LIMIT', error: `${label}: rate limited`, uncertain: false};
    const body = fields instanceof FormData ? fields : new URLSearchParams(Object.entries(fields).map(([k, v]) => [k, typeof v === 'string' ? v : JSON.stringify(v)]));
    const timeout = AbortSignal.timeout(timeoutMs);
    const merged = signal ? AbortSignal.any([timeout, signal]) : timeout;
    let response;
    try {
      response = await fetchFn(`${site}/api/v1/${path}${method === 'GET' && [...body].length ? '?' + body : ''}`, {method, headers: {Authorization: authorization}, body: method === 'GET' ? undefined : body, signal: merged, redirect: 'error'});
    } catch (error) {
      const kind = timeout.aborted ? 'timeout' : signal?.aborted ? 'aborted' : 'network';
      const result = {ok: false, kind, status: 0, code: kind, error: `${label}: ${kind}`, uncertain: method !== 'GET'};
      if (kind !== 'aborted') log('http', `${result.error} after ${Math.round(performance.now() - started)}ms (${error.name})`);
      return result;
    }
    let data;
    try { data = await response.json(); } catch { data = {}; }
    const ms = Math.round(performance.now() - started);
    if (response.status === 429) {
      rateLimitedUntilMs = Date.now() + (Number(data['retry-after'] ?? response.headers.get('retry-after')) || 10) * 1000;
      log('http', `${label} 429 rate limited until +${Math.round((rateLimitedUntilMs - Date.now()) / 1000)}s`);
    }
    if (!response.ok || data.result !== 'success') {
      const result = {ok: false, kind: 'http', status: response.status, code: data.code ?? 'ERROR', error: `${label}: HTTP ${response.status} ${data.msg ?? response.statusText}`, uncertain: response.status >= 500};
      if (path !== 'events') log('http', `${result.error} ${ms}ms`);
      return result;
    }
    if (path !== 'events') log('http', `${label} ok ${ms}ms`);
    return {ok: true, data, ms};
  }
  return {site, call, callsLastMinute: () => { while (callTimes[0] < Date.now() - 60000) callTimes.shift(); return callTimes.length; }, totalCalls: () => totalCalls, rateLimitedForMs: () => Math.max(0, rateLimitedUntilMs - Date.now()), callLog: callTimes};
}

/**
 * Long-polls one event queue; re-registers when it is lost.
 * `kick(reason)` restarts the poll right away (page resumed, network back); a timer also kicks after a frozen page.
 * `onInit(state, isResync)` runs after each registration; `onEvent(event)` per event; `onState(info)` on connection changes.
 */
export function createEvents({zulip, log, eventTypes, onInit, onEvent, onState}) {
  let queue = null, abort = null, wake = null, state = 'connecting', failures = 0, lastOkMs = 0, resync = false, running = false, pollStartMs = 0;
  const setState = next => { if (next !== state) { log('conn', `${state} -> ${next}`); state = next; onState(info()); } };
  const info = () => ({state, failures, queueId: queue?.queue_id ?? null, lastOkAgoMs: lastOkMs ? Date.now() - lastOkMs : null, pollAgeMs: pollStartMs ? Date.now() - pollStartMs : null});

  function kick(reason) {
    log('conn', `kick: ${reason}`);
    abort?.abort();
    wake?.();
  }
  const backoff = () => new Promise(resolve => {
    const ms = Math.min(500 * 2 ** (failures - 1), 8000);
    const timer = setTimeout(resolve, ms);
    wake = () => { clearTimeout(timer); resolve(); };
  });

  async function run() {
    let quick = true;
    for (;;) {
      if (!queue) {
        setState(failures ? 'retrying' : 'connecting');
        const r = await zulip.call('POST', 'register', {event_types: eventTypes, apply_markdown: false}, {timeoutMs: 20000});
        if (!r.ok) { failures++; setState(failures >= 3 ? 'offline' : 'retrying'); await backoff(); continue; }
        queue = {queue_id: r.data.queue_id, last_event_id: r.data.last_event_id};
        log('conn', `registered queue ${queue.queue_id} after ${r.ms}ms`);
        await onInit(r.data, resync);
        resync = false; quick = true;
      }
      abort = new AbortController();
      pollStartMs = Date.now();
      const r = await zulip.call('GET', 'events', {queue_id: queue.queue_id, last_event_id: queue.last_event_id, dont_block: quick}, {timeoutMs: 65000, signal: abort.signal});
      pollStartMs = 0;
      if (r.ok) {
        failures = 0; lastOkMs = Date.now(); quick = false; setState('live');
        for (const event of r.data.events) {
          queue.last_event_id = Math.max(queue.last_event_id, event.id);
          if (event.type !== 'heartbeat') onEvent(event);
        }
        if (r.data.events.length) log('event', `${r.data.events.length} events (${r.data.events.map(e => e.type).join(',')}) poll ${r.ms}ms`);
        onState(info());
      } else if (r.kind === 'aborted') { quick = true; }
      else if (r.code === 'BAD_EVENT_QUEUE_ID') { log('conn', 'queue expired; re-registering'); queue = null; resync = true; }
      else {
        failures++; quick = true; setState(failures >= 3 ? 'offline' : 'retrying');
        if (r.kind === 'timeout' || r.kind === 'network') { /* socket likely dead; the next poll opens a new one */ }
        await backoff();
      }
    }
  }

  let lastTickMs = Date.now();
  setInterval(() => {
    const now = Date.now();
    if (now - lastTickMs > 10000) kick(`page frozen ${Math.round((now - lastTickMs) / 1000)}s`);
    lastTickMs = now;
    if (state === 'live' && pollStartMs && now - pollStartMs > 70000) kick('poll stalled');
    onState(info());
  }, 3000);
  globalThis.addEventListener?.('online', () => kick('online'));
  globalThis.addEventListener?.('offline', () => log('conn', 'browser reports offline'));
  globalThis.addEventListener?.('pageshow', () => kick('pageshow'));
  globalThis.document?.addEventListener('visibilitychange', () => { log('app', `visibility ${document.visibilityState}`); if (document.visibilityState === 'visible') kick('visible'); });

  return {
    start() { if (!running) { running = true; run(); } },
    kick,
    info,
    queueId: () => queue?.queue_id ?? null,
    sleepMs,
  };
}
