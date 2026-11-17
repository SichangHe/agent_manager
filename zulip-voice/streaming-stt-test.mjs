/** Sixty seconds of supplied speech through MediaRecorder, the UI proxy, and real GPU STT; no Zulip writes. */
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createServer} from './server.mjs';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const fixture=await readFile(process.argv[2]);
const server=createServer('<!doctype html><title>Streaming speech measurement</title>','http://127.0.0.1:18771');
await new Promise(resolve=>server.listen(18771,'127.0.0.1',resolve));
const browser=await chromium.launch({headless:true,args:['--autoplay-policy=no-user-gesture-required']});
try{
  const page=await browser.newPage();await page.route('**/fixture.wav',route=>route.fulfill({contentType:'audio/wav',body:fixture}));await page.goto('http://127.0.0.1:18771');
  const result=await page.evaluate(async()=>{
    const context=new AudioContext();await context.resume();const buffer=await context.decodeAudioData(await(await fetch('/fixture.wav')).arrayBuffer());
    const destination=context.createMediaStreamDestination(),source=context.createBufferSource();source.buffer=buffer;source.loop=true;source.connect(destination);
    navigator.mediaDevices.getUserMedia=async()=>{source.start();return destination.stream;};
    const {createAudio}=await import('/browser-audio.mjs');const startedMs=performance.now(),requests=[],applied=[],errors=[];let stopped=false;
    const audio=createAudio({status:()=>{},transcribe:async(blob,signal,metadata)=>{
      const request={id:metadata.id,uploadMs:performance.now()-startedMs,bytes:blob.size};requests.push(request);
      const response=await fetch('/api/stt',{method:'POST',body:blob,signal:AbortSignal.timeout(10000)});const result=await response.json();
      request.completeMs=performance.now()-startedMs;request.text=result.text;if(!response.ok)throw new Error(result.error);return result;
    }});
    await audio.record((result,metadata)=>applied.push({id:metadata.id,text:result.text,ms:performance.now()-startedMs}),()=>errors.push('Transcription failed'),{agent:'test',prefix:''},()=>{stopped=true;});
    await new Promise(resolve=>setTimeout(resolve,60000));const finishMs=performance.now();audio.finish();
    while((!stopped||applied.length<requests.length)&&!errors.length&&performance.now()-finishMs<12000)await new Promise(resolve=>setTimeout(resolve,10));
    const finishToTranscriptMs=performance.now()-finishMs;source.stop();await context.close();audio.stop();
    return {fixtureSeconds:buffer.duration,captureMs:finishMs-startedMs,finishToTranscriptMs,requests,applied,errors};
  });
  console.log(JSON.stringify(result));assert.deepEqual(result.errors,[]);assert.equal(result.applied.length,result.requests.length);assert.deepEqual(result.applied.map(item=>item.id),result.requests.map(item=>item.id));assert.ok(result.applied.some(item=>item.ms<10000));assert.match(result.applied.map(item=>item.text).join(' '),/country/i);
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
