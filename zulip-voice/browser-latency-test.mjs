/** Real MediaRecorder and CUDA speech through an isolated UI server; no Zulip writes. */
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createServer} from './server.mjs';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const fixture=await readFile(process.argv[2]);
const server=createServer(await readFile(new URL('./index.html',import.meta.url)),'http://127.0.0.1:18769');
await new Promise(resolve=>server.listen(18769,'127.0.0.1',resolve));
const browser=await chromium.launch({headless:true,args:['--autoplay-policy=no-user-gesture-required']});
try{
  const page=await browser.newPage();await page.route('**/fixture.wav',route=>route.fulfill({contentType:'audio/wav',body:fixture}));
  await page.goto('http://127.0.0.1:18769');
  await page.evaluate(async()=>{
    const context=new AudioContext();await context.resume();
    const buffer=await context.decodeAudioData(await(await fetch('/fixture.wav')).arrayBuffer());
    const destination=context.createMediaStreamDestination();
    navigator.mediaDevices.getUserMedia=async()=>{const source=context.createBufferSource();source.buffer=buffer;source.connect(destination);source.start();return destination.stream;};
    window.fixtureDuration=buffer.duration;
  });
  await page.locator('#dictate').click();
  await page.waitForTimeout((await page.evaluate(()=>window.fixtureDuration))*1000);
  const started=performance.now();await page.locator('#stop').click();
  await page.waitForFunction(()=>document.getElementById('draft').value.trim().length>0,null,{timeout:12000});
  const dictationMs=Math.round(performance.now()-started);
  assert.match(await page.locator('#draft').inputValue(),/country/i);
  const playback=await page.evaluate(async()=>{
    const {createAudio}=await import('/browser-audio.mjs');const audio=createAudio({status:()=>{}});audio.unlock();
    const begin=performance.now();let firstMs,endedMs;
    return await new Promise((resolve,reject)=>{
      audio.speak('Please review the next slide before we continue.',()=>{
        endedMs=performance.now();audio.speak('The following sentence should already be ready.',()=>{},()=>reject(new Error('Playback failed')),()=>{resolve({firstMs,gapMs:Math.round(performance.now()-endedMs)});audio.stop();});
      },()=>reject(new Error('Playback failed')),()=>{firstMs=Math.round(performance.now()-begin);audio.prefetch('The following sentence should already be ready.');});
    });
  });
  console.log(JSON.stringify({dictationMs,...playback}));
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
