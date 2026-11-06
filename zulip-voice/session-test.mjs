import assert from 'node:assert/strict';
import http from 'node:http';
import {mkdtemp,readFile,stat,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createSessionAuth} from './session.mjs';
import {createServer} from './server.mjs';

const directory=await mkdtemp(join(tmpdir(),'voice-auth-'));
const tokenPath=join(directory,'token');
const auth=createSessionAuth(tokenPath);
let sends=0;
const transport={conversations:[{id:'agent',name:'Agent'}],authorize:()=>false,
  messages:async()=>({data:[{sender:'agent',text:'Reply\n[switch: agent]'}]}),
  events:async()=>({data:{cursor:1,events:[{id:1,agent:'agent',text:'Reply\n[switch: agent]'}]}}),
  send:async input=>{sends++;return {data:input};}};
const server=createServer('private page','http://localhost:8769',transport,auth);
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
async function request(path,headers={},body){
  return new Promise((resolve,reject)=>{
    const req=http.request({hostname:'127.0.0.1',port:server.address().port,path,method:body?'POST':'GET',headers:{Host:'localhost:8769',...headers}},res=>{
      let data='';res.on('data',c=>data+=c);res.on('end',()=>resolve({status:res.statusCode,headers:res.headers,body:data}));
    });req.on('error',reject);req.end(body);
  });
}
try{
  assert.equal((await stat(tokenPath)).mode&0o777,0o600);
  const token=(await readFile(tokenPath,'utf8')).trim();
  assert.equal((await request('/api/mode')).status,401);
  assert.equal((await request('//[')).status,400);
  assert.equal((await request('/?t='+token,{Host:'evil.invalid'})).status,403);
  assert.equal((await readFile(tokenPath,'utf8')).trim(),token);
  const login=await request('/?t='+token);
  assert.equal(login.status,303);assert.equal(login.headers.location,'/');
  assert.match(login.headers['set-cookie'][0],/HttpOnly; SameSite=Strict/);
  assert.equal(login.headers['referrer-policy'],'no-referrer');
  assert.notEqual((await readFile(tokenPath,'utf8')).trim(),token);
  assert.equal((await request('/?t='+token)).status,401);
  const Cookie=login.headers['set-cookie'][0].split(';')[0];
  assert.equal((await request('/api/mode',{Cookie})).status,200);
  const events=JSON.parse((await request('/api/events',{Cookie})).body);
  assert.equal(events.events[0].switchTo,'agent');assert.equal(events.events[0].text,'Reply');
  const messages=JSON.parse((await request('/api/messages?agent=agent',{Cookie})).body);
  assert.equal(messages[0].text,'Reply');
  const body=JSON.stringify({id:crypto.randomUUID(),agent:'agent',text:'Explicit user send'});
  const headers={Cookie,'Content-Type':'application/json',Origin:'http://localhost:8769'};
  assert.equal((await request('/api/send',{...headers,Origin:'http://evil.invalid'},body)).status,403);
  assert.equal((await request('/api/send',{'Content-Type':'application/json',Origin:headers.Origin},body)).status,401);
  assert.equal(sends,0);
  assert.equal((await request('/api/send',headers,body)).status,200);assert.equal(sends,1);
  assert.equal((await request('/api/send',{...headers,Host:'127.0.0.1:9999',Origin:'http://127.0.0.1:9999'},body)).status,200);
}finally{await new Promise(resolve=>server.close(resolve));await rm(directory,{recursive:true,force:true});}
