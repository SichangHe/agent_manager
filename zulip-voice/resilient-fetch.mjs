// 🧑 "The client also needs to be able to handle connection drops in intermittently"
/** Returns `{response}` or `{error, aborted, retryable}`. POST retries require a stable server-deduplicated ID; bodies must be replayable strings or Blobs, never streams. */
export async function resilientFetch(url, init={}, {retries=3,baseDelayMs=500,maxDelayMs=8000,timeoutMs=20000,onState=()=>{}}={}) {
  const signal=init.signal;
  if(!Number.isInteger(retries)||retries<0||retries>10||![baseDelayMs,maxDelayMs,timeoutMs].every(value=>Number.isFinite(value)&&value>0))return {error:'Invalid retry settings.',aborted:false,retryable:false};
  if(init.body&&typeof init.body.getReader==='function')return {error:'Streaming request bodies cannot be retried.',aborted:false,retryable:false};
  const aborted=()=>({error:'Request cancelled.',aborted:true,retryable:false});
  for(let attempt=0;attempt<=retries;attempt++){
    if(signal?.aborted)return aborted();
    const controller=new AbortController();
    const abort=()=>controller.abort();
    signal?.addEventListener('abort',abort,{once:true});
    const timer=setTimeout(abort,timeoutMs);
    let response,failure;
    try{const incoming=await fetch(url,{...init,signal:controller.signal});const body=await incoming.arrayBuffer();response=new Response(body,{status:incoming.status,statusText:incoming.statusText,headers:incoming.headers});}
    catch(error){failure=error;}
    finally{clearTimeout(timer);signal?.removeEventListener('abort',abort);}
    if(signal?.aborted){await response?.body?.cancel().catch(()=>{});return aborted();}
    const transient=!response||[502,503,504].includes(response.status);
    if(!transient){onState({state:'connected',attempt});return {response};}
    if(attempt===retries){onState({state:response?'server-error':'offline',attempt});return response?{response}:{error:`${init.method||'GET'} ${String(url).split('?')[0]}: ${failure?.name||'Network error'}: ${failure?.message||'Request timed out'}. Saved item retained.`,aborted:false,retryable:true};}
    await response?.body?.cancel().catch(()=>{});
    const delayMs=Math.min(maxDelayMs,baseDelayMs*2**attempt);
    onState({state:'retrying',attempt:attempt+1,delayMs});
    if(!await wait(delayMs,signal))return aborted();
  }
}

function wait(delayMs,signal){
  return new Promise(resolve=>{
    if(signal?.aborted){resolve(false);return;}
    const finish=value=>{clearTimeout(timer);signal?.removeEventListener('abort',abort);resolve(value);};
    const abort=()=>finish(false),timer=setTimeout(()=>finish(true),delayMs);
    signal?.addEventListener('abort',abort,{once:true});
  });
}

/** Scope must identify the account and live/mock transport, preventing queued items crossing accounts. No queue method sends requests. */
export async function openDurableQueue(scope){
  if(typeof scope!=='string'||!scope.trim())return {error:'A queue account and transport scope is required.'};
  if(!globalThis.indexedDB)return {error:'Durable browser storage is unavailable. Keep this page open; nothing is saved offline.'};
  const opened=await new Promise(resolve=>{
    let request,settled=false;
    const finish=result=>{if(!settled){settled=true;resolve(result);}else result.db?.close();};
    try{request=indexedDB.open('zulip-voice-outbox:'+scope,1);}catch{return finish({error:'Cannot open offline storage.'});}
    request.onupgradeneeded=()=>request.result.createObjectStore('items',{keyPath:'id'});
    request.onsuccess=()=>finish({db:request.result});
    request.onerror=()=>finish({error:'Cannot open offline storage: '+(request.error?.name||'unknown error')});
    request.onblocked=()=>finish({error:'Offline storage is blocked by another tab. Close older tabs and retry.'});
  });
  if(opened.error)return opened;
  const db=opened.db;
  db.onversionchange=()=>db.close();
  function transaction(mode,action){
    return new Promise(resolve=>{
      let tx,request;
      try{tx=db.transaction('items',mode);request=action(tx.objectStore('items'));}
      catch{return resolve({error:'Offline storage is unavailable; the item has not been changed.'});}
      tx.oncomplete=()=>resolve({data:request.result});
      tx.onabort=()=>resolve({error:'Offline storage failed: '+(tx.error?.name||request.error?.name||'transaction aborted')});
      tx.onerror=()=>{};
    });
  }
  return {data:{
    /** Adds once; duplicate IDs fail instead of overwriting a previously queued recipient or payload. Audio metadata may include draft prefix and compose mode. */
    async add(item){
      if(!item||typeof item.id!=='string'||!item.id||typeof item.agent!=='string'||!item.agent||!['send','audio'].includes(item.kind))return {error:'Offline items require an ID, recipient agent and send/audio kind.'};
      if(item.kind==='send'&&(item.payload?.id!==item.id||item.payload?.agent!==item.agent||typeof item.payload?.text!=='string'||!item.payload.text.trim()))return {error:'Send payload must contain the same ID and recipient, and nonempty text.'};
      if(item.kind==='audio'&&(!(item.blob instanceof Blob)||!item.blob.size))return {error:'Audio items require a nonempty recording Blob.'};
      return transaction('readwrite',store=>store.add({...item,createdAtMs:Date.now()}));
    },
    async list(){const result=await transaction('readonly',store=>store.getAll());if(result.data)result.data.sort((a,b)=>a.createdAtMs-b.createdAtMs||a.id.localeCompare(b.id));return result;},
    get(id){return transaction('readonly',store=>store.get(id));},
    /** Remove only after a confirmed receipt, successful transcription handed to the draft, or explicit user discard. */
    remove(id){return transaction('readwrite',store=>store.delete(id));},
    close(){db.close();}
  }};
}
