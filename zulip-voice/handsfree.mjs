/** Normalize recognized command words without changing the user's draft. */
const words=text=>text.toLowerCase().replace(/[^\p{L}\p{N}]+/gu,' ').trim();

// 🧑 "listen constantly and identify when I Say pause" ... "At the end of my response I can say Resume autoplay than the response will be sent out"
export function createHandsfree({onPause,onReply,onState=()=>{},onDraft=()=>{},onError=()=>{},onRecording=null,onTranscribed=async()=>{},getContext=()=>({})}){
  let state='off',generation=0,captureGeneration=0,stream=null,recorder=null,context=null,analyser=null,timer=null,controller=null;
  let draft='',queue=[],processing=false;
  const active=()=>state==='listening'||state==='responding';
  const setState=value=>{state=value;onState(value,{text:draft});};
  function stopCapture(){
    captureGeneration++;clearInterval(timer);timer=null;
    if(recorder?.state==='recording')recorder.stop();
    recorder=null;queue=[];
  }
  function stop(){
    generation++;stopCapture();controller?.abort();controller=null;
    stream?.getTracks().forEach(track=>track.stop());stream=null;
    context?.close().catch(()=>{});context=null;analyser=null;
    document.removeEventListener('visibilitychange',visibility);
    setState('off');return draft;
  }
  function fail(error){stop();onError(error instanceof Error?error.message:String(error));}
  function visibility(){if(document.hidden){stop();onError('Hands-free stopped because the page is hidden. Your draft is preserved.');}}
  async function accept(text,item,token,consume,id){
    if(token!==generation||item.epoch!==captureGeneration||!active())return false;
    const normalized=words(text);
    if(!normalized)return true;
    if(state==='listening'){
      if(normalized!=='pause')return true;
      stopCapture();setState('pausing');
      await onPause();
      if(token!==generation)return false;
      draft='';onDraft(draft);setState('responding');capture(token);return true;
    }
    draft=[draft,text.trim()].filter(Boolean).join(' ');
    const resume=/\bresume[\s.!?,;:]+auto[\s-]*play\b[\s.!?,;:]*$/i.exec(draft);
    if(!resume){await onDraft(draft,id);await consume();if(token===generation)onState(state,{text:draft});return true;}
    draft=draft.slice(0,resume.index).trim();await onDraft(draft,id);
    await consume();
    if(token!==generation)return true;
    stopCapture();setState('sending');
    const result=await onReply(draft);
    if(token!==generation)return true;
    if(result!==true){fail('The reply was not confirmed sent. Your draft is preserved; review it before retrying.');return true;}
    draft='';onDraft(draft);setState('listening');capture(token);
    return true;
  }
  async function drain(token){
    if(processing)return;
    processing=true;
    try{
      while(queue.length&&token===generation){
        const item=queue.shift();
        const saved=await item.saved;
        if(saved.error)throw saved.error;
        if(token!==generation)return;
        if(item.epoch!==captureGeneration)continue;
        const requestController=new AbortController();controller=requestController;
        const fetched=await resilientFetch('/api/stt',{method:'POST',headers:{'Content-Type':item.blob.type||'application/octet-stream'},body:item.blob,signal:requestController.signal},{retries:0,timeoutMs:10000});
        if(fetched.error)throw new Error(fetched.error);
        const result=await fetched.response.json();
        if(!fetched.response.ok)throw new Error(result.error||'Transcription failed.');
        if(typeof result.text!=='string')throw new Error('Invalid transcription response.');
        let consumed=false;
        const consume=async()=>{if(consumed)return;if(saved.id)await onTranscribed(saved.id);consumed=true;};
        if(await accept(result.text,item,token,consume,saved.id))await consume();
      }
    }catch(error){if(token===generation)fail(error);}
    finally{processing=false;if(queue.length&&active())drain(generation);}
  }
  /** Each recording is stopped and finalized before upload; timeslice chunks are not independent MP4/WebM files. */
  function capture(token){
    if(token!==generation||!active()||!stream)return;
    const epoch=captureGeneration;
    const mimeType=['audio/mp4','audio/webm;codecs=opus','audio/webm'].find(type=>MediaRecorder.isTypeSupported(type));
    const current=new MediaRecorder(stream,mimeType?{mimeType}:undefined);recorder=current;
    const chunks=[],samples=new Float32Array(analyser.fftSize),recordingState=state,recordingContext={...getContext()};
    const startedMs=performance.now();let lastSpeechMs=startedMs,heardSpeech=false;
    current.ondataavailable=event=>{if(event.data.size)chunks.push(event.data);};
    current.onerror=()=>{if(token===generation)fail('Microphone recording failed. Your draft is preserved.');};
    current.onstop=()=>{
      let item=null;
      if(heardSpeech&&chunks.length){
        const metadata={state:recordingState,draft,context:recordingContext};
        const blob=new Blob(chunks,{type:current.mimeType});
        const saved=Promise.resolve().then(async()=>{
          if(!onRecording)return {id:null};
          const id=await onRecording(blob,metadata);
          if(typeof id!=='string'||!id)throw new Error('Recording could not be saved.');
          return {id};
        }).catch(error=>{if(token===generation)fail(error);else onError('Recording could not be saved.');return {error};});
        item={blob,epoch,saved};
      }
      if(token!==generation||epoch!==captureGeneration||!active())return;
      clearInterval(timer);timer=null;
      if(item){
        if(queue.length>=4){fail('Transcription cannot keep up. Hands-free stopped; saved recordings remain available for review.');return;}
        queue.push(item);
      }
      capture(token);drain(token);
    };
    current.start();
    timer=setInterval(()=>{
      if(token!==generation||epoch!==captureGeneration||current.state!=='recording')return;
      analyser.getFloatTimeDomainData(samples);
      const energy=Math.sqrt(samples.reduce((sum,value)=>sum+value*value,0)/samples.length);
      const nowMs=performance.now();
      if(energy>0.012){heardSpeech=true;lastSpeechMs=nowMs;}
      if((heardSpeech&&nowMs-lastSpeechMs>=600&&nowMs-startedMs>=700)||nowMs-startedMs>=6000){clearInterval(timer);timer=null;current.stop();}
    },100);
  }
  async function start(){
    if(state!=='off')return true;
    if(document.hidden){onError('Keep this page visible for hands-free listening.');return false;}
    if(!window.isSecureContext||!navigator.mediaDevices?.getUserMedia||!window.MediaRecorder){onError('Microphone recording requires HTTPS or localhost and a supported browser.');return false;}
    const token=++generation;setState('starting');
    document.addEventListener('visibilitychange',visibility);
    try{
      const AudioContext=window.AudioContext||window.webkitAudioContext;
      context=new AudioContext();await context.resume();
      if(token!==generation)return false;
      const acquired=await navigator.mediaDevices.getUserMedia({audio:{echoCancellation:true,noiseSuppression:true,autoGainControl:true}});
      if(token!==generation){acquired.getTracks().forEach(track=>track.stop());return false;}
      stream=acquired;analyser=context.createAnalyser();analyser.fftSize=1024;
      context.createMediaStreamSource(stream).connect(analyser);
      for(const track of stream.getAudioTracks())track.addEventListener('ended',()=>{if(token===generation)fail('Microphone disconnected. Hands-free stopped.');});
      setState('listening');capture(token);return true;
    }catch(error){if(token===generation)fail(error);return false;}
  }
  return {start,stop,get state(){return state;},get draft(){return draft;}};
}
import {resilientFetch} from './resilient-fetch.mjs';
