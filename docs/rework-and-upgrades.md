# amh rework and seamless upgrades

(authored by agents unless marked 🧑)

Status: design, not implemented. Builds on branches `daemon-design`, `daemon-stage1`, `daemon-stage2` (section "What we keep from the earlier daemon work").

🧑 Goals, the human's words (Zulip, stored in work logs as `manager_mail/zulip-639533262.txt` and `manager_mail/zulip-639538168.txt`):
- "amh still slow. Quick fixes? Use more Omnigent agents?"
- "Such slowness in amh can never happen again. Rework amh to be as efficient and reliable as agents can possible design"
- "Our infra also needs seemless upgrade. So maybe design it as a distributed system that can tolerate varying versions already"

## Summary

- Observed causes of slowness and failure: lock waits (bad until 2026-11-11, nearly gone since the lock-preparation fixes), an unbounded retry loop flooding the database since 2026-11-13 13:01, a sequential watcher loop where one stalled external call delays all delivery, and reads that scan 2 M events or ask Omnigent live. Data cannot rank them by latency cost, because nothing logs command duration.
- Load is tiny (fact: ~2,000–20,000 commands a day, under 0.25/s), so throughput is not the problem; unbounded work, unbounded retries and head-of-line blocking are.
- Decision: one daemon (`amhd`) owns the database and every external side effect; the `amh` CLI becomes a thin client. The deciding reason is upgrades and side-effect ownership, not speed: with in-process CLIs, every running binary version writes the same state.
- Upgrades: one release path installs versioned binaries; systemd holds the socket so short restarts queue clients; mutating requests carry ids so retries never double-apply; schema changes are additive so the previous binary still runs for rollback.
- Stage 0 fixes the observed causes inside today's architecture first; the daemon is built and tested against a production copy and then cut over at once, not run beside old writers for weeks.

## Measurements

Sources, under `~/.local/state/amh/`: `amh-lock-waits.log` (waits ≥ 5 s, 2026-10-19 to 2026-11-13), `amh-watch.log` (watcher phases ≥ 1 s; `request state timing` every ≤ 30 s), `amh-actions.log` (one line per run with exit code, no duration), the database opened read-only, and commands timed on 2026-11-13 ~15:55. Gaps: no command duration and no lock hold time are logged, and the lock file keeps the last holder's name after release, so wait attribution by holder is unreliable.

Lock waits (fact):
- all: 2,499 waits ≥ 5 s; p50 11 s, p99 159 s, max 764 s; 52,226 s waited in total
- 2026-11-11 alone: 825 waits, 29,084 s, mostly 07:00–10:59
- after d73bf0f and 7006c5b (2026-11-11 18:00, lock preparation ~4x cheaper): 10 waits, all ≤ 13 s, all at 13:02 and 13:35 on 2026-11-13 around a watcher restart
- code check (fact): `task start`, `agent replace`, `agent stop` and `tell agent` already make their Omnigent calls outside the lock; the watcher runs `request_effects` and `maintain` inside it

