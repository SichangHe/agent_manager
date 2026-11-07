/** Explicitly authorized bot self-DM relay benchmark; cleans up only its own messages/uploads. */
import {readFile} from 'node:fs/promises';
import {homedir} from 'node:os';
const values=Object.fromEntries((await readFile(homedir()+'/.config/amh/zulip.env','utf8')).split('\n').filter(line=>/^ZULIP_\w+=/.test(line)).map(line=>{const i=line.indexOf('=');return [line.slice(0,i),line.slice(i+1).trim().replace(/^['"]|['"]$/g,'')];}));
const site=values.ZULIP_SITE.replace(/\/$/,''),auth='Basic '+Buffer.from(values.ZULIP_BOT_EMAIL+':'+values.ZULIP_BOT_API_KEY).toString('base64');
async function api(method,path,fields={}){
  const form=fields instanceof FormData?fields:new URLSearchParams(Object.entries(fields).map(([k,v])=>[k,typeof v==='string'?v:JSON.stringify(v)]));
  const response=await fetch(site+'/api/v1/'+path+(method==='GET'?'?'+form:''),{method,headers:{Authorization:auth},body:method==='GET'?undefined:form,signal:AbortSignal.timeout(20000)});
  const data=await response.json();if(!response.ok||data.result!=='success')throw new Error(path+': '+response.status+' '+data.msg);return data;
}
const me=await api('GET','users/me');if(!me.is_bot)throw new Error('Expected bot credentials.');
const tag='voice-relay-benchmark-'+crypto.randomUUID(),messages=[],uploads=[],queues=[];
async function send(content){const result=await api('POST','messages',{type:'direct',to:[me.user_id],content:tag+' '+content});messages.push(result.id);return result.id;}
async function register(){const q=await api('POST','register',{event_types:['message'],narrow:[['is','private']],apply_markdown:false});queues.push(q.queue_id);return {id:q.queue_id,last:q.last_event_id};}
async function see(q,id){for(let n=0;n<10;n++){const result=await api('GET','events',{queue_id:q.id,last_event_id:q.last,dont_block:false});for(const event of result.events){q.last=Math.max(q.last,event.id);if(event.type==='message'&&event.message.id===id){if(event.message.sender_id!==me.user_id||event.message.display_recipient.some(r=>r.id!==me.user_id))throw new Error('Not a self DM');return;}}}throw new Error('Event missing');}
async function upload(bytes,name,type){const form=new FormData();form.append('file',new Blob([bytes],{type}),name);const result=await api('POST','user_uploads',form);uploads.push(result.uri);return result.uri;}
async function download(uri){const response=await fetch(site+uri,{headers:{Authorization:auth},signal:AbortSignal.timeout(15000)});if(!response.ok)throw new Error('Download '+response.status);return Buffer.from(await response.arrayBuffer());}
async function clean(){
  const failures=[];
  for(const id of messages.splice(0)){try{await api('DELETE','messages/'+id);}catch(error){failures.push(error.message);}}
  if(uploads.length){const own=await api('GET','attachments');for(const uri of uploads.splice(0)){const item=own.attachments.find(a=>uri.endsWith(a.path_id));if(!item)continue;try{await api('DELETE','attachments/'+item.id);}catch(error){failures.push(error.message);}}const remaining=await api('GET','attachments');if(remaining.attachments.some(a=>a.name.startsWith(tag)))failures.push('Benchmark uploads remain');}
  for(const queue_id of queues.splice(0)){try{await api('DELETE','events',{queue_id});}catch(error){failures.push(error.message);}}
  if(failures.length)throw new Error('Cleanup: '+failures.join('; '));
}
const rows=[];
try{
  const canary=await send('cleanup check');await api('DELETE','messages/'+canary);messages.splice(messages.indexOf(canary),1);
  const worker=await register(),client=await register(),clip=await readFile(process.argv[2]);
  for(let i=0;i<5;i++){
    let start=performance.now();
    const compressed=process.argv[2].endsWith('.m4a');const uri=await upload(clip,tag+(compressed?'.m4a':'.wav'),compressed?'audio/mp4':'audio/wav');const request=await send('[audio]('+uri+')');await see(worker,request);const arrived=performance.now();
    const audio=await download(uri);const response=await fetch('http://127.0.0.1:8771/stt',{method:'POST',body:audio,signal:AbortSignal.timeout(10000)});const transcript=await response.json();if(!response.ok)throw new Error('STT failed');
    const reply=await send('transcript '+transcript.text);await see(client,reply);rows.push({kind:'stt',notify_ms:Math.round(arrived-start),total_ms:Math.round(performance.now()-start)});
    start=performance.now();const ttsRequest=await send('speak Please review the next slide before we continue.');await see(worker,ttsRequest);const ttsArrived=performance.now();
    const speech=await fetch('http://127.0.0.1:8770/tts',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text:'Please review the next slide before we continue.'}),signal:AbortSignal.timeout(10000)});if(!speech.ok)throw new Error('TTS failed');
    const audioURI=await upload(await speech.arrayBuffer(),tag+'.wav','audio/wav');const ttsReply=await send('[audio]('+audioURI+')');await see(client,ttsReply);await download(audioURI);rows.push({kind:'tts',notify_ms:Math.round(ttsArrived-start),total_ms:Math.round(performance.now()-start)});
  }
}finally{await clean();}
for(const kind of ['stt','tts']){const times=rows.filter(r=>r.kind===kind).map(r=>r.total_ms).sort((a,b)=>a-b);console.log(JSON.stringify({kind,n:times.length,p50_ms:times[Math.floor(times.length/2)],p95_ms:times[Math.ceil(times.length*.95)-1]}));}
console.log(JSON.stringify({rows,cleanup:'complete'}));
