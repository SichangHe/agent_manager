/** Pure voice-command grammar: word matching, agent-name ranking, navigation phrases, start/end/cancel words. No DOM, no state. */
import {soundsLikeWord, stripTrailingControl, leadingControlCount} from './text.mjs';

export const RESUME_DEFAULT = 'tomato';   // the human's pick from the end-word study: 99% on-device detection, 0 false accepts in 4.9 h of speech
export const START_DEFAULT = 'attention';   // chosen from the trigger-word study: long, no clipped final consonant, 98-100% detection, 0 false accepts
export const CMD_DEFAULT = 'katana';
export const CANCEL_WORD_DEFAULT = 'macaroni';   // the human picked it (round-2 study: 96% hit, 0 false accepts in 4.9 h; "bamboo" 88%, replaced); "cancel" and friends keep working as server-path aliases
export const LONG_SILENCE_DEFAULT_S = 20;   // the end word is the main end signal; a short silence cut replies off while the user was still thinking
export const SWITCH_MIN_SCORE = 0.8;

export const normalize = t => t.toLowerCase().replace(/[^a-z0-9 ]+/g, ' ').replace(/\s+/g, ' ').trim();

export function editDistance(a, b) {
  const d = Array.from({length: a.length + 1}, (_, i) => [i, ...Array(b.length).fill(0)]);
  for (let j = 1; j <= b.length; j++) d[0][j] = j;
  for (let i = 1; i <= a.length; i++) for (let j = 1; j <= b.length; j++) d[i][j] = Math.min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1));
  return d[a.length][b.length];
}

// ---- agent names ----
const ABBREVIATIONS = {mgr: 'manager', manager: 'mgr', viz: 'visualization', impl: 'implementation', cfg: 'config', config: 'cfg', dev: 'developer', ops: 'operations', eval: 'evaluation', exp: 'experiment', doc: 'documentation', docs: 'documentation', repo: 'repository', infra: 'infrastructure', ui: 'uiclient', pr: 'pullrequest', ci: 'continuous', ml: 'machinelearning', db: 'database'};
/** 0..1: how well every spoken word matches some word of the topic name ("human manager" ~ human_mgr), allowing abbreviations, prefixes and initials. */
export function tokenScore(query, name) {
  const qs = normalize(query).split(' ').filter(Boolean), ns = normalize(name.replace(/[_-]/g, ' ')).split(' ').filter(Boolean);
  if (!qs.length || !ns.length) return 0;
  const same = (a, b) => a === b || ABBREVIATIONS[a] === b || ABBREVIATIONS[b] === a || (a.length >= 3 && b.startsWith(a)) || (b.length >= 3 && a.startsWith(b)) || (ABBREVIATIONS[b] && ABBREVIATIONS[b].startsWith(a) && a.length >= 3) || (ABBREVIATIONS[a] && ABBREVIATIONS[a].startsWith(b) && b.length >= 3) || (a.length >= 4 && editDistance(a, b) <= 1);
  const matched = qs.filter(q => ns.some(n => same(q, n))).length / qs.length;
  const cover = ns.filter(n => qs.some(q => same(q, n))).length / ns.length;
  return matched === 1 ? 0.7 + 0.3 * cover : matched * 0.7;
}
/** `topics` (`{name, maxId}`) ranked by how well they match `query` ("terminal viz" finds terminal_viz); all, in the given order, when the query is empty. */
export function rankTopics(topics, query) {
  const q = normalize(query), qFlat = q.replace(/ /g, '');
  if (!qFlat) return topics.map(t => ({t, score: 1}));
  return topics.map(t => {
    const name = normalize(t.name.replace(/[_-]/g, ' ')), flat = name.replace(/ /g, ''), ts = tokenScore(query, t.name);
    const score = ts >= 0.85 ? ts : flat === qFlat ? 1 : flat.includes(qFlat) ? 0.9 : q.split(' ').every(w => name.includes(w)) ? 0.8 : 1 - editDistance(flat.slice(0, qFlat.length + 2), qFlat) / Math.max(qFlat.length, 1);
    return {t, score};
  }).filter(x => x.score >= 0.4).sort((a, b) => b.score - a.score || b.t.maxId - a.t.maxId);
}
/** Is the top match clearly better than the next? */
export const clearWinner = ranked => ranked.length > 0 && ranked[0].score >= SWITCH_MIN_SCORE && (ranked.length === 1 || ranked[0].score - ranked[1].score >= 0.15);

