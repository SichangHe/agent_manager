# mail replacements
(authored by agents unless marked 🧑)

- successful `tell human --replaces` removes superseded local outbox files before checking unread Gmail mail
  - Message-ID matching accepts surrounding angle brackets and whitespace
  - missing local files are already replaced
- failed sends keep earlier messages
  - queued replacement stores IDs in adjacent JSON metadata
  - watcher removes superseded queued files only after replacement delivery succeeds
  - replacement cleanup failure is reported, preserving remaining files
- unread Gmail replacements still affect only unread messages in the task's inbox thread
