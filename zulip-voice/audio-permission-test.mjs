/** Exercise denied media permission without requiring a physical iOS device. */
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createServer} from 'node:http';
const {webkit}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const server=createServer(async(req,res)=>{if(req.url==='/'){res.end('<title>Audio permission test</title>');return;}res.setHeader('Content-Type','text/javascript');res.end(await readFile(new URL('.'+req.url,import.meta.url)));});await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await webkit.launch();
try{
 const page=await browser.newPage();await page.goto('http://127.0.0.1:'+server.address().port);
 const result=await page.evaluate(async()=>{
  let count=0,denyAll=false;const calls=[],acks=[],status=[];
  const NativeAudio=Audio;window.Audio=function(){const element=new NativeAudio();element.testId=++count;element.play=async()=>{calls.push(element.testId);if(denyAll||element.testId!==1)throw new DOMException('Permission denied','NotAllowedError');};return element;};
  window.fetch=async(path,options)=>{if(path==='/api/audio-ack')acks.push(JSON.parse(options.body).id);return Response.json({});};
  const {createAudio}=await import('/browser-audio.mjs');const audio=createAudio({status:text=>status.push(text),transcribe:()=>{}});
  audio.cachePrepared({text:'First sentence.',audioURL:'data:audio/wav;base64,',relayId:'first'});await audio.speak('First sentence.');
  const fallback=[...calls];audio.stop();await new Promise(resolve=>setTimeout(resolve));denyAll=true;
  audio.cachePrepared({text:'Second sentence.',audioURL:'data:audio/wav;base64,',relayId:'second'});await audio.speak('Second sentence.');await new Promise(resolve=>setTimeout(resolve));
  const retained=!acks.includes('second');audio.stop();return {fallback,retained,status};
 });
 assert.deepEqual(result.fallback,[2,1]);assert.equal(result.retained,true);assert.ok(result.status.includes('Playback needs permission. Tap Play to continue.'));
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
