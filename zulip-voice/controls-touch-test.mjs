/** Exercise control gestures and real scroll changes in mobile WebKit. */
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
const {webkit}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await webkit.launch({headless:true});
try{
  const page=await browser.newPage({viewport:{width:390,height:844},hasTouch:true,isMobile:true});
  await page.setContent('<div id="messages" style="height:200px;overflow:auto"><div style="height:2000px">Messages</div></div><section id="composer"><textarea id="draft"></textarea><button id="dictate">Dictate</button></section>');
  await page.addScriptTag({type:'module',content:(await readFile(new URL('./reader-ui.mjs',import.meta.url),'utf8'))+'\nwindow.controls=installCompactControls({composer:document.querySelector("#composer"),draft:document.querySelector("#draft"),dictate:document.querySelector("#dictate")});'});
  await page.waitForFunction(()=>window.controls);
  async function swipe(selector,dy){await page.locator(selector).evaluate((element,dy)=>{
    const touch=y=>({identifier:1,target:element,clientX:100,clientY:y});
    for(const [name,y] of [['touchstart',150],['touchend',150+dy]]){const event=new Event(name,{bubbles:true,cancelable:true});Object.defineProperties(event,{touches:{value:name==='touchstart'?[touch(y)]:[]},changedTouches:{value:[touch(y)]}});element.dispatchEvent(event);}
  },dy);}
  await swipe('.reader-dock',-80);assert.equal(await page.locator('#composer').isVisible(),true);
  await swipe('#composer',80);assert.equal(await page.locator('#composer').isVisible(),false);
  await swipe('.reader-dock',-80);await swipe('.reader-dock',80);assert.equal(await page.locator('#composer').isVisible(),false);
  for(const top of [600,200]){
    await swipe('.reader-dock',-80);
    await page.locator('#messages').evaluate((element,top)=>{element.scrollTop=top;},top);
    await page.waitForFunction(()=>document.querySelector('#composer').hidden);
  }
  console.log('WebKit mobile: dock and composer swipe, both message scroll directions collapse correctly. Synthetic touch events do not establish physical iOS gesture behavior.');
}finally{await browser.close();}
