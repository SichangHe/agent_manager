import {readFile} from 'node:fs/promises';
import {homedir} from 'node:os';
import {join} from 'node:path';
import {createServer} from './server.mjs';
import {connectZulipSSH} from './zulip.mjs';
import {createSessionAuth} from './session.mjs';

// 🧑 "Make the real thing. We test in prod"
const directory=join(homedir(),'.config','zulip-voice');
const result=await connectZulipSSH(join(directory,'zuliprc'),join(directory,'settings.json'));
if(result.error){console.error(result.error);process.exit(1);}
const auth=createSessionAuth(join(directory,'private','login-token'));
const html=await readFile(new URL('./index.html',import.meta.url));
createServer(html,'http://127.0.0.1:8769',result.data,auth).listen(8769,'127.0.0.1',()=>console.log('Live voice listening on loopback port 8769.'));
