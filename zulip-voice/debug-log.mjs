/** 🧑 "You need to add in debugging support." Ring-buffer log of requests, connection, audio and errors; uploaded each minute to the muted voice-relay topic "debug". */
const lines=[];let seq=0;
const short=url=>{try{const u=new URL(url,location.href);return (u.origin===location.origin?'':u.host)+u.pathname.replace(/\/[0-9a-f]{2}\/[^/]+\/[^/]+$/,'/…').slice(0,80);}catch{return String(url).slice(0,80);}};
export function log(...parts){const now=new Date();lines.push(now.toISOString().slice(11,23)+' '+parts.join(' '));if(lines.length>2000)lines.splice(0,lines.length-2000);}
export function wrapFetch(fetcher,tag){return async(input,options={})=>{const id=++seq,start=performance.now(),method=options.method||'GET',path=short(typeof input==='string'?input:input.url);try{const response=await fetcher(input,options);const ms=Math.round(performance.now()-start);if(!response.ok||ms>3000||tag==='net')log(tag,id,method,path,response.status,ms+'ms');return response;}catch(error){log(tag,id,method,path,'ERR',error?.name+': '+error?.message,Math.round(performance.now()-start)+'ms');throw error;}};}
export function installDebug({post}){
  for(const name of ['online','offline'])addEventListener(name,()=>log('net',name));
  document.addEventListener('visibilitychange',()=>log('page',document.visibilityState));
  addEventListener('error',e=>log('js-error',e.message,(e.filename||'').split('/').pop()+':'+e.lineno));
  addEventListener('unhandledrejection',e=>log('js-reject',e.reason?.name||'',e.reason?.message||String(e.reason)));
  for(const name of ['play','playing','pause','ended','waiting','stalled','error','abort','emptied'])document.addEventListener(name,e=>{if(e.target instanceof HTMLMediaElement)log('audio',name,e.target.error?'code'+e.target.error.code:'',e.target.readyState,e.target.networkState);},true);
  const status=document.getElementById('status');if(status)new MutationObserver(()=>log('status',status.textContent.slice(0,200))).observe(status,{childList:true,characterData:true,subtree:true});
  log('start',navigator.userAgent);
  let sent=0,uploading=false;
  setInterval(async()=>{if(uploading||sent>=lines.length)return;uploading=true;const end=lines.length,body=lines.slice(sent,end).join('\n').slice(-9000);try{if(await post('```\n'+body+'\n```'))sent=end;}catch{}finally{uploading=false;}},60000);
}
