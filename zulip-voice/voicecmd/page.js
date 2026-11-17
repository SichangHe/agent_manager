/** Test page logic: feature report, energy VAD on a worklet (same rule as v2 `recorder.mjs`), TF.js speech-commands (18w) keyword spotting. Results in `window.R`. */
const $ = id => document.getElementById(id);
const R = window.R = {detections: [], vad: [], features: {}, infer: null, t0: 0};
const say = (...a) => { $('log').textContent += a.join(' ') + '\n'; };
const params = new URLSearchParams(location.search);

const WORKLET = `registerProcessor('lv', class extends AudioWorkletProcessor {
  process(inputs) { const c = inputs[0][0]; if (c) { let s = 0; for (const v of c) s += v * v; this.port.postMessage(Math.sqrt(s / c.length)); } return true; }
});`;

function features() {
  const gl2 = !!document.createElement('canvas').getContext('webgl2');
  const f = {ua: navigator.userAgent, webgl2: gl2, audioWorklet: !!window.AudioWorklet, webgpu: !!navigator.gpu, audioSession: 'audioSession' in navigator,
    gum: !!navigator.mediaDevices?.getUserMedia, supported: navigator.mediaDevices?.getSupportedConstraints?.() ?? {}, sab: typeof SharedArrayBuffer !== 'undefined', crossOriginIsolated: self.crossOriginIsolated};
  try { f.wasmSimd = WebAssembly.validate(new Uint8Array([0,97,115,109,1,0,0,0,1,5,1,96,0,1,123,3,2,1,0,10,10,1,8,0,65,0,253,15,253,98,11])); } catch { f.wasmSimd = false; }
  return f;
}

/** Energy VAD with adaptive noise floor, identical rule to `recorder.mjs`. */
function startVad(stream, ctx) {
  let noise = 0.004, speaking = false, last = 0;
  return ctx.audioWorklet.addModule(URL.createObjectURL(new Blob([WORKLET], {type: 'text/javascript'}))).then(() => {
    const node = new AudioWorkletNode(ctx, 'lv');
    node.port.onmessage = ({data: level}) => {
      const on = level > Math.max(0.012, noise * 3);
      if (!on) noise = noise * 0.98 + level * 0.02;
      const t = performance.now() - R.t0;
      if (on !== speaking) { speaking = on; R.vad.push({t: Math.round(t), on}); }
      $('bar').style.width = Math.min(100, level * 800) + '%';
      if (t - last > 200) { last = t; $('state').textContent = `VAD ${on ? 'SPEECH' : 'silence'}  level ${level.toFixed(4)}  floor ${noise.toFixed(4)}  threshold ${Math.max(0.012, noise * 3).toFixed(4)}`; }
    };
    ctx.createMediaStreamSource(stream).connect(node);
  });
}

async function timeInference(recognizer) {
  const input = new Float32Array(recognizer.modelInputShape().slice(1).reduce((a, b) => a * b, 1));
  for (let i = 0; i < 3; i++) await recognizer.recognize(input);
  const t = performance.now(), n = 30;
  for (let i = 0; i < n; i++) await recognizer.recognize(input);
  return {backend: tf.getBackend(), msPerInference: (performance.now() - t) / n};
}

let stopFns = [];
async function start() {
  R.t0 = performance.now(); R.detections = []; R.vad = [];
  const aec = $('aec').checked, constraints = {echoCancellation: aec, noiseSuppression: aec, autoGainControl: aec};
  try { navigator.audioSession.type = 'play-and-record'; } catch {}
  const ctx = new AudioContext(); ctx.resume();
  say('gum1');
  const stream = await navigator.mediaDevices.getUserMedia({audio: constraints});
  R.features.trackSettings = stream.getAudioTracks()[0].getSettings?.();
  R.features.sampleRate = ctx.sampleRate; say('gum1 ok');
  await startVad(stream, ctx);
  const recognizer = speechCommands.create('BROWSER_FFT', undefined, new URL('models/18w/model.json', location).href, new URL('models/18w/metadata.json', location).href);
  await recognizer.ensureModelLoaded();
  R.infer = await timeInference(recognizer);
  const labels = recognizer.wordLabels(), thr = +(params.get('thr') ?? 0.75);
  await recognizer.listen(r => {
    const best = r.scores.reduce((b, s, i) => s > r.scores[b] ? i : b, 0), w = labels[best];
    if (!w.startsWith('_') && r.scores[best] > thr) { const d = {t: Math.round(performance.now() - R.t0), word: w, p: +r.scores[best].toFixed(2)}; R.detections.push(d); say(JSON.stringify(d)); }
  }, {probabilityThreshold: 0.5, overlapFactor: 0.5, invokeCallbackOnNoiseAndUnknown: true, audioTrackConstraints: constraints});
  stopFns = [() => recognizer.stopListening(), () => stream.getTracks().forEach(t => t.stop()), () => ctx.close()];
  R.ready = true;
  say('listening; backend', R.infer.backend, R.infer.msPerInference.toFixed(1) + ' ms/inference');
}

$('start').onclick = () => start().catch(e => { R.error = String(e) + (e.stack ?? ''); say('ERROR', e); });
$('stop').onclick = () => { stopFns.forEach(f => f()); stopFns = []; };
$('play').onclick = () => $('tts').play();
R.features = features();
if (params.get('backend')) tf.setBackend(params.get('backend')).then(() => say('backend', tf.getBackend()));
say(JSON.stringify(R.features));
