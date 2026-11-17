/** Real browser recording with delayed/out-of-order transcription responses; no Zulip writes. */
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const server=createServer(async(req,res)=>{
  if(req.url==='/'){res.end('<!doctype html><title>Dictation test</title>');return;}
  if(!['/browser-audio.mjs','/resilient-fetch.mjs'].includes(req.url)){res.writeHead(404).end();return;}
  res.setHeader('Content-Type','text/javascript');res.end(await readFile(new URL('.'+req.url,import.meta.url)));
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({headless:true,args:['--use-fake-device-for-media-stream','--use-fake-ui-for-media-stream']});
try{
  const page=await browser.newPage();await page.goto('http://127.0.0.1:'+server.address().port);
  const result=await page.evaluate(async()=>{
    const {createAudio}=await import('/browser-audio.mjs');
    const requests=[],applied=[],failures=[],states=[];let stopped=0;
    const audio=createAudio({status:()=>{},clipsChanged:clips=>states.push(clips),transcribe:(blob,signal,meta)=>new Promise((resolve,reject)=>requests.push({resolve,reject,meta,size:blob.size,signal}))});
    const wait=async predicate=>{for(let i=0;i<100&&!predicate();i++)await new Promise(resolve=>setTimeout(resolve,20));if(!predicate())throw new Error('Timed out');};
    const record=async id=>{await audio.record(async(result,meta)=>{await new Promise(resolve=>setTimeout(resolve,10));applied.push([meta.id,result.text]);},()=>failures.push(id),{id},()=>stopped++);await new Promise(resolve=>setTimeout(resolve,120));audio.finish();await wait(()=>stopped===id);};
    await record(1);audio.stop();await record(2);await wait(()=>requests.length===2);
    requests[1].resolve({text:'second'});await new Promise(resolve=>setTimeout(resolve,30));
    const beforeFirst=applied.slice();requests[0].reject(new Error('Temporary failure'));await wait(()=>failures.length===1);
    audio.retryClip(1);await wait(()=>requests.length===3);requests[2].resolve({text:'first'});await wait(()=>applied.length===2);
    audio.stop();return {stopped,applied,beforeFirst,failures,sizes:requests.map(r=>r.size),aborted:requests.map(r=>r.signal.aborted),ids:requests.map(r=>r.meta.id),last:states.at(-1)};
  });
  assert.equal(result.stopped,2);assert.deepEqual(result.beforeFirst,[]);assert.deepEqual(result.applied,[[result.ids[0],'first'],[result.ids[1],'second']]);assert.notEqual(result.ids[0],result.ids[1]);assert.equal(result.ids[0],result.ids[2]);assert.deepEqual(result.failures,[1]);assert.ok(result.sizes.every(size=>size>0));assert.ok(result.aborted.every(value=>!value));assert.deepEqual(result.last,[]);
  if(process.env.LONG_CAPTURE){
    const measured=await page.evaluate(async()=>{
      const {createAudio}=await import('/browser-audio.mjs');const started=performance.now(),received=[],applied=[];let stopped=0;
      const audio=createAudio({status:()=>{},transcribe:async(blob,signal,metadata)=>{
        const arrivedMs=performance.now()-started;received.push({arrivedMs,size:blob.size,id:metadata.id});
        await new Promise(resolve=>setTimeout(resolve,received.length%2?650:200));return {text:metadata.id};
      }});
      await audio.record((result,metadata)=>applied.push(metadata.id),()=>{}, {agent:'test',prefix:''},()=>stopped++);
      await new Promise(resolve=>setTimeout(resolve,60000));const finishMs=performance.now();audio.finish();
      while(!stopped||applied.length<received.length)await new Promise(resolve=>setTimeout(resolve,10));
      return {captureMs:finishMs-started,finishToTranscriptMs:performance.now()-finishMs,received,applied};
    });
    assert.ok(measured.received.length>=7);assert.deepEqual(measured.applied,measured.received.map(item=>item.id));assert.ok(measured.received.slice(0,-1).every(item=>item.arrivedMs<measured.captureMs));assert.ok(measured.finishToTranscriptMs<1500);console.log(JSON.stringify(measured));
  }
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
