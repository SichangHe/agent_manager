import {existsSync} from 'node:fs';
import {readFile,rm,writeFile} from 'node:fs/promises';
import {homedir} from 'node:os';
import {join} from 'node:path';
import {execFileSync} from 'node:child_process';
import {setTimeout as delay} from 'node:timers/promises';
import {connectZulipSSH} from './zulip.mjs';
import {createServer} from './server.mjs';

const directory=join(homedir(),'.config','zulip-voice');
const tokenPath=join(directory,'private','login-token');
let lastError='';
function report(message){
  try{execFileSync('amh',['tell','agent','terminal_viz.md','--task-file','zulip_voice.md',message],{stdio:'ignore',timeout:15000});}
  catch{console.error('Steering notification failed; inspect live activation status.');}
}
while(true){
  if(!existsSync(join(directory,'zuliprc'))){await delay(10000);continue;}
  const result=await connectZulipSSH(join(directory,'zuliprc'),join(directory,'settings.json'));
  if(result.error){
    if(result.error!==lastError){report('Automatic live activation kept mock running: '+result.error);lastError=result.error;}
    await delay(30000);continue;
  }
  const html=await readFile(new URL('./index.html',import.meta.url));
  await rm(tokenPath,{force:true});
  const server=createServer(html,'http://127.0.0.1:8769',result.data,null,true);
  const mockExists=execFileSync('systemctl',['--user','show','zulip-voice-ssh-8769','--property=LoadState','--value'],{encoding:'utf8',timeout:15000}).trim()!=='not-found';
  if(mockExists)execFileSync('systemctl',['--user','stop','zulip-voice-ssh-8769'],{stdio:'ignore',timeout:15000});
  try{
    await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(8769,'127.0.0.1',resolve);});
  }catch(error){
    if(mockExists)execFileSync('systemctl',['--user','start','zulip-voice-ssh-8769'],{stdio:'ignore',timeout:15000});
    report('Live bind failed. Check service status.');
    throw error;
  }
  const notified=join(directory,'activation-notified');
  if(!existsSync(notified)){
    report('Live SSH service activated without login token, as requested. Loopback port8769; Host/Origin checks retained; no test messages sent.');
    await writeFile(notified,'activated\n',{mode:0o600});
  }
  break;
}
