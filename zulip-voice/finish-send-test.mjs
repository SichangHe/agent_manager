/** Real MediaRecorder and page; delayed STT and isolated mock recipients. */
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createServer} from './server.mjs';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const origin='http://127.0.0.1:18772';
const server=createServer(await readFile(new URL('./index.html',import.meta.url)),origin);
await new Promise(resolve=>server.listen(18772,'127.0.0.1',resolve));
const browser=await chromium.launch({headless:true,args:['--use-fake-device-for-media-stream','--use-fake-ui-for-media-stream']});
try{
  const page=await browser.newPage();let sent=[],release;
  await page.route('**/api/stt',async route=>{await new Promise(resolve=>{release=resolve;});await route.fulfill({json:{text:'Recorded reply'}});});
  page.on('request',request=>{if(request.url().endsWith('/api/send'))sent.push(request.postDataJSON());});
  await page.goto(origin);await page.locator('#dictate').click();
  await page.getByText('Listening. Transcripts appear while you speak.',{exact:true}).waitFor();
  await page.waitForTimeout(200);await page.locator('#finish-send').click();
  for(let i=0;i<100&&!release;i++)await page.waitForTimeout(20);
  assert.equal(sent.length,0);assert.equal(await page.locator('#agent').isDisabled(),true);
  release();await page.getByText('Mock message delivered to Main manager (mock).',{exact:true}).waitFor();
  assert.equal(sent.length,1);assert.equal(sent[0].agent,'manager');assert.equal(sent[0].text,'Recorded reply');
  await page.locator('#dictate').click();await page.waitForTimeout(200);
  await page.unroute('**/api/stt');await page.route('**/api/stt',route=>route.fulfill({status:503,json:{error:'Unavailable'}}));
  await page.locator('#finish-send').click();await page.getByText('Transcription failed. Retry the clip, then send your draft.',{exact:true}).waitFor();
  assert.equal(sent.length,1);
  await page.reload();await page.getByRole('button',{name:'Compose',exact:true}).click();await page.locator('#draft').fill('Old draft must not send');
  await page.evaluate(()=>{navigator.mediaDevices.getUserMedia=()=>new Promise((resolve,reject)=>{window.rejectMic=reject;});});
  await page.locator('#dictate').click();assert.equal(await page.locator('#finish-send').isDisabled(),true);
  await page.evaluate(()=>window.rejectMic(new Error('Permission denied')));
  await page.waitForFunction(()=>!document.querySelector('#dictate').disabled);
  assert.equal(await page.locator('#finish-send').isDisabled(),true);assert.equal(sent.length,1);
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
