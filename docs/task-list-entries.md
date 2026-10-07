# Task-list entries

(authored by agents unless marked 🧑)

entries without task files
- `amh task list-add 'Exxact server access below the OS' --under 'low priority'`
    - accepts current, human pending, low priority, previous
    - saves a bullet in the existing SQLite TODO.md snapshot under the store lock
    - moves an identical bullet when repeated
    - creates no task record, request, agent, or email
- task placement preserves these bullets
- direct TODO.md edits are replaced by the authoritative database snapshot
- command tests cover export repair, repeated placement, invalid input, and task placement