const SPOKEN_EXPANSIONS = {mgr: 'manager', impl: 'implementation', cfg: 'config', dev: 'developer', eval: 'evaluation', exp: 'experiment', ops: 'operations'};
/** How an agent name is said: underscores and dashes become spaces, abbreviations are spelled out ("human_mgr" -> "human manager"). */
export const spokenName = topic => normalize(topic.replace(/[_-]/g, ' ')).split(' ').filter(Boolean).map(w => SPOKEN_EXPANSIONS[w] ?? w).join(' ');

// ---- On-device command phrases (command mode only). The study (research-kws/trig/cmdstudy.py) found only these reliable: "switch to <agent>" (78-98% hit, no confusions, 0 false accepts in 4.9 h),
// "cancel" (98%), "stop" (82%). next/back/skip/again/play/yes/no and number chains are confused with each other, so the server transcript stays their path. ----
const LOCAL_CMD_THRESHOLD = 0.3;
/** Keyword-spotter phrases for command mode, given the raw topic names (most recent first). */
export function commandPhrases(topics) {
  const names = topics.map(spokenName).filter(Boolean);
  const unique = [...new Set(names)].filter(n => !names.some(o => o !== n && o.startsWith(n + ' ')));   // "alpha" is left out while "alpha beta" exists: it would fire first
  const fixed = [['CANCEL', 'cancel'], ['STOP', 'stop'], ['POSITIVE', 'positive'], ['NEGATIVE', 'negative'], ['SKIP_FORWARD', 'skip forward'], ['SKIP_BACKWARD', 'skip backward'], ['NUMBER_ONE', 'number one'], ['NUMBER_TWO', 'number two'], ['NUMBER_THREE', 'number three']];
  return [...fixed.map(([tag, text]) => ({tag, text, threshold: LOCAL_CMD_THRESHOLD})),
    ...unique.map(n => ({tag: 'SWITCH_TO_' + n.toUpperCase().replace(/[^A-Z0-9]+/g, '_'), text: 'switch to ' + n, threshold: LOCAL_CMD_THRESHOLD, say: 'switch to ' + n}))];
}

// ---- start, end, command and cancel words ----
const START_ALIASES = {attention: ['attention', 'attension', 'attentions', 'attenshun', 'a tension', 'attending'], pause: ['pause', 'paws', 'pose', 'pours', 'paused', 'pawz', 'pulse', 'pulls', 'polls']};
/** `text` after a leading start word `word` (normalized; fuzzy, repeats collapsed), or null when it does not start with one. */
export function afterStartWord(text, word) {
  const accepted = new Set([word, ...(START_ALIASES[word] ?? [])]);
  const words = text.trim().split(/\s+/).filter(Boolean);
  if (word === 'attention') { const c = leadingControlCount('attention', words); if (c) return words.slice(c).join(' '); }
  const isStart = w => { const n = normalize(w); return n && (accepted.has(n) || editDistance(n, word) <= Math.floor(word.length / 4)); };
  if (!words.length || !isStart(words[0])) return null;
  let i = 0;
  while (i < words.length && isStart(words[i])) i++;
  return words.slice(i).join(' ');
}

/** `text` without a trailing end phrase (`phrase` setting: "tomato" by sound, "over" also "over over"/"over and out", anything else fuzzily), or null when it does not end with one. */
export function stripResumePhrase(text, phraseSetting) {
  const words = text.trim().split(/\s+/).filter(Boolean), norm = words.map(normalize);
  const phrase = normalize(phraseSetting);
  if (phrase === 'tomato' || phrase === 'over') {
    let n = norm.length;
    if (phrase === 'over' && n >= 3 && norm[n - 1] === 'out' && norm[n - 2] === 'and' && norm[n - 3] === 'over') n -= 2;
    let kept = n;
    if (phrase === 'over') while (kept > 0 && norm[kept - 1] === 'over') kept--;
    else kept = stripTrailingControl(words.slice(0, n), 'tomato').length;
    return kept < norm.length ? words.slice(0, kept).join(' ') : null;
  }
  const k = phrase.split(' ').length;
  if (phrase && words.length >= k && editDistance(normalize(words.slice(-k).join(' ')), phrase) <= Math.floor(phrase.length / 6)) return words.slice(0, -k).join(' ');
  return null;
}

