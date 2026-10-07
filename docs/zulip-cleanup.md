# Zulip cleanup
(authored by agents unless marked 🧑)

- 🧑 “Implement both. Make sane decisions.”
- daily watcher pass; `zulip.cleanup_days` defaults to 30
  - 0 disables cleanup
  - at most 500 messages and 20 uploads per pass
- messages: configured AMH stream, bot sender, completed task, no open requests
  - human must appear in read receipts
  - missing/private/unavailable receipt preserves message
  - pagination wraps after reaching recent messages
  - archive linked bot uploads before message deletion; Zulip automatically deletes last-reference uploads
  - unknown/other-user upload links or unavailable bytes preserve the message
- uploads: bot uploads with no message references, older than retention period
  - configured bot is dedicated to AMH; other users’ uploads are inaccessible
  - uncertain-send payload references preserved
  - current and legacy API reference/timestamp schemas supported
  - archive bytes and metadata, check byte count, recheck references before deletion
  - redirects retained; credentials never follow redirects
  - persisted upload cursor prevents failures starving later uploads
- archive: private files under local state `zulip-archive`
  - message JSON, upload JSON and bytes written and synced before deletion
  - linked-upload metadata stored separately for each message
  - conflicting existing archive stops cleanup
- sources
  - Zulip [read receipts](https://zulip.com/api/get-read-receipts): “IDs of users who have marked the target message as read and whose read status is available to the current user”
  - Zulip [attachments](https://zulip.com/api/get-attachments): “Fetch metadata on files uploaded by the requesting user”
  - Zulip [attachment deletion](https://zulip.com/api/remove-attachment): “automatically deleted once the last message containing a link to them is deleted”
