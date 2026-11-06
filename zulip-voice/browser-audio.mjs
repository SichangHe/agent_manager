// 🧑 "Use the server to do STT and TTS instead of the client, i.e., send audio around"
import {resilientFetch} from './resilient-fetch.mjs';
export function createAudio({status,transcribe}){
  const player=new Audio();let generation=0,controller=null,objectURL=null,recorder=null,stream=null,timer=null,unlocked=false,current=null;
  let rate=1;try{rate=Number(localStorage.getItem('voice-speed'))||1;}catch{}
  function setRate(value){rate=Math.max(0.5,Math.min(3,value));player.playbackRate=rate;try{localStorage.setItem('voice-speed',String(rate));}catch{}}
  function unlock(){
    if(unlocked||player.src)return;unlocked=true;player.onended=null;player.onerror=null;
    const buffer=new ArrayBuffer(46),view=new DataView(buffer);
    for(const [offset,text] of [[0,'RIFF'],[8,'WAVE'],[12,'fmt '],[36,'data']])for(let i=0;i<text.length;i++)view.setUint8(offset+i,text.charCodeAt(i));
    view.setUint32(4,38,true);view.setUint32(16,16,true);view.setUint16(20,1,true);view.setUint16(22,1,true);view.setUint32(24,24000,true);view.setUint32(28,48000,true);view.setUint16(32,2,true);view.setUint16(34,16,true);view.setUint32(40,2,true);
    objectURL=URL.createObjectURL(new Blob([buffer],{type:'audio/wav'}));player.src=objectURL;player.play().catch(()=>{});
  }
  function stop(){generation++;controller?.abort();player.pause();player.removeAttribute('src');current=null;if(objectURL)URL.revokeObjectURL(objectURL);objectURL=null;clearTimeout(timer);if(recorder?.state==='recording')recorder.stop();stream?.getTracks().forEach(track=>track.stop());}
  async function speak(text,done,failed,started){
    const token=generation;controller=new AbortController();
    try{
      const result=await resilientFetch('/api/tts',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text}),signal:controller.signal},{timeoutMs:120000,onState:({state})=>{if(state!=='connected')status('Audio connection '+state+'…');}});
      if(result.error)throw new Error(result.error);const response=result.response;
      if(!response.ok)throw new Error((await response.json()).error);
      const blob=await response.blob();if(token!==generation)return;
      current={blob,done,failed,text};
      if(objectURL)URL.revokeObjectURL(objectURL);objectURL=URL.createObjectURL(blob);player.src=objectURL;
      player.playbackRate=rate;
      player.onloadedmetadata=null;
      player.onended=()=>{if(token===generation){current=null;done?.();}};
      player.onerror=()=>{if(token===generation){status('Audio playback failed. Tap a sentence to retry.');failed?.();}};
      await player.play();
      if(token===generation)started?.();
    }catch(error){if(token!==generation)return;status('Audio: '+error.message);failed?.();}
  }
  async function record(done,failed,metadata){
    const token=generation;
    try{
      const acquired=await navigator.mediaDevices.getUserMedia({audio:true});
      if(token!==generation){acquired.getTracks().forEach(track=>track.stop());return;}stream=acquired;
      const chunks=[];recorder=new MediaRecorder(stream);
      const mimeType=recorder.mimeType;
      recorder.ondataavailable=event=>{if(event.data.size)chunks.push(event.data);};
      recorder.onstop=async()=>{
        clearTimeout(timer);acquired.getTracks().forEach(track=>track.stop());
        status('Transcribing…');controller=new AbortController();
        try{const result=await transcribe(new Blob(chunks,{type:mimeType}),token===generation?controller.signal:AbortSignal.abort(),metadata);if(token!==generation)return;done(result);}
        catch(error){if(token===generation){status('Transcription: '+error.message);failed();}}
      };
      recorder.start();timer=setTimeout(()=>finish(),115000);status('Listening. Tap Finish dictating when ready.');
    }catch(error){if(token===generation){status('Microphone: '+error.message);failed();}}
  }
  function finish(){if(recorder?.state==='recording')recorder.stop();}
  function pause(){generation++;controller?.abort();player.pause();}
  return {speak,record,finish,stop,unlock,setRate,pause,elapsed:()=>current?player.currentTime:0,getRate:()=>rate};
}
