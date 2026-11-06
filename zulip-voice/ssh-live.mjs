import {readFile,rm} from 'node:fs/promises';
import {homedir} from 'node:os';
import {join} from 'node:path';
import {createServer} from './server.mjs';
import {connectZulipSSH} from './zulip.mjs';

// 🧑 "Make the real thing. We test in prod"
const directory=join(homedir(),'.config','zulip-voice');
const result=await connectZulipSSH(join(directory,'zuliprc'),join(directory,'settings.json'));
if(result.error){console.error(result.error);process.exit(1);}
await rm(join(directory,'private','login-token'),{force:true});
const html=await readFile(new URL('./index.html',import.meta.url));
createServer(html,'http://127.0.0.1:8769',result.data,null,true).listen(8769,'127.0.0.1',()=>console.log('Live voice listening on loopback port 8769.'));
