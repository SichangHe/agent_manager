/** Limit this page's Zulip API starts, leaving budget for other clients.
 * 🧑 "Why is the client side so laggy and glitchy?" Debug log showed calls taking ~0.27 s on the network but ~6 s waiting here behind cleanup deletes.
 * So: a larger burst, and cleanup (DELETE, attachment listing) runs only when nothing user-facing is waiting, keeping a reserve. */
export function budgetRequests(network,{perMinute=120,burst=30,reserve=10}={}){
  let tokens=burst,lastMs=Date.now(),timer=null;const high=[],low=[];
  const refill=()=>{const now=Date.now();tokens=Math.min(burst,tokens+(now-lastMs)*perMinute/60000);lastMs=now;};
  function pump(){
    timer=null;refill();
    while(high.length&&tokens>=1){tokens--;high.shift().go();}
    while(!high.length&&low.length&&tokens>=reserve+1){tokens--;low.shift().go();}
    if(high.length||low.length)timer=setTimeout(pump,Math.ceil(60000/perMinute));
  }
  return (input,options={})=>{
    const url=new URL(typeof input==='string'?input:input.url),method=(options.method||'GET').toUpperCase();
    if(url.origin!=='https://shagent.zulipchat.com'||!url.pathname.startsWith('/api/v1/'))return network(input,options);
    const cleanup=method==='DELETE'||(method==='GET'&&url.pathname.startsWith('/api/v1/attachments'));
    return new Promise((resolve,reject)=>{
      const entry={go:()=>{if(options.signal?.aborted)return reject(new DOMException('Cancelled','AbortError'));network(input,options).then(resolve,reject);}};
      (cleanup?low:high).push(entry);if(!timer)pump();
    });
  };
}
