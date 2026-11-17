/** Outbound-only speech relay. Importing this module never starts a worker. */
import {mkdir,readFile,writeFile,rename} from 'node:fs/promises';
import {resolve} from 'node:path';
import {pathToFileURL} from 'node:url';
import {splitSentences} from './reader-ui.mjs';

export function parseRequest(message,humanId,botId,streamId){
  if(message.type!=='stream'||message.stream_id!==streamId||message.sender_id!==humanId)return null;
  let request;try{request=JSON.parse(message.content);}catch{return null;}
  if(request.voice!==1||!/^[a-zA-Z0-9_-]{8,100}$/.test(request.id)||!['stt','tts','ack'].includes(request.op))return null;
  if(message.subject!==request.op)return null;
  if(request.op==='tts'&&(typeof request.text!=='string'||!request.text.trim()||request.text.length>10000))return null;
  if(request.op==='stt'&&typeof request.uri!=='string')return null;
  return request;
}

export function attachmentURL(uri,site){
  const url=new URL(uri,site);
  if(url.origin!==new URL(site).origin||!/^\/user_uploads\/[^?#]+$/.test(url.pathname)||url.search||url.hash)throw Error('Invalid attachment URL');
  return url;
}

export async function limitedBytes(response,maxBytes=12*1024*1024){
  if(!response.ok)throw Error('Audio request failed');
  if(Number(response.headers.get('content-length'))>maxBytes)throw Error('Audio exceeds size limit');
  const chunks=[];let size=0;
  for await(const chunk of response.body){size+=chunk.length;if(size>maxBytes){throw Error('Audio exceeds size limit');}chunks.push(chunk);}
  return Buffer.concat(chunks);
}

export function createAPI({site,email,key,fetchImpl=fetch}){
  site=new URL(site).origin;
  if(!site.startsWith('https://'))throw Error('Zulip must use HTTPS');
  const authorization='Basic '+Buffer.from(email+':'+key).toString('base64');
  return {
    site,
    async call(method,path,fields={}){
      const form=fields instanceof FormData?fields:new URLSearchParams(Object.entries(fields).map(([k,v])=>[k,typeof v==='string'?v:JSON.stringify(v)]));
      const response=await fetchImpl(site+'/api/v1/'+path+(method==='GET'?'?'+form:''),{method,headers:{Authorization:authorization},body:method==='GET'?undefined:form,signal:AbortSignal.timeout(path==='events'?90000:20000),redirect:'error'});
      const data=await response.json();
      if(!response.ok||data.result!=='success'){
        if(method==='DELETE'&&/^(messages|attachments)\/\d+$/.test(path)&&(response.status===404||(data.code==='BAD_REQUEST'&&/^Invalid (message\(s\)|message|attachment)\.?$/.test(data.msg))))return {result:'success'};
        throw Error('Zulip request failed: '+method+' '+path+' '+response.status);
      }
      return data;
    },
    async download(uri){
      let url=attachmentURL(uri,site);const signal=AbortSignal.timeout(20000);
      for(let redirects=0;redirects<=4;redirects++){
        const response=await fetchImpl(url,{headers:url.origin===site?{Authorization:authorization}:{},redirect:'manual',signal});
        if(![301,302,303,307,308].includes(response.status))return limitedBytes(response);
        const location=response.headers.get('location');await response.body?.cancel();
        if(!location)throw Error('Missing audio redirect');
        url=new URL(location,url);if(url.protocol!=='https:'||url.username||url.password)throw Error('Invalid audio redirect');
      }
      throw Error('Too many audio redirects');
    },
  };
}

/** Create the private relay channel and mute all human subscription notifications. */
export async function provisionChannel(api,botId){
  // 🧑 "Maybe we need a channel who's push notification. for my user is disabled"
  const name='voice-relay';
  const me=await api.call('GET','users/me');
  await api.call('POST','users/me/subscriptions',{subscriptions:[{name,description:'Private speech processing attachments; automatically cleaned.'}],invite_only:true,principals:[botId,me.user_id]});
  const {subscriptions}=await api.call('GET','users/me/subscriptions');
  const channel=subscriptions.find(s=>s.name===name);
  if(!channel?.invite_only)throw Error('Relay channel must be private');
  const properties=['push_notifications','desktop_notifications','audible_notifications','email_notifications','wildcard_mentions_notify'].map(property=>({stream_id:channel.stream_id,property,value:false}));
  await api.call('POST','users/me/subscriptions/properties',{subscription_data:properties});
  return channel.stream_id;
}

export async function createRelay({api,humanId,botId,streamId,stateDir,fetchImpl=fetch,now=Date.now,ttlMs=600000}){
  if(!Number.isSafeInteger(streamId)||streamId<=0)throw Error('Relay channel required');
  await mkdir(stateDir,{recursive:true,mode:0o700});
  const statePath=resolve(stateDir,'requests.json');let records={};
  try{records=JSON.parse(await readFile(statePath,'utf8'));}catch(error){if(error.code!=='ENOENT')throw error;}
  let saving=Promise.resolve();
  function save(){saving=saving.catch(()=>{}).then(async()=>{await writeFile(statePath+'.tmp',JSON.stringify(records),{mode:0o600});await rename(statePath+'.tmp',statePath);});return saving;}
  async function cleanup(record){
    if(record.responseId){await api.call('DELETE','messages/'+record.responseId);delete record.responseId;await save();}
    if(record.uri){
      const {attachments}=await api.call('GET','attachments');
      const item=attachments.find(a=>'/user_uploads/'+a.path_id===record.uri);
      if(item)await api.call('DELETE','attachments/'+item.id);
      delete record.uri;await save();
    }
    record.status='done';await save();
  }
  return {
    async handle(message){
      const request=parseRequest(message,humanId,botId,streamId);if(!request)return;
      if(request.op==='ack'){if(records[request.id]&&!records[request.id].proactive){records[request.id].acked=true;await save();try{await cleanup(records[request.id]);}catch{}}return;}
      if(records[request.id])return;
      await this.expire();
      const active=Object.values(records).filter(r=>r.status!=='done');
      if(request.proactive&&active.length>=48)return;
      if(!request.proactive&&active.length>=64){const oldest=active.filter(r=>r.proactive&&r.status==='sent').sort((a,b)=>a.created-b.created)[0];if(oldest)await cleanup(oldest);}
      const record=records[request.id]={created:now(),status:'working',proactive:!!request.proactive};await save();
      const result={voice:1,id:request.id,op:'result'};
      if(request.op==='tts'){result.text=request.text;if(request.proactive){result.proactive=true;result.messageId=request.messageId;result.sentence=request.sentence;}}
      try{
        if(request.op==='tts'&&Object.values(records).filter(r=>r.uri).length>=64)throw Error('Audio buffer full');
        if(request.op==='stt'){
          const audio=await api.download(request.uri);
          const response=await fetchImpl('http://127.0.0.1:8771/stt',{method:'POST',body:audio,signal:AbortSignal.timeout(15000)});
          if(!response.ok)throw Error('Transcription failed');
          const data=await response.json();if(typeof data.text!=='string')throw Error('Invalid transcript');result.text=data.text;
        }else{
          const response=await fetchImpl('http://127.0.0.1:8770/tts',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text:request.text}),signal:AbortSignal.timeout(15000)});
          const bytes=await limitedBytes(response);const form=new FormData();form.append('file',new Blob([bytes],{type:'audio/wav'}),'voice-'+request.id+'.wav');
          const uploaded=await api.call('POST','user_uploads',form);attachmentURL(uploaded.uri,api.site);record.uri=uploaded.uri;await save();result.uri=uploaded.uri;result.attachment='[audio]('+uploaded.uri+')';
          const temporary=await api.call('GET',uploaded.uri.slice(1));const audioURL=new URL(temporary.url,api.site);
          if(audioURL.origin!==api.site||!audioURL.pathname.startsWith('/user_uploads/temporary/')||audioURL.username||audioURL.password)throw Error('Invalid temporary audio URL');
          result.audioURL=audioURL.href;result.audioURLExpiresAt=now()+55000;
        }
      }catch{result.error='Speech processing failed. Try a new request.';}
      record.status='sending';await save();
      const sent=await api.call('POST','messages',{type:'stream',to:streamId,topic:request.op,content:JSON.stringify(result)});
      record.responseId=sent.id;record.status='sent';await save();
    },
    async prepare(message,taskStreamId){
      if(message.type!=='stream'||message.stream_id!==taskStreamId||message.sender_id!==botId)return;
      const text=message.content.split('\n').filter(line=>!/^\[switch: .+\]$/.test(line)).join('\n').replace(/^\[[^\]\n]+\][^\n]*(?:\n|$)/,'').trim();
      const sentences=splitSentences(text).flatMap(part=>part.match(/[\s\S]{1,1800}/g)||[]).slice(0,20);
      for(const [sentence,text] of sentences.entries())await this.handle({type:'stream',stream_id:streamId,subject:'tts',sender_id:humanId,content:JSON.stringify({voice:1,id:'proactive_'+message.id+'_'+sentence,op:'tts',text,proactive:true,messageId:message.id,sentence})});
    },
    async expire(){for(const [id,record] of Object.entries(records)){
      if(record.status==='done'&&now()-record.created>86400000){delete records[id];await save();continue;}
      if(record.status==='sending'){
        const recent=await api.call('GET','messages',{narrow:[{operator:'stream',operand:streamId}],anchor:'newest',num_before:100,num_after:0,apply_markdown:false});
        const sent=(recent.messages||[]).find(m=>{if(m.sender_id!==botId)return false;try{const body=JSON.parse(m.content);return body.voice===1&&body.id===id&&body.op==='result';}catch{return false;}});
        if(sent){record.responseId=sent.id;record.status='sent';await save();}
      }
      if(record.status!=='done'&&(record.acked||now()-record.created>ttlMs)){try{await cleanup(record);}catch{}}
    }},
  };
}

