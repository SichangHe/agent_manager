/** Builds the static client into OUTPUT_DIRECTORY. Never uploads or publishes. Every module URL carries a content hash so a phone never mixes versions. */
import {createHash} from 'node:crypto';
import {mkdir, readFile, writeFile} from 'node:fs/promises';
import {resolve} from 'node:path';

const output = process.argv[2];
if (!output) throw Error('Usage: node build.mjs OUTPUT_DIRECTORY');
const files = ['index.html', 'latency.mjs', 'kws.mjs', 'test.html', 'selftest.mjs', 'app.css', 'app.mjs', 'log.mjs', 'model.mjs', 'player.mjs', 'recorder.mjs', 'view.mjs', 'voice.mjs', 'zulip.mjs', 'text.mjs'];
const here = name => new URL('./' + name, import.meta.url);
const nm = name => new URL('./node_modules/' + name, import.meta.url);
const sources = Object.fromEntries(await Promise.all(files.map(async name => [name, await readFile(here(name), 'utf8')])));
const version = createHash('sha1').update(files.map(n => sources[n]).join('\0')).digest('hex').slice(0, 10);
// Keyword spotter assets (sherpa-onnx WebAssembly, pinned in package.json; `npm install` first; model from research-kws/) in a content-hashed directory.
// The npm wasm build targets Node: the two patches below let it run in a browser with the in-memory file system.
const kwsModel = new URL('../research-kws/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01/', import.meta.url);
const kwsFiles = [['kws-shim.js', 'shim.js', here('kws-shim.js')], ['sherpa-onnx-wasm-nodejs.wasm', 'sherpa-onnx-wasm.wasm', nm('sherpa-onnx/sherpa-onnx-wasm-nodejs.wasm')], ['sherpa-onnx-kws.js', 'sherpa-onnx-kws.js', nm('sherpa-onnx/sherpa-onnx-kws.js')],
  ...['encoder', 'decoder', 'joiner'].map(n => [n, n + '.int8.onnx', new URL(`${n}-epoch-12-avg-2-chunk-16-left-64.int8.onnx`, kwsModel)]), ['tokens', 'tokens.txt', new URL('tokens.txt', kwsModel)]];
let wasmJs = await readFile(nm('sherpa-onnx/sherpa-onnx-wasm-nodejs.js'), 'utf8');
const rawFsGuard = 'if(!ENVIRONMENT_IS_NODE){throw new Error("NODERAWFS is currently only supported on Node.js environment.")}', rawFsLoop = 'for(var _key in NODERAWFS){FS[_key]=_wrapNodeError(NODERAWFS[_key])}';
if (!wasmJs.includes(rawFsGuard) || !wasmJs.includes(rawFsLoop)) throw Error('sherpa-onnx wasm loader changed; the browser patch no longer applies');
wasmJs = wasmJs.replace(rawFsGuard, '').replace(rawFsLoop, 'if(ENVIRONMENT_IS_NODE)' + rawFsLoop);
const syncInit = 'wasmExports=createWasm();run();';
if (!wasmJs.includes(syncInit)) throw Error('sherpa-onnx wasm loader changed; the async instantiate patch no longer applies');
wasmJs = wasmJs.replace(syncInit, 'wasmExports=await createWasm();run();');   // the factory is an async function: awaiting lets the browser compile the module asynchronously (Chrome refuses a synchronous compile over 8 MB)
const kwsBlobs = await Promise.all(kwsFiles.map(async ([, , url]) => readFile(url)));
const kwsHash = createHash('sha1').update(wasmJs); for (const b of kwsBlobs) kwsHash.update(b);
const kwsDir = 'kws-' + kwsHash.digest('hex').slice(0, 10);
await mkdir(resolve(output, kwsDir), {recursive: true});
await writeFile(resolve(output, kwsDir, 'sherpa-onnx-wasm-web.js'), wasmJs);
for (const [i, [, to]] of kwsFiles.entries()) await writeFile(resolve(output, kwsDir, to), kwsBlobs[i]);
await mkdir(output, {recursive: true});
for (const name of files) {
  let text = sources[name];
  if (name.endsWith('.mjs')) text = text.replace(/(from '\.\/[\w-]+\.mjs)'/g, `$1?v=${version}'`).replace(/(import\('\.\/[\w-]+\.mjs)'\)/g, `$1?v=${version}')`);
  if (name === 'test.html') text = text.replace('src="selftest.mjs"', `src="selftest.mjs?v=${version}"`);
  if (name === 'index.html') text = text.replace('href="app.css"', `href="app.css?v=${version}"`).replace('src="app.mjs"', `src="app.mjs?v=${version}"`);
  if (name === 'kws.mjs') text = text.replace("const KWS_BASE = './kws/';", `const KWS_BASE = './${kwsDir}/';`);
  if (name === 'app.mjs') text = text.replace("const BUILD = 'v2.0'", `const BUILD = 'v2.0-${version}'`);
  await writeFile(resolve(output, name), text);
}
console.log(`Built ${files.length} files, version ${version}, into ${output}; nothing published.`);
