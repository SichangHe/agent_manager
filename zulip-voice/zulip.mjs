import {closeSync,existsSync,fsyncSync,mkdirSync,openSync,readFileSync,renameSync,statSync,writeFileSync} from 'node:fs';
import {join} from 'node:path';

/** Load a private configuration and verify the human identity without sending messages. */
export async function connectZulip(configPath) {
  return connect(configPath);
}

// 🧑 "Make the real thing. We test in prod"
export async function connectZulipSSH(zuliprcPath,settingsPath) {
  return connect(settingsPath,zuliprcPath);
}

async function connect(configPath,zuliprcPath) {
  let config;
  try {
    const info=statSync(configPath);
    if(!info.isFile()||(info.mode&0o077)||info.uid!==process.getuid())return {error:'Live configuration must be owned by this user with mode 0600.'};
    config=JSON.parse(readFileSync(configPath,'utf8'));
    if(zuliprcPath){
      const info=statSync(zuliprcPath);
      const privateOwner=(info.mode&0o777)===0o600&&info.uid===process.getuid();
      const sharedOwner=(info.mode&0o777)===0o640&&info.uid===config.credentialOwnerUid&&info.gid===config.credentialGroupGid&&process.getgroups().includes(info.gid);
      if(!info.isFile()||(!privateOwner&&!sharedOwner))return {error:'Zulip credentials need service-owned mode 0600 or configured human-owned group-readable mode 0640.'};
      let section='';const credentials={};
      for(const line of readFileSync(zuliprcPath,'utf8').split(/\r?\n/)){
        const text=line.trim();
        if(text.startsWith('[')){section=text;continue;}
        const field=text.match(/^(email|key|site)\s*=\s*(.+)$/);
        if(section==='[api]'&&field)credentials[field[1]]=field[2].trim();
      }
      Object.assign(config,{email:credentials.email,apiKey:credentials.key,site:credentials.site});
      config.conversations=[];
    }
    const site=new URL(config.site);
    if(site.protocol!=='https:'||site.username||site.password||site.pathname!=='/'||site.search||site.hash) return {error:'Zulip site must be an HTTPS origin.'};
    if(!Number.isSafeInteger(config.humanId)||!Number.isSafeInteger(config.streamId)||!config.email||!config.apiKey||(!zuliprcPath&&!config.tailscaleLogin)||!config.stateDir)return {error:'Incomplete live configuration.'};
    if(!Number.isSafeInteger(config.botId))return {error:'Live mode requires the configured agent bot ID.'};
    if(!zuliprcPath&&(!Array.isArray(config.conversations)||!config.conversations.length||config.conversations.some(c=>typeof c.id!=='string'||!c.id.trim()||typeof c.name!=='string'||!c.name.trim())||new Set(config.conversations.map(c=>c.id)).size!==config.conversations.length))return {error:'Configure unique, explicit Zulip task topics.'};
    mkdirSync(config.stateDir,{recursive:true,mode:0o700});
    const directory=statSync(config.stateDir);
    if((directory.mode&0o077)||directory.uid!==process.getuid())return {error:'Receipt directory must be private and owned by this user.'};
  }catch{return {error:'Cannot load private live configuration or receipt directory.'};}
  const authorization='Basic '+Buffer.from(config.email+':'+config.apiKey).toString('base64');
  async function request(method,path,fields={}) {
    const query=new URLSearchParams(fields);
    try {
      const response=await fetch(config.site.replace(/\/$/,'')+'/api/v1/'+path+(method==='GET'?'?'+query:''),{method,redirect:'error',headers:{Authorization:authorization,...(method==='POST'?{'Content-Type':'application/x-www-form-urlencoded'}:{})},body:method==='POST'?query:undefined,signal:AbortSignal.timeout(15000)});
      const data=await response.json();
      if(!response.ok||data.result!=='success')return {error:'Zulip rejected the request. Check account permissions and configuration.'};
      return {data};
    }catch{return {error:'Zulip connection failed or timed out.'};}
  }
  const identity=await request('GET','users/me');
  if(identity.error)return identity;
  if(identity.data.user_id!==config.humanId||identity.data.is_bot!==false)return {error:'Credentials do not belong to the configured human Zulip account.'};
  if(config.botId!==undefined){
    if(!Number.isSafeInteger(config.botId))return {error:'Invalid configured bot ID.'};
    const bot=await request('GET','users/'+config.botId);
    if(bot.error)return bot;
    if(bot.data.user?.is_bot!==true)return {error:'Configured agent account is not a Zulip bot.'};
  }
  async function refresh(){
    if(!zuliprcPath)return {data:config.conversations};
    const result=await request('GET',`users/me/${config.streamId}/topics`);
    if(result.error)return result;
    if(!Array.isArray(result.data.topics)||result.data.topics.some(t=>typeof t.name!=='string'||!Number.isSafeInteger(t.max_id)))return {error:'Unexpected Zulip topic response.'};
    config.conversations.splice(0,config.conversations.length,...result.data.topics.sort((a,b)=>b.max_id-a.max_id).map(t=>({id:t.name,name:t.name})));
    return {data:config.conversations};
  }
  const topics=await refresh();if(topics.error)return topics;
  const known=agent=>config.conversations.some(c=>c.id===agent);
  return {data:{
    scope:config.site+':'+config.humanId,
    conversations:config.conversations,
    refresh,
    authorize:headers=>!zuliprcPath&&headers['tailscale-user-login']===config.tailscaleLogin,
    async messages(agent,anchor='newest'){
      if(!known(agent))return {error:'Unknown Zulip task topic.'};
      const result=await request('GET','messages',{anchor:String(anchor),num_before:anchor==='newest'?'30':'15',num_after:anchor==='newest'?'0':'15',apply_markdown:'false',narrow:JSON.stringify([{operator:'stream',operand:config.streamId},{operator:'topic',operand:agent}])});
      if(result.error)return result;
      if(!Array.isArray(result.data.messages))return {error:'Unexpected Zulip message response.'};
      return {data:result.data.messages.filter(m=>m.sender_id===config.humanId||m.sender_id===config.botId).map(m=>({id:m.id,sender:m.sender_id===config.humanId?'human':'agent',text:m.content,unread:!m.flags?.includes('read')}))};
    },
    async unread(agent){
      if(agent&&!known(agent))return {error:'Unknown Zulip task topic.'};
      const result=await request('GET','messages',{anchor:'oldest',num_before:'0',num_after:'100',apply_markdown:'false',narrow:JSON.stringify([{operator:'stream',operand:config.streamId},{operator:'sender',operand:config.botId},{operator:'is',operand:'unread'},...(agent?[{operator:'topic',operand:agent}]:[])])});
      if(result.error)return result;
      return {data:result.data.messages.filter(m=>m.sender_id===config.botId&&known(m.subject)).map(m=>({id:m.id,agent:m.subject,text:m.content,unread:true}))};
    },
    async markRead(id){
      if(!Number.isSafeInteger(id)||id<1)return {error:'Invalid message ID.'};
      return request('POST','messages/flags',{messages:JSON.stringify([id]),op:'add',flag:'read'});
    },
    async events(after){
      if(after!==null&&(!Number.isSafeInteger(after)||after<0))return {error:'Invalid Zulip event cursor.'};
      const topics=await refresh();if(topics.error)return topics;
      const result=await request('GET','messages',{anchor:after===null?'newest':String(after),num_before:after===null?'1':'0',num_after:after===null?'0':'100',include_anchor:after===null?'true':'false',apply_markdown:'false',narrow:JSON.stringify([{operator:'stream',operand:config.streamId}])});
      if(result.error)return result;
      if(!Array.isArray(result.data.messages)||result.data.messages.some(m=>!Number.isSafeInteger(m.id)))return {error:'Unexpected Zulip message response.'};
      const messages=result.data.messages.filter(m=>after===null||m.id>after).sort((a,b)=>a.id-b.id);
      const cursor=messages.at(-1)?.id??after??0;
      return {data:{cursor,events:after===null?[]:messages.filter(m=>m.sender_id===config.botId&&known(m.subject)).map(m=>({id:m.id,agent:m.subject,text:m.content}))}};
    },
    async send(input){
      if(!input||typeof input.id!=='string'||!/^[a-zA-Z0-9_-]{1,100}$/.test(input.id)||!known(input.agent)||typeof input.text!=='string'||!input.text.trim())return {error:'Invalid submission or unknown Zulip task topic.'};
      const path=join(config.stateDir,input.id+'.json');
      const uncertain={error:'Delivery is uncertain. Check the Zulip topic before composing another send. This submission will not be sent again.',uncertain:true};
      if(existsSync(path)){
        try{const previous=JSON.parse(readFileSync(path,'utf8'));if(previous.agent!==input.agent||previous.text!==input.text)return {error:'Submission ID already used for different content.'};return previous.receipt?{data:previous.receipt}:uncertain;}catch{return uncertain;}
      }
      try{const fd=openSync(path,'wx',0o600);try{writeFileSync(fd,JSON.stringify(input));fsyncSync(fd);}finally{closeSync(fd);}const dir=openSync(config.stateDir,'r');try{fsyncSync(dir);}finally{closeSync(dir);}}catch{return {error:'Cannot reserve this submission. No message was sent.'};}
      const result=await request('POST','messages',{type:'stream',to:String(config.streamId),topic:input.agent,content:input.text});
      if(result.error||!Number.isSafeInteger(result.data.id))return uncertain;
      const receipt={...input,mock:false,messageId:result.data.id};
      try{const temp=path+'.completed';const fd=openSync(temp,'w',0o600);try{writeFileSync(fd,JSON.stringify({...input,receipt}));fsyncSync(fd);}finally{closeSync(fd);}renameSync(temp,path);const dir=openSync(config.stateDir,'r');try{fsyncSync(dir);}finally{closeSync(dir);}}catch{return uncertain;}
      return {data:receipt};
    }
  }};
}
