/** Manual dictation from the composer's microphone button: clips are transcribed into the draft; "send" goes at once (unfinished clips as audio). */
import {DOMAIN_PROMPT, DOMAIN_PROMPT_ZH} from './text.mjs';
import {t, getLang} from './i18n.mjs';
import {$, toast, showBanner, hideBanner} from './chrome.mjs';

const joinText = (base, parts) => [base.trim(), ...parts].filter(Boolean).join(' ');

/** ctx: {log, player, voice, recorder(), draft, autosize(), saveDraft(), voiceDown(), isLive(), resumeAfterPause(), maybeAutoplay(), send(), sendLocal(text, items)} */
export function createDictation(ctx) {
  const {log, player, voice, draft} = ctx;
  let dict = null;   // `{base, parts, stopping, sendAfter, cancelled, wasPlaying, sentAway}`

  function sync() {
    const done = dict.parts.filter(p => p.state === 'done').map(p => p.text);
    const pending = dict.parts.some(p => p.state === 'pending');
    draft.value = joinText(dict.base, done) + (pending ? ' …' : '');
    ctx.autosize();
    draft.scrollTop = draft.scrollHeight;
  }
  function setRecordingUi(on) {
    $('composer').classList.toggle('rec', on); $('cancel-rec').hidden = !on; $('level').hidden = !on;
    draft.readOnly = on; $('mic').setAttribute('aria-label', on ? t('dictate.stop') : t('dictate')); $('send').setAttribute('aria-label', on ? t('send.finish') : t('send'));
  }

  /** The UI flips to recording on the tap itself; microphone setup finishes in the background and a failure reverts it. */
  async function start() {
    if (dict) return;
    player.unlock();
    if (ctx.voiceDown()) toast(t('dict.voiceDown'));
    const wasPlaying = ['playing', 'loading'].includes(player.position().status);
    const mine = dict = {base: draft.value, parts: [], stopping: false, sendAfter: false, cancelled: false, wasPlaying};
    if (wasPlaying) player.pause();
    setRecordingUi(true); $('send').disabled = false;
    $('composer').classList.add('warming'); draft.placeholder = t('dict.starting');   // not ready yet: the first moments of speech would be cut off
    const r = await ctx.recorder().start();
    if (r.ok && dict === mine) {
      const flowing = await ctx.recorder().whenFlowing();
      if (dict === mine) { $('composer').classList.remove('warming'); draft.placeholder = t('message'); if (flowing) ctx.recorder().beep('ready'); else toast(t('dict.slowMic')); log('mic', `dictation mic ${flowing ? 'ready' : 'not delivering audio after 3 s'}`); }
    }
    if (dict !== mine || !r.ok) { $('composer').classList.remove('warming'); draft.placeholder = t('message'); }
    if (r.ok || dict !== mine) return;   // started, or the user already stopped it
    mine.stopping = true; mine.cancelled = true;
    if (r.error) toast(r.error);
    maybeFinish();
  }

  function onClip(blob, meta = {}) {
    if (!dict || dict.cancelled) return;
    const part = {state: 'pending', text: '', blob, speechMs: meta.speechMs};
    dict.parts.push(part);
    transcribe(part, dict);
    sync();
  }

  /** Transcribes `part`, retrying with backoff while the network is bad; the clip stays pending (shown as `…`) meanwhile. */
  function transcribe(part, owner) {
    part.state = 'pending';
    (async () => {
      let r;
      for (let attempt = 0; attempt < 12; attempt++) {
        r = await voice.stt(part.blob, (part.speechMs ?? 9e9) >= 1500 ? (getLang() === 'zh' ? DOMAIN_PROMPT_ZH : DOMAIN_PROMPT) : undefined);   // domain words bias reply dictation only
        if (r.ok) break;
        log('stt', `clip attempt ${attempt + 1} failed: ${r.error}`);
        for (let waited = 0; waited < 20000 && !ctx.isLive(); waited += 500) await new Promise(res => setTimeout(res, 500));
        await new Promise(res => setTimeout(res, Math.min(1000 * (attempt + 1), 8000)));
      }
      return r;
    })().then(r => {
      if (owner.sentAway) return;
      part.state = r.ok ? 'done' : 'failed';
      part.text = r.ok ? r.text : '';
      if (!r.ok) log('stt', `clip failed: ${r.error}`);
      if (owner === dict) { sync(); maybeFinish(); } else if (r.ok) { draft.value = joinText(draft.value, [r.text]); ctx.autosize(); ctx.saveDraft(); }
      showFailedClips(owner);
    });
  }

  function showFailedClips(owner) {
    const failed = owner.parts.filter(p => p.state === 'failed');
    if (failed.length) showBanner(t('dict.failedClips', {n: failed.length}), 'warn', () => { for (const p of failed) transcribe(p, owner); hideBanner(); });
  }

  function stop({send, cancel}) {
    if (!dict || dict.stopping) return;
    dict.stopping = true; dict.sendAfter = send; dict.cancelled = !!cancel;
    ctx.recorder().stop();
    if (cancel) draft.value = dict.base;
    if (send && !cancel) { sendNow(dict); return; }   // no waiting for transcripts
    maybeFinish();
  }
  /** Send now: finished transcripts go as text, unfinished clips as uploads for the service to transcribe; the UI is free at once. */
  function sendNow(finished) {
    finished.sentAway = true;
    const items = [];
    const prefix = finished.base.trim();
    if (prefix) items.push({text: prefix});
    for (const p of finished.parts) items.push(p.state === 'done' ? {text: p.text} : {blob: p.blob});
    const preview = [prefix, ...finished.parts.map(p => p.state === 'done' ? p.text : '…')].filter(Boolean).join(' ');
    setRecordingUi(false);
    dict = null; draft.value = ''; ctx.autosize(); ctx.saveDraft();
    if (!items.some(i => i.blob || i.text.trim())) { if (finished.wasPlaying) ctx.resumeAfterPause(); return; }
    const local = ctx.sendLocal(preview || '…', items);
    log('send', `voice send ${local.sendId}: ${items.length} parts, ${items.filter(i => i.blob).length} still to transcribe`);
    if (finished.wasPlaying) ctx.resumeAfterPause(); else ctx.maybeAutoplay();
  }

  function maybeFinish() {
    if (!dict?.stopping || dict.parts.some(p => p.state === 'pending')) return;
    const finished = dict;
    setRecordingUi(false);
    dict = null;
    ctx.autosize(); ctx.saveDraft();
    if (finished.parts.some(p => p.state === 'failed')) { showFailedClips(finished); finished.parts = finished.parts.filter(p => p.state === 'failed'); }
    // Never send with a clip missing: a failed clip keeps the draft and offers a retry.
    if (finished.sendAfter && draft.value.trim() && !finished.parts.some(p => p.state === 'failed')) ctx.send();
    if (finished.wasPlaying) ctx.resumeAfterPause(); else ctx.maybeAutoplay();
  }

  $('mic').onclick = () => { if (dict) stop({send: false}); else start(); };
  $('cancel-rec').onclick = () => stop({cancel: true});
  return {onClip, stop, active: () => !!dict, relocalize: () => { setRecordingUi(!!dict); if (!dict) draft.placeholder = t('message'); }};
}
