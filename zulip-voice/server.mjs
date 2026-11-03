import http from 'node:http';
import {readFile} from 'node:fs/promises';
import {pathToFileURL} from 'node:url';
import {connectZulip} from './zulip.mjs';

export function createServer(html, origin, live=null) {
  const conversations=live?.conversations||[{id:'manager',name:'Main manager (mock)'},{id:'quals',name:'Quals assistant (mock)'}];
  const messages=new Map(conversations.map(c=>[c.id,[{sender:'agent',text:'This is a mock conversation. Try dictating a message and reviewing it aloud.'}]]));
  const receipts=new Map();
  return http.createServer(async(req,res)=>{
    const reply=(code,data)=>{res.writeHead(code,{'Content-Type':'application/json','Cache-Control':'no-store','X-Content-Type-Options':'nosniff'});res.end(JSON.stringify(data));};
    if(req.headers.host!==new URL(origin).host)return reply(403,{error:'Unexpected host'});
    if(live&&!live.authorize(req.headers))return reply(403,{error:'Connect using the authorized human Tailscale identity.'});
    const url=new URL(req.url,origin);
    if(req.method==='GET'&&url.pathname==='/'){res.writeHead(200,{'Content-Type':'text/html; charset=utf-8','Cache-Control':'no-store','X-Frame-Options':'DENY','Referrer-Policy':'no-referrer','Permissions-Policy':'microphone=(self)'});res.end(html);return;}
    if(req.method==='GET'&&url.pathname==='/api/conversations')return reply(200,conversations);
    if(req.method==='GET'&&url.pathname==='/api/mode')return reply(200,{mock:!live});
    if(req.method==='GET'&&url.pathname==='/api/messages'){
      const agent=url.searchParams.get('agent');
      if(!messages.has(agent))return reply(404,{error:'Unknown agent'});
      if(live){const result=await live.messages(agent);return result.error?reply(502,result):reply(200,result.data);}
      return reply(200,messages.get(agent));
    }
    if(req.method!=='POST'||url.pathname!=='/api/send')return reply(404,{error:'Not found'});
    if(req.headers.origin!==origin||req.headers['content-type']!=='application/json')return reply(403,{error:'Expected same-origin JSON request'});
    let body='';
    try{for await(const chunk of req){body+=chunk;if(body.length>16000){reply(413,{error:'Message too large'});return;}}}catch{return;}
    let input;try{input=JSON.parse(body);}catch{return reply(400,{error:'Invalid JSON'});}
    if(!input||typeof input.id!=='string'||!/^[\da-f-]{36}$/i.test(input.id)||!messages.has(input.agent)||typeof input.text!=='string'||!input.text.trim()||input.text.length>8000)return reply(400,{error:'Invalid message or recipient'});
    if(live){const result=await live.send({id:input.id,agent:input.agent,text:input.text});return result.error?reply(409,result):reply(200,result.data);}
    const previous=receipts.get(input.id);
    if(previous)return previous.agent===input.agent&&previous.text===input.text?reply(200,previous):reply(409,{error:'Submission ID already used for different content'});
    if(receipts.size>=10000)return reply(503,{error:'Mock session full; restart the prototype'});
    const receipt={id:input.id,agent:input.agent,text:input.text,mock:true};
    receipts.set(input.id,receipt);
    messages.get(input.agent).push({sender:'human',text:input.text},{sender:'agent',text:'Mock reply: your message arrived in '+conversations.find(c=>c.id===input.agent).name+'.'});
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
