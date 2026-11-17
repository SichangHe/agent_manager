import {randomBytes,timingSafeEqual} from 'node:crypto';
import {mkdirSync,statSync,openSync,writeFileSync,fsyncSync,closeSync,renameSync} from 'node:fs';
import {dirname} from 'node:path';

/** Loopback login tokens are private, single-use, and replaced before issuing a session. */
export function createSessionAuth(tokenPath){
  const directory=dirname(tokenPath);
  mkdirSync(directory,{recursive:true,mode:0o700});
  const info=statSync(directory);
  if(info.uid!==process.getuid()||(info.mode&0o077))throw new Error('Session directory must be private and owned by this user.');
  let token='';
  const sessions=new Map(),ttlMs=12*60*60*1000;
  function rotate(){
    const next=randomBytes(32).toString('hex');
    const temp=tokenPath+'.'+randomBytes(8).toString('hex');
    const fd=openSync(temp,'wx',0o600);
    try{writeFileSync(fd,next+'\n');fsyncSync(fd);}finally{closeSync(fd);}
    renameSync(temp,tokenPath);
    token=next;
  }
  rotate();
  return {
    accept(value){
      if(typeof value!=='string'||!/^[a-f0-9]{64}$/.test(value)||!timingSafeEqual(Buffer.from(value),Buffer.from(token)))return null;
      rotate();
      const now=Date.now();
      for(const [key,expires] of sessions)if(expires<=now)sessions.delete(key);
      const session=randomBytes(32).toString('hex');
      sessions.set(session,now+ttlMs);
      return 'zulip_voice_session='+session+'; Path=/; HttpOnly; SameSite=Strict; Max-Age='+ttlMs/1000;
    },
    authorize(headers){
      const cookies=(headers.cookie||'').split(';').map(c=>c.trim()).filter(c=>c.startsWith('zulip_voice_session='));
      if(cookies.length!==1)return false;
      const expires=sessions.get(cookies[0].slice('zulip_voice_session='.length));
      return !!expires&&expires>Date.now();
    }
  };
}
