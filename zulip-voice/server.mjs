import http from 'node:http';
import {readFile} from 'node:fs/promises';
import {pathToFileURL} from 'node:url';
import {connectZulip} from './zulip.mjs';

/** Remove routing lines and validate the requested conversation. */
export function routeReply(text,conversations){
  let switchTo=null,notice=null;
  const clean=text.split('\n').filter(line=>{
    const match=line.match(/^\[switch: (.+)\]$/);
    if(!match)return true;
    const target=match[1].replace(/\.md$/,'');
    if(conversations.some(c=>c.id===target))switchTo=target;
    else notice='Unknown conversation: '+match[1];
    return false;
  }).join('\n').trim();
  return {text:clean,switchTo,notice};
}

export function createServer(html, origin, live=null,sessionAuth=null,ssh=false) {
  const conversations=live?.conversations||[{id:'manager',name:'Main manager (mock)'},{id:'quals',name:'Quals assistant (mock)'}];
  const messages=new Map(conversations.map(c=>[c.id,[{sender:'agent',text:'This is a mock conversation. Try dictating a message and reviewing it aloud.'}]]));
  const receipts=new Map();
  const events=[];
  let audioRequests=0;
  return http.createServer(async(req,res)=>{
    const reply=(code,data)=>{res.writeHead(code,{'Content-Type':'application/json','Cache-Control':'no-store','X-Content-Type-Options':'nosniff'});res.end(JSON.stringify(data));};
    const loopback=/^(localhost|127\.0\.0\.1|\[::1\])(?::[0-9]{1,5})?$/.test(req.headers.host||'');
    const loopbackAllowed=(!live||sessionAuth||ssh)&&loopback;
    if((sessionAuth||ssh)&&!loopback)return reply(403,{error:'Expected a loopback browser address.'});
    if(!loopbackAllowed&&req.headers.host!==new URL(origin).host)return reply(403,{error:'Unexpected host'});
    const requestOrigin=loopbackAllowed?'http://'+req.headers.host:origin;
    let url;
    try{url=new URL(req.url,origin);}catch{return reply(400,{error:'Invalid request URL.'});}
    if(sessionAuth){
      if(req.method==='GET'&&url.pathname==='/'&&url.searchParams.has('t')){
        let cookie;
        try{cookie=sessionAuth.accept(url.searchParams.get('t'));}catch{return reply(503,{error:'Cannot rotate login token. No session created.'});}
        if(!cookie)return reply(401,{error:'Login link expired or invalid. Use the current token file.'});
        res.writeHead(303,{'Location':'/','Set-Cookie':cookie,'Cache-Control':'no-store','Referrer-Policy':'no-referrer'});res.end();return;
      }
      if(!sessionAuth.authorize(req.headers))return reply(401,{error:'Open the one-time login link to access Zulip.'});
    }else if(live&&!ssh&&!live.authorize(req.headers))return reply(403,{error:'Connect using the authorized human Tailscale identity.'});
    if(req.method==='GET'&&url.pathname==='/'){res.writeHead(200,{'Content-Type':'text/html; charset=utf-8','Cache-Control':'no-store','X-Frame-Options':'DENY','Referrer-Policy':'no-referrer','Permissions-Policy':'microphone=(self)'});res.end(html);return;}
    if(req.method==='GET'&&['/reader-ui.mjs','/reader-ui.css','/browser-audio.mjs','/handsfree.mjs','/resilient-fetch.mjs'].includes(url.pathname)){
      res.writeHead(200,{'Content-Type':url.pathname.endsWith('.css')?'text/css':'text/javascript','Cache-Control':'no-store'});
      res.end(await readFile(new URL('.'+url.pathname,import.meta.url)));return;
    }
    if(req.method==='GET'&&url.pathname==='/api/conversations')return reply(200,conversations);
    if(req.method==='GET'&&url.pathname==='/api/mode')return reply(200,{mock:!live,scope:live?.scope||'mock'});
    if(req.method==='POST'&&['/api/tts','/api/stt'].includes(url.pathname)){
      if(req.headers.origin!==requestOrigin)return reply(403,{error:'Expected same-origin audio request.'});
      if(audioRequests>=3)return reply(503,{error:'Speech server busy; retry shortly.'});
      audioRequests++;
      try{
        const chunks=[];let size=0;const limit=url.pathname==='/api/tts'?16000:12*1024*1024;
        for await(const chunk of req){size+=chunk.length;if(size>limit)return reply(413,{error:'Recording or text too large.'});chunks.push(chunk);}
        const response=await fetch('http://127.0.0.1:8770/'+url.pathname.slice(5),{method:'POST',headers:{'Content-Type':url.pathname==='/api/tts'?'application/json':'application/octet-stream'},body:Buffer.concat(chunks),signal:AbortSignal.timeout(120000)});
        res.writeHead(response.status,{'Content-Type':response.headers.get('content-type'),'Cache-Control':'no-store'});res.end(Buffer.from(await response.arrayBuffer()));
      }catch{reply(503,{error:'Speech server unavailable. Retry shortly.'});}finally{audioRequests--;}
      return;
    }
    if(req.method==='GET'&&url.pathname==='/api/unread'){
      const agent=url.searchParams.get('agent');
      const result=live?await live.unread(agent):{data:events.filter(e=>!e.read&&(!agent||e.agent===agent))};
      return result.error?reply(502,result):reply(200,result.data.map(e=>({...e,...routeReply(e.text,conversations)})));
    }
    if(req.method==='GET'&&url.pathname==='/api/events'){
      if(live){
        if(!live.events)return reply(501,{error:'Live reply events are not configured.'});
        const after=url.searchParams.has('after')?Number(url.searchParams.get('after')):null;
        if(after!==null&&(!Number.isSafeInteger(after)||after<0))return reply(400,{error:'Invalid reply cursor.'});
        const result=await live.events(after);
        if(result.error)return reply(502,result);
        return reply(200,{...result.data,conversations,events:result.data.events.map(e=>({...e,...routeReply(e.text,conversations)}))});
      }
      const after=Number(url.searchParams.get('after')||0);
      return reply(200,{cursor:events.length,events:events.filter(e=>e.id>after)});
    }
    if(req.method==='GET'&&url.pathname==='/api/messages'){
      const agent=url.searchParams.get('agent');
      if(!conversations.some(c=>c.id===agent))return reply(404,{error:'Unknown agent'});
      const anchor=url.searchParams.has('anchor')?Number(url.searchParams.get('anchor')):'newest';
      if(anchor!=='newest'&&(!Number.isSafeInteger(anchor)||anchor<1))return reply(400,{error:'Invalid message anchor.'});
      if(live){const result=await live.messages(agent,anchor);return result.error?reply(502,result):reply(200,result.data.map(m=>({...m,text:m.sender==='agent'?routeReply(m.text,conversations).text:m.text})));}
      return reply(200,messages.get(agent));
    }
    if(req.method!=='POST'||!['/api/send','/api/read'].includes(url.pathname))return reply(404,{error:'Not found'});
    if(req.headers.origin!==requestOrigin||req.headers['content-type']!=='application/json')return reply(403,{error:'Expected same-origin JSON request'});
    let body='';
    try{for await(const chunk of req){body+=chunk;if(body.length>16000){reply(413,{error:'Message too large'});return;}}}catch{return;}
    let input;try{input=JSON.parse(body);}catch{return reply(400,{error:'Invalid JSON'});}
    if(url.pathname==='/api/read'){
      if(!Number.isSafeInteger(input?.id)||input.id<1)return reply(400,{error:'Invalid message ID.'});
      if(live){const result=await live.markRead(input.id);return result.error?reply(502,result):reply(200,{ok:true});}
      const event=events.find(e=>e.id===input.id);if(event)event.read=true;
      return reply(200,{ok:true});
    }
    if(!input||typeof input.id!=='string'||!/^[\da-f-]{36}$/i.test(input.id)||!conversations.some(c=>c.id===input.agent)||typeof input.text!=='string'||!input.text.trim()||input.text.length>8000)return reply(400,{error:'Invalid message or recipient'});
    if(live){const result=await live.send({id:input.id,agent:input.agent,text:input.text});return result.error?reply(409,result):reply(200,result.data);}
    const previous=receipts.get(input.id);
    if(previous)return previous.agent===input.agent&&previous.text===input.text?reply(200,previous):reply(409,{error:'Submission ID already used for different content'});
    if(receipts.size>=10000)return reply(503,{error:'Mock session full; restart the prototype'});
    const receipt={id:input.id,agent:input.agent,text:input.text,mock:true};
    receipts.set(input.id,receipt);
    const target=conversations.find(c=>c.id!==input.agent&&(input.text.toLowerCase().includes(c.id)||input.text.toLowerCase().includes(c.name.replace(' (mock)','').toLowerCase())));
    const routed=routeReply('Mock reply: your message arrived in '+conversations.find(c=>c.id===input.agent).name+'.'+(target?'\n[switch: '+target.id+']':''),conversations);
    const event={id:events.length+1,agent:input.agent,...routed};
    messages.get(input.agent).push({sender:'human',text:input.text},{id:event.id,sender:'agent',text:routed.text,unread:true});
    events.push(event);
    reply(200,receipt);
  });
}
if(import.meta.url===pathToFileURL(process.argv[1]).href){
  const port=Number(process.env.VOICE_PORT||8767);
  const origin=process.env.VOICE_ORIGIN||`http://127.0.0.1:${port}`;
  const html=await readFile(new URL('./index.html',import.meta.url));
  const live=process.env.VOICE_CONFIG?await connectZulip(process.env.VOICE_CONFIG):{data:null};
  if(live.error){console.error(live.error);process.exit(1);}
  if(live.data&&new URL(origin).protocol!=='https:'){console.error('Live mode requires the private HTTPS Serve origin.');process.exit(1);}
  createServer(html,origin,live.data).listen(port,'127.0.0.1',()=>console.log(`${live.data?'Live':'Mock'} voice UI listening on 127.0.0.1:${port}`));
}
