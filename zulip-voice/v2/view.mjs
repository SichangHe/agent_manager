/** DOM builders for messages and agent rows. No state. */
import {inlineHtml} from './text.mjs';
import {parsed} from './model.mjs';

const el = (tag, className, html) => { const e = document.createElement(tag); if (className) e.className = className; if (html !== undefined) e.innerHTML = html; return e; };
const textEl = (tag, className, text) => { const e = el(tag, className); e.textContent = text; return e; };

/** An agent message: title caption, then markdown blocks whose spoken chunks are tappable `.s` spans (`data-from`..`data-to` = chunk idx range). */
export function agentEl(message) {
  const p = parsed(message);
  const root = el('article', 'msg agent');
  root.dataset.id = message.id;
  if (p.title) root.append(textEl('div', 'title', p.title.replace(/^\[[^\]]*\]\s*/, '') || p.title));
  const body = el('div', 'body');
  p.blocks.forEach((block, b) => {
    if (block.kind === 'code') { const pre = el('pre'); pre.append(textEl('code', '', block.text)); body.append(pre); return; }
    const node = el(block.kind === 'h' ? 'h4' : 'p', block.kind === 'li' ? 'li' : '');
    if (block.kind === 'li') node.prepend('• ');
    let span = null;
    for (const chunk of p.chunks.filter(c => c.block === b)) {
      if (chunk.idx >= 0 && chunk.text === '' && span) { span.dataset.to = chunk.idx; continue; }
      span = el('span', chunk.idx >= 0 ? 's' : '', inlineHtml(chunk.text));
      if (chunk.idx >= 0) { span.dataset.from = chunk.idx; span.dataset.to = chunk.idx; }
      node.append(span);
    }
    body.append(node);
  });
  if (!p.blocks.length) body.append(textEl('p', 'muted', p.switchTo ? `Switching to ${p.switchTo}` : '(empty)'));
  root.append(body);
  setUnread(root, !message.flags?.includes('read'));
  return root;
}

export const setUnread = (root, unread) => root.classList.toggle('unread', unread);

/** The human's message bubble; `state` is `sent`, `sending` or `failed`. */
export function mineEl(text, state, handlers) {
  const root = el('article', `msg mine ${state === 'sent' ? '' : state === 'queued' ? 'sending' : state}`);
  root.append(textEl('div', 'bubble', text));
  const note = el('div', 'state');
  if (state === 'sending') note.textContent = 'Sending…';
  if (state === 'queued') note.textContent = 'Waiting for connection…';
  if (state === 'failed') {
    note.append('Not sent');
    for (const [label, fn] of [['Retry', handlers.retry], ['Delete', handlers.discard]]) { const b = textEl('button', '', label); b.type = 'button'; b.onclick = fn; note.append(b); }
  }
  root.append(note);
  return root;
}

export function ago(timestampS) {
  const s = Math.max(0, Date.now() / 1000 - timestampS);
  return s < 90 ? 'now' : s < 3600 ? Math.round(s / 60) + 'm' : s < 86400 ? Math.round(s / 3600) + 'h' : Math.round(s / 86400) + 'd';
}

export function topicRow(topic, snippet, current, onPick) {
  const li = el('li');
  li.setAttribute('role', 'button');
  if (current) li.setAttribute('aria-current', 'true');
  li.append(textEl('span', 'name', topic.name), textEl('span', 'snip', snippet));
  if (topic.unread) li.append(textEl('span', 'badge', String(topic.unread)));
  li.onclick = onPick;
  return li;
}
