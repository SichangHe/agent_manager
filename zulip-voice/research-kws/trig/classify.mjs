// Classifies stress.json transcripts with the app's own grammar (v2/grammar.mjs), mirroring runCommands' order. Usage: node classify.mjs stress.json > stress_report.json
import {readFileSync} from 'node:fs';
import {normalize, isCommandWord, isCancel, hasCancelWord, navCommand, NEXT_AGENT_RE, SEND_LOG_RE, SWITCH_BACK_RE, afterStartWord, stripResumePhrase} from '../../v2/grammar.mjs';
const rows = JSON.parse(readFileSync(process.argv[2], 'utf8'));
function command(text) {
  const tokens = normalize(text).split(' ').filter(Boolean).filter(t => !isCommandWord(t, 'katana'));
  const j = n => tokens.slice(0, n).join(' '), two = j(2), t = tokens[0];
  if (SEND_LOG_RE.test(two) || SEND_LOG_RE.test(j(3))) return 'send-log';
  if (NEXT_AGENT_RE.test(j(4)) || NEXT_AGENT_RE.test(j(3)) || NEXT_AGENT_RE.test(two)) return 'next-agent';
  if (SWITCH_BACK_RE.test(j(3)) || SWITCH_BACK_RE.test(two)) return 'switch-back';
  if (/^(switch|switched|switching|go)$/.test(t) && /^(to|agent|agents)$/.test(tokens[1] ?? '')) return 'switch:' + tokens.slice(/^(agent|agents)$/.test(tokens[1]) && tokens[2] === 'to' ? 3 : 2).join(' ');
  if (isCancel(two, 'macaroni') || isCancel(t ?? '', 'macaroni')) return 'cancel';
  const take = [3, 2, 1].find(n => navCommand(tokens.slice(0, n).join(' ')));
  if (take) { const n = navCommand(tokens.slice(0, take).join(' ')); return `nav:${n.dir}:${n.whole ? 'msg' : 'chunk'}:${n.count}`; }
  if (/^(stop|pause|wait)$/.test(t)) return 'pause';
  if (/^(play|resume|continue)$/.test(t)) return 'play';
  return 'none';
}
const EXPECT = {'next': 'nav:1:chunk:1', 'back': 'nav:-1:chunk:1', 'again': 'nav:0:chunk:1', 'next message': 'nav:1:msg:1', 'previous message': 'nav:-1:msg:1', 'skip forward three': 'nav:1:chunk:3', 'stop': 'pause', 'play': 'play', 'switch to human manager': 'switch:human manager', 'next agent': 'next-agent', 'switch back': 'switch-back', 'send log': 'send-log', 'cancel': 'cancel'};
const WORDS = {attention: 'ATTENTION', katana: 'KATANA', tomato: 'TOMATO', macaroni: 'MACARONI'};
const LONG = 'attention please check the build and tell me what failed tomato';
const stat = {};   // phrase -> cond -> {n, kws, stt}
for (const r of rows) {
  const s = (stat[r.phrase] ??= {})[r.cond] ??= {n: 0, kws: 0, stt: 0};
  s.n++;
  if (r.phrase in WORDS) {
    if (r.kws.includes(WORDS[r.phrase])) s.kws++;
    const w = r.phrase, t = r.stt;
    const ok = w === 'attention' ? afterStartWord(t, 'attention') !== null : w === 'tomato' ? stripResumePhrase(t, 'tomato') !== null : w === 'macaroni' ? hasCancelWord(t, 'macaroni') : normalize(t).split(' ').some(x => isCommandWord(x, 'katana'));
    if (ok) s.stt++;
  } else if (r.phrase === LONG) {
    const after = afterStartWord(r.stt, 'attention'), body = after === null ? null : stripResumePhrase(after, 'tomato');
    s.kws += r.kws.includes('ATTENTION') && r.kws.includes('TOMATO') ? 1 : 0;
    if (body !== null && /check the build/.test(normalize(body)) && /what failed/.test(normalize(body))) s.stt++;
  } else if (command(r.stt) === EXPECT[r.phrase]) s.stt++;
}
const cell = (s, k) => s ? Math.round(100 * s[k] / s.n) : null;
const out = {};
for (const [p, byCond] of Object.entries(stat)) {
  const all = Object.values(byCond).reduce((a, s) => ({n: a.n + s.n, kws: a.kws + s.kws, stt: a.stt + s.stt}), {n: 0, kws: 0, stt: 0});
  out[p] = {kws_pct: cell(all, 'kws'), stt_pct: cell(all, 'stt'), n: all.n, by_cond_stt: Object.fromEntries(Object.entries(byCond).map(([c, s]) => [c, cell(s, 'stt')])), by_cond_kws: Object.fromEntries(Object.entries(byCond).map(([c, s]) => [c, cell(s, 'kws')]))};
}
console.log(JSON.stringify(out, null, 1));
// misses, for the report
const miss = rows.filter(r => !(r.phrase in WORDS) && r.phrase !== LONG && command(r.stt) !== EXPECT[r.phrase]).slice(0, 400).map(r => `${r.phrase} | ${r.cond} | ${r.voice} | ${JSON.stringify(r.stt)} -> ${command(r.stt)}`);
import('node:fs').then(fs => fs.writeFileSync('stress_misses.txt', miss.join('\n') + '\n'));
