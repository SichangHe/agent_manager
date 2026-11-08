import {writeFileSync} from 'node:fs'; import {spawnSync} from 'node:child_process';
const tts = async t => Buffer.from(await (await fetch('http://127.0.0.1:8770/tts',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text:t})})).arrayBuffer());
const ff = (wav, af, out) => { const r = spawnSync('ffmpeg',['-nostdin','-v','error','-i','pipe:0','-af',af,'-ar','16000','-ac','1','-y',out],{input:wav}); };
const pos = {pause:'Pause.', pause2:'Pause, pause.', pause_in:'Pause. Yes, deploy it to staging.', over:'Over.', switch:'Switch to terminal viz.'};
for (const [k,t] of Object.entries(pos)) { const w = await tts(t); ff(w,'anull',`clips/pos_${k}.wav`); ff(w,'atempo=1.5',`clips/pos_${k}_fast.wav`); ff(w,'volume=0.15',`clips/pos_${k}_quiet.wav`); ff(w,'volume=0.3,highpass=f=300,lowpass=f=3000,aecho=0.8:0.6:40:0.4',`clips/pos_${k}_room.wav`); }
const neg = ['The weather is quite nice today and I think we should go outside.','We paused the experiment because the quota ran out.','Please pass me the salt.','I think the results are over there on the table.','Switch the lights off when you leave.','Another thing I am not happy with is how we switch agents.','Cause and effect are not always clear in these papers.','Pose the question to the manager tomorrow.','Paws and claws were all over the floor.'];
for (const [i,t] of neg.entries()) { const w = await tts(t); ff(w,'anull',`clips/neg_${i}.wav`); }
const noise = {white:'anoisesrc=color=white:amplitude=0.1',pink:'anoisesrc=color=pink:amplitude=0.2',brown:'anoisesrc=color=brown:amplitude=0.3'};
for (const [k,src] of Object.entries(noise)) spawnSync('ffmpeg',['-nostdin','-v','error','-f','lavfi','-i',src,'-t','5','-ar','16000','-ac','1','-y',`clips/noise_${k}.wav`]);
