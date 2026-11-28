/**
 * Microphone capture cut into independent 16 kHz mono WAV clips at pauses in speech (or at `MAX_CLIP_S`).
 * Raw PCM through an AudioWorklet avoids MediaRecorder, whose chunks are not independently decodable and which is flaky on iOS.
 * Clips without speech are dropped.
 */
const TARGET_HZ = 16000;
const MAX_CLIP_S = 10;
const PAUSE_MS = 700;
const REPLY_PAUSE_MS = 1100, REPLY_MAX_CLIP_S = 30;   // a spoken reply (voice control) is cut into fewer, longer clips: fewer uploads and transcriptions
const LISTEN_PAUSE_MS = 250;   // a start-word clip ends sooner: the command is one short word
// Input mode (a spoken reply) keeps ONE growing piece instead of cutting at every pause: the recognizer does better with long context.
// While speaking, the piece so far is re-sent every PIECE_REFRESH_MS (and once when a pause starts) so the text stays live; the piece ends at
// PIECE_MAX_S (or PIECE_SOFT_S at a pause). Speed measured 2026-11-26: Qwen3-ASR takes about 0.1 s per second of speech, so 15 s is about 1.5 s.
export const PIECE_MAX_S = 15, PIECE_SOFT_S = 12, PIECE_REFRESH_MS = 3000, PIECE_SILENCE_KEEP_MS = 1500, PIECE_PAUSE_MS = 700;
const MIN_SPEECH_MS = 250;
const PREROLL_MS = 300;
const MIN_LEVEL = 0.008;     // lowered from 0.012 and the margin from 3x to 2.5x: a false trigger is harmless now (the start word gates everything)
const NOISE_MARGIN = 2.5;
const BACKOFF_MARGIN = 5;   // after repeated empty results the room is noisy: only clearly louder speech may start a clip
const BARGE_MS = 180;
const LISTEN_PREROLL_MS = 1200;
export const EARCON_NOTE_S = 0.14;
const BACKOFF_BARGE_MS = 300;
const REPLY_BARGE_MS = 100;        // while a spoken reply is open, a short word like "over" must still become a clip
const REPLY_MIN_SPEECH_MS = 100;

const WORKLET = `registerProcessor('cap', class extends AudioWorkletProcessor {
  constructor() { super(); this.buf = new Float32Array(2048); this.n = 0; }
  process(inputs) {
    const ch = inputs[0][0];
    if (!ch) return true;
    for (let i = 0; i < ch.length; i++) { this.buf[this.n++] = ch[i]; if (this.n === this.buf.length) { this.port.postMessage(this.buf.slice()); this.n = 0; } }
    return true;
  }
});`;

/** G.711 mu-law byte of a sample in [-1, 1]. */
function mulaw(x) {
  const sign = x < 0 ? 0x80 : 0;
  const magnitude = Math.min(32635, Math.round(Math.abs(x) * 32767)) + 132;
  let exponent = 7;
  for (let mask = 0x4000; (magnitude & mask) === 0 && exponent > 0; mask >>= 1) exponent--;
  return ~(sign | (exponent << 4) | ((magnitude >> (exponent + 3)) & 0x0f)) & 0xff;
}

function resampleToWav(frames, fromHz) {
  const total = frames.reduce((n, f) => n + f.length, 0);
  const flat = new Float32Array(total);
  let at = 0;
  for (const f of frames) { flat.set(f, at); at += f.length; }
  const outLen = Math.floor(total * TARGET_HZ / fromHz);
  // 8-bit mu-law (WAVE format 7) halves the upload against 16-bit PCM; speech recognition is unaffected at 16 kHz.
  const view = new DataView(new ArrayBuffer(46 + outLen));
  const ascii = (o, str) => { for (let i = 0; i < str.length; i++) view.setUint8(o + i, str.charCodeAt(i)); };
  ascii(0, 'RIFF'); view.setUint32(4, 38 + outLen, true); ascii(8, 'WAVEfmt '); view.setUint32(16, 18, true);
  view.setUint16(20, 7, true); view.setUint16(22, 1, true); view.setUint32(24, TARGET_HZ, true); view.setUint32(28, TARGET_HZ, true);
  view.setUint16(32, 1, true); view.setUint16(34, 8, true); view.setUint16(36, 0, true); ascii(38, 'data'); view.setUint32(42, outLen, true);
  const ratio = fromHz / TARGET_HZ;
  for (let i = 0; i < outLen; i++) {
    const from = Math.floor(i * ratio), to = Math.min(total, Math.max(from + 1, Math.floor((i + 1) * ratio)));
    let sum = 0;
    for (let j = from; j < to; j++) sum += flat[j];
    view.setUint8(46 + i, mulaw(Math.max(-1, Math.min(1, sum / (to - from)))));
  }
  return new Blob([view], {type: 'audio/wav'});
}

