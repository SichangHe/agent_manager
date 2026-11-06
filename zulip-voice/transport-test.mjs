/** Isolated Zulip API simulations; these do not verify production or send real messages. */
import assert from 'node:assert/strict';
import {mkdtempSync,writeFileSync,readFileSync,statSync,chmodSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {connectZulipSSH} from './zulip.mjs';

const directory=mkdtempSync(join(tmpdir(),'zulip-transport-'));
const credentials=join(directory,'zuliprc'),settings=join(directory,'settings.json'),stateDir=join(directory,'receipts');
writeFileSync(credentials,'[api]\nemail = human@example.invalid\nkey = synthetic-secret\nsite = https://zulip.example.invalid\n',{mode:0o600});
writeFileSync(settings,JSON.stringify({humanId:10,botId:20,streamId:30,stateDir}),{mode:0o600});
const originalFetch=globalThis.fetch;
let humanId=10,humanIsBot=false,agentIsBot=true,posts=0,loseResponse=false;
let topics=[{name:'older',max_id:90},{name:'newer',max_id:100}];
let channel=[{id:100,sender_id:20,subject:'newer',content:'Earlier reply'}];
const requests=[];
globalThis.fetch=async (address,options)=>{
  const url=new URL(address),path=url.pathname.replace('/api/v1/','');
  requests.push({path,query:url.searchParams,method:options.method});
  assert.equal(url.origin,'https://zulip.example.invalid');
  assert.equal(options.headers.Authorization,'Basic '+Buffer.from('human@example.invalid:synthetic-secret').toString('base64'));
  let data;
  if(path==='users/me')data={user_id:humanId,is_bot:humanIsBot};
  else if(path==='users/20')data={user:{user_id:20,is_bot:agentIsBot}};
  else if(path==='users/me/30/topics')data={topics:structuredClone(topics)};
  else if(path==='messages'&&options.method==='POST'){
    posts++;
    assert.equal(options.body.get('to'),'30');
    const reservation=JSON.parse(readFileSync(join(stateDir,loseResponse?'uncertain.json':'sent.json'),'utf8'));
    assert.equal(reservation.text,options.body.get('content'));
    assert.equal(reservation.receipt,undefined);
    if(loseResponse)throw new Error('Synthetic lost response after server acceptance');
    data={id:300};
  }else if(path==='messages'){
    const query=url.searchParams,narrow=JSON.parse(query.get('narrow'));
    assert.deepEqual(narrow[0],{operator:'stream',operand:30});
    const topic=narrow.find(n=>n.operator==='topic')?.operand;
    let messages=channel.filter(m=>!topic||m.subject===topic);
    if(query.get('anchor')==='newest')messages=messages.slice(-Number(query.get('num_before')));
    else{
      assert.equal(query.get('include_anchor'),'false');
      assert.ok(Number(query.get('num_after'))<=100);
      messages=messages.filter(m=>m.id>Number(query.get('anchor'))).slice(0,Number(query.get('num_after')));
    }
    data={messages};
  }else assert.fail('Unexpected API request: '+path);
  return new Response(JSON.stringify({result:'success',...data}),{status:200,headers:{'Content-Type':'application/json'}});
};
try{
  humanId=99;assert.match((await connectZulipSSH(credentials,settings)).error,/human/);
  humanId=10;humanIsBot=true;assert.match((await connectZulipSSH(credentials,settings)).error,/human/);
  humanIsBot=false;agentIsBot=false;assert.match((await connectZulipSSH(credentials,settings)).error,/not a Zulip bot/);
  agentIsBot=true;
  const config=JSON.parse(readFileSync(settings,'utf8'));
  chmodSync(credentials,0o640);
  assert.ok((await connectZulipSSH(credentials,settings)).error);
  config.credentialOwnerUid=statSync(credentials).uid;
  config.credentialGroupGid=statSync(credentials).gid;
  writeFileSync(settings,JSON.stringify(config));
  assert.equal((await connectZulipSSH(credentials,settings)).error,undefined);
  assert.equal(statSync(credentials).mode&0o777,0o640);
  config.credentialOwnerUid++;
  writeFileSync(settings,JSON.stringify(config));
  assert.ok((await connectZulipSSH(credentials,settings)).error);
  chmodSync(credentials,0o600);
  const connection=await connectZulipSSH(credentials,settings);assert.equal(connection.error,undefined);
  const live=connection.data,conversations=live.conversations;
  assert.deepEqual(conversations.map(c=>c.id),['newer','older']);
  const count=requests.length;
  assert.ok((await live.messages('missing')).error);
  assert.ok((await live.send({id:'bad',agent:'missing',text:'No'})).error);
  assert.equal(requests.length,count);
  assert.deepEqual((await live.events(null)).data,{cursor:100,events:[]});
  channel.push(...Array.from({length:105},(_,i)=>({id:101+i,sender_id:i%3===0?20:i%3===1?10:99,subject:'older',content:'Reply '+i})));
  topics=[{name:'older',max_id:205},{name:'newer',max_id:100}];
  const first=(await live.events(100)).data;
  assert.equal(first.cursor,200);
  assert.deepEqual(first.events.map(m=>m.id),channel.filter(m=>m.id>100&&m.id<=200&&m.sender_id===20).map(m=>m.id));
  assert.strictEqual(live.conversations,conversations);
  assert.deepEqual(conversations.map(c=>c.id),['older','newer']);
  const second=(await live.events(first.cursor)).data;
  assert.equal(second.cursor,205);
  assert.deepEqual(second.events.map(m=>m.id),[203]);
  assert.deepEqual((await live.events(205)).data,{cursor:205,events:[]});
  assert.ok((await live.events(-1)).error);
  const history=(await live.messages('older')).data;
  assert.ok(history.some(m=>m.sender==='human'));
  assert.ok(history.some(m=>m.sender==='agent'));
  assert.ok(history.every(m=>channel.find(c=>c.id===m.id).sender_id!==99));
  const input={id:'sent',agent:'older',text:'Synthetic send'};
  const receipt=await live.send(input);
  assert.equal(receipt.data.messageId,300);
  assert.deepEqual(await live.send(input),receipt);
  assert.ok((await live.send({...input,text:'Changed content'})).error);
  assert.equal(posts,1);
  assert.equal(statSync(join(stateDir,'sent.json')).mode&0o777,0o600);
  loseResponse=true;
  const uncertain={id:'uncertain',agent:'older',text:'Lost response'};
  assert.equal((await live.send(uncertain)).uncertain,true);
  assert.equal(posts,2);
  const restarted=(await connectZulipSSH(credentials,settings)).data;
  assert.deepEqual(await restarted.send(input),receipt);
  assert.equal((await restarted.send(uncertain)).uncertain,true);
  assert.equal(posts,2);
  assert.equal(statSync(join(stateDir,'uncertain.json')).mode&0o777,0o600);
}finally{
  globalThis.fetch=originalFetch;
  rmSync(directory,{recursive:true,force:true});
}
