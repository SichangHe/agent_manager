import assert from 'node:assert/strict';
import {createBrowserRelay} from './browser-relay.mjs';
const values=new Map();globalThis.localStorage={getItem:k=>values.get(k),setItem:(k,v)=>values.set(k,v)};
const config={site:'https://zulip.example',email:'human',key:'test',humanId:1,botId:2,relayStreamId:3,streamId:4};
const pause=()=>new Promise(r=>setTimeout(r,10));
for(const stage of ['before','register','upload']){
  values.clear();const controller=new AbortController();const sends=[];
  const adapter={config,async request(method,path,fields){
    if(path==='register'){if(stage==='register')controller.abort();return {data:{queue_id:'q',last_event_id:0}};}
    if(path==='messages'&&method==='POST'){sends.push(fields);return {data:{id:9}};}
    if(path==='attachments')return {data:{attachments:[]}};
    return {data:{messages:[]}};
  }};
  const network=async(url)=>{if(String(url).endsWith('/user_uploads')){controller.abort();return Response.json({result:'success',uri:'/user_uploads/1/audio'});}return Response.json({result:'success'});};
  if(stage==='before')controller.abort();
  const relay=createBrowserRelay(adapter,network);
  await assert.rejects(()=>relay.fetch('/api/stt',{body:new Blob(['audio']),signal:controller.signal}),{name:'AbortError'});
  await pause();assert.equal(sends.length,0,stage);
}
values.clear();const key='voice-relay-cleanup:'+config.site+':'+config.humanId;
values.set(key,JSON.stringify({abandoned:{requestId:10,uri:'/user_uploads/1/old',complete:false}}));
const deleted=[];
createBrowserRelay({config,async request(method,path){
  if(path==='messages'&&method==='POST')return {data:{id:11}};
  if(path==='attachments')return {data:{attachments:[{id:20,path_id:'1/old'}]}};
  return {data:{messages:[]}};
}},async(url)=>{deleted.push(String(url));return Response.json({result:'error',code:'BAD_REQUEST',msg:'Invalid message(s)'},{status:400});});
await pause();assert.equal(values.get(key),'{}');assert.equal(deleted.length,3);

values.clear();let callbackValue,finishPoll,proactiveCalls=0;
const proactive={type:'stream',stream_id:3,sender_id:2,subject:'tts',content:JSON.stringify({voice:1,op:'result',id:'proactive_99_0',proactive:true,text:'Ready ahead.',messageId:99,sentence:0,uri:'/user_uploads/1/ready.wav',audioURL:'https://zulip.example/user_uploads/temporary/ready.wav',audioURLExpiresAt:Date.now()+55000})};
const proactiveRelay=createBrowserRelay({config,async request(method,path){
  proactiveCalls++;
  if(path==='register')return {data:{queue_id:'q',last_event_id:0}};
  if(path==='messages'&&method==='GET')return {data:{messages:[proactive]}};
  if(path==='user_uploads/1/ready.wav')return {data:{url:'https://cdn.example/ready.wav'}};
  if(path==='messages')return {data:{id:30}};
  return {data:{}};
}},async(url)=>String(url).includes('/events?')?new Promise(resolve=>{finishPoll=()=>resolve(Response.json({result:'success',events:[]}));}):Response.json({result:'success'}));
await proactiveRelay.start(value=>{callbackValue=value;});
assert.equal(callbackValue.text,'Ready ahead.');assert.equal(callbackValue.relayId,'proactive_99_0');
const cached=await proactiveRelay.fetch('/api/tts',{body:JSON.stringify({text:'Ready ahead.'})});assert.equal((await cached.json()).audioURL,'https://zulip.example/user_uploads/temporary/ready.wav');assert.equal(proactiveCalls,2);
const callsBeforeRelease=proactiveCalls;await proactiveRelay.release('proactive_99_0');assert.equal(values.get(key),'{}');assert.equal(proactiveCalls,callsBeforeRelease);
const realTimeout=globalThis.setTimeout,scheduled=[];let registrations=0;
globalThis.setTimeout=(callback,delay)=>{scheduled.push({callback,delay});return 1;};
try{
 const reconnectRelay=createBrowserRelay({config,async request(method,path){if(path==='register'){registrations++;if(registrations===2)return {error:'Offline'};return {data:{queue_id:'r',last_event_id:0}};}return {data:{messages:[]}};}},async()=>{throw Error('Offline');});
 await reconnectRelay.start(()=>{});await new Promise(resolve=>setImmediate(resolve));
 assert.equal(scheduled[0].delay,1000);scheduled[0].callback();await new Promise(resolve=>setImmediate(resolve));
 assert.equal(scheduled[1].delay,2000);scheduled[1].callback();await new Promise(resolve=>setImmediate(resolve));assert.equal(registrations,3);
}finally{globalThis.setTimeout=realTimeout;}
values.clear();let observed=0;
const deniedRelay=createBrowserRelay({config,observeRateLimit(response){observed=response.status;},async request(method,path){return {data:path==='register'?{queue_id:'q',last_event_id:0}:path==='attachments'?{attachments:[]}:{messages:[]}};}},async()=>Response.json({result:'error',msg:'Insufficient permission'},{status:403}));
await assert.rejects(()=>deniedRelay.fetch('/api/stt',{body:new Blob(['voice'])}),/POST user_uploads: HTTP 403 Insufficient permission/);assert.equal(observed,403);
let attempted=false;
const coolingRelay=createBrowserRelay({config,checkCooldown(){throw Error('HTTP 429: retry in 60s');},async request(){return {data:{queue_id:'q',last_event_id:0,attachments:[]}};}},async()=>{attempted=true;return Response.json({});});
await assert.rejects(()=>coolingRelay.fetch('/api/stt',{body:new Blob(['voice'])}),/HTTP 429/);assert.equal(attempted,false);
const received=[],flags=[];let eventCalls=0;
const unified=createBrowserRelay({config,ingestMessage:message=>received.push(message),ingestFlags:event=>flags.push(event),async request(method,path,fields){if(path==='register'){assert.equal(fields.narrow,undefined);assert.deepEqual(JSON.parse(fields.event_types),['message','update_message_flags']);return {data:{queue_id:'unified',last_event_id:0}};}return {data:{messages:[]}};}},async()=>{if(eventCalls++)return new Promise(()=>{});return Response.json({result:'success',events:[{id:1,type:'message',message:{type:'stream',stream_id:4,id:55}},{id:2,type:'message',message:{type:'stream',stream_id:99,id:56}},{id:3,type:'update_message_flags',flag:'read',messages:[55]}]});});
await unified.start(()=>{});await new Promise(resolve=>setImmediate(resolve));assert.deepEqual(received.map(m=>m.id),[55]);assert.equal(flags.length,1);
