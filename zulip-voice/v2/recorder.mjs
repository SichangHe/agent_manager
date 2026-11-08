/**
 * Microphone capture cut into independent 16 kHz mono WAV clips at pauses in speech (or at `MAX_CLIP_S`).
 * Raw PCM through an AudioWorklet avoids MediaRecorder, whose chunks are not independently decodable and which is flaky on iOS.
 * Clips without speech are dropped.
 */
const TARGET_HZ = 16000;
const MAX_CLIP_S = 10;
const PAUSE_MS = 700;
const MIN_SPEECH_MS = 250;
const PREROLL_MS = 300;
const MIN_LEVEL = 0.012;
const GRACE_MS = 45000;
const BARGE_MS = 300;

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
export function createRecorder({log, onClip, onLevel, onBarge, onCommandClip}) {
  let ctx = null, stream = null, node = null, sourceNode = null, warm = null, capturing = false, listening = false, sustainMs = 0, idleTimer = 0;
  let frames = [], preroll = [], speechMs = 0, silentMs = 0, clipMs = 0, noise = 0.004, clipsOut = 0;

  function cut(final) {
    if (speechMs >= MIN_SPEECH_MS) {
      const blob = resampleToWav(frames, ctx.sampleRate);
      clipsOut++;
      log('mic', `clip ${clipsOut}: ${Math.round(clipMs)}ms audio, ${Math.round(speechMs)}ms speech, ${blob.size}B${final ? ' (final)' : ''}`);
      (capturing ? onClip : onCommandClip)(blob, {ms: clipMs});
    }
    frames = []; speechMs = 0; silentMs = 0; clipMs = 0;
  }

  function onFrame(frame) {
    if (!ctx || (!capturing && !listening)) return;   // frames after stop(), or while idle in the grace period
    const ms = frame.length / ctx.sampleRate * 1000;
    let sum = 0;
    for (let i = 0; i < frame.length; i++) sum += frame[i] * frame[i];
    const level = Math.sqrt(sum / frame.length);
    const speaking = level > Math.max(MIN_LEVEL, noise * 3);
    if (!speaking) noise = noise * 0.98 + level * 0.02;
    onLevel?.(Math.min(1, level * 8));
    if (!capturing && !frames.length) {   // voice control: only sustained speech (not echo blips) barges in
      sustainMs = speaking ? sustainMs + ms : 0;
      preroll.push(frame); while (preroll.length * ms > PREROLL_MS + sustainMs) preroll.shift();
      if (sustainMs < BARGE_MS) return;
      sustainMs = 0; log('mic', `barge-in: speech sustained ${BARGE_MS}ms`); onBarge?.();
      frames = preroll; preroll = []; clipMs = frames.length * ms; speechMs = BARGE_MS; silentMs = 0;
      return;
    }
    if (!frames.length && !speaking) { preroll.push(frame); while (preroll.length * ms > PREROLL_MS) preroll.shift(); return; }
    if (!frames.length) { frames = preroll; preroll = []; clipMs = frames.length * ms; }
    frames.push(frame); clipMs += ms;
    if (speaking) { speechMs += ms; silentMs = 0; } else silentMs += ms;
    if (silentMs >= PAUSE_MS || clipMs >= MAX_CLIP_S * 1000) cut(false);
  }

  /** Builds the context, stream and worklet once; reused by later dictations until `GRACE_MS` after the last one. */
  async function build() {
    try { navigator.audioSession.type = 'play-and-record'; } catch { /* unsupported */ }
    ctx = new (window.AudioContext || window.webkitAudioContext)();
    const resumed = ctx.resume();
    stream = await navigator.mediaDevices.getUserMedia({audio: {echoCancellation: true, noiseSuppression: true, autoGainControl: true}});
    await resumed;
    const url = URL.createObjectURL(new Blob([WORKLET], {type: 'text/javascript'}));
    await ctx.audioWorklet.addModule(url);
    URL.revokeObjectURL(url);
    sourceNode = ctx.createMediaStreamSource(stream);
    node = new AudioWorkletNode(ctx, 'cap');
    node.connect(ctx.destination);
    node.port.onmessage = event => onFrame(event.data);
    sourceNode.connect(node);
    log('mic', `ready at ${ctx.sampleRate}Hz`);
  }

  function teardown() {
    node?.disconnect(); sourceNode?.disconnect();
    stream?.getTracks().forEach(track => track.stop());
    ctx?.close().catch(() => {});
    try { navigator.audioSession.type = 'playback'; } catch { /* unsupported */ }
    ctx = stream = node = sourceNode = warm = null;
    log('mic', 'released');
  }

  return {
    /** Starts capturing at once; the first call also pays the one-time setup. Safe to call repeatedly or during startup. */
    /** Voice control on/off: keeps the warm microphone open and watches for sustained speech. `{ok}` or `{ok: false, error}`. */
    async listen(on) {
      listening = on; sustainMs = 0; clearTimeout(idleTimer);
      if (!on) { if (!capturing) idleTimer = setTimeout(() => { if (!capturing && !listening) teardown(); }, GRACE_MS); return {ok: true}; }
      try { warm ??= build().catch(error => { teardown(); throw error; }); await warm; if (ctx?.state === 'suspended') await ctx.resume(); return {ok: true}; }
      catch (error) { listening = false; return {ok: false, error: error.name === 'NotAllowedError' ? 'Microphone permission denied.' : `Microphone unavailable (${error.name}).`}; }
    },
    async start() {
      clearTimeout(idleTimer);
      if (capturing) return {ok: true};
      capturing = true;
      frames = []; preroll = []; speechMs = silentMs = clipMs = 0; clipsOut = 0; sustainMs = 0;
      try {
        try { navigator.audioSession.type = 'play-and-record'; } catch { /* unsupported */ }
        warm ??= build().catch(error => { teardown(); throw error; });
        await warm;
        if (ctx?.state === 'suspended') await ctx.resume();
        return capturing ? {ok: true} : {ok: false, cancelled: true};
      } catch (error) {
        capturing = false;
        log('mic', `start failed: ${error.name} ${error.message}`);
        return {ok: false, error: error.name === 'NotAllowedError' ? 'Microphone permission denied. Allow it in Settings > Safari > Microphone.' : `Microphone unavailable (${error.name}).`};
      }
    },
    /** Emits the last partial clip and stops capturing; the microphone is released after the grace period. Also cancels a startup in progress. */
    stop() {
      if (capturing && ctx && frames.length) cut(true);
      capturing = false;
      onLevel?.(0);
      try { navigator.audioSession.type = 'playback'; } catch { /* unsupported */ }
      clearTimeout(idleTimer);
      sustainMs = 0; frames = []; preroll = [];
      idleTimer = setTimeout(() => { if (!capturing && !listening) teardown(); }, GRACE_MS);
    },
    active: () => capturing,
  };
}
