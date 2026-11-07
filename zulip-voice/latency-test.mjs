/** Measure speech through the UI proxy using a supplied real recording; never send Zulip messages. */
import {readFile} from 'node:fs/promises';
import {performance} from 'node:perf_hooks';
const origin=process.env.VOICE_UI_URL||'http://127.0.0.1:8769';
if(!process.argv[2])throw new Error('Usage: node latency-test.mjs RECORDING.wav');
const audio=await readFile(process.argv[2]);
const text='Please review the next slide before we continue.';
async function measure(kind){
  const started=performance.now();
  const response=await fetch(origin+'/api/'+kind,{method:'POST',headers:{Origin:origin,'Content-Type':kind==='tts'?'application/json':'audio/wav'},body:kind==='tts'?JSON.stringify({text}):audio,signal:AbortSignal.timeout(12000)});
  const body=await response.arrayBuffer();
  if(!response.ok)throw new Error(kind+' status '+response.status+': '+Buffer.from(body));
  return {kind,ms:Math.round(performance.now()-started),bytes:body.byteLength};
}
const serial=[];
for(let i=0;i<5;i++)for(const kind of ['stt','tts'])serial.push(await measure(kind));
const concurrent=await Promise.all(['stt','tts'].map(measure));
console.log(JSON.stringify({serial,concurrent},null,2));
