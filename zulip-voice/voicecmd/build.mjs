/** `node build.mjs [DIR=/tmp/voicecmd-dist]`: static test page = index.html, page.mjs, v2's zulip.mjs and voice.mjs (copied, not edited), speech.m4a (AAC mono 32 kbps like the app's TTS). */
import {copyFileSync, mkdirSync} from 'node:fs';
import {execFileSync} from 'node:child_process';

const dir = process.argv[2] ?? '/tmp/voicecmd-dist', here = new URL('.', import.meta.url).pathname, v2 = here + '../v2/';
mkdirSync(dir, {recursive: true});
for (const f of ['index.html', 'page.mjs']) copyFileSync(here + f, `${dir}/${f}`);
for (const f of ['zulip.mjs', 'voice.mjs']) copyFileSync(v2 + f, `${dir}/${f}`);
execFileSync('ffmpeg', ['-y', '-v', 'error', '-i', here + 'clips/speech.wav', '-t', '9', '-ac', '1', '-c:a', 'aac', '-b:a', '32k', `${dir}/speech.m4a`]);
console.log('built', dir);
