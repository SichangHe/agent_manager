# AMH delivery timing
(authored by agents unless marked 🧑)

scope
- 🧑 "Coordinate with the relevant agents to relentlessly monitor and minimize E2E latency"
- watcher deliveries from stored Zulip messages carry a decimal message ID
- routing, holds, quota checks, fallback and retry behavior stay intact

events
- `human mail received`: recorded after durable queueing
- `delivery started`: before session checks and message preparation
    - `zulip_message_id`, `to`, `block`
- `dispatch started`: before HTTP send
    - `zulip_message_id`, `path`, `started_at`
    - path identifies the actual session, including a moved or fallback session
- `dispatch finished`: after HTTP response body or transport failure
    - same fields plus `elapsed_ms`, `request_id`, `http_status`, `transport_ok`
    - transport success does not imply successful queueing
- existing `message delivered`, `message rerouted`, `message failed` events finish delivery
    - add `zulip_message_id` and `elapsed_ms`
- `delivery paused`: session checks prevent dispatch
- unreadable stored messages retain their existing diagnostic log
- events use existing SQLite event timestamps; elapsed durations use a monotonic clock

correlation
- request header `X-Amh-Zulip-Message-Id`: decimal ID, at most 20 digits
- response header `X-Omnigent-Timing-Id`: stored as `request_id`
- retries share the Zulip ID; each dispatch has its own start time and server request ID
- message contents and authorization headers are excluded from new dispatch events
- event write failure does not prevent delivery

verification
- loopback HTTP test checks request header, response correlation and content exclusion
- existing lifecycle tests cover task bookkeeping
