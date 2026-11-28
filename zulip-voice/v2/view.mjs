/** DOM builders for messages and agent rows. No state. */
import {t} from './i18n.mjs';
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
    if (block.kind === 'table') {   // read aloud cell by cell; scrolls sideways when wide
      const wrap = el('div', 'table-wrap'), table = el('table'), thead = el('thead'), headRow = el('tr');
      for (const c of block.head) { const th = el('th'); th.innerHTML = inlineHtml(c); headRow.append(th); }
      thead.append(headRow); table.append(thead);
      const tbody = el('tbody');
      for (const row of block.rows) { const tr = el('tr'); block.head.forEach((_, i) => { const td = el('td'); td.innerHTML = inlineHtml(row[i] ?? ''); tr.append(td); }); tbody.append(tr); }
      table.append(tbody); wrap.append(table); body.append(wrap);
      // each spoken chunk maps to a cell: "Table start" and "Row N" join the row's first cell, so the highlight and tap-to-seek follow the audio
      const rows = [...table.querySelectorAll('tr')], span = new Map();
      for (const chunk of p.chunks.filter(x => x.block === b && x.r !== undefined && x.idx >= 0)) {
        const cell = rows[Math.max(0, chunk.r)].children[Math.max(0, chunk.c)];
        let s = span.get(cell);
        if (!s) { s = el('span', 's'); s.dataset.from = chunk.idx; s.append(...cell.childNodes); cell.append(s); span.set(cell, s); }
        s.dataset.to = chunk.idx;
      }
      return;
    }
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
  if (!p.blocks.length) body.append(textEl('p', 'muted', p.switchTo ? t('msg.switching', {name: p.switchTo}) : t('msg.empty')));
  root.append(body);
  if (p.footers.length) {   // amh item footers: shown on wide screens, folded into a chip on narrow ones
    const items = p.footers.filter(l => /^\s*[-*]\s+/.test(l)).length;
    const box = el('div', 'footer');
    const chip = textEl('button', 'footer-chip', t('msg.items', {n: items || p.footers.length}));
    chip.type = 'button'; chip.onclick = () => box.classList.toggle('open');
    const text = textEl('div', 'footer-text', p.footers.join('\n'));
    box.append(chip, text); root.append(box);
  }
  if (p.attachments.length) root.append(attachmentsEl(p.attachments, getUrl));
  const mark = textEl('button', 'mark-read', t('msg.markRead')); mark.type = 'button'; mark.dataset.markRead = message.id; root.append(mark);   // shown only while unread (CSS): clears the dot without playing
  setUnread(root, !message.flags?.includes('read'));
  return root;
}

const IMAGE = /\.(png|jpe?g|gif|webp|bmp|heic|avif|svg)$/i;
const readSet = key => { try { return new Set(JSON.parse(localStorage.getItem(key) || '[]')); } catch { return new Set(); } };
const collapsedKey = 'voice2-att-collapsed', expandedKey = 'voice2-att-expanded';
const saveSet = (key, set) => { try { localStorage.setItem(key, JSON.stringify([...set].slice(-500))); } catch { /* ignore */ } };
export const hideAllAttachments = {on: (() => { try { return localStorage.getItem('voice2-hide-att') === '1'; } catch { return false; } })()};

/** Inline attachments: images as lazy thumbnails, other files as chips. Swipe sideways to collapse to a chip; tap a chip to expand. */
const sessionCollapsed = new Set();   // images collapsed by a swipe: until the page reloads
export function attachmentsEl(attachments, getUrl) {
  const wrap = el('div', 'atts');
  try { localStorage.removeItem(collapsedKey); } catch { /* ignore */ }   // old builds saved sideways swipes forever: those previews stayed hidden after any reload
  const collapsed = sessionCollapsed, expanded = readSet(expandedKey);
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
        const chip = textEl('button', 'chipbtn', (isImage ? '🖼 ' : '📎 ') + a.name + (isImage ? t('att.tapShow') : ''));
        chip.onclick = () => { if (isImage) { collapsed.delete(a.uri); expanded.add(a.uri); saveSet(expandedKey, expanded); render(); } else open(); };
        box.append(chip);
        return;
      }
      const img = el('img'); img.alt = a.name; img.loading = 'lazy'; img.className = 'attimg';
      let tries = 0;
      const load = async () => {
        const r = await getUrl(a.uri, tries > 0);   // a retry asks for a fresh signed link
        if (r.ok) img.src = r.url; else { img.alt = a.name + ' (unavailable)'; if (++tries < 4) setTimeout(load, 2000 * tries); }
        window.dispatchEvent(new CustomEvent('att-log', {detail: `${a.name}: ${r.ok ? 'link ok' : 'link failed'} (try ${tries + 1})`}));
      };
      img.onerror = () => { if (++tries < 4) setTimeout(load, 1500 * tries); else img.alt = a.name + ' (could not load)'; };   // expired or dropped: fetch again instead of leaving a blank
      new IntersectionObserver((entries, o) => { if (entries[0].isIntersecting) { o.disconnect(); load(); } }).observe(img);
      img.onclick = open;
      const cap = el('div', 'attcap'), name = textEl('span', 'attname', a.name), hide = textEl('button', 'atthide', t('att.hide'));   // the name is always visible: agents refer to pictures by it
      hide.type = 'button'; hide.onclick = () => { collapsed.add(a.uri); expanded.delete(a.uri); saveSet(expandedKey, expanded); render(); };
      cap.append(name, hide);
      box.append(img, cap);
    };
    let x0 = null, y0 = 0;
    box.addEventListener('touchstart', e => { x0 = e.touches[0].clientX; y0 = e.touches[0].clientY; }, {passive: true});
    box.addEventListener('touchend', e => { const dx = x0 === null ? 0 : e.changedTouches[0].clientX - x0, dy = e.changedTouches[0].clientY - y0; if (x0 !== null && isImage && Math.abs(dx) > 110 && Math.abs(dy) < 25) {   // a clearly horizontal swipe only: scrolling the feed over a picture must not hide it
 collapsed.add(a.uri); expanded.delete(a.uri); render(); } x0 = null; }, {passive: true});   // swipe-collapse: this session only
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
  if (state === 'sending') note.textContent = t('out.sending');
  if (state === 'queued') note.textContent = t('out.queued');
  if (state === 'failed') {
    note.append(t('out.failed'));
    for (const [label, fn] of [[t('out.retry'), handlers.retry], [t('out.delete'), handlers.discard]]) { const b = textEl('button', '', label); b.type = 'button'; b.onclick = fn; note.append(b); }
  }
  root.append(note);
  return root;
}

export function ago(timestampS) {
  const s = Math.max(0, Date.now() / 1000 - timestampS);
  return s < 90 ? t('ago.now') : s < 3600 ? t('ago.m', {n: Math.round(s / 60)}) : s < 86400 ? t('ago.h', {n: Math.round(s / 3600)}) : t('ago.d', {n: Math.round(s / 86400)});
}

export function topicRow(topic, snippet, current, onPick) {
  const li = el('li');
  li.setAttribute('role', 'button');
  if (current) li.setAttribute('aria-current', 'true');
  const [chan, ...rest] = topic.name.split(' · ');   // "channel · topic" for channels other than AMH
  li.append(textEl('span', 'name', rest.length ? rest.join(' · ') : topic.name), ...(rest.length ? [textEl('span', 'chan', chan)] : []), textEl('span', 'snip', snippet));
  if (topic.unread) li.append(textEl('span', 'badge', String(topic.unread)));
  li.onclick = onPick;
  return li;
}
