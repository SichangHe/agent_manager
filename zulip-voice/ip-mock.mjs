import {readFile} from 'node:fs/promises';
import {execFileSync} from 'node:child_process';
import {createServer} from './server.mjs';

const address=execFileSync('tailscale',['ip','-4'],{encoding:'utf8',timeout:5000}).trim();
if(!/^100\.(?:6[4-9]|[7-9]\d|1[01]\d|12[0-7])\.\d{1,3}\.\d{1,3}$/.test(address))throw new Error('Expected a Tailscale IPv4 address');
const port=8768;
const html=await readFile(new URL('./index.html',import.meta.url));
const server=createServer(html,`http://${address}:${port}`);
server.on('request',(_req,res)=>res.on('finish',()=>console.log(`Mock HTTP response ${res.statusCode}`)));
server.listen(port,address,()=>console.log(`Mock only: http://${address}:${port}`));
