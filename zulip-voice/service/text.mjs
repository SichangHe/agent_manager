/** Pure text logic shared by the browser client and `relay-worker.mjs` (so both chunk a message identically). */

const MAX_CHUNK_CHARS = 240;
const MAX_TTS_CHARS = 1800;
const MAX_CHUNKS_PER_MESSAGE = 20;

export function splitSentences(text) {
  if (typeof Intl.Segmenter === 'function') return [...new Intl.Segmenter(undefined, {granularity: 'sentence'}).segment(text)].map(part => part.segment);
  return text.match(/[^.!?]+[.!?]*(?:\s+|$)|[.!?]+/g) || [text];
}

/**
 * amh footer blocks ("pending item created:", "pending item deleted:", "pending item resolved", "removed open item", "open items:") with their bullet lines.
 * Returns the message lines without them and the footer lines separately; they are never spoken, and the web client folds them on small screens.
 */
export function splitFooters(lines) {
  const body = [], footers = [];
  const heading = /^(pending items?|removed open items?|open items?)\b[^\n]*$/i;
  for (let i = 0; i < lines.length; i++) {
    const t = lines[i].trim();
    if (heading.test(t) && (t.endsWith(':') || /^\s*[-*]\s+/.test(lines[i + 1] ?? ''))) {
      footers.push(lines[i]);
      while (/^\s*[-*]\s+/.test(lines[i + 1] ?? '')) footers.push(lines[++i]);
    } else body.push(lines[i]);
  }
  return {body, footers};
}

/** Zulip uploads linked in `content` as `[name](/user_uploads/...)`; returns the text without them and the list. */
export function extractAttachments(content) {
  const attachments = [];
  const text = content.replace(/!?\[([^\]]*)\]\((\/user_uploads\/[^)\s]+)\)/g, (_, name, uri) => { attachments.push({name: name || decodeURIComponent(uri.split('/').pop()), uri}); return ''; });
  return {text, attachments};
}

/** Splits an agent message into `{title, body, switchTo}`: the `[task] title` first line and `[switch: TASK]` lines are not body text. */
export function parseAgentMessage(content) {
  let switchTo = null;
  const extracted = extractAttachments(content);
  content = extracted.text;
  const lines = content.split('\n').filter(line => {
    const match = line.match(/^\[switch: (.+?)\]$/);
    if (match) switchTo = match[1].trim().replace(/\.md$/, '');
    return !match;
  });
  const split = splitFooters(lines);
  const text = split.body.join('\n');
  const first = text.match(/^\[[^\]\n]+\][^\n]*(?:\n|$)/);
  return {title: first ? first[0].trim() : '', body: (first ? text.slice(first[0].length) : text).trim(), switchTo, attachments: extracted.attachments, footers: split.footers};
}

