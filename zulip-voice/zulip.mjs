import {closeSync,existsSync,fsyncSync,mkdirSync,openSync,readFileSync,renameSync,statSync,writeFileSync} from 'node:fs';
import {join} from 'node:path';

/** Load a private configuration and verify the human identity without sending messages. */
export async function connectZulip(configPath) {
  let config;
  try {
    const info=statSync(configPath);
    if(!info.isFile()||(info.mode&0o077)||info.uid!==process.getuid())return {error:'Live configuration must be owned by this user with mode 0600.'};
    config=JSON.parse(readFileSync(configPath,'utf8'));
    const site=new URL(config.site);
    if(site.protocol!=='https:'||site.username||site.password||site.pathname!=='/'||site.search||site.hash) return {error:'Zulip site must be an HTTPS origin.'};
    if(!Number.isSafeInteger(config.humanId)||!Number.isSafeInteger(config.streamId)||!config.email||!config.apiKey||!config.tailscaleLogin||!config.stateDir)return {error:'Incomplete live configuration.'};
    if(!Array.isArray(config.conversations)||!config.conversations.length||config.conversations.some(c=>typeof c.id!=='string'||!c.id.trim()||typeof c.name!=='string'||!c.name.trim())||new Set(config.conversations.map(c=>c.id)).size!==config.conversations.length)return {error:'Configure unique, explicit Zulip task topics.'};
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
  return {data:{
    conversations:config.conversations,
    authorize:headers=>headers['tailscale-user-login']===config.tailscaleLogin,
    async messages(agent){
      const result=await request('GET','messages',{anchor:'newest',num_before:'30',num_after:'0',apply_markdown:'false',narrow:JSON.stringify([{operator:'stream',operand:config.streamId},{operator:'topic',operand:agent}])});
      if(result.error)return result;
      if(!Array.isArray(result.data.messages))return {error:'Unexpected Zulip message response.'};
      return {data:result.data.messages.map(m=>({id:m.id,sender:m.sender_id===config.humanId?'human':'agent',text:m.content}))};
    },
    async send(input){
      const path=join(config.stateDir,input.id+'.json');
      const uncertain={error:'Delivery is uncertain. Check the Zulip topic before composing another send. This submission will not be sent again.',uncertain:true};
      if(existsSync(path)){
        try{const previous=JSON.parse(readFileSync(path,'utf8'));if(previous.agent!==input.agent||previous.text!==input.text)return {error:'Submission ID already used for different content.'};return previous.receipt?{data:previous.receipt}:uncertain;}catch{return uncertain;}
      }
      try{const fd=openSync(path,'wx',0o600);try{writeFileSync(fd,JSON.stringify(input));fsyncSync(fd);}finally{closeSync(fd);}const dir=openSync(config.stateDir,'r');try{fsyncSync(dir);}finally{closeSync(dir);}}catch{return {error:'Cannot reserve this submission. No message was sent.'};}
      const result=await request('POST','messages',{type:'stream',to:String(config.streamId),topic:input.agent,content:input.text});
      if(result.error||!Number.isSafeInteger(result.data.id))return uncertain;
      const receipt={...input,mock:false,messageId:result.data.id};
      try{const temp=path+'.completed';const fd=openSync(temp,'w',0o600);try{writeFileSync(fd,JSON.stringify({...input,receipt}));fsyncSync(fd);}finally{closeSync(fd);}renameSync(temp,path);}catch{return uncertain;}
      return {data:receipt};
    }
  }};
}