export async function run(env=process.env){
  const humanId=Number(env.VOICE_HUMAN_ID);if(!Number.isSafeInteger(humanId)||humanId<=0||!env.VOICE_RELAY_STATE)throw Error('Set VOICE_HUMAN_ID and VOICE_RELAY_STATE');
  const api=createAPI({site:env.ZULIP_SITE,email:env.ZULIP_BOT_EMAIL,key:env.ZULIP_BOT_API_KEY});
  const me=await api.call('GET','users/me');if(!me.is_bot||me.user_id===humanId)throw Error('Expected separate bot identity');
  const streamId=Number(env.VOICE_RELAY_STREAM_ID);
  const taskStreamId=Number(env.VOICE_TASK_STREAM_ID);
  const relay=await createRelay({api,humanId,botId:me.user_id,streamId,stateDir:env.VOICE_RELAY_STATE});
  let queue;let backoffMs=1000,queued=0,preparing=Promise.resolve();
  for(;;){
    try{
      if(!queue){
        queue=await api.call('POST','register',{event_types:['message'],apply_markdown:false});
        const backlog=await api.call('GET','messages',{narrow:[{operator:'stream',operand:streamId}],anchor:'newest',num_before:100,num_after:0,apply_markdown:false});
        for(const message of backlog.messages||[]){if(Date.now()-message.timestamp*1000<3600000)await relay.handle(message);}
      }
      const data=await api.call('GET','events',{queue_id:queue.queue_id,last_event_id:queue.last_event_id,dont_block:false});
      for(const event of data.events){if(event.type==='message'){await relay.handle(event.message);if(event.message.type==='stream'&&event.message.stream_id===taskStreamId&&event.message.sender_id===me.user_id&&queued<3){queued++;preparing=preparing.then(()=>relay.prepare(event.message,taskStreamId)).catch(()=>console.error('Background speech preparation failed.')).finally(()=>queued--);}}queue.last_event_id=event.id;}
      await relay.expire();backoffMs=1000;
    }catch(error){console.error('Voice relay request failed; reconnecting:',error.message);queue=null;await new Promise(r=>setTimeout(r,backoffMs));backoffMs=Math.min(backoffMs*2,30000);}
  }
}

if(process.argv[1]&&import.meta.url===pathToFileURL(resolve(process.argv[1])).href)run().catch(()=>{console.error('Voice relay startup failed. Check configuration.');process.exitCode=1;});