/** Markdown-ish blocks: `{kind: 'p'|'li'|'h'|'code'|'table', text} (a table also has `head` and `rows`)`. */
export function parseBlocks(body) {
  const blocks = [];
  let paragraph = [], code = null, table = [];
  const flush = () => { if (paragraph.length) blocks.push({kind: 'p', text: paragraph.join(' ')}); paragraph = []; };
  const cells = line => line.trim().replace(/^\|/, '').replace(/\|$/, '').split('|').map(c => c.trim());
  /** Pipe lines are a table only when the second one is a separator row (`|---|:--:|`); otherwise they are plain text. */
  const flushTable = () => {
    if (!table.length) return;
    if (table.length >= 2 && /^[\s|:-]+$/.test(table[1]) && table[1].includes('-')) blocks.push({kind: 'table', head: cells(table[0]), rows: table.slice(2).map(cells), text: ''});
    else paragraph.push(...table.map(l => l.trim()));
    table = [];
  };
  for (const line of body.split('\n')) {
    if (!code && /^\s*\|.*\|\s*$/.test(line)) { flush(); table.push(line); continue; }
    flushTable();
    if (/^\s*```/.test(line)) {
      if (code) { blocks.push({kind: 'code', text: code.join('\n')}); code = null; } else { flush(); code = []; }
    } else if (code) code.push(line);
    else if (!line.trim()) flush();
    else if (/^\s*(?:[-*+]|\d+[.)])\s+/.test(line)) { flush(); blocks.push({kind: 'li', text: line.replace(/^\s*(?:[-*+]|\d+[.)])\s+/, '')}); }
    else if (/^#{1,6}\s/.test(line)) { flush(); blocks.push({kind: 'h', text: line.replace(/^#{1,6}\s+/, '')}); }
    else paragraph.push(line.trim());
  }
  if (code) blocks.push({kind: 'code', text: code.join('\n')});
  flushTable();
  flush();
  return blocks;
}

/** What a speech engine should say for markdown `text`. */
export function speechText(text) {
  return text
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/https?:\/\/\S+/g, 'link')
    .replace(/`([^`]*)`/g, '$1')
    .replace(/(\*\*|__|\*|~~)/g, '')
    .replace(/(\w)_(?=\w)/g, '$1 ')
    .replace(/^>\s?/, '')
    .replace(/\s+/g, ' ')
    .trim();
}

/**
 * Chunks of one message in reading order: `{block, text, speech, idx}`.
 * `idx` numbers spoken chunks (stable request key); `speech` is empty for unspoken ones.
 */
export function messageChunks(body) {
  const chunks = [];
  const blocks = parseBlocks(body);
  blocks.forEach((block, b) => {
    if (block.kind === 'code') return;   // not read aloud
    if (block.kind === 'table') {   // "Table start.", then per row "Row N." and every cell as its own chunk (its own audio clip: long pauses between cells)
      chunks.push({block: b, text: 'Table start.', r: -1, c: -1});
      [block.head, ...block.rows].forEach((row, r) => {
        if (r > 0) chunks.push({block: b, text: `Row ${r}.`, r, c: -1});   // the header is read right after "Table start." and is not a numbered row
        block.head.forEach((_, c) => { const text = (row[c] ?? '').trim(); if (text) chunks.push({block: b, text: /[.!?]$/.test(text) ? text : text + '.', r, c}); });
      });
      return;
    }
    let group = '';
    const emit = () => { if (group) chunks.push({block: b, text: group}); group = ''; };
    for (const sentence of splitSentences(block.text)) {
      if (group && group.length + sentence.length > MAX_CHUNK_CHARS) emit();
      group += sentence;
    }
    emit();
  });
  let idx = 0;
  const spoken = [];
  for (const chunk of chunks) {
    const parts = speechText(chunk.text).match(new RegExp(`[\\s\\S]{1,${MAX_TTS_CHARS}}`, 'g')) || [];
    if (!parts.length) { spoken.push({...chunk, speech: '', idx: -1}); continue; }
    parts.forEach((speech, k) => spoken.push({...chunk, text: k ? '' : chunk.text, speech, idx: idx++}));
  }
  return {blocks, chunks: spoken};
}

/** Speech strings the relay pre-renders for an agent message; index = chunk `idx`. */
export function speechPieces(content) {
  return messageChunks(parseAgentMessage(content).body).chunks.filter(c => c.speech).map(c => c.speech).slice(0, MAX_CHUNKS_PER_MESSAGE);
}

const escapeHtml = s => s.replace(/[&<>"]/g, c => ({'&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;'}[c]));

/** Inline markdown to safe HTML (code, bold, italic, http links). */
export function inlineHtml(text) {
  return escapeHtml(text)
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>')
    .replace(/(^|[\s(])\*([^*\s][^*]*)\*/g, '$1<i>$2</i>')
    .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
}

/* ---- Control words (shared by the client and the relay): matched by sound, not from a fixed list of spellings. ---- */

/** A rough phonetic key: letters only, c/q/ck -> k, doubled letters collapsed, every vowel run -> "a" ("Cangaroo", "Canguro", "kanguru" all become "kangara"). */
export function soundKey(word) {
  return word.toLowerCase().replace(/[^a-z]/g, '').replace(/ck/g, 'k').replace(/[cq]/g, 'k').replace(/([a-z])\1+/g, '$1').replace(/[aeiouy]+/g, 'a');
}
function distance(a, b) {
  let row = Array.from({length: b.length + 1}, (_, j) => j);
  for (let i = 1; i <= a.length; i++) {
    const next = [i];
    for (let j = 1; j <= b.length; j++) next[j] = Math.min(row[j] + 1, next[j - 1] + 1, row[j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1));
    row = next;
  }
  return row[b.length];
}
/** Per control word: its key, the allowed edit distance on keys, and a sanity check on the heard key that keeps ordinary words ("Canada", "tension") out. */
const CONTROL = {
  kangaroo: {key: soundKey('kangaroo'), max: 2, ok: k => /^ka?n?g/.test(k) && k.includes('r') && k.length >= 5},   // the previous end word: still removed from replies
  tomato: {key: soundKey('tomato'), max: 1, ok: k => /^t/.test(k) && k.includes('m') && !/s$/.test(k) && k.length >= 5},   // the end word; plurals ("tomatoes") are ordinary text
  attention: {key: soundKey('attention'), max: 1, multi: 1, ok: (k, raw) => /^a/.test(raw) && /^a/.test(k) && k.includes('t') && k.includes('n') && k.length >= 6},
};
/** Whether the spoken words `parts` (1 to 3 adjacent words, e.g. "kanga" "roo") sound like the control word `name`. */
export function soundsLike(name, parts) {
  const c = CONTROL[name], raw = parts.join('').toLowerCase().replace(/[^a-z]/g, ''), k = soundKey(raw);
  return !!c && c.ok(k, raw) && distance(k, c.key) <= (parts.length > 1 ? c.multi ?? 0 : c.max);   // split forms must match exactly, so "tomato is" stays text
}
/** How many trailing words of `words` form a control word (0 when none); tries 1, 2 and 3 words ("kanga roo", "can guru"). */
function endCount(name, words) {
  for (const k of [1, 2, 3]) if (words.length >= k && soundsLike(name, words.slice(-k))) return k;
  return 0;
}
/** `words` without trailing control words (repeats too: "tomato tomato"); the number removed is `words.length - result.length`. */
export function stripTrailingControl(words, name = 'tomato', extra = []) {
  let n = words.length;
  const flat = w => w.toLowerCase().replace(/[^a-z]/g, '');
  for (;;) {
    const k = extra.includes(flat(words[n - 1] ?? '')) ? 1 : endCount(name, words.slice(0, n));
    if (!k) break;
    n -= k;
  }
  return words.slice(0, n);
}
/** Leading start word(s) removed: how many leading words of `words` are "attention" (also split, e.g. "a tension"). */
export function leadingControlCount(name, words) {
  let i = 0;
  for (;;) {
    let k = 0;
    for (const m of [1, 2, 3]) if (i + m <= words.length && soundsLike(name, words.slice(i, i + m))) { k = m; break; }
    if (!k) return i;
    i += k;
  }
}

/** Domain words for reply dictation (the speech model's context prompt): the assistant, model and tool names this household says a lot. */
export const DOMAIN_PROMPT = 'Claude, Codex, Haiku, Sonnet, Opus, quota, Zulip, nvim, Neovim, agent, topic, GPU, GitHub, Anthropic, OpenAI';

/** `words` without any word that sounds like the end word, wherever it stands: a stray "Tomato." in the middle of a dictation is never content. */
export function removeControlWords(words, name = 'tomato') { return words.filter(w => !soundsLike(name, [w])); }

/** Whether the spoken words `parts` sound like an arbitrary configured keyword (single words: one edit on the sound key; split forms: exact key). */
export function soundsLikeWord(word, parts) {
  const target = soundKey(word), k = soundKey(parts.join(''));
  return target.length >= 3 && k.length >= 3 && distance(k, target) <= (parts.length > 1 ? 0 : target.length >= 5 ? 1 : 0);
}

/** The last word of a reply that the on-device spotter just confirmed as the end word: a clipped start of it ("Tomat" for "tomato") is within 2 edits of its sound key. */
export function looksLikeClippedEndWord(word) { const k = soundKey(word); return k.length >= 5 && distance(k, CONTROL.tomato.key) <= 2; }

/* ---- Reply assembly rules shared by the client and the relay's `send` ---- */
const COMMAND_VOCAB = new Set('attention katana tomato macaroni bamboo cancel never mind skip jump forward backward back next previous message messages again stop play resume switch to agent agents number one two three four five six seven eight nine ten positive negative yes no over send pencil'.split(' '));
/**
 * `texts` (one transcript per clip, in order) -> `{text, discard}`. Rules: a transcript containing the cancel word ("bamboo", phonetic: "Rambo") discards everything; "cancel"/"never mind"
 * as the last utterance discards everything; in each clip the command word and all that follows it is dropped; stray end words go; a reply made only of control words is empty.
 */
export function assembleReply(texts, {cancelWord = 'macaroni', commandWord = 'katana'} = {}) {
  const tokens = t => t.split(/\s+/).filter(Boolean), flat = w => w.toLowerCase().replace(/[^a-z]/g, '');
  const parts = [];
  for (const raw of texts) {
    const words = tokens(raw ?? '');
    if (words.some(w => flat(w) === cancelWord || soundsLikeWord(cancelWord, [flat(w)]))) return {text: '', discard: true, why: 'cancel word'};
    if (words.some(w => soundsLikeWord('kangaroo', [flat(w)]))) return {text: '', discard: true, why: 'retired end word (command speech)'};   // "kangaroo" no longer ends a reply: hearing it (Cangaroo, Canguro) means control speech, so nothing is sent
    const at = words.findIndex(w => flat(w) === commandWord || soundsLikeWord(commandWord, [flat(w)]));
    const kept = stripTrailingControl(removeControlWords(stripTrailingControl(at >= 0 ? words.slice(0, at) : words, 'tomato', ['over'])), 'kangaroo');
    if (kept.length) parts.push(kept.join(' '));
  }
  const last = (parts.at(-1) ?? '').toLowerCase().replace(/[^a-z ]+/g, ' ').replace(/\s+/g, ' ').trim();
  if (/^(cancel|cancel that|never mind|scratch that|forget it)$/.test(last) || /(^| )(cancel that|never mind)$/.test(last)) return {text: '', discard: true, why: 'cancel as the last utterance'};
  const text = parts.join(' ').trim();
  if (!text || tokens(text).every(w => COMMAND_VOCAB.has(flat(w)))) return {text: '', discard: false, why: 'only control words'};
  return {text, discard: false};
}
