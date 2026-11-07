/** Digital residual-echo benchmark using actual MediaRecorder, handsfree logic and CUDA STT; no Zulip sends. Supply a real spoken pause WAV as argv[2], otherwise the command is synthetic. */
import http from 'node:http';
import {readFile} from 'node:fs/promises';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const tts=process.env.VOICE_AUDIO_URL||'http://127.0.0.1:8770';
const stt=process.env.VOICE_STT_URL||'http://127.0.0.1:8771';
async function speech(text){
  const response=await fetch(tts+'/tts',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text}),signal:AbortSignal.timeout(12000)});
  if(!response.ok)throw new Error('TTS '+response.status);
  return Buffer.from(await response.arrayBuffer());
}
const fixtures=new Map([
  ['/pause.wav',process.argv[2]?await readFile(process.argv[2]):await speech('Pause.')],
  ['/reply.wav',await speech('Your presentation should begin with the main research question. Explain the motivation and then introduce the evidence supporting your argument.')],
]);
let transcripts=[];
const server=http.createServer(async(req,res)=>{
  try{
    if(['/handsfree.mjs','/resilient-fetch.mjs'].includes(req.url)){res.setHeader('Content-Type','text/javascript');res.end(await readFile(new URL('.'+req.url,import.meta.url)));return;}
    if(fixtures.has(req.url)){res.setHeader('Content-Type','audio/wav');res.end(fixtures.get(req.url));return;}
    if(req.url==='/api/stt'){
      const parts=[];for await(const part of req)parts.push(part);
      const startedMs=performance.now();
      const response=await fetch(stt+'/stt',{method:'POST',body:Buffer.concat(parts),signal:AbortSignal.timeout(12000)});
      const body=await response.text();transcripts.push({body,ms:Math.round(performance.now()-startedMs)});
      res.writeHead(response.status,{'Content-Type':'application/json'});res.end(body);return;
    }
    res.setHeader('Content-Type','text/html');res.end('<button>Start</button>');
  }catch(error){res.writeHead(500);res.end(String(error));}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({headless:true,args:['--autoplay-policy=no-user-gesture-required','--no-sandbox']});
try{
  for(const echoGain of process.env.ECHO_CONTROL?[0.3]:[0,0.03,0.1,0.3]){
    for(let trial=0;trial<(process.env.ECHO_CONTROL?1:3);trial++){
      transcripts=[];
      const page=await browser.newPage();await page.goto('http://127.0.0.1:'+server.address().port);
      await page.evaluate(async({echoGain,control})=>{
        const {createHandsfree}=await import('/handsfree.mjs');
        const context=new AudioContext(),microphone=context.createMediaStreamDestination();
        const buffers={};for(const name of ['pause','reply'])buffers[name]=await context.decodeAudioData(await(await fetch('/'+name+'.wav')).arrayBuffer());
        navigator.mediaDevices.getUserMedia=async()=>microphone.stream;
        window.result={detected:false,errors:[]};
        const handsfree=createHandsfree({onPause:async()=>{result.detected=true;result.latencyMs=Math.round(performance.now()-window.commandEndedMs);},onReply:async()=>false,onError:error=>result.errors.push(error)});
        window.finish=()=>{handsfree.stop();context.close();};
        document.querySelector('button').onclick=async()=>{
          await context.resume();await handsfree.start();
          const reply=context.createBufferSource();reply.buffer=buffers.reply;reply.connect(context.destination);
          const echo=context.createGain();echo.gain.value=echoGain;reply.connect(echo);echo.connect(microphone);reply.start();
          const command=context.createBufferSource();command.buffer=buffers.pause;command.connect(microphone);
          window.commandEndedMs=performance.now()+1000+buffers.pause.duration*1000;if(!control)command.start(context.currentTime+1);
        };
      },{echoGain,control:!!process.env.ECHO_CONTROL});
      await page.click('button');
      await page.waitForFunction(()=>result.detected||result.errors.length,null,{timeout:14000}).catch(()=>{});
      const result=await page.evaluate(()=>{finish();return result;});
      console.log(JSON.stringify({fixture:process.env.ECHO_CONTROL?'no command':process.argv[2]?'provided recording':'synthetic Kokoro pause',echoGain,trial,...result,transcripts}));
      await page.close();
    }
  }
}finally{await browser.close();server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
