import {readFileSync, writeFileSync} from 'node:fs';
const text = readFileSync('corpus.txt','utf8').replace(/\s+/g,' ');
const sentences = text.match(/[^.!?]+[.!?]+\s*/g) ?? [text];
const chunks = []; let cur = '';
for (const s of sentences) { if ((cur + s).length > 1500) { chunks.push(cur); cur = ''; } cur += s; }
chunks.push(cur);
const pcms = []; let seconds = 0;
for (const c of chunks) {
  if (seconds > 2100) break;
  const r = await fetch('http://127.0.0.1:8770/tts',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text:c.slice(0,1800)})});
  if (!r.ok) continue;
  const w = Buffer.from(await r.arrayBuffer()); const pcm = w.subarray(w.indexOf('data')+8); pcms.push(pcm); seconds += pcm.length/48000;
}
const data = Buffer.concat(pcms), first = Buffer.alloc(44); first.write('RIFF',0); first.writeUInt32LE(36+data.length,4); first.write('WAVEfmt ',8); first.writeUInt32LE(16,16); first.writeUInt16LE(1,20); first.writeUInt16LE(1,22); first.writeUInt32LE(24000,24); first.writeUInt32LE(48000,28); first.writeUInt16LE(2,32); first.writeUInt16LE(16,34); first.write('data',36); first.writeUInt32LE(data.length,40);
writeFileSync('corpus_speech.wav', Buffer.concat([first,data])); console.log('speech seconds', Math.round(seconds));
