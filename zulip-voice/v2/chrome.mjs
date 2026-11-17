/** Small UI chrome shared by the app modules: toast, banner, tap flash, microphone level meter. */
export const $ = id => document.getElementById(id);

let toastTimer = 0;
/** A short message; with `action`, a button that runs `onAction` (shown longer). */
export function toast(text, action, onAction) {
  const t = $('toast');
  t.replaceChildren(text);
  if (action) { const b = document.createElement('button'); b.textContent = action; b.onclick = () => { t.hidden = true; onAction(); }; t.append(b); }
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, action ? 7000 : 3200);
}

let bannerAction = null, kind = '';
export function showBanner(text, k, onTap) { const b = $('banner'); b.textContent = text; b.className = k; kind = k; b.hidden = false; bannerAction = onTap; }
export function hideBanner() { $('banner').hidden = true; bannerAction = null; }
export const bannerKind = () => kind;
$('banner').onclick = () => bannerAction?.();

/** A brief centred symbol confirming a tap (pause, play). */
export function flash(text) { const el = $('tap-flash'); el.textContent = text; el.classList.remove('show'); void el.offsetWidth; el.classList.add('show'); }

export function drawLevel(level) {
  const canvas = $('level'), ctx = canvas.getContext('2d');
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue('--danger') || '#c00';
  const bars = 10, w = canvas.width / bars;
  for (let i = 0; i < bars; i++) { const h = Math.max(3, canvas.height * Math.min(1, level * (0.5 + Math.sin(i * 1.7 + performance.now() / 120) * 0.5 + 0.4))); ctx.fillRect(i * w + 2, (canvas.height - h) / 2, w - 4, h); }
}
