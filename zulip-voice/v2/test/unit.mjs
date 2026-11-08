/** Pure-logic checks (text chunking, message parsing, relay request validation, WAV duration). Prints only failures. */
import assert from 'node:assert/strict';
import {parseAgentMessage, messageChunks, speechPieces, speechText, splitSentences} from '../text.mjs';
import {parseRequest, wavDurationMs} from '../../service/relay.mjs';
import {createModel, itemsOf} from '../model.mjs';
import {speechWav} from './lib.mjs';

const real = '[terminal_viz] Zulip voice: Claude agent now steering\n\nGot these three.\n"Why does autoplay turn off by itself?" The code switches it off.\n\n- first point, e.g. this one\n- second `code` point with **bold**\n\n```js\nconst x = 1;\n```\nSee [docs](https://example.com/a/b) now.\n[switch: other_task.md]';
const p = parseAgentMessage(real);
assert.equal(p.title, '[terminal_viz] Zulip voice: Claude agent now steering');
assert.equal(p.switchTo, 'other_task');
assert.ok(!p.body.includes('switch:') && !p.body.includes('Zulip voice: Claude'));
assert.deepEqual(parseAgentMessage('no title here'), {title: '', body: 'no title here', switchTo: null, attachments: [], footers: []});
assert.equal(parseAgentMessage('[switch: a]').body, '');

const att = parseAgentMessage('[t] x\n\nLook [shot.png](/user_uploads/1/ab/shot.png) here.');
assert.deepEqual(att.attachments, [{name: 'shot.png', uri: '/user_uploads/1/ab/shot.png'}]);
assert.ok(!att.body.includes('user_uploads') && !speechPieces('[t] x\n\nLook [shot.png](/user_uploads/1/ab/shot.png) here.').join(' ').includes('shot'));
const real2 = '[terminal_viz] terminal_viz: fixes\n\nDone with the relay change.\n\npending item created:\n- "Do you get the debug log I sent"\n- "second item"\npending item deleted:\n- "old one"\n\nRemoved open item\n- "gone"\n\nStill speaking here.';
const f = parseAgentMessage(real2);
assert.equal(f.footers.filter(l => /^\s*-/.test(l)).length, 4, 'four bullets folded: ' + JSON.stringify(f.footers));
assert.ok(!/pending item|Removed open|debug log|gone/.test(f.body) && /Done with the relay/.test(f.body) && /Still speaking here/.test(f.body), 'body keeps the real text only: ' + f.body);
assert.ok(!speechPieces(real2).join(' ').match(/pending|debug log|gone|second item/), 'footers are never spoken');
const {chunks} = messageChunks(p.body);
const spoken = chunks.filter(c => c.speech);
assert.deepEqual(spoken.map(c => c.idx), spoken.map((_, i) => i), 'chunk idx must be 0..n-1 in order');
assert.ok(!spoken.some(c => /const x/.test(c.speech)), 'code blocks are not spoken');
assert.ok(!spoken.some(c => /\*\*|`|https?:/.test(c.speech)), 'markdown is cleaned for speech: ' + JSON.stringify(spoken.map(c => c.speech)));
assert.deepEqual(speechPieces(real), spoken.map(c => c.speech).slice(0, 20), 'relay pieces equal client chunks');
assert.equal(speechText('call terminal_viz now'), 'call terminal viz now');
assert.equal(splitSentences('One. Two! Three?').length, 3);

const long = Array.from({length: 400}, (_, i) => `Sentence number ${i} is here.`).join(' ');
const longChunks = messageChunks(long).chunks;
assert.ok(longChunks.every(c => c.text.length <= 280), 'chunks stay short');
assert.equal(speechPieces('[t] x\n\n' + long).length, 20, 'relay renders at most 20 pieces');

const msg = (id, content) => ({id, sender_id: 2, subject: 't', content, timestamp: id, flags: []});
const model = createModel({botId: 2, humanId: 1});
assert.ok(model.add(msg(1, '[t] A\n\nOne. Two.')));
assert.ok(!model.add(msg(1, 'dup')));
model.add(msg(2, '[t] B\n\nThree.'));
assert.equal(model.unreadAgentMessages('t').length, 2);
model.markRead([1]);
model.markLoaded('t');
assert.equal(model.unreadAgentMessages('t').length, 1);
assert.equal(model.topics()[0].unread, 1);
assert.equal(itemsOf(model.messages('t')[1]).at(-1).last, true);

const req = (over, subject = 'tts') => ({type: 'stream', stream_id: 5, sender_id: 1, subject, content: JSON.stringify({voice: 1, id: 'abcdefgh1', op: subject, text: 'hi', uri: '/user_uploads/x', ...over})});
assert.ok(parseRequest(req({}), 1, 5));
assert.equal(parseRequest(req({}), 2, 5), null, 'other sender');
assert.equal(parseRequest(req({}), 1, 6), null, 'other channel');
assert.equal(parseRequest({...req({}), subject: 'stt'}, 1, 5), null, 'topic must equal op');
assert.equal(parseRequest(req({text: ' '}), 1, 5), null, 'empty text');
assert.equal(parseRequest(req({id: 'short'}), 1, 5), null, 'bad id');
assert.ok(parseRequest(req({ids: ['a', 'b']}, 'ack'), 1, 5));
assert.equal(parseRequest(req({ids: [1]}, 'ack'), 1, 5), null);

const wav = await speechWav('Duration check for the synthesized speech.');
const ms = wavDurationMs(wav);
assert.ok(ms > 1000 && ms < 8000, 'wav duration ' + ms);
console.log('unit ok');
