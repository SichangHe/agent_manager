# Rework stage 2: cut-over to amhd and rollback

🤖 Runbook for moving the live amh from the CLI-only build (each command opens the store; `amh-watch.service` runs the watcher) to the daemon (`amhd.service` owns the store, the outbox and the watcher; commands are clients). It is rehearsed end to end by `bench/rehearse.sh` on an isolated copy; run that first, on the exact commit to be released.

## What changes

- `amhd.service` (`systemd/amhd.service`) runs `amh daemon` with `AMH_DAEMON=1 AMH_DAEMON_LIVE=1`. It has `Conflicts=amh-watch.service`: starting it stops the old watcher, because the daemon runs the watcher itself.
- The file `amhd.client` in the state dir sends every command through `amhd.sock`. Without it, commands run in-process as before. `AMH_CLIENT=1` or `AMH_CLIENT=0` overrides it per command.
- A client never writes beside the daemon. While the daemon is not answering (a restart), it retries the connection for about 30 s and then exits 75 with nothing run. A client that has sent its request retries with the same request id for about 30 s, and the retry gets the stored outcome.
- `amh release` restarts `amhd.service` when `amhd.client` exists, else `amh-watch.service`. After restarting `amhd` it fails, naming `amh release --rollback`, if the socket does not answer within 30 s.
- `amh-quota.service` (`amh quota serve`) stays a separate process that opens the store itself, as it did beside the watcher. SQLite serializes it with the daemon (the writer waits up to 30 s on a lock).
- Schema: the daemon applies migrations V1–V3 at start. They are additive for the CLI-only build, which ignores the new tables and columns. V3 recreates `outbox`, which only the daemon uses. The rehearsal runs the CLI-only build on the migrated store.

## Delivery guarantees (outbox, on by default; `AMHD_OUTBOX=0` sends directly)

- A message is a durable row before `amh tell agent` answers. It is sent at least once, in order per recipient.
- After a crash (`kill -9` or abort) at any point, no message and no request is lost. The only possible duplicates are sends that were in flight (at most one per recipient) whose result was not yet stored. At boot those rows return to `pending` with `last_error` saying they may be delivered twice.
- A request re-run after a crash queues nothing twice: message ids derive from the request id.
- Failures back off 2^n s, capped at 10 min. After 8 attempts a row becomes `dead`.
- A recipient held when its message is sent gets the row `dead` with `held at send time: …`. This matches `amh tell` refusing held agents.
- Check with `bench/run.sh` and `BENCH_CRASH=1`: four crash scenarios, 1600 tells.

## Cut-over

1. Merge `amhd` into `main` and push. In a clean checkout of `origin/main`, run `bench/rehearse.sh LIVE_STATE WORK_LOGS LIVE_RELEASE_DIR CHECKOUT`. It must end with `REHEARSAL PASSED`.
2. Every `amh` on any agent's PATH must be the release symlink: `which -a amh` and `readlink -f` each. A separate binary (today `~/.local/bin/amh`, an October build) ignores `amhd.client` and writes beside the daemon. Replace it with a symlink to `~/.cargo/bin/amh`.
3. `cpu-run LOW -c 8 -o OWNER -e END -- amh release` in that checkout. This is the new build in CLI mode, with `amh-watch` restarted on it. Check `amh agent list`, `amh todo list`, and `tail amh-watch.log`.
4. Install the unit: copy `systemd/amhd.service` to `~/.config/systemd/user/`, then run `systemctl --user daemon-reload` and `systemd-analyze --user verify amhd.service`. Its PATH and working directory must match `amh-watch.service`.
5. In one scripted step, so in-process commands and the daemon overlap only for that moment:
   ```
   systemctl --user start amhd.service && until [ -S ~/.local/state/amh/amhd.sock ]; do sleep 0.1; done && touch ~/.local/state/amh/amhd.client
   ```
   Starting `amhd` stops `amh-watch`. Check `tail amhd.log` for migration and import lines with no errors.
6. Verify:
   - `pgrep -af 'amh manager watch'` shows nothing; the daemon is the only watcher.
   - The Zulip and mail cursors advance in `amhd.log` within a few minutes; the rehearsal has no Zulip or mail, so this is checked only live.
   - `amh agent list` and `amh agent status X` answer.
   - A `tell` to a test agent arrives.
   - New lines in `amh-actions.log` end with `via=amhd`.
   - `systemctl --user is-active amh-watch.service` says `inactive`.
7. `systemctl --user disable amh-watch.service`, then `systemctl --user enable amhd.service`, in that order, so a reboot starts only the daemon.

Later upgrades are the normal `amh release`, which restarts `amhd.service`. Clients bridge the restart with retries.

## Rollback to the CLI-only build

1. `rm ~/.local/state/amh/amhd.client`. New commands run in-process again. The daemon keeps delivering what it already queued.
2. Wait until the outbox is drained, i.e. this query returns 0:
   ```
   SELECT COUNT(*) FROM outbox WHERE state IN ('pending','sending')
   ```
   It takes seconds, unless a recipient is backing off. Then list the remaining rows and resend them by hand after the switch. Also list the `dead` rows.
3. Wait 30 s after step 1, so no client is still retrying against the daemon. Then `systemctl --user stop amhd.service`, then `systemctl --user start amh-watch.service`. Then `systemctl --user disable amhd.service` and `systemctl --user enable amh-watch.service`.
4. `amh release --rollback`, repeated until `readlink ~/.cargo/bin/amh` names the CLI-only release; note its name before the cut-over, and keep its directory in `~/.local/lib/amh` (`history`). It restarts `amh-watch`, because the marker is gone.
5. Verify: exactly one `amh manager watch` process, a task start, a tell that arrives, `agent status`, `agent list`, `todo list`, and no panic in `amh-watch.log`.

Never run `amh release --rollback` to a CLI-only build while `amhd.client` exists or `amhd.service` runs. The old binary has no `daemon` command and ignores the marker.

## Rehearsal

`bench/rehearse.sh` copies the live store (SQLite backup API, read only), the holds and scan files, the top-level task files, and the live CLI-only release into a fake `HOME`. It uses the Omnigent stub, no Zulip, no mail credentials, and a fake `systemctl` that runs the two units as processes. Not covered by the rehearsal: the real unit file (checked by `systemd-analyze verify`), enable/disable at boot, `amh-quota.service`, and the Zulip and mail intake. It then runs, checking each phase with a task start, a tell that must arrive, status, list and todo:

1. The CLI-only build with `amh-watch`.
2. `amh release --hotfix` of the daemon build.
3. The cut-over (steps 5–6 above).
4. 20 tells across an `amhd` restart, all of which must arrive.
5. The rollback (steps 1–5 above) to the CLI-only build on the migrated store.
