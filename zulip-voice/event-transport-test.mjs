/** Task reads update from events without repeated API fetches. */
import assert from 'node:assert/strict';
import {createBrowserZulip} from './browser-zulip.mjs';
let calls=0;
const adapter=createBrowserZulip({site:'https://shagent.zulipchat.com',humanId:1,botId:2,streamId:3,email:'test',key:'synthetic'},{storage:{},fetch:async url=>{calls++;return Response.json({result:'success',...(String(url).includes('/topics')?{topics:[{name:'agent',max_id:10}]}:{messages:[]})});}});
async function read(path){return (await adapter.fetch(path,{})).json();}
await read('/api/conversations');await read('/api/messages?agent=agent');await read('/api/unread?agent=agent');await read('/api/events');
const initial=calls;
for(let i=0;i<20;i++){await read('/api/events?after=0');await read('/api/unread?agent=agent');}
assert.equal(calls,initial);
adapter.ingestMessage({id:11,type:'stream',stream_id:3,subject:'agent',sender_id:2,content:'A new reply.',flags:[]});
assert.equal((await read('/api/events?after=0')).events[0].id,11);
assert.equal((await read('/api/messages?agent=agent'))[0].id,11);
assert.equal((await read('/api/unread?agent=agent'))[0].id,11);
adapter.ingestFlags({flag:'read',op:'add',messages:[11]});assert.deepEqual(await read('/api/unread?agent=agent'),[]);assert.equal(calls,initial);
