/** Static browser smoke test with synthetic Zulip responses; no live API calls. */
import assert from 'node:assert/strict';
import http from 'node:http';
import {readFile} from 'node:fs/promises';
import {resolve,extname} from 'node:path';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const root=resolve(process.argv[2]);
const server=http.createServer(async(req,res)=>{try{const path=resolve(root,'.'+(req.url==='/'?'/index.html':req.url));if(!path.startsWith(root+'/')){res.writeHead(403);res.end();return;}res.setHeader('Content-Type',extname(path)==='.mjs'?'text/javascript':extname(path)==='.css'?'text/css':'text/html');res.end(await readFile(path));}catch{res.writeHead(404);res.end();}});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({headless:true});
try{
  const page=await browser.newPage(),errors=[];let posts=0;
  page.on('pageerror',error=>errors.push(error.message));
await page.addInitScript(()=>localStorage.setItem('zulip-voice-account',JSON.stringify({site:'https://shagent.zulipchat.com',email:'human@example.invalid',key:'synthetic',humanId:1212497,botId:2,streamId:3})));
  await page.route('https://shagent.zulipchat.com/**',route=>{const req=route.request(),url=new URL(req.url());let data={};if(url.pathname.endsWith('/topics'))data={topics:[{name:'manager',max_id:10}]};else if(url.pathname.endsWith('/users/me'))data={user_id:1212497,is_bot:false};else if(url.pathname.endsWith('/subscriptions'))data={subscriptions:[{stream_id:652749,invite_only:true,name:'voice-relay'}]};else if(url.pathname.endsWith('/register'))data={queue_id:'test',last_event_id:0};else if(url.pathname.endsWith('/events'))return route.abort();else if(url.pathname.endsWith('/messages')&&req.method()==='POST'){posts++;data={id:11};}else if(url.pathname.endsWith('/messages'))data={messages:[],found_oldest:true};return route.fulfill({json:{result:'success',...data}});});
  await page.goto('http://127.0.0.1:'+server.address().port);
  await page.locator('body.reader-layout').waitFor();
  assert.equal(await page.locator('#agent').inputValue(),'manager');
  await page.getByRole('button',{name:'Compose',exact:true}).click();await page.locator('#draft').fill('Synthetic direct send');await page.locator('#send').click();
  await page.getByText('Message delivered to manager.',{exact:true}).waitFor();assert.equal(posts,1);assert.deepEqual(errors,[]);
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
