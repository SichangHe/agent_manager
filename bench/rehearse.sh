#!/usr/bin/env bash
# Rehearse the stage-2 cut-over and rollback of docs/rework-and-upgrades.md end to end on an isolated copy.
# Usage: bench/rehearse.sh SRC_STATE_DIR SRC_WORK_LOGS OLD_RELEASE_DIR NEW_CHECKOUT
#   SRC_STATE_DIR    live state dir to copy (read only: SQLite backup API, holds and scan files)
#   SRC_WORK_LOGS    live work_logs folder (its top-level *.md are copied)
#   OLD_RELEASE_DIR  the live CLI-only release directory (copied as the fake home's live release)
#   NEW_CHECKOUT     clean git checkout of the daemon build to release (with --hotfix: it is not on origin/main yet)
# Everything runs under a fake HOME with a fake `systemctl` (process-managed units), the Omnigent stub, no Zulip and no mail credentials.
# Env: REHEARSE_KEEP=1 keeps the folder.
set -euo pipefail
src_state=$(realpath "$1"); src_logs=$(realpath "$2"); old_release=$(realpath "$3"); checkout=$(realpath "$4")
here=$(dirname "$(realpath "$0")")
# The live toolchain, so the fake home builds without downloading crates.
cargo_home=${CARGO_HOME:-$HOME/.cargo}; rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
R=$(mktemp -d /tmp/amh-rehearse.XXXXXX)
H=$R/home; S=$H/.local/state/amh; AMH=$H/.cargo/bin/amh
mkdir -p "$H/.cargo/bin" "$H/.local/lib/amh" "$S" "$R/root" "$R/bin" "$R/units" "$R/work" "$R/tmp"
step() { printf '\n== %s\n' "$*"; }
fail() { echo "REHEARSAL FAILED: $*" >&2; exit 1; }
stub_pid=
cleanup() {
    "$R/bin/systemctl" --user stop amhd.service >/dev/null 2>&1 || true
    "$R/bin/systemctl" --user stop amh-watch.service >/dev/null 2>&1 || true
    [ -n "$stub_pid" ] && kill "$stub_pid" 2>/dev/null || true
    [ "${REHEARSE_KEEP:-}" = 1 ] && echo "kept $R" >&2 || { chmod -R u+w "$R"; rm -rf "$R"; }
}
trap cleanup EXIT

# A fake `systemctl --user` for the two units, run as processes of the fake home; starting one unit stops the other (Conflicts=).
cat >"$R/bin/systemctl" <<'UNIT'
#!/usr/bin/env bash
set -u
[ "$1" = --user ] && shift
verb=$1; unit=${2%.service}; dir=$REHEARSE_UNITS
run() {
    case $unit in
        amh-watch) setsid "$HOME/.cargo/bin/amh" manager watch >>"$HOME/.local/state/amh/amh-watch.log" 2>&1 & ;;
        amhd) setsid env AMH_DAEMON=1 AMH_DAEMON_LIVE=1 "$HOME/.cargo/bin/amh" daemon >>"$HOME/.local/state/amh/amhd.log" 2>&1 & ;;
    esac
    echo $! >"$dir/$unit.pid"
}
active() { [ -f "$dir/$unit.pid" ] && kill -0 "$(cat "$dir/$unit.pid")" 2>/dev/null; }
stop() { if active; then kill "$(cat "$dir/$unit.pid")"; for _ in $(seq 100); do active || break; sleep 0.05; done; fi; rm -f "$dir/$unit.pid"; }
other() { [ "$unit" = amhd ] && echo amh-watch || echo amhd; }
case $verb in
    start) active && exit 0; (unit=$(other); stop); run ;;
    stop) stop ;;
    restart) stop; (unit=$(other); stop); run ;;
    try-restart) active && { stop; run; } ; exit 0 ;;
    is-active) active && echo active || { echo inactive; exit 3; } ;;
    enable|disable|daemon-reload) ;;
    *) echo "fake systemctl: $verb unsupported" >&2; exit 1 ;;
esac
UNIT
chmod +x "$R/bin/systemctl"

