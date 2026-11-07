import assert from 'node:assert/strict';
import {createBrowserZulip} from './browser-zulip.mjs';

const store=new Map();const storage={getItem:k=>store.get(k)??null,setItem:(k,v)=>store.set(k,v)};
const config={site:'https://zulip.example',email:'human@example',key:'isolated-test',humanId:1,botId:2,streamId:3};
let clock=1000000,sends=0,failAfterSend=false,reconcileFails=false;
const posted=[];
async function network(input,options){
  const url=new URL(input),fields=options.method==='GET'?url.searchParams:options.body;
  const path=url.pathname.replace('/api/v1/','');let data={result:'success'};
  if(path==='users/me')data={...data,user_id:1,is_bot:false};
  else if(path.endsWith('/topics'))data.topics=[{name:'manager',max_id:10},{name:'quals',max_id:9}];
  else if(path==='messages'&&options.method==='POST'){
    sends++;posted.push({id:sends+10,sender_id:1,subject:fields.get('topic'),content:fields.get('content'),timestamp:Math.floor(clock/1000)});
    if(failAfterSend)throw Error('Response lost after acceptance');
    data.id=sends+10;
  }else if(path==='messages'){
    if(reconcileFails)throw Error('Offline');
    data.messages=posted;data.found_oldest=true;
  }else if(path!=='messages/flags')throw Error('Unexpected URL '+path);
  return new Response(JSON.stringify(data),{status:200,headers:{Date:new Date(clock).toUTCString()}});
}
const make=()=>createBrowserZulip(config,{fetch:network,storage,now:()=>clock});
const submit=(client,id,text='hello')=>client.fetch('/api/send',{method:'POST',body:JSON.stringify({id,agent:'manager',text})}).then(r=>r.json());
let client=make();assert.deepEqual(await client.verify(),{data:true});
assert.equal((await client.fetch('/api/conversations').then(r=>r.json()))[0].id,'manager');
failAfterSend=true;
assert.equal((await submit(client,'lost')).uncertain,true);assert.equal(sends,1);
client=make();
assert.equal((await submit(client,'lost')).messageId,11);assert.equal(sends,1);
assert.match((await submit(client,'lost','changed')).error,/different content/);
reconcileFails=true;
store.set('zulip-voice-receipt:https://zulip.example:1:3:offline',JSON.stringify({id:'offline',agent:'manager',text:'hello',startedSeconds:900,serverSeconds:900,baselineId:11}));
assert.equal((await submit(client,'offline')).uncertain,true);assert.equal(sends,1);
reconcileFails=false;failAfterSend=false;
store.set('zulip-voice-receipt:https://zulip.example:1:3:absent',JSON.stringify({id:'absent',agent:'manager',text:'new message',startedSeconds:900,serverSeconds:900,baselineId:11}));
assert.equal((await submit(client,'absent','new message')).messageId,12);assert.equal(sends,2);
await Promise.all([submit(client,'simultaneous','once'),submit(client,'simultaneous','once')]);assert.equal(sends,3);
const noStorage= createBrowserZulip(config,{fetch:network,storage:{getItem:()=>null,setItem:()=>{throw Error('quota');}}});
assert.match((await submit(noStorage,'quota')).error,/No message sent/);assert.equal(sends,3);
failAfterSend=true;
const skewed=()=>createBrowserZulip(config,{fetch:network,storage,now:()=>clock+60000});
assert.equal((await submit(skewed(),'skew','clock skew')).uncertain,true);
assert.equal((await submit(skewed(),'skew','clock skew')).messageId,14);assert.equal(sends,4);
const noDate=async(...args)=>{const r=await network(...args);r.headers.delete('Date');return r;};
const undated=()=>createBrowserZulip(config,{fetch:noDate,storage,now:()=>clock});
assert.equal((await submit(undated(),'undated','missing server time')).uncertain,true);
clock+=30000;
assert.equal((await submit(undated(),'undated','missing server time')).uncertain,true);assert.equal(sends,5);
const beforeDelivery=async(input,options)=>{if(options.method==='POST'&&new URL(input).pathname.endsWith('/messages'))throw Error('Before send');return network(input,options);};
const repeat=createBrowserZulip(config,{fetch:beforeDelivery,storage,now:()=>clock});
await repeat.fetch('/api/messages?agent=manager');
assert.equal((await submit(repeat,'repeat','missing server time')).uncertain,true);
assert.equal((await submit(repeat,'repeat','missing server time')).uncertain,true);assert.equal(sends,5);
