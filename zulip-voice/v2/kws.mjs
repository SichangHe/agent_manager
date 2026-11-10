/**
 * Keyword spotting in the browser (sherpa-onnx streaming zipformer, WebAssembly) for the start word "attention" and the end word "kangaroo".
 * Loaded lazily when voice control turns on; assets live in the hashed `kws-<hash>/` directory written by `build.mjs`.
 * The detector hears the microphone directly, so a start word pauses playback with no upload and no server round trip.
 */
const KWS_BASE = './kws/';   // build.mjs rewrites this to the hashed asset directory
const KEYWORDS = '▁AT T ENT ION @ATTENTION\n▁K A NG AR O O @KANGAROO';
const THRESHOLD = 0.25;   // tuned in research-kws: 41/42 clips, no false hit in 35 minutes of agent speech

const loadScript = src => new Promise((resolve, reject) => { const el = document.createElement('script'); el.src = src; el.onload = resolve; el.onerror = () => reject(Error('failed to load ' + src)); document.head.append(el); });

export function createKeywordSpotter({log}) {
  let kws = null, stream = null, loading = null, status = 'off', detail = '';   // status: off | loading | ready | failed
  async function load() {
    const t0 = performance.now(), base = new URL(KWS_BASE, import.meta.url).href;
    const stage = name => log('kws', `${name} +${Math.round(performance.now() - t0)}ms`);
    await loadScript(base + 'shim.js');
    await loadScript(base + 'sherpa-onnx-wasm-web.js');
    await loadScript(base + 'sherpa-onnx-kws.js');
    stage('scripts loaded');
    const bytes = async name => new Uint8Array(await (await fetch(base + name)).arrayBuffer());
    const wasmBinary = await bytes('sherpa-onnx-wasm.wasm');
    stage('wasm fetched');
    const module = await window.Module({wasmBinary, locateFile: p => base + p, instantiateWasm: (imports, done) => { WebAssembly.instantiate(wasmBinary, imports).then(r => done(r.instance, r.module), error => log('kws', `wasm instantiate failed: ${error.message}`)); return {}; }});   // asynchronous compile: Chrome refuses a synchronous one for a module over 8 MB on the main thread (the loader is patched in build.mjs to await it)
    stage('wasm compiled');
    for (const name of ['encoder', 'decoder', 'joiner']) module.FS.writeFile(`${name}.onnx`, await bytes(`${name}.int8.onnx`));
    module.FS.writeFile('tokens.txt', await bytes('tokens.txt'));
    stage('model files written');
    kws = new (Function('return Kws')())({   // a top-level class of a classic script is a global binding, not a window property
      featConfig: {samplingRate: 16000, featureDim: 80},
      modelConfig: {transducer: {encoder: './encoder.onnx', decoder: './decoder.onnx', joiner: './joiner.onnx'}, tokens: './tokens.txt', provider: 'cpu', modelType: '', numThreads: 1, debug: 0, modelingUnit: 'bpe', bpeVocab: ''},
      maxActivePaths: 4, numTrailingBlanks: 1, keywordsScore: 1.0, keywordsThreshold: THRESHOLD, keywords: KEYWORDS,
    }, module);
    stream = kws.createStream();
    status = 'ready'; detail = `${Math.round(performance.now() - t0)}ms`;
    log('kws', `ready in ${detail}`);
  }
  return {
    ready: () => !!kws,
    /** `{status, detail}` for the debug sheet. */
    state: () => ({status, detail}),
    /** Starts loading; resolves `{ok: true}` or `{ok: false, error}` (never throws). */
    async load() {
      if (!loading) { status = 'loading'; log('kws', 'loading the on-device model'); }
      loading ??= Promise.race([load(), new Promise((_, reject) => setTimeout(() => reject(Error('timed out after 40 s')), 40000))]).then(() => ({ok: true}), error => { loading = null; status = 'failed'; detail = error.message; log('kws', `unavailable: ${error.message}`); return {ok: false, error: error.message}; });
      return loading;
    },
    /** Feeds 16 kHz mono float samples; returns the keywords heard (`ATTENTION`, `KANGAROO`). */
    feed(samples) {
      if (!kws) return [];
      stream.acceptWaveform(16000, samples);
      const hits = [];
      while (kws.isReady(stream)) {
        kws.decode(stream);
        const r = kws.getResult(stream);
        if (r.keyword) { hits.push(r.keyword); kws.reset(stream); }
      }
      return hits;
    },
    /** Frees the model (voice control off). */
    unload() { try { stream?.free(); kws?.free(); } catch { /* already freed */ } kws = stream = loading = null; status = 'off'; detail = ''; },
  };
}