step "copy the live state (read only) and the CLI-only release"
python3 -c 'import sqlite3,sys; s=sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True); d=sqlite3.connect(sys.argv[2]); s.backup(d)' "$src_state/amh.sqlite3" "$S/amh.sqlite3"
for f in amh-holds.json amh-agent-states.json; do [ -f "$src_state/$f" ] && cp "$src_state/$f" "$S/"; done
cp "$src_logs"/*.md "$R/root/" 2>/dev/null || true
old=$(basename "$old_release")
cp -r "$old_release" "$H/.local/lib/amh/$old"
echo "$old" >"$H/.local/lib/amh/history"
ln -s "$H/.local/lib/amh/$old/amh" "$AMH"

python3 "$here/omnigent_stub.py" "$R/port" --latency-ms 5 &
stub_pid=$!
for _ in $(seq 100); do [ -s "$R/port" ] && break; sleep 0.05; done
url="http://127.0.0.1:$(cat "$R/port")"
python3 "$here/isolate_env.py" /nonexistent "$R/root" "$url" >"$S/env.toml"
# The live limit (2 starts/min) queues the smoke starts behind the watcher's own restarts of copied agents; lift it as the load bench does.
mkdir -p "$H/.config/amh"; printf '[launch]\nstarts_per_minute = 1000\n' >"$H/.config/amh/config.toml"

export REHEARSE_UNITS=$R/units
amh() { env -i HOME="$H" PATH="$R/bin:$here/bin:/usr/bin:/bin" TMPDIR="$R/tmp" REHEARSE_UNITS="$R/units" "$AMH" "$@"; }
sysd() { env -i HOME="$H" PATH="$R/bin:$here/bin:/usr/bin:/bin" REHEARSE_UNITS="$R/units" "$R/bin/systemctl" "$@"; }
received() { python3 -c 'import json,sys,urllib.request; m=json.load(urllib.request.urlopen(sys.argv[1]+"/stub/messages")); sys.exit(0 if any(sys.argv[2] in x["text"] for x in m) else 1)' "$url" "$1"; }
wait_for() { for _ in $(seq 200); do "$@" && return 0; sleep 0.1; done; return 1; }
open_outbox() { python3 -c 'import sqlite3,sys; d=sqlite3.connect(sys.argv[1]); print(d.execute("SELECT COUNT(*) FROM outbox WHERE state IN (\"pending\",\"sending\")").fetchone()[0])' "$S/amh.sqlite3"; }
printf 'Rehearsal task: do nothing.\n' >"$R/work/prompt.md"
smoke() { # $1: label
    local n=$1
    out=$(amh task start "rh-$n.md" --tool claude --dir "$R/work" --prompt "$R/work/prompt.md" --manager omnigent://stub-main 2>&1) || fail "$n: task start: $out"
    out=$(amh tell agent "rh-$n.md" "rehearse-$n" --from rehearsal 2>&1) || fail "$n: tell: $out"
    wait_for received "rehearse-$n" || fail "$n: message never arrived"
    out=$(amh agent status "rh-$n.md" 2>&1) || fail "$n: agent status: $out"
    out=$(amh agent list 2>&1) || fail "$n: agent list: $out"
    out=$(amh --task-file "rh-$n.md" todo list 2>&1) || fail "$n: todo list: $out"
    echo "smoke $n: start, tell (delivered), status, list, todo ok"
}

step "1. before: CLI-only build $old with amh-watch"
sysd --user start amh-watch.service
smoke before

step "2. release the daemon build (CLI mode, watcher restarts on it)"
( cd "$checkout" && env -i HOME="$H" PATH="$R/bin:$here/bin:$cargo_home/bin:/usr/bin:/bin" CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" REHEARSE_UNITS="$R/units" "$AMH" release --hotfix ) || fail "amh release"
new=$(basename "$(dirname "$(readlink "$AMH")")")
[ "$new" != "$old" ] || fail "release did not switch"
sysd --user is-active amh-watch.service >/dev/null || fail "watcher not running after release"
smoke released

step "3. cut over: amhd replaces amh-watch, clients go through it"
sysd --user start amhd.service
wait_for test -S "$S/amhd.sock" || fail "amhd did not open its socket"
touch "$S/amhd.client"
sysd --user is-active amh-watch.service >/dev/null && fail "amh-watch still running next to amhd"
pgrep -f "$H/.cargo/bin/amh manager watch" >/dev/null && fail "a watcher process runs next to amhd"
smoke daemon
grep -q 'via=amhd' "$S/amh-actions.log" || fail "no command went through amhd"
for f in amh-holds.json; do [ -f "$S/$f" ] || fail "$f moved away by the daemon (holds would be ignored)"; done
amh agent hold rh-daemon.md --reason rehearsal >/dev/null || fail "agent hold"
amh tell agent rh-daemon.md "held-must-not-arrive" --from rehearsal >/dev/null 2>&1 && fail "a tell to a held agent was accepted"
sleep 3; received "held-must-not-arrive" && fail "a held agent got a message"
amh agent release rh-daemon.md >/dev/null || fail "agent release"
echo "hold respected through amhd"
# A long task body: its older part moves out of the task row; it must read whole, and survive the rollback below.
pad=$(printf 'x%.0s' $(seq 600))
for i in $(seq 90); do amh task note rh-daemon.md "chunk-check-$i $pad" >/dev/null || fail "task note $i"; done
wait_for grep -q "chunk-check-90 " "$R/root/rh-daemon.md" && grep -q "chunk-check-1 " "$R/root/rh-daemon.md" || fail "the older body of a long task is missing from its file"
echo "long task body split and read whole"

step "4. daemon restart under load loses nothing"
pids=()
for i in $(seq 20); do amh tell agent rh-daemon.md "restart-$i" --from rehearsal >/dev/null & pids+=($!); done
sysd --user restart amhd.service
for pid in "${pids[@]}"; do wait "$pid" || fail "a tell failed across the restart"; done
for i in $(seq 20); do wait_for received "restart-$i" || fail "restart-$i lost"; done
echo "20 tells across a daemon restart: all delivered"

step "5. roll back to $old"
old_has_daemon=$("$H/.local/lib/amh/$old/amh" daemon 2>&1 | grep -c "the daemon is off" || true)
# One command: amh release --rollback leaves the daemon (marker, drain, units) when the older release has none.
out=$(amh release --rollback 2>&1) || fail "amh release --rollback: $out"
echo "$out" | grep "resend by hand" | grep -E "rehearse-|restart-" && fail "a rehearsal message never left the outbox"
[ "$(basename "$(dirname "$(readlink "$AMH")")")" = "$old" ] || fail "rollback did not switch to $old"
sleep 3
if [ "$old_has_daemon" = 0 ]; then
    [ -e "$S/amhd.client" ] && fail "the cut-over marker is still there"
    sysd --user is-active amhd.service >/dev/null && fail "amhd still runs after the rollback"
    sysd --user is-active amh-watch.service >/dev/null || fail "old watcher not running"
    [ "$(pgrep -fc "$H/.cargo/bin/amh manager watch")" = 1 ] || fail "not exactly one watcher after rollback"
else
    sysd --user is-active amhd.service >/dev/null || fail "the older amhd is not running after the rollback"
    wait_for test -S "$S/amhd.sock" || fail "the older amhd did not open its socket"
fi
python3 -c "import sqlite3,sys; t=sqlite3.connect(sys.argv[1]).execute(\"SELECT text FROM tasks WHERE name='rh-daemon.md'\").fetchone()[0]; sys.exit(0 if 'chunk-check-1 ' in t and 'older_body' not in t else 1)" "$S/amh.sqlite3" || fail "a long task row is not whole after the rollback"
wait_for grep -q "chunk-check-1 " "$R/root/rh-daemon.md" || fail "the exported long task lost its older body"
echo "long task whole after the rollback"
smoke rolledback
grep -qi panick "$S/amh-watch.log" "$S/amhd.log" 2>/dev/null && fail "a watcher log has a panic"

step "REHEARSAL PASSED: $old -> $new (CLI) -> amhd -> restart -> rollback to $old"
