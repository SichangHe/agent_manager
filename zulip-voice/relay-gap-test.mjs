/** Real browser playback through the private speech channel; never sends agent instructions. */
import {readFile} from 'node:fs/promises';
import {createServer} from 'node:http';
import {homedir} from 'node:os';
const {chromium,webkit}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const ini=Object.fromEntries((await readFile(homedir()+'/.config/zulip-voice/zuliprc','utf8')).split('\n').filter(s=>/^\s*(email|key|site)\s*=/.test(s)).map(s=>{const i=s.indexOf('=');return [s.slice(0,i).trim(),s.slice(i+1).trim()];}));
const server=createServer(async(req,res)=>{if(req.url==='/'){res.end('<title>Voice buffer test</title>');return;}if(!['/browser-audio.mjs','/browser-zulip.mjs','/browser-relay.mjs','/resilient-fetch.mjs'].includes(req.url)){res.writeHead(404).end();return;}res.setHeader('Content-Type','text/javascript');res.end(await readFile(new URL('.'+req.url,import.meta.url)));});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await(process.env.BROWSER==='webkit'?webkit:chromium).launch({headless:true,...(process.env.BROWSER==='webkit'?{}:{args:['--autoplay-policy=no-user-gesture-required']})});
try{
 const page=await browser.newPage();await page.goto('http://127.0.0.1:'+server.address().port);
 const result=await page.evaluate(async config=>{
  const {createBrowserZulip}=await import('/browser-zulip.mjs'),{createBrowserRelay}=await import('/browser-relay.mjs'),{createAudio}=await import('/browser-audio.mjs');
  const failures=[],relay=createBrowserRelay(createBrowserZulip(config),fetch.bind(window));window.fetch=async(path,options)=>{try{return await relay.fetch(path,options);}catch(error){failures.push(error.message);throw error;}};
  const errors=[],audio=createAudio({status:text=>errors.push(text),transcribe:()=>{}}),sentences=['This is a private playback measurement, checking the pause between consecutive sentences.','The next sentence should already be prepared while the first sentence is playing.','Keeping a small buffer reduces waiting without filling the disk with unused audio.','The recording finishes here, and all temporary speech attachments should be removed.'];
  const gaps=[],started=performance.now();let firstMs,previousEndMs;
  await new Promise((resolve,reject)=>{const timeout=setTimeout(()=>reject(Error('Playback timeout: '+errors.join(';'))),60000);function next(index){if(index===sentences.length){clearTimeout(timeout);resolve();return;}audio.speak(sentences[index],()=>{previousEndMs=performance.now();next(index+1);},()=>{clearTimeout(timeout);reject(Error([...errors,...failures].join(';')));},()=>{const now=performance.now();if(index)gaps.push(Math.round(now-previousEndMs));else firstMs=Math.round(now-started);});audio.prefetch(sentences.slice(index+1));}next(0);}).catch(error=>errors.push(error.message));
  audio.stop();const key='voice-relay-cleanup:'+config.site+':'+config.humanId;for(let n=0;n<200&&localStorage.getItem(key)!=='{}';n++)await new Promise(resolve=>setTimeout(resolve,100));
  return {firstMs,gapsMs:gaps,cleanup:localStorage.getItem(key)==='{}',errors};
 },{...ini,site:ini.site.replace(/\/$/,''),humanId:1212497,botId:1212502,relayStreamId:Number(process.env.RELAY_STREAM_ID||652749)});
 console.log(JSON.stringify(result));if(result.errors.length||!result.cleanup)process.exitCode=1;
}finally{await browser.close();await new Promise(resolve=>server.close(resolve));}