export const isCommandWord = (w, commandWord) => { const n = normalize(w); return !!n && (n === commandWord || soundsLikeWord(commandWord, [n])); };
/** Normalized `t` is a cancel phrase: "cancel", "never mind" ..., or only the rare cancel word. */
export const isCancel = (t, cancelWord) => /^(?:cancel|cancel that|never mind|scratch that|forget it)$/.test(t) || t.split(' ').every(x => x === cancelWord || soundsLikeWord(cancelWord, [x]));
/** The rare cancel word counts anywhere in an utterance. */
export const hasCancelWord = (text, cancelWord) => normalize(text).split(' ').some(t => t === cancelWord || soundsLikeWord(cancelWord, [t]));
/** True when the last sentence of `text` is only a cancel phrase ("Some reply. Cancel."). */
export const endsWithCancel = (text, cancelWord) => isCancel(text.split(/[.!?]+/).map(x => normalize(x)).filter(Boolean).at(-1) ?? '', cancelWord);
export const NEXT_AGENT_RE = /^(next (agent|thread|unread|unread agent|unread thread|one)|(switch|go)( to)?( the)? next( agent| thread| unread| one)?)$/;   // "switch next", "switch to next", "go next", "next agent"...
export const SEND_LOG_RE = /^(send|upload) (the )?(log|logs|debug log)$/;
export const SWITCH_BACK_RE = /^(switch back|go back|previous agent|(go|switch) back to (the )?previous agent|switch to (the )?previous agent)$/;
export const POSITIVE_RE = /^(positive|yes|yeah|yep|sure|ok|okay|correct)$/;
export const NEGATIVE_RE = /^(negative|no|nope|cancel|cancel that|none|never mind|scratch that|stop)$/;

/** The answer `w` (normalized) to an open switch question `{names, confirm}`: the picked name, or null. A numbered list also accepts the name itself. */
export function choiceAnswer(w, {names, confirm}, rank) {
  if (confirm) return POSITIVE_RE.test(w) ? names[0] : null;
  const t = w.split(' ').filter(Boolean), n = t[0] === 'number' || t[0] === 'the' ? t[1] : t.length === 1 ? t[0] : null;   // "number one", "number one please", "the first", "one"
  const i = n ? ['one first 1 won', 'two second 2 to too', 'three third 3'].findIndex(g => g.split(' ').includes(n)) : -1;
  if (i >= 0) return names[i];
  const r = rank(w.replace(/^(switch to|go to|agent)\s+/, ''));
  return w && clearWinner(r) ? r[0].t.name : null;
}

const NUMBERS = {to: 2, too: 2, won: 1, for: 4, one: 1, two: 2, three: 3, four: 4, five: 5, six: 6, seven: 7, eight: 8, nine: 9, ten: 10, '1': 1, '2': 2, '3': 3, '4': 4, '5': 5, '6': 6, '7': 7, '8': 8, '9': 9, '10': 10};
/**
 * A whole utterance that is only navigation: "back", "previous", "back back", "previous message", "next three", "skip forward two", "again".
 * `{dir: -1|0|1, whole, count}` (dir 0 = replay the current chunk), or null.
 */
export function navCommand(words) {
  const tokens = words.split(' ').filter(Boolean);
  const whole = tokens.includes('message') || tokens.includes('messages');
  const rest = tokens.filter(t => t !== 'message' && t !== 'messages');
  const dirWord = {forward: 1, forwards: 1, ahead: 1, backward: -1, backwards: -1}[rest[1]];
  if (rest[0] === 'skip' && dirWord && rest.length <= 3) { const c = rest.length === 3 ? NUMBERS[rest[2]] : 1; if (c) return {dir: dirWord, whole, count: c}; }
  const kind = t => ['back', 'previous', 'last'].includes(t) ? -1 : ['next', 'skip', 'forward'].includes(t) ? 1 : ['again', 'repeat'].includes(t) ? 0 : null;
  const n = NUMBERS[rest.at(-1)];
  if (rest.length === 2 && n && !NUMBERS[rest[0]]) { const d = kind(rest[0]); if (d) return {dir: d, whole, count: n}; }   // "next three", "skip five", "back two"
  if (!rest.length || rest.length > 5) return null;
  const dirs = rest.map(kind);
  if (dirs.some(d => d === null) || new Set(dirs).size > 1) return null;
  return {dir: dirs[0], whole, count: dirs[0] === 0 ? 1 : rest.length};
}

/** Near-identical texts (word-set Jaccard >= 0.8 over words longer than 2 letters). */
export function similarText(a, b) {
  const words = t => new Set(normalize(t).split(' ').filter(w => w.length > 2));
  const x = words(a), y = words(b);
  if (!x.size || !y.size) return false;
  let n = 0; for (const w of x) if (y.has(w)) n++;
  return n / (x.size + y.size - n) >= 0.8;
}
