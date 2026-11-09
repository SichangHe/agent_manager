# timeline.py design

purpose: one server-clock timeline per Zulip message id; per-segment durations; median/p90 summary
- run: `python3 -I timeline.py <id> | --last N | --summary N [--topic T] [--human-id ID]`
- read-only; stdlib only
- topic default `human_mgr`; AMH task file = `<topic>.md`

sources, joined on the Zulip message id
- zulip api (zuliprc): 1 call for `--last/--summary` (newest 40N msgs in topic); `<id>` adds 1 call to learn the topic
  - `timestamp` is whole seconds: segments touching it have ±1 s error
  - round trip = human msg + first non-human msg after it (before the next human msg); human = `--human-id`
- `amh log <topic>.md -n 5000`: events whose data mentions `zulip-<id>.txt`
  - `message queued`, `human mail received`, `message delivered` (gives session id)
- omnigent server logs `latency {...}` JSON (grep by session id, only files modified since earliest msg)
  - exact join if a record has `zulip_message_id == id`; else inferred: first `POST events` arrival within -2..+10 s of delivery
  - shown as note (arrival, ack); not a segment, since amh logs `delivered` after the server acks
  - rotated logs lose old sessions: server note missing for old trips
- relay journal: `tts id=proactive_<replyid>_<n> ... work=Nms`; audio ready = last chunk log time
- phone `client.log`: phone clock never used; only relative span between lines for the reply (`bundle active_<reply>_`, `audio  start <reply>:`) and `send id=<id>`

segments
- human_post>amh_queued, amh_queued>received, amh_received>delivered, delivered>reply_posted, reply>tts_ready, total
- `delivered>reply_posted` = agent session time (model + tools + posting), includes server dispatch (~0.1 s)
- summary flags worst segment by median and by p90

limits
- reply = first non-human message; may not answer that message (agent can batch or reply later), inflating think time
- trips with no reply, or reply not voiced, lack later marks and are skipped per segment
