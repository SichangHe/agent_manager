// Create a generic Zulip bot through the human's logged-in browser (its own tab, closed after) and print its login lines.
// usage: node zulip-new-bot.mjs FULL_NAME SHORT_NAME > BOT.env   (env: ZULIP_SITE; ZULIP_CDP, default http://127.0.0.1:9279)
// Bots cannot create bots, so this uses the human's session; used by `scripts/new-machine-bots`.
const [fullName, shortName] = process.argv.slice(2);
const cdp = process.env.ZULIP_CDP || 'http://127.0.0.1:9279';
const site = process.env.ZULIP_SITE;
if (!fullName || !shortName || !site) throw new Error('usage: ZULIP_SITE=... node zulip-new-bot.mjs FULL_NAME SHORT_NAME');
const target = await (await fetch(`${cdp}/json/new?about:blank`, { method: 'PUT' })).json();
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((ok) => ws.addEventListener('open', ok));
let next = 1;
const pending = new Map();
ws.addEventListener('message', (event) => {
  const reply = JSON.parse(event.data);
  if (pending.has(reply.id)) { pending.get(reply.id)(reply); pending.delete(reply.id); }
});
const call = (method, params = {}) => new Promise((ok) => { const id = next++; pending.set(id, ok); ws.send(JSON.stringify({ id, method, params })); });
const run = async (expression) => (await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })).result?.result?.value;
try {
  await call('Page.navigate', { url: `${site}/` });
  await new Promise((ok) => setTimeout(ok, 3000));
  for (let i = 0; i < 60; i++) {
    await new Promise((ok) => setTimeout(ok, 500));
    if ((await run('document.readyState')) === 'complete' && String(await run('location.href')).startsWith(site)) break;
  }
  const bot = await run(`(async () => {
    const csrf = (document.cookie.match(/csrftoken=([^;]+)/) || [])[1] || document.querySelector('input[name=csrfmiddlewaretoken]')?.value || window.page_params?.csrf_token || '';
    const body = new URLSearchParams({ full_name: ${JSON.stringify(fullName)}, short_name: ${JSON.stringify(shortName)}, bot_type: '1' });
    return await (await fetch('/json/bots', { method: 'POST', body, headers: { 'X-CSRFToken': csrf }, credentials: 'same-origin' })).json();
  })()`);
  if (bot?.result !== 'success') throw new Error(`Zulip refused the bot: ${JSON.stringify(bot)}`);
  const email = `${shortName}-bot@${new URL(site).host}`;
  process.stdout.write(`ZULIP_SITE=${site}\nZULIP_BOT_EMAIL=${email}\nZULIP_BOT_API_KEY=${bot.api_key}\n`);
} finally {
  await call('Target.closeTarget', { targetId: target.id }).catch(() => {});
  ws.close();
}
