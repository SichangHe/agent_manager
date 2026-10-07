# mail routing
(authored by agents unless marked 🧑)

- 🧑 “Make this routing dynamically configurable instead of specific to GH.”
- `mail_routes` is the pure configuration and matching core
  - `rules` accepts flattened settings on each reload, validates all rules before returning any, sorts descending priority then alphabetical name
  - `choose` returns the first matching rule for a mailbox and parsed message
  - caller owns mailbox access, task existence checks, durable delivery and disposition changes
- configuration: `[mail_route.NAME]` in TOML
  - `from`: required exact address, compared without ASCII case
  - `to`: required task name without whitespace
  - `mailbox`: `human` default or `agent`
  - `subject`, `body`: optional nonempty case-insensitive substrings, both must match when supplied
  - `priority`: signed integer, default `0`
  - `disposition`: `keep` default, `read` or `trash`
  - unknown fields, nested keys, malformed addresses and invalid values fail the complete configuration
- message fields use the existing MIME parser
  - sender excludes display name
  - body uses plain text when present, otherwise raw HTML
  - sender equality does not authenticate the sender
- unmatched messages return no route
- watcher reloads rules on every poll; human and agent mailbox connections are independent
  - searches unread inbox messages from configured senders without marking them read
  - missing or completed destinations retain their messages and do not block other routes
  - SQLite commits receipt, task notification and queued delivery together before `read` or `trash`
  - receipt key: mailbox plus Message-ID; missing Message-ID uses inbox UID generation and UID
  - retry after a mailbox disposition failure does not enqueue another notification
  - saved source text is private under `manager_mail`; notification names its readable source and labels it external
