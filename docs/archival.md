# AMH archival

(authored by agents unless marked 🧑)

events
- 🧑 "Yes, do rolling 30 day archival. Keep everything displayed in markdown tho, so agents can grep them"
- `archive_events` groups events older than 30 days into UTC date directories
    - one `events.md` per date, containing complete JSON records in ID order
    - merges existing records for retry after interruption
    - invalid timestamps remain in SQLite
- writes and syncs the file and containing directory before deleting exact database IDs
    - failed writes preserve database events
    - interrupted pruning may leave duplicate copies, never missing copies
    - the caller holds the shared store lock
- `archive_requests` writes old resolved requests to dated `requests.md` before removing their records
    - retains every request named by any blocker
    - migration starts a fresh retention window for legacy resolved records with unknown dates
    - invalid timestamps stay in SQLite
    - preserves task bodies, counters, open requests and delivery records
    - uses the same durable batch writer as events

git
- 🧑 "Keep the md files in a git repo like we do now with work_logs, and automatically periodically commit all changes."
- `commit_markdown` commits tracked and new Markdown in the configured work-log repository
    - requires an existing HEAD and git author configuration
    - `git commit --only` preserves staging for other file types
    - includes Markdown working-tree content even when that Markdown was partially staged
    - leaves new Markdown intent-to-add entries if the commit fails
    - no push
- watcher archives daily and commits every 10 min; only `*.md`, so no artifact or binary is committed
    - `[archive] commit = false` disables commits
    - runs commands with a 30-second timeout

# Git history (src/history.rs)
- every 10 min the watcher calls `history::commit_and_push`
  - private index `.git/amh.index` + `commit-tree`; shared index, hooks, and worktree are never used to build commits
  - candidates: changed or new `*.md`, not ignored, not under `amh_views`, not removed from the shared index by someone
  - files over 5 MB are skipped; main manager told once per change of the set (record `oversize-notified`)
  - fetch origin; diverged => object-space merge, per-file `git merge-file --union` (logs are append-mostly); push; retry 3x
  - push or fetch failure => main manager told once per distinct error (record `push-failure-notified`)
- after commit or merge, local branch moves by compare-and-swap `update-ref`
  - shared index entry moves only if it still equals the old commit's blob
  - worktree file moves only if untouched; if edited, old->new is union-merged into it, so edits stay edits
- finished task files (status `done`) move to `YYYYMM/` (month of last root-file change); reopening moves them back (record `month:NAME`)
