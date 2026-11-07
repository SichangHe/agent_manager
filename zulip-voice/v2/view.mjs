/** DOM builders for messages and agent rows. No state. */
import {inlineHtml, extractAttachments} from './text.mjs';
import {parsed} from './model.mjs';

const el = (tag, className, html) => { const e = document.createElement(tag); if (className) e.className = className; if (html !== undefined) e.innerHTML = html; return e; };
const textEl = (tag, className, text) => { const e = el(tag, className); e.textContent = text; return e; };

/** An agent message: title caption, then markdown blocks whose spoken chunks are tappable `.s` spans (`data-from`..`data-to` = chunk idx range). */
export function agentEl(message, getUrl) {
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
  if (p.attachments.length) root.append(attachmentsEl(p.attachments, getUrl));
  setUnread(root, !message.flags?.includes('read'));
  return root;
}

const IMAGE = /\.(png|jpe?g|gif|webp|bmp|heic|avif|svg)$/i;
const readSet = key => { try { return new Set(JSON.parse(localStorage.getItem(key) || '[]')); } catch { return new Set(); } };
const collapsedKey = 'voice2-att-collapsed', expandedKey = 'voice2-att-expanded';
const saveSet = (key, set) => { try { localStorage.setItem(key, JSON.stringify([...set].slice(-500))); } catch { /* ignore */ } };
export const hideAllAttachments = {on: (() => { try { return localStorage.getItem('voice2-hide-att') === '1'; } catch { return false; } })()};

/** Inline attachments: images as lazy thumbnails, other files as chips. Swipe sideways to collapse to a chip; tap a chip to expand. */
export function attachmentsEl(attachments, getUrl) {
  const wrap = el('div', 'atts');
  const collapsed = readSet(collapsedKey), expanded = readSet(expandedKey);
  for (const a of attachments) {
    const box = el('div', 'att');
    box.dataset.uri = a.uri;
    const isImage = IMAGE.test(a.name) || IMAGE.test(a.uri);
    const hidden = () => (hideAllAttachments.on ? !expanded.has(a.uri) : collapsed.has(a.uri));
    const open = async () => { const r = await getUrl(a.uri); if (r.ok) window.open(r.url, '_blank', 'noopener'); };
    const render = () => {
      box.replaceChildren();
      box.classList.toggle('chip', hidden() || !isImage);
      if (hidden() || !isImage) {
        const chip = textEl('button', 'chipbtn', (isImage ? '🖼 ' : '📎 ') + a.name);
        chip.onclick = () => { if (isImage) { collapsed.delete(a.uri); expanded.add(a.uri); saveSet(collapsedKey, collapsed); saveSet(expandedKey, expanded); render(); } else open(); };
        box.append(chip);
        return;
      }
      const img = el('img'); img.alt = a.name; img.loading = 'lazy'; img.className = 'attimg';
      const load = async () => { const r = await getUrl(a.uri); if (r.ok) img.src = r.url; else img.alt = a.name + ' (unavailable)'; };
      new IntersectionObserver((entries, o) => { if (entries[0].isIntersecting) { o.disconnect(); load(); } }).observe(img);
      img.onclick = open;
      box.append(img);
    };
    let x0 = null;
    box.addEventListener('touchstart', e => { x0 = e.touches[0].clientX; }, {passive: true});
    box.addEventListener('touchend', e => { if (x0 !== null && isImage && Math.abs(e.changedTouches[0].clientX - x0) > 50) { collapsed.add(a.uri); expanded.delete(a.uri); saveSet(collapsedKey, collapsed); saveSet(expandedKey, expanded); render(); } x0 = null; }, {passive: true});
    box.collapse = () => { collapsed.add(a.uri); expanded.delete(a.uri); render(); };
    render();
    wrap.append(box);
  }
  return wrap;
}

export const setUnread = (root, unread) => root.classList.toggle('unread', unread);

/** The human's message bubble; `state` is `sent`, `sending` or `failed`. */
export function mineEl(text, state, handlers, getUrl) {
  const root = el('article', `msg mine ${state === 'sent' ? '' : state === 'queued' ? 'sending' : state}`);
  const {text: plain, attachments} = extractAttachments(text);
  if (plain.trim() || !attachments.length) root.append(textEl('div', 'bubble', plain.trim()));
  if (attachments.length && getUrl) root.append(attachmentsEl(attachments, getUrl));
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
