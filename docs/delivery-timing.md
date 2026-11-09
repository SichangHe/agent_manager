# AMH delivery timing
(authored by agents unless marked 🧑)

scope
- 🧑 "Coordinate with the relevant agents to relentlessly monitor and minimize E2E latency"
- watcher deliveries from stored Zulip messages carry a decimal message ID
- routing, holds, quota checks, fallback and retry behavior stay intact

events
- `human mail received`: recorded after durable queueing
- `zulip pickup`: original provider `source_timestamp_s`, local `poll_started_at` and `zulip_message_id`
    - recorded only for a newly queued message ID
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
- watcher diagnostic log reports phases taking at least one second
    - archival, Git, IMAP, Zulip intake, request notifications, message delivery, compression, outbox, scheduled jobs and cleanup
    - durations expose serial work that delays the next nominal five-second poll
- slow request-state phases also log actual Rust subphase timings
    - `acquire_ms`: lock-file setup and lock acquisition, including waiting
    - `import_ms`: lock-owner metadata, database setup/migration and Markdown edit import
    - `export_ms`: repair/regeneration of task and event Markdown exports
    - `dependencies_ms`: request synchronization and dependency release, while retaining the lock
    - failed lock preparation retains existing errors without a timing breakdown
- IMAP intake owns its connections on one separate thread
    - measured serial intake took 25.5 seconds before Zulip pickup
    - existing receipt locks, deduplication, retries and database wakeups are retained
    - only the main watcher delivers queued messages
- queued messages are delivered after request-state updates and before request notifications
    - a live pickup waited 5.14 seconds behind a 5.05-second notification phase
    - notification retries retain the same durable queue and delivery checks

verification
- loopback HTTP test checks request header, response correlation and content exclusion
- existing lifecycle tests cover task bookkeeping
