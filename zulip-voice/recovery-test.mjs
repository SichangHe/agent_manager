/** Browser persistence and reconnection test; only an isolated mock server receives sends. */
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createServer} from './server.mjs';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const origin='http://127.0.0.1:18768';
const server=createServer(await readFile(new URL('./index.html',import.meta.url)),origin);
await new Promise(resolve=>server.listen(18768,'127.0.0.1',resolve));
const browser=await chromium.launch({headless:true});
try{
  const page=await browser.newPage();await page.goto(origin);
  await page.getByRole('button',{name:'Compose',exact:true}).click();
  await page.locator('#draft').fill('Queued during network loss');
  await page.route('**/api/send',route=>route.abort());
  await page.locator('#send').click();
  await page.getByText('Connection lost.',{exact:false}).waitFor();
  assert.equal(await page.locator('#draft').isDisabled(),true);
  await page.unroute('**/api/send');
  await page.reload();
  await page.getByText('Queued message delivered to manager.',{exact:true}).waitFor({timeout:20000});
  const history=await(await fetch(origin+'/api/messages?agent=manager')).json();
  assert.equal(history.filter(m=>m.sender==='human').length,1);
  await page.evaluate(async()=>{
    const {openDurableQueue}=await import('/resilient-fetch.mjs');
    const {data:queue}=await openDurableQueue('mock:true');
    await queue.add({id:'saved-recording',kind:'audio',agent:'quals',blob:new Blob(['isolated recording']),prefix:''});queue.close();
  });
  await page.route('**/api/stt',route=>route.fulfill({json:{text:'Recovered words'}}));
  await page.reload();
  await page.getByText('Recovered recording in quals. Review before sending.',{exact:true}).waitFor({timeout:20000});
  await page.locator('#agent').selectOption('quals');
  await page.getByRole('button',{name:'Compose',exact:true}).click();
  assert.equal(await page.locator('#draft').inputValue(),'Recovered words');
  assert.equal((await(await fetch(origin+'/api/messages?agent=quals')).json()).filter(m=>m.sender==='human').length,0);
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
