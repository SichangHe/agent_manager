/** Marks client-log entries up to a time as analyzed; the relay prunes them within the hour. Usage: node mark-analyzed.mjs [ISO_TIME] (default: now). */
import {writeFile} from 'node:fs/promises';
const state = process.env.VOICE_RELAY_STATE || process.env.HOME + '/.local/state/zulip-voice/relay';
const when = new Date(process.argv[2] ?? Date.now());
if (Number.isNaN(+when)) throw Error('Bad time');
await writeFile(state + '/client.log.analyzed', when.toISOString(), {mode: 0o600});
console.log(`entries up to ${when.toISOString()} will be pruned`);