/** `start()` resolves to `{ok: true}` or `{ok: false, error}`. `onClip(blob, {ms})` per clip, `onLevel(0..1)` ~23 times a second. */
export const EARCON_NOTES = {ready: [880, 1320], done: [1320, 880], cancel: [1319, 1047, 784], command: [740, 0, 740], tick: [1568], updated: [988, 740, 988], interrupted: [1000, 850, 720, 620], empty: [1100, 0, 1100], error: [1400, 1000, 1400, 1000], limited: [1568, 0, 1568, 0, 1568], pulseInput: [220, 0, 330], pulseCommand: [220, 0, 220]};   // every note is 600-1600 Hz: phone speakers play lower tones too quietly

export function createRecorder({log, partialReady = () => true, onClip, onLevel, onBarge, onCommandClip, onEarcon, onKwsAudio, onSpeechOnset}) {
  /** Every change of the iOS audio session type goes through here and is logged (`route ...`): changes make the output volume jump. */
  const setSession = type => { try { if (navigator.audioSession.type !== type) { navigator.audioSession.type = type; log('route', `audioSession.type -> ${type}`); } } catch { /* unsupported */ } };
  let epoch = 0, ctx = null, stream = null, node = null, sourceNode = null, warm = null, capturing = false, listening = false, replyMode = false, backoff = false, earconUntilMs = 0, speechMuteUntilMs = 0, overPromptFired = false, sustainMs = 0, idleTimer = 0;
  let flowWaiters = [], framesSeen = 0;
  let lastSpeechAt = 0;
  let frames = [], preroll = [], speechMs = 0, silentMs = 0, clipMs = 0, noise = 0.004, clipsOut = 0;
  let pieceMode = false, pieceId = 0, pieceSeq = 0, emittedSpeechMs = 0, sinceEmitMs = 0, lastEmitSettled = false;

  /** Input mode: sends the piece so far (`final`: it is complete, the next speech starts a new piece). */
  function emitPiece(final) {
    const settled = final || silentMs >= PIECE_PAUSE_MS, seq = ++pieceSeq;
    const blob = resampleToWav(frames, ctx.sampleRate);
    log('mic', `piece ${pieceId} #${seq}${final ? ' final' : settled ? ' settled' : ''}: ${Math.round(clipMs)}ms audio, ${Math.round(speechMs)}ms speech, ${blob.size}B`);
    onCommandClip(blob, {ms: clipMs, speechMs, piece: {id: pieceId, seq}, final, settled});
    emittedSpeechMs = speechMs; sinceEmitMs = 0; lastEmitSettled = settled;
    if (final) { frames = []; speechMs = silentMs = clipMs = 0; emittedSpeechMs = 0; pieceId++; pieceSeq = 0; lastEmitSettled = false; }
  }

  function cut(final) {
    if (pieceMode && !capturing) {
      if (speechMs >= REPLY_MIN_SPEECH_MS) emitPiece(true); else { frames = []; speechMs = silentMs = clipMs = 0; emittedSpeechMs = 0; }
      return;
    }
    if (speechMs >= (replyMode ? REPLY_MIN_SPEECH_MS : MIN_SPEECH_MS)) {
      const blob = resampleToWav(frames, ctx.sampleRate);
      clipsOut++;
      log('mic', `clip ${clipsOut}: ${Math.round(clipMs)}ms audio, ${Math.round(speechMs)}ms speech, ${blob.size}B${final ? ' (final)' : ''}`);
      (capturing ? onClip : onCommandClip)(blob, {ms: clipMs, speechMs});
    }
    frames = []; speechMs = 0; silentMs = 0; clipMs = 0;
  }

  /** 16 kHz copy of `frame` by box averaging, for the in-browser keyword spotter. */
  function toKwsRate(frame) {
    const ratio = ctx.sampleRate / 16000, n = Math.floor(frame.length / ratio), out = new Float32Array(n);
    for (let i = 0; i < n; i++) { const from = Math.floor(i * ratio), to = Math.max(from + 1, Math.floor((i + 1) * ratio)); let sum = 0; for (let j = from; j < to; j++) sum += frame[j]; out[i] = sum / (to - from); }
    return out;
  }

  function onFrame(frame) {
    if (!ctx || (!capturing && !listening)) return;   // frames after stop(), or while idle in the grace period
    if (listening && !capturing && onKwsAudio && performance.now() >= speechMuteUntilMs) onKwsAudio(toKwsRate(frame));
    const ms = frame.length / ctx.sampleRate * 1000;
    if (capturing && flowWaiters.length && (framesSeen++ >= 2 || frame.some(x => x !== 0))) { const w = flowWaiters.splice(0); w.forEach(f => f(true)); }   // the microphone really delivers audio now (some start with a silent gap)
    let sum = 0;
    for (let i = 0; i < frame.length; i++) sum += frame[i] * frame[i];
    const level = Math.sqrt(sum / frame.length);
    const speaking = level > Math.max(MIN_LEVEL, noise * (backoff ? BACKOFF_MARGIN : NOISE_MARGIN));
    if (!speaking) noise = noise * 0.98 + level * 0.02; else lastSpeechAt = performance.now();
    onLevel?.(Math.min(1, level * 8));
    if (!capturing && !frames.length) {   // voice control: only sustained speech (not echo blips) barges in
      if (performance.now() < earconUntilMs) {   // our own chime or prompt must not count as speech: no barge-in, no clip
        if (performance.now() >= speechMuteUntilMs && speaking) { sustainMs += ms; if (sustainMs >= BARGE_MS && !overPromptFired) { overPromptFired = true; log('mic', 'speech over our own prompt: spotters restarted'); onSpeechOnset?.(); } } else { sustainMs = 0; if (!speaking) overPromptFired = false; }   // but the keyword spotters still restart on a start word spoken over it
        preroll = []; return;
      }
      if (!speaking && sustainMs >= 80 && sustainMs < BARGE_MS) log('mic', `near miss: speech ${Math.round(sustainMs)}ms < ${BARGE_MS}ms (level ${level.toFixed(3)}, floor ${noise.toFixed(3)})`);
      sustainMs = speaking ? sustainMs + ms : 0;
      preroll.push(frame); while (preroll.length * ms > LISTEN_PREROLL_MS) preroll.shift();   // ring buffer: speech before the decision is kept
      if (sustainMs < (replyMode ? REPLY_BARGE_MS : backoff ? BACKOFF_BARGE_MS : BARGE_MS)) return;
      sustainMs = 0; log('mic', `barge-in: speech sustained ${replyMode ? REPLY_BARGE_MS : BARGE_MS}ms`); onBarge?.(); onSpeechOnset?.();
      frames = preroll; preroll = []; clipMs = frames.length * ms; speechMs = replyMode ? REPLY_BARGE_MS : BARGE_MS; silentMs = 0;
      return;
    }
    if (!frames.length && !speaking) { preroll.push(frame); while (preroll.length * ms > PREROLL_MS) preroll.shift(); return; }
    if (!frames.length) { frames = preroll; preroll = []; clipMs = frames.length * ms; }
    if (speaking) { speechMs += ms; silentMs = 0; if (speechMs - emittedSpeechMs > 0) lastEmitSettled = false; } else silentMs += ms;
    if (pieceMode && !capturing) {   // one growing piece: a pause does not end it
      if (speaking || silentMs <= PIECE_SILENCE_KEEP_MS) { frames.push(frame); clipMs += ms; }   // a long silence inside a piece is not kept
      sinceEmitMs += ms;
      if (clipMs >= PIECE_MAX_S * 1000 || (clipMs >= PIECE_SOFT_S * 1000 && silentMs >= PIECE_PAUSE_MS)) { cut(false); return; }
      const grown = speechMs - emittedSpeechMs, pausedNow = silentMs >= PIECE_PAUSE_MS && !lastEmitSettled && speechMs > 0;
      if (speechMs >= MIN_SPEECH_MS && partialReady() && ((grown >= 300 && sinceEmitMs >= PIECE_REFRESH_MS) || pausedNow)) emitPiece(false);
      return;
    }
    frames.push(frame); clipMs += ms;
    if (silentMs >= (replyMode && !capturing ? REPLY_PAUSE_MS : capturing || replyMode ? PAUSE_MS : LISTEN_PAUSE_MS) || clipMs >= (replyMode && !capturing ? REPLY_MAX_CLIP_S : MAX_CLIP_S) * 1000) cut(false);
  }

  /** Builds the context, stream and worklet once; released by `teardown` as soon as neither dictation nor voice control needs it. */
  async function build() {
    const mine = ++epoch;
    const cancelled = () => epoch !== mine;
    setSession('play-and-record');
    const c = new (window.AudioContext || window.webkitAudioContext)();
    let s = null;
    const abandon = () => { s?.getTracks().forEach(t => t.stop()); c.close().catch(() => {}); throw Error('cancelled'); };   // `teardown` ran while this build was still waiting
    try {
      const resumed = c.resume();
      s = await navigator.mediaDevices.getUserMedia({audio: {echoCancellation: true, noiseSuppression: true, autoGainControl: true}});
      if (cancelled()) abandon();
      for (const t of s.getTracks()) for (const ev of ['mute', 'unmute', 'ended']) t.addEventListener(ev, () => log('route', `mic track ${ev}`));
      c.addEventListener('statechange', () => log('route', `audio context ${c.state}`));
      await Promise.race([resumed, new Promise(r => setTimeout(r, 400))]);   // stays suspended until a gesture on iOS; a later tap resumes it
      if (cancelled()) abandon();
      const url = URL.createObjectURL(new Blob([WORKLET], {type: 'text/javascript'}));
      await c.audioWorklet.addModule(url);
      URL.revokeObjectURL(url);
      if (cancelled()) abandon();
    } catch (error) { if (error.message !== 'cancelled') { s?.getTracks().forEach(t => t.stop()); c.close().catch(() => {}); } throw error; }
    ctx = c; stream = s;
    sourceNode = ctx.createMediaStreamSource(stream);
    node = new AudioWorkletNode(ctx, 'cap');
    node.connect(ctx.destination);
    node.port.onmessage = event => onFrame(event.data);
    sourceNode.connect(node);
    log('mic', `ready at ${ctx.sampleRate}Hz`);
  }

  /** One in-flight `build`; a failure forgets it, and a cancelled one (superseded by `teardown`) leaves the newer build alone. */
  function built() {
    const p = build().catch(error => { if (warm === p) warm = null; if (error.message !== 'cancelled') teardown(); throw error; });
    return p;
  }

  function teardown() {
    epoch++;   // abandons a build that is still in flight
    node?.disconnect(); sourceNode?.disconnect();
    stream?.getTracks().forEach(track => track.stop());
    ctx?.close().catch(() => {});
    setSession('playback');
    ctx = stream = node = sourceNode = warm = null;
    log('mic', 'released');
  }

  return {
    /** Starts capturing at once; the first call also pays the one-time setup. Safe to call repeatedly or during startup. */
    /** Voice control on/off: keeps the warm microphone open and watches for sustained speech. `{ok}` or `{ok: false, error}`. */
    async listen(on) {
      listening = on; sustainMs = 0; clearTimeout(idleTimer);
      if (!on) { if (!capturing) teardown(); return {ok: true}; }
      try {
        warm ??= built();
        await warm;
        if (ctx?.state === 'suspended') await Promise.race([ctx.resume(), new Promise(r => setTimeout(r, 400))]);   // without a gesture iOS may keep it suspended
        return {ok: true, running: ctx?.state === 'running'};
      }
      catch (error) { log('mic', `listen failed: ${error.name} ${error.message}`); listening = false; return {ok: false, error: error.name === 'NotAllowedError' ? 'Microphone permission denied.' : `Microphone unavailable (${error.name}).`}; }
    },
    async start() {
      if (capturing) return {ok: true};
      capturing = true; framesSeen = 0;
      frames = []; preroll = []; speechMs = silentMs = clipMs = 0; clipsOut = 0; sustainMs = 0;
      try {
        warm ??= built();
        await warm;
        if (ctx?.state === 'suspended') await ctx.resume();
        return capturing ? {ok: true} : {ok: false, cancelled: true};
      } catch (error) {
        if (error.message === 'cancelled') return {ok: false, cancelled: true};   // stop() superseded this start; a newer start owns `capturing`
        capturing = false;
        log('mic', `start failed: ${error.name} ${error.message}`);
        return {ok: false, error: error.name === 'NotAllowedError' ? 'Microphone permission denied. Allow it in Settings > Safari > Microphone.' : `Microphone unavailable (${error.name}).`};
      }
    },
    /** Resolves true when audio frames are really arriving while capturing; false after `timeoutMs`. */
    whenFlowing(timeoutMs = 3000) { return new Promise(res => { flowWaiters.push(res); setTimeout(() => { const i = flowWaiters.indexOf(res); if (i >= 0) { flowWaiters.splice(i, 1); res(false); } }, timeoutMs); }); },
    /** Emits the last partial clip and stops capturing; the microphone is released after the grace period. Also cancels a startup in progress. */
    stop() {
      if (capturing && ctx && frames.length) cut(true);
      capturing = false;
      onLevel?.(0);
      sustainMs = 0; frames = []; preroll = [];
      if (!capturing && !listening) teardown();
    },
    /** Short quiet chime on the warm audio context: one of EARCON_NOTES. Voice detection ignores it while it plays. */
    beep(kind) {
      const notes = EARCON_NOTES[kind];
      onEarcon?.(kind);   // played by the app on an <audio> element: no extra AudioContext, same output path and level as speech
      earconUntilMs = performance.now() + notes.length * EARCON_NOTE_S * 1000 + 250;
    },
    /** Milliseconds until the chime in progress has finished (0 if none): playback must not start over it. */
    earconLeftMs: () => Math.max(0, earconUntilMs - 250 - performance.now()),
    /** Ignore the microphone for `ms` (our own speech or chime is playing). */
    muteFor(ms, keepSpotting = false) { earconUntilMs = performance.now() + ms + 250; if (!keepSpotting) speechMuteUntilMs = earconUntilMs; },   // keepSpotting: our own prompt is playing but start words may still interrupt it
    unmute() { earconUntilMs = speechMuteUntilMs = 0; },
    /** Cuts the clip in progress right now and hands it over (a spotted end word does not wait for a pause in speech). */
    flush() { if (listening && !capturing && frames.length) { speechMs = Math.max(speechMs, replyMode ? REPLY_MIN_SPEECH_MS : MIN_SPEECH_MS); cut(true); } },
    /** Reply mode lowers the speech thresholds so short words are never dropped. */
    setReplyMode(on) { replyMode = on; if (!on) pieceMode = false; },
    /** Input mode: one growing piece (see PIECE_MAX_S). Off in command mode, where each phrase is its own clip. */
    setPieceMode(on) { pieceMode = on && replyMode; },
    /** The open piece: `{open, id, speechMs}`. */
    pieceInfo: () => ({open: pieceMode && frames.length > 0, id: pieceId, speechMs}),
    /** Throws away the open piece without sending it (its text is already known). */
    drop() { frames = []; speechMs = silentMs = clipMs = 0; emittedSpeechMs = 0; lastEmitSettled = false; },
    /** Noisy-room mode: higher level and longer duration needed to start a start-word clip. */
    setBackoff(on) { backoff = on; },
    /** True while an utterance is being recorded (speech since the last clip cut): the user is talking right now. */
    inUtterance: () => frames.length > 0 && (!pieceMode || silentMs < PIECE_PAUSE_MS),   // a piece stays open through pauses: only recent speech counts
    /** ms since the microphone last heard speech (Infinity when the mic is off): playback starts only after a quiet spell. */
    quietMs: () => (!ctx || (!capturing && !listening)) ? Infinity : performance.now() - lastSpeechAt,
    active: () => capturing,
    /** True while the microphone stream is open (dictation or voice control). */
    micOpen: () => !!ctx,
  };
}
