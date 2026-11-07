/** Builds the static client into OUTPUT_DIRECTORY. Never uploads or publishes. Every module URL carries a content hash so a phone never mixes versions. */
import {createHash} from 'node:crypto';
import {mkdir, readFile, writeFile} from 'node:fs/promises';
import {resolve} from 'node:path';

const output = process.argv[2];
if (!output) throw Error('Usage: node build.mjs OUTPUT_DIRECTORY');
const files = ['index.html', 'test.html', 'selftest.mjs', 'app.css', 'app.mjs', 'log.mjs', 'model.mjs', 'player.mjs', 'recorder.mjs', 'view.mjs', 'voice.mjs', 'zulip.mjs', 'text.mjs'];
const here = name => new URL('./' + name, import.meta.url);
const sources = Object.fromEntries(await Promise.all(files.map(async name => [name, await readFile(here(name), 'utf8')])));
const version = createHash('sha1').update(files.map(n => sources[n]).join('\0')).digest('hex').slice(0, 10);
await mkdir(output, {recursive: true});
for (const name of files) {
  let text = sources[name];
  if (name.endsWith('.mjs')) text = text.replace(/(from '\.\/[\w-]+\.mjs)'/g, `$1?v=${version}'`).replace(/(import\('\.\/[\w-]+\.mjs)'\)/g, `$1?v=${version}')`);
  if (name === 'test.html') text = text.replace('src="selftest.mjs"', `src="selftest.mjs?v=${version}"`);
  if (name === 'index.html') text = text.replace('href="app.css"', `href="app.css?v=${version}"`).replace('src="app.mjs"', `src="app.mjs?v=${version}"`);
  if (name === 'app.mjs') text = text.replace("const BUILD = 'v2.0'", `const BUILD = 'v2.0-${version}'`);
  await writeFile(resolve(output, name), text);
}
console.log(`Built ${files.length} files, version ${version}, into ${output}; nothing published.`);
