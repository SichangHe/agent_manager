/** Unpublished static mode: credentials remain on this browser origin. */
import {createBrowserZulip} from './browser-zulip.mjs';
import {createBrowserRelay} from './browser-relay.mjs';
import {budgetRequests} from './request-budget.mjs';
import {wrapFetch,installDebug,log} from './debug-log.mjs';
const rawFetch=window.fetch.bind(window),nativeFetch=wrapFetch(budgetRequests(wrapFetch(rawFetch,'net')),'api'),storageKey='zulip-voice-account';
window.voiceBrowserOnly=true;
let config;try{config=JSON.parse(localStorage.getItem(storageKey));}catch{}
if(config&&(config.site!=='https://shagent.zulipchat.com'||config.humanId!==1212497)){localStorage.removeItem(storageKey);config=null;}
async function login(previous){
  const dialog=document.createElement('dialog');
  dialog.innerHTML='<form><h2>Voice</h2><p>A personal client. Paste your zuliprc once. Your key stays in this browser storage.</p><label>Account configuration<textarea name="zuliprc" required autocomplete="off" spellcheck="false"></textarea></label><p role="status"></p><button>Connect</button></form>';
  document.body.append(dialog);dialog.showModal();const form=dialog.querySelector('form');
  for(const [key,value] of Object.entries(previous||{site:'https://shagent.zulipchat.com',streamId:642517,botId:1212502}))if(form.elements.namedItem(key))form.elements.namedItem(key).value=value;
  return new Promise(resolve=>{form.onsubmit=async event=>{event.preventDefault();const values={};let section='';for(const line of form.elements.zuliprc.value.split(/\r?\n/)){const text=line.trim();if(text.startsWith('[')){section=text;continue;}const match=text.match(/^(site|email|key)\s*=\s*(.+)$/);if(section==='[api]'&&match)values[match[1]]=match[2].trim();}try{const site=new URL(values.site);if(site.origin!=='https://shagent.zulipchat.com')throw new Error('This personal client uses shagent.zulipchat.com.');const result=await nativeFetch(site.origin+'/api/v1/users/me',{headers:{Authorization:'Basic '+btoa(values.email+':'+values.key)},signal:AbortSignal.timeout(15000)});const me=await result.json();if(!result.ok||me.is_bot!==false||me.user_id!==1212497)throw new Error('Use the configured human account.');const next={...values,site:site.origin,humanId:me.user_id,streamId:642517,botId:1212502};localStorage.setItem(storageKey,JSON.stringify(next));dialog.remove();resolve(next);}catch(error){form.querySelector('[role=status]').textContent=error.message;}};});
}
if(!config)config=await login();
let adapter=createBrowserZulip(config,{fetch:nativeFetch,storage:localStorage});
if((await adapter.verify()).error){config=await login(config);adapter=createBrowserZulip(config,{fetch:nativeFetch,storage:localStorage});}
config.relayStreamId=652749;
const subscriptions=await adapter.request('GET','users/me/subscriptions');
const privateChannel=subscriptions.data?.subscriptions?.find(s=>s.stream_id===config.relayStreamId&&s.invite_only&&s.name==='voice-relay');
const muted=await adapter.request('POST','users/me/subscriptions/properties',{subscription_data:JSON.stringify(['push_notifications','desktop_notifications','audible_notifications','email_notifications','wildcard_mentions_notify'].map(property=>({stream_id:config.relayStreamId,property,value:false})))});
const relayReady=privateChannel&&!muted.error;
if(!relayReady){const notice=document.createElement('p');notice.setAttribute('role','alert');notice.textContent='Voice is disabled until its private channel and muted notifications can be verified. Reload to retry.';document.querySelector('header').after(notice);}
const relay=createBrowserRelay(adapter,nativeFetch);
const account=document.createElement('button');account.textContent='Sign out…';account.onclick=()=>{if(confirm('Sign out of this account?')){localStorage.removeItem(storageKey);location.reload();}};document.querySelector('header').append(account);
window.fetch=wrapFetch(async(input,options={})=>{
  const path=typeof input==='string'?input:new URL(input.url).pathname;
  if(path==='/api/audio-ack'){await relay.release(JSON.parse(options.body).id);return new Response('{}',{headers:{'Content-Type':'application/json'}});}
  if(path.startsWith('/api/tts')||path.startsWith('/api/stt'))return relayReady?relay.fetch(path,options):Response.json({error:'Private voice channel not verified. Reload to retry.'},{status:503});
  if(path.startsWith('/api/'))return adapter.fetch(path,options);
  return nativeFetch(input,options);
},'app');
installDebug({post:async content=>{const r=await rawFetch(config.site+'/api/v1/messages',{method:'POST',headers:{Authorization:'Basic '+btoa(config.email+':'+config.key)},body:new URLSearchParams({type:'stream',to:String(config.relayStreamId),topic:'debug',content}),signal:AbortSignal.timeout(15000)});return r.ok;}});
log('relay',relayReady?'ready':'NOT ready');
window.startVoicePreload=callback=>relayReady?relay.start(callback):Promise.resolve();
