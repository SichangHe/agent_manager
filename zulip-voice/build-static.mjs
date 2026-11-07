/** Build an unpublished static directory; does not serve, upload or publish it. */
import {mkdir,readFile,writeFile,copyFile} from 'node:fs/promises';
import {resolve} from 'node:path';
const output=process.argv[2];if(!output)throw new Error('Usage: node build-static.mjs OUTPUT_DIRECTORY');
await mkdir(output,{recursive:true});
const html=(await readFile(new URL('./index.html',import.meta.url),'utf8')).replace('<script type="module">','<script type="module">\nimport "./static-bootstrap.mjs";').replaceAll('from \'/','from \'./').replaceAll('href="/reader-ui.css"','href="./reader-ui.css"').replaceAll('Zulip voice','Voice').replaceAll('Live Zulip · sends as your account','Personal client · sends as your account').replaceAll('Send to Zulip','Send message').replaceAll("document.title=mock?'Voice · mock prototype':'Zulip voice'","document.title='Voice'");
await writeFile(resolve(output,'index.html'),html.replaceAll('Voice · mock prototype','Voice'));
for(const file of ['reader-ui.mjs','reader-ui.css','browser-audio.mjs','handsfree.mjs','resilient-fetch.mjs','static-bootstrap.mjs','browser-zulip.mjs','browser-relay.mjs','request-budget.mjs','debug-log.mjs'])await copyFile(new URL('./'+file,import.meta.url),resolve(output,file));
console.log('Static files built locally; nothing published.');
