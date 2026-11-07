/** Pure text logic shared by the browser client and `relay-worker.mjs` (so both chunk a message identically). */

const MAX_CHUNK_CHARS = 240;
const MAX_TTS_CHARS = 1800;
const MAX_CHUNKS_PER_MESSAGE = 20;

export function splitSentences(text) {
  if (typeof Intl.Segmenter === 'function') return [...new Intl.Segmenter(undefined, {granularity: 'sentence'}).segment(text)].map(part => part.segment);
  return text.match(/[^.!?]+[.!?]*(?:\s+|$)|[.!?]+/g) || [text];
}

/** Removes `pending item created:` / `deleted:` / `resolved:` headings and the `- ...` bullets that follow them. */
function dropPendingFooters(lines) {
  const out = [];
  let skipping = false;
  for (const line of lines) {
    if (/^pending items?\s+\w+.*:\s*$/i.test(line.trim())) { skipping = true; continue; }
    if (skipping && /^\s*[-*]\s+/.test(line)) continue;
    skipping = false;
    out.push(line);
  }
  return out;
}

/** Splits an agent message into `{title, body, switchTo}`: the `[task] title` first line and `[switch: TASK]` lines are not body text. */
export function parseAgentMessage(content) {
  let switchTo = null;
  const lines = content.split('\n').filter(line => {
    const match = line.match(/^\[switch: (.+?)\]$/);
    if (match) switchTo = match[1].trim().replace(/\.md$/, '');
    return !match;
  });
  const text = dropPendingFooters(lines).join('\n');
  const first = text.match(/^\[[^\]\n]+\][^\n]*(?:\n|$)/);
  return {title: first ? first[0].trim() : '', body: (first ? text.slice(first[0].length) : text).trim(), switchTo};
}

/** Markdown-ish blocks: `{kind: 'p'|'li'|'h'|'code', text}`. */
export function parseBlocks(body) {
  const blocks = [];
  let paragraph = [], code = null;
  const flush = () => { if (paragraph.length) blocks.push({kind: 'p', text: paragraph.join(' ')}); paragraph = []; };
  for (const line of body.split('\n')) {
    if (/^\s*```/.test(line)) {
      if (code) { blocks.push({kind: 'code', text: code.join('\n')}); code = null; } else { flush(); code = []; }
    } else if (code) code.push(line);
    else if (!line.trim()) flush();
    else if (/^\s*(?:[-*+]|\d+[.)])\s+/.test(line)) { flush(); blocks.push({kind: 'li', text: line.replace(/^\s*(?:[-*+]|\d+[.)])\s+/, '')}); }
    else if (/^#{1,6}\s/.test(line)) { flush(); blocks.push({kind: 'h', text: line.replace(/^#{1,6}\s+/, '')}); }
    else paragraph.push(line.trim());
  }
  if (code) blocks.push({kind: 'code', text: code.join('\n')});
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
    if (block.kind === 'code') return;
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
