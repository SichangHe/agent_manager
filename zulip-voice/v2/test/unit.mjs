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

// voice-command grammar
{
  const G = await import('../grammar.mjs');
  assert.deepEqual(G.navCommand('next three'), {dir: 1, whole: false, count: 3});
  assert.deepEqual(G.navCommand('skip backward two'), {dir: -1, whole: false, count: 2});
  assert.deepEqual(G.navCommand('back back message'), {dir: -1, whole: true, count: 2});
  assert.deepEqual(G.navCommand('skip previous message'), {dir: -1, whole: true, count: 1});   // was split into skip (next) + previous message
  assert.deepEqual(G.navCommand('skip back'), {dir: -1, whole: false, count: 1});
  assert.deepEqual(G.navCommand('skip next message'), {dir: 1, whole: true, count: 1});
  assert.deepEqual(G.navCommand('again'), {dir: 0, whole: false, count: 1});
  assert.equal(G.navCommand('next back'), null);
  assert.equal(G.navCommand('hello'), null);
  const topics = [{name: 'human_mgr', maxId: 2}, {name: 'terminal_viz', maxId: 3}, {name: 'alpha', maxId: 1}];
  assert.equal(G.rankTopics(topics, 'human manager')[0].t.name, 'human_mgr');
  assert.equal(G.rankTopics(topics, 'terminal viz')[0].t.name, 'terminal_viz');
  assert.ok(G.clearWinner(G.rankTopics(topics, 'alpha')));
  assert.equal(G.choiceAnswer('second', {names: ['a', 'b'], confirm: false}, q => G.rankTopics(topics, q)), 'b');
  assert.equal(G.choiceAnswer('yes', {names: ['a'], confirm: true}, () => []), 'a');
  assert.equal(G.choiceAnswer('number one please', {names: ['a', 'b'], confirm: false}, () => []), 'a');
  assert.equal(G.choiceAnswer('number 3', {names: ['a', 'b', 'c'], confirm: false}, () => []), 'c');
  assert.equal(G.afterStartWord('Attention, send the build', 'attention'), 'send the build');
  assert.equal(G.afterStartWord('send the build', 'attention'), null);
  assert.equal(G.stripResumePhrase('ship it over and out', 'over'), 'ship it');
  assert.equal(G.stripResumePhrase('ship it', 'over'), null);
  assert.ok(G.isCancel('never mind', 'bamboo') && G.isCancel('bamboo', 'bamboo') && !G.isCancel('send it', 'bamboo'));
  assert.ok(G.endsWithCancel('Some reply. Cancel.', 'bamboo'));
  assert.equal(G.spokenName('human_mgr'), 'human manager');
  assert.ok(G.commandPhrases(['alpha', 'alpha_beta']).every(p => p.text !== 'switch to alpha'));
  assert.ok(G.similarText('The build finished with two warnings today', 'The build finished with two warnings today!'));
}
{ // 🧑 "I'm still seeing old threads ... agent switching also gives them as options": closed tasks' topics are hidden unless unread or open
  const m = createModel({botId: 2, humanId: 1});
  m.setTopics([{name: 'live', max_id: 3}, {name: 'done_task', max_id: 2}, {name: 'done_unread', max_id: 1}, {name: 'done_open', max_id: 4}], new Map([['done_unread', 1]]));
  m.setClosed(['done_task', 'done_unread', 'done_open'], n => n === 'done_open');
  assert.deepEqual(m.topics().map(t => t.name).sort(), ['done_open', 'done_unread', 'live']);
  assert.equal(m.allTopics().length, 4);
}
console.log('unit ok');
{
  const {parseBlocks, messageChunks} = await import('../text.mjs');
  const b = parseBlocks('Intro\n\n| a | b |\n|---|:-:|\n| 1 | **x** |\n| 2 |\n\n| not | table |\nafter');
  const t = b.find(x => x.kind === 'table');
  if (!t || t.head.join() !== 'a,b' || t.rows.length !== 2 || t.rows[0][1] !== '**x**') throw new Error('table parse: ' + JSON.stringify(b));
  if (b.filter(x => x.kind === 'table').length !== 1) throw new Error('pipe lines without a separator row must stay text');
  if (!b.some(x => x.kind === 'p' && x.text.includes('| not | table | after'))) throw new Error('fallback text lost: ' + JSON.stringify(b));
  const spoken = messageChunks('| a | b |\n|---|---|\n| 1 | 2 |').chunks.filter(c => c.speech).map(c => c.speech);
  assert.deepEqual(spoken, ['Table start.', 'a.', 'b.', 'Row 1.', '1.', '2.']);
}

{   // 🧑 "The speech drops numbers. E.g. 6.0% gets pronounced as zero percent": symbols next to numbers made the speech engine drop or garble them (measured with the production workers)
  const cases = [
    ['It allows ~2%/hour.', 'It allows about 2 percent per hour.'],
    ['Use ~6.0%.', 'Use about 6%.'],
    ['It costs ~$5.', 'It costs about $5.'],
    ['Roughly ≈6.0% of cases.', 'Roughly about 6% of cases.'],
    ['From 1–2% to 6.0–8.0%.', 'From 1 to 2% to 6 to 8%.'],
    ['Between 50-60% of users.', 'Between 50 to 60% of users.'],
    ['Went 3%→8% today.', 'Went 3% to 8% today.'],
    ['It is <6% now, >4% then.', 'It is less than 6% now, more than 4% then.'],
    ['Gain of +6.0%.', 'Gain of +6%.'],
    ['Rate 6.0%/hour here.', 'Rate 6 percent per hour here.'],
    ['10k users, 2.5M events, ratio 3:1, at 12:30.', '10 thousand users, 2.5 million events, ratio 3 to 1, at 12:30.'],
    ['On 2026-11-17, v2.0 had 6.5% and 3.5 seconds.', 'On 2026-11-17, v2.0 had 6.5% and 3.5 seconds.'],   // dates, versions, real decimals stay
    ['Use <br> tags and a>b, in ~/bin.', 'Use <br> tags and a>b, in ~/bin.'],
    ['An Apple M1 did 779M routes.', 'An Apple M1 did 779 million routes.'],
  ];
  for (const [from, to] of cases) assert.equal(speechText(from), to);
}

{   // The relay's own function named `process` shadows Node's `process`: any `process.env` inside createRelay reads a property of that function and throws, which broke every ping (and so priorities) for days.
  const {readFileSync} = await import('node:fs');
  const src = readFileSync(new URL('../../service/relay.mjs', import.meta.url), 'utf8');
  const from = src.indexOf('export async function createRelay'), inner = src.slice(from, src.indexOf('\n}\n', from));   // the function body only: startup code after it may use process
  assert.equal(/\bprocess\.(env|argv|exit|cwd|platform)\b/.test(inner.replace(/globalThis\.process\./g, '')), false, 'use globalThis.process inside createRelay: a local function named `process` shadows it');
}