Cost of each lock acquisition (fact, `request state timing`, the watcher's acquisitions):
- 14 days: import p50 423 ms, export p50 1,235 ms
- today: import p50 72 / p99 393 ms, export p50 63 / p99 517 ms
- inference: every writing command still pays ~135 ms p50 of shared rebuild before its own work, with all other writers blocked meanwhile

Commands timed now (fact, 3 runs each):
- `amh task show`: 14 ms; `amh ls`: ~200 ms; `amh todo list`: 217–406 ms; `amh agent tree`: 215–231 ms
- `amh agent list`: 4.3–5.1 s (live Omnigent scan)
- `amh log TASK`: 20.9–23.3 s (scans `events`, no index on `subject`)
- isolated benchmark (`daemon-stage1` `docs/bench-results.md`, old amh 71d2194): `tell` p50/p99 150/1,985 and 204/2,653 ms; 100 starts took 19–35 s

Retry storm (fact, ongoing at time of writing):
- `events` has 2.27 M rows, database 1.6 GB; 1.93 M rows written on 2026-11-13, nearly all `message failed`
- per hour: 13:00 645,574; 14:00 797,600; 15:00 485,957 by 15:56 (~100 MB of event data per 25 min)
- cause: the watcher's `request notifications` phase (`deliver_requests`) retries every ~1.2 s, without end, notices to tasks whose address is `retired` or not on Omnigent (e.g. `mail_cleanup_x.md`, "retired is not an Omnigent session"), writing one event per failure
- began with the watcher restart at 13:01 (c102e80/ffdf2c4 went live); same pattern at lower rates 2026-11-07 to 11-11 (21 k–98 k events/day)

Watcher (fact): IMAP polling and the agent scan run on their own threads; every other phase runs in one sequential loop.
- 14 days, phases over 1 s: Zulip intake max 1,231 s; message delivery p99 96 s, max 385 s; archive max 377 s
- `Zulip intake failed` logged 2,342 times (DNS failures; "API usage exceeded rate limit")

Upgrades (fact):
- 236 `watcher started` lines in 12 days (up to 39 a day), one per `cargo install` + restart
- schema has no version (`PRAGMA user_version` = 0); tasks are whole Markdown snapshots in `tasks.text`
- from the manager's account (not verified here): agents installing from different branches overwrote each other's builds; Omnigent 0.16 → 0.21 changed model handling (fix 8ac4bc9)

## Root causes

1. Unbounded work per event: retries without limit, one stored event per retry, a full-table scan per `amh log`.
2. Head-of-line blocking: the watcher's sequential phases, and (until 2026-11-11) every writer behind one lock whose holder rebuilt exports.
3. Shared derived state rebuilt by each short-lived process (edit import, export), because only the watcher lives long.
4. No single owner of side effects: any process may send, retry, or start; nothing deduplicates retried commands.
5. No versioning anywhere: not the schema, not the files, not the binary; anyone can install.

## Decision: daemon owning state, thin CLI

Options:
- keep in-process CLI, fix causes 1–3 locally, add a schema version check that refuses a newer schema
    - fixes slowness, cheaply (stage 0 does exactly this)
    - leaves 4 and 5: every agent's binary writes the same schema, files and side effects, so every upgrade is a mixed-version write by up to 160 processes, and a refused schema means agents fail mid-upgrade
- daemon owning state and side effects
    - one writer version at a time; clients speak one versioned protocol; one place to bound retries and record request outcomes
    - cost: a single point of failure (below); a large refactor of output and environment handling
- chosen: daemon, because the human asked for seamless upgrades with mixed versions and only this reduces the writers to one; stage 0 comes first because its fixes are needed in either design
- not chosen: multiple hosts, Postgres, per-agent databases; one host and SQLite with one writer far exceed the load

Failure model, stated plainly: one authoritative daemon, restarted by systemd. If it crash-loops, every mutating amh command fails (exit 75 after a 30 s jittered connect wait) until someone rolls back. Mitigations: `Restart=always`; a crash-loop alert sent by a separate systemd `OnFailure=` unit, not by the daemon; manual rollback in one command; the previous binary always kept.

Shape:
- `amhd.socket` (systemd holds the listening Unix socket, mode 0600, peer uid checked) + `amhd.service` (`Type=notify`, `Restart=always`) run `amh daemon` from a versioned path; replaces `amh-watch.service`
- inside `amhd`, each part supervised: a dead writer thread kills the process (systemd restarts it); other workers restart alone
    - writer thread: the only SQLite writer; a write = preconditions + statements; transaction time logged and gated
    - request threads: one per connection; read through their own WAL connection, then submit writes
    - outbox: every external send (Omnigent, Zulip, mail) is a durable row first; one delivery worker per source/channel sends with bounded exponential backoff; FIFO per recipient task; a message failing permanently goes to a dead-letter state and one notice to the sender's manager
    - inbox: incoming Zulip/IMAP items stored with their source id under a unique constraint before acknowledging the source
    - start jobs: durable `spawn_jobs` rows, reconciled against Omnigent on restart, rate-limited by `launch.starts_per_minute`
    - one single-flight worker per external source (Zulip, IMAP, git commit, archive, Omnigent scan), each call with an I/O deadline; a worker never starts a second run while one is in progress
    - session cache from the Omnigent scan, with `last_success` time shown in output; past 60 s stale, `agent list` says so; staleness never turns a send into a permanent failure (only Omnigent's own answer does)
    - exporter: writes a task file only if its current hash equals the hash amhd last wrote; otherwise it imports the file first (no lost typed edit)
- CLI: parses with clap locally; sends a request id, argv, cwd and an allowlist of request-scoped env (`--task-file`, caller session); global settings (state dir, endpoints) are the daemon's own and never taken from a request; interactive parts (`amh edit` editor, `amh view` terminal) run in the client; long waits (`todo wait`) are client-side polls of short requests
- CLI never opens SQLite

## Mixed versions

Compatibility promise: adjacent releases (N-1, N) work together in every pairing; larger skew is refused with a clear message.

Compatibility surfaces, each with a rule:
- protocol: request has `proto` and the client build id; the daemon accepts `proto` N-1 and N
    - argv is the payload, but argv alone does not version meaning; so every command keeps its N-1 flags, defaults and exit codes, a changed meaning gets a new flag name, and CI runs every command's golden tests with the N-1 client against the N daemon and the reverse
- mutating requests: client-made request id; the daemon stores the outcome (exit code and output) for 24 h; a retry with the same id returns the stored outcome instead of running again
- schema: `schema_version` steps (from `daemon-stage2`), additive only and permanent: new tables, defaulted columns, indexes; columns are never dropped; every query names its columns
    - each step states what N-1 does with rows N writes (e.g. an outbox `kind` N-1 does not know stays `pending` until N returns), tested by running N-1 on a database N has written
    - a large index (e.g. on `events`) is one `CREATE INDEX` run at a measured cost on a production copy, not "in the background"
- generated Markdown and typed edits: the task-file format keeps parsing lines older releases write; the importer ignores unknown header fields
- JSON side files: imported once into tables by stage 2 (`daemon-stage2` already does this), then gone
- config (`env.toml`) and systemd units: shipped inside the release directory with the binary, so a rollback restores them together

External versions (Omnigent, Zulip): one adapter module each; the daemon logs Omnigent's version at start; an Omnigent upgrade is tested on the stub-backed suite plus a real smoke run (start a session, send, read history) before switching, the same release procedure as ours.

Dropped from `daemon-design` as unneeded: the epoch fence (the lifetime flock already guarantees one writer, and no fence can recall a POST already sent), the `versions` table and its 24-hour drop rule (columns are never dropped), auto-rollback canary (no reliable owner; rollback is manual).

## Upgrade and rollback

Release path (stops install overwrites):
- `amh release` replaces `cargo install` + `amh manager watchers`
    - builds a clean commit on `origin/revive`; runs tests
    - copies binary, units and default config to `~/.local/lib/amh/<version>-<commit>/`, read-only; keeps the last 5
    - atomically repoints `~/.cargo/bin/amh` (a symlink) and the unit's `ExecStart` path; running processes keep their open binary
    - restarts the service (`amh-watch.service` in stage 0, `amhd.service` later)
    - one release at a time (flock on `~/.local/lib/amh/release.lock`); `--hotfix` skips the `origin/revive` check but still records the commit
- enforcement limit: agents run as the same Unix user, so policy plus detection is all this gives; the daemon logs every client build id not found under `~/.local/lib/amh/` and tells the release owner once. Real prevention needs a separate Unix user owning the install (open question 2).

Procedure:
1. `amh release` switches the symlink, then restarts the service
2. old daemon on SIGTERM stops accepting (new connections wait in the systemd-held socket), finishes in-flight requests up to 5 s, exits; outbox rows mid-send stay `sending`
3. new daemon takes the flock, runs additive migrations, signals ready, accepts; recovers `sending` rows by searching recipient history for the `[amh:<id>]` trailer
4. a client whose connection dropped mid-request retries with the same request id: it gets the stored outcome or the request runs once
5. rollback: `amh release --rollback` does steps 1–3 with the previous directory; possible because the schema only grows

Delivery guarantee, honest: at-least-once. Omnigent has no idempotency key, so a crash between Omnigent accepting a send and amhd recording it can duplicate that one message; the trailer search shrinks the window but cannot close it. Gate on the measured duplicate rate, not on zero.

## What we keep from the earlier daemon work

- keep: daemon shape and units; single writer with preconditions; additive `schema_version` migrations; state-file import; chunked typed-edit import with exporter skip (`daemon-stage2`, 1,246 lines with tests); outbox with trailer recovery; `spawn_jobs`; Omnigent stub and benchmark harness (`daemon-stage1`); the nightly real-Omnigent delivery measurement
- change
    - CLI never opens SQLite; env split into request-scoped allowlist and daemon config
    - request ids with stored outcomes for every mutating command (new)
    - start rate limit kept inside the start queue (the quota concerns behind it remain)
    - one atomic cutover after testing on a production copy, instead of a long period of daemon and old CLIs both writing
    - group commit becomes optional: at < 0.25 writes/s a plain writer thread suffices; keep the existing code only if it costs nothing
    - exporter checks the file hash before every overwrite
- drop: spool and exit 73 (systemd holds the socket; crash loops are handled by rollback); epoch fence; `versions` table; auto-rollback; per-pool removal of start limits; `AMH_NO_DAEMON` after cutover
- out of scope: account pools, burner leases, item hygiene (sections B–D of `daemon-design`)

## Staged plan with targets

Every gate is measured over 3 days of production from the stage 0 duration logs, per command and per binary version, and paired with an error-rate gate so fast failures cannot improve latency.

Stage 0: fix the observed causes, today's architecture (≈ 3 days)
- log `duration_ms`, `lock_wait_ms`, `lock_hold_ms`, build id in each `amh-actions.log` exit line; clear the lock file's holder on release
- `deliver_requests`: retired / non-Omnigent addresses become a dead letter with one notice; other failures back off to 10 min; one event per state change, not per attempt
- `events(subject, id)` index, built once (measured on a copy first); delete the flood rows in batches; no `VACUUM` unless disk requires it
- Zulip intake, archive and git commit leave the sequential loop: one single-flight thread each, with I/O deadlines
- `amh release` with the stage 0 watcher unit
- gates
    - lock wait p99 < 200 ms, max < 2 s; lock hold p99 < 300 ms, max < 1 s
    - `events` writes ≤ 5 per logical operation; database growth < 50 MB/day
    - `amh log` p99 < 500 ms; `todo list` p99 < 300 ms
    - oldest pending human message < 60 s while Zulip intake is failing (fault test on the isolated setup)
    - command error rate not above the 2026-11-05..13 rate per command (e.g. `agent status` 12%, `tell agent` 3%)

Stage 1: complete daemon, off production (≈ 7 days; `daemon-stage2` rebased on `revive` plus the client protocol)
- writer, outbox/inbox, start jobs, single-flight workers, session cache, exporter hash check, request ids, protocol, env split
- tested on a copy of the production state dir with the Omnigent stub and in the `daemon-stage1` load harness
- gates (harness): `tell`, `todo list`, `task show`, `ls` p50 < 20 ms, p99 < 100 ms; `agent list` p99 < 100 ms; writer transaction p99 < 10 ms; old-vs-new golden output identical; N-1/N cross tests pass; kill -9 chaos: no lost message, duplicate rate recorded

Stage 2: atomic cutover (≈ 1 day)
- `amh release` installs the daemon release: stops `amh-watch.service`, migrates, starts `amhd`; side files imported
- rollback rehearsed first on the copy: previous in-process release on the migrated database
- gates (production, 3 days): stage 1 latency gates hold; `amh-lock-waits.log` silent; human-to-agent delivery p99 < 10 s on the nightly real-Omnigent run; daemon RSS < 500 MB and CPU < 5% of one core averaged

Stage 3: upgrade drills (≈ 2 days)
- previous release and HEAD under load: 10 upgrades and 10 rollbacks
- gates: zero refused connections; restart gap p99 < 1 s (measured, from SIGTERM to first accept); no request applied twice; message duplicate rate ≤ 1 per 10,000

## Open questions

1. Release owner: which task or person owns `amh release`?
2. Is a separate Unix user for the install worth it, to make the single owner enforced rather than policy?
3. Is a visible `[amh:<id>]` trailer on delivered messages acceptable (Omnigent has no idempotency key)?
