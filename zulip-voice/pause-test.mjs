/** Pause/reload/resume through the real page, isolated mock messages only. */
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createServer} from './server.mjs';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const origin='http://127.0.0.1:18771';
const server=createServer(await readFile(new URL('./index.html',import.meta.url)),origin);await new Promise(resolve=>server.listen(18771,'127.0.0.1',resolve));
await fetch(origin+'/api/send',{method:'POST',headers:{Origin:origin,'Content-Type':'application/json'},body:JSON.stringify({id:crypto.randomUUID(),agent:'manager',text:'Pause test'})});
const browser=await chromium.launch({headless:true});
try{
  const page=await browser.newPage();
  await page.addInitScript(()=>{window.voiceBrowserOnly=true;navigator.mediaDevices.getUserMedia=async()=>{throw new Error('Test microphone denied');};HTMLMediaElement.prototype.play=async function(){};HTMLMediaElement.prototype.pause=function(){};Object.defineProperty(HTMLMediaElement.prototype,'src',{set(){},get(){return '';}});});
  await page.route('**/api/tts',route=>route.fulfill({body:'audio',contentType:'audio/wav'}));
  await page.goto(origin);await page.locator('article[data-message-id="1"] .reader-sentence').first().click();
  let microphoneRequests=0,sends=0;await page.exposeFunction('testMicrophone',()=>{microphoneRequests++;});
  await page.evaluate(()=>{navigator.mediaDevices.getUserMedia=async()=>{await window.testMicrophone();throw new Error('Test microphone denied');};});
  page.on('request',request=>{if(request.url().endsWith('/api/send'))sends++;});
  await page.locator('#pause').click();await page.getByRole('button',{name:'Play',exact:true}).waitFor();
  assert.equal(microphoneRequests,0);assert.equal(sends,0);
  const saved=await page.evaluate(()=>JSON.parse(localStorage.getItem('voice-position:mock:true')));assert.equal(saved.id,1);assert.equal(saved.index,0);
  await page.reload();await page.getByRole('button',{name:'Play',exact:true}).waitFor();await page.locator('#pause').click();
  await page.waitForFunction(()=>document.querySelector('article[data-message-id="1"] .reading'));
  assert.equal(await page.locator('#pause').textContent(),'Pause');
  assert.equal(microphoneRequests,0);assert.equal(sends,0);
  await page.locator('#auto').click();
  await page.locator('#dictate').click();
  await page.locator('#draft').fill('A reply while playback is stopped');
  await page.locator('#send').click();
  await page.waitForFunction(()=>!document.querySelector('#send').disabled||!document.querySelector('#draft').value);
  await page.locator('#auto').click();
  await page.waitForFunction(()=>document.querySelector('article[data-message-id="1"] .reading'));
  const resumed=await page.evaluate(()=>JSON.parse(localStorage.getItem('voice-position:mock:true')));
  assert.equal(resumed.id,1);assert.equal(resumed.index,0);
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
