# Zulip cleanup
(authored by agents unless marked 🧑)

- 🧑 “Don't back up.”
- 🧑 “Make sure this cleanup is script-driven and uses no agents in its normal path”
- unattended daily watcher pass with direct API requests
  - no agents, downloads, or backups
  - `zulip.cleanup_days` defaults to 30
  - 0 disables cleanup
  - at most 500 messages and 20 uploads per pass
- messages: configured AMH stream, bot sender, completed task, no open requests
  - human must appear in read receipts
  - missing/private/unavailable receipt preserves message
  - pagination wraps after reaching recent messages
  - final task check and deletion share the task-store lock
  - Zulip automatically deletes last-reference uploads
  - unknown/other-user upload links preserve the message
- uploads: bot uploads with no message references, older than retention period
  - configured bot is dedicated to AMH; other users’ uploads are inaccessible
  - uncertain-send payload references preserved
  - current and legacy API reference/timestamp schemas supported
  - recheck references before deletion
  - persisted upload cursor prevents failures starving later uploads
- bounded message/upload scans have separate 15-second budgets
  - persisted cursor files under local state
  - malformed cursors restart safely
- sources
  - Zulip [read receipts](https://zulip.com/api/get-read-receipts): “IDs of users who have marked the target message as read and whose read status is available to the current user”
  - Zulip [attachments](https://zulip.com/api/get-attachments): “Fetch metadata on files uploaded by the requesting user”
  - Zulip [attachment deletion](https://zulip.com/api/remove-attachment): “automatically deleted once the last message containing a link to them is deleted”
