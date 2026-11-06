/** Real browser recording and server STT with prompt-driven synthetic audio, never Zulip sends. Requires playwright-core and the local speech worker. */
import assert from 'node:assert/strict';
import http from 'node:http';
import {readFile} from 'node:fs/promises';
import {createRequire} from 'node:module';
const {chromium}=process.env.PLAYWRIGHT_MODULE?await import(process.env.PLAYWRIGHT_MODULE):createRequire(import.meta.url)('playwright-core');
const worker=process.env.VOICE_AUDIO_URL||'http://127.0.0.1:8770';
const fixtures=new Map();
for(const [id,text] of [['pause','Pause.'],['reply','Please review my presentation tomorrow.'],['resume','Resume autoplay.']]){
  const response=await fetch(worker+'/tts',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text}),signal:AbortSignal.timeout(90000)});
  assert.equal(response.status,200);fixtures.set('/audio/'+id,Buffer.from(await response.arrayBuffer()));
}
let attempts=0,failures=0;
const server=http.createServer(async(req,res)=>{
  try{
    if(['/handsfree.mjs','/resilient-fetch.mjs'].includes(req.url)){
      res.setHeader('Content-Type','text/javascript');res.end(await readFile(new URL('.'+req.url,import.meta.url)));return;
    }
    if(fixtures.has(req.url)){res.setHeader('Content-Type','audio/wav');res.end(fixtures.get(req.url));return;}
    if(req.url==='/api/stt'){
      const parts=[];for await(const part of req)parts.push(part);attempts++;
      if(failures>0){failures--;res.writeHead(503,{'Content-Type':'application/json'});res.end('{"error":"Temporary test outage"}');return;}
      const response=await fetch(worker+'/stt',{method:'POST',body:Buffer.concat(parts),signal:AbortSignal.timeout(90000)});
      res.writeHead(response.status,{'Content-Type':'application/json'});res.end(await response.text());return;
    }
    res.setHeader('Content-Type','text/html');res.end('<button>Start</button>');
  }catch(error){res.writeHead(500);res.end(String(error));}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({headless:true,...(process.env.CHROMIUM_EXECUTABLE?{executablePath:process.env.CHROMIUM_EXECUTABLE}:{}),args:['--autoplay-policy=no-user-gesture-required','--no-sandbox']});
try{
  for(const {sendSuccess,outage} of [{sendSuccess:true,outage:false},{sendSuccess:false,outage:false},{sendSuccess:false,outage:true}]){
    failures=outage?4:1;attempts=0;
    const page=await browser.newPage();await page.goto('http://127.0.0.1:'+server.address().port);
    await page.evaluate(async sendSuccess=>{
      const {createHandsfree}=await import('/handsfree.mjs');
      const input=new AudioContext(),destination=input.createMediaStreamDestination(),audio={};
      for(const id of ['pause','reply','resume'])audio[id]=await input.decodeAudioData(await(await fetch('/audio/'+id)).arrayBuffer());
      navigator.mediaDevices.getUserMedia=async()=>destination.stream;
      function play(id){return new Promise(resolve=>{const source=input.createBufferSource();source.buffer=audio[id];source.connect(destination);source.onended=resolve;source.start();});}
      const wait=ms=>new Promise(resolve=>setTimeout(resolve,ms));
      window.result={calls:[],saved:[],removed:[],errors:[],states:[],prompted:false};
      window.h=createHandsfree({
        getContext:()=>({agent:'manager'}),
        onRecording:async(blob,metadata)=>{const id=crypto.randomUUID();result.saved.push({id,metadata,size:blob.size});return id;},
        onTranscribed:async id=>{result.removed.push(id);},
        onState:state=>result.states.push(state),onError:error=>result.errors.push(error),
        onPause:async()=>{result.prompted=true;setTimeout(async()=>{await play('reply');await wait(2000);await play('resume');},1000);},
        onReply:async text=>{result.calls.push(text);return sendSuccess;},
      });
      document.querySelector('button').onclick=async()=>{await input.resume();await h.start();await wait(300);await play('pause');};
    },sendSuccess);
    await page.click('button');
    await page.waitForFunction(()=>result.calls.length>0||result.errors.length>0,null,{timeout:90000});
    await page.waitForFunction(()=>h.state==='off'||(result.calls.length>0&&h.state==='listening'),null,{timeout:5000});
    const result=await page.evaluate(()=>({...result,state:h.state,draft:h.draft}));
    if(outage){
      assert.equal(result.state,'off');assert.equal(result.prompted,false);assert.deepEqual(result.calls,[]);
      assert.ok(result.saved.length>0);assert.equal(result.removed.length,0);assert.equal(attempts,4);
      console.log(JSON.stringify({outage,attempts,state:result.state,retained:result.saved.length}));
      await page.close();continue;
    }
    assert.equal(result.prompted,true);assert.deepEqual(result.calls,['Please review my presentation tomorrow.']);
    assert.equal(result.saved.length,result.removed.length,JSON.stringify(result));
    assert.equal(new Set(result.removed).size,result.removed.length);
    assert.ok(result.saved.every(item=>item.metadata.context.agent==='manager'));
    assert.equal(result.state,sendSuccess?'listening':'off');assert.equal(result.draft,sendSuccess?'':result.calls[0]);
    assert.equal(result.errors.length,sendSuccess?0:1);assert.ok(attempts>=4);
    console.log(JSON.stringify({sendSuccess,attempts,state:result.state,reply:result.calls[0],saved:result.saved.length,consumed:result.removed.length}));
    await page.evaluate(()=>h.stop());await page.close();
  }
}finally{await browser.close();server.closeAllConnections();server.close();}
