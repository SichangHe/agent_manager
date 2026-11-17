#!/usr/bin/env bash
# Run the load workload (bench/load.py) against amh BINARY in an isolated state dir and the Omnigent stub; print a JSON summary.
# Two fresh runs: one for latencies, one under strace for lock waits (strace slows each command).
# Usage: bench/run.sh BINARY
# Env: AMH_BENCH_ENV (env.toml to copy; default the machine's), BENCH_STUB_LATENCY_MS (default 5),
#      BENCH_STARTS_PER_MIN (launch.starts_per_minute; default 1000), BENCH_KEEP=1 keeps the temp dir.
set -euo pipefail
amh=$(realpath "$1")
here=$(dirname "$(realpath "$0")")
stub_pid=
cleanup() {
    [ -n "$stub_pid" ] && kill "$stub_pid" 2>/dev/null || true
    # Detached starts (setsid) outlive load.py; kill every process whose environment names this state dir.
    for pid in $(pgrep -f "$amh" || true); do
        grep -qzx "AMH_STATE_DIR=$tmp/state" "/proc/$pid/environ" 2>/dev/null && kill "$pid" 2>/dev/null || true
    done
    [ "${BENCH_KEEP:-}" = 1 ] && echo "kept $tmp" >&2 || rm -rf "$tmp"
}
one_run() {
    tmp=$(mktemp -d /tmp/amh-bench.XXXXXX)
    trap cleanup EXIT
    mkdir -p "$tmp"/{root,state,cfg,tmp,home,work}
    printf 'current:\n\nprevious:\n' >"$tmp/root/TODO.md"
    python3 "$here/omnigent_stub.py" "$tmp/port" --latency-ms "${BENCH_STUB_LATENCY_MS:-5}" &
    stub_pid=$!
    for _ in $(seq 100); do [ -s "$tmp/port" ] && break; sleep 0.05; done
    url="http://127.0.0.1:$(cat "$tmp/port")"
    python3 "$here/isolate_env.py" "${AMH_BENCH_ENV:-$HOME/.local/state/amh/env.toml}" "$tmp/root" "$url" >"$tmp/state/env.toml"
    printf '[launch]\nstarts_per_minute = %s\n' "${BENCH_STARTS_PER_MIN:-1000}" >"$tmp/cfg/config.toml"
    client=0
    daemon_pid=
    # BENCH_DAEMON=1: run amhd on the isolated state and send every command through it.
    if [ "${BENCH_DAEMON:-}" = 1 ]; then
        env -i HOME="$tmp/home" PATH="$here/bin:/usr/bin:/bin" TMPDIR="$tmp/tmp" \
            AMH_STATE_DIR="$tmp/state" AMH_CFG_DIR="$tmp/cfg" AMH_DAEMON=1 AMHD_OUTBOX="${AMHD_OUTBOX:-}" "$amh" daemon >"$tmp/daemon.log" 2>&1 &
        daemon_pid=$!
        for _ in $(seq 400); do [ -S "$tmp/state/amhd.sock" ] && break; sleep 0.05; done
        [ -S "$tmp/state/amhd.sock" ] || { echo "amhd did not start:" >&2; cat "$tmp/daemon.log" >&2; }
        client=1
    fi
    script=load.py
    # BENCH_CRASH=1: the crash test, which runs and kills its own daemon.
    if [ "${BENCH_CRASH:-}" = 1 ]; then script=crash.py; client=1; fi
    # BENCH_DRILL=1 (with BENCH_DRILL_OLD=BINARY): stage-3 upgrade and rollback drills between two daemon builds.
    if [ "${BENCH_DRILL:-}" = 1 ]; then script=drills.py; client=1; fi
    env -i HOME="$tmp/home" PATH="$here/bin:/usr/bin:/bin" TMPDIR="$tmp/tmp" \
        AMH_STATE_DIR="$tmp/state" AMH_CFG_DIR="$tmp/cfg" AMH_CLIENT="$client" \
        BENCH_DRILL_OLD="${BENCH_DRILL_OLD:-}" DRILL_RATE="${DRILL_RATE:-54}" DRILL_EVERY_S="${DRILL_EVERY_S:-6}" \
        python3 "$here/$script" "$amh" "$url" "$tmp/work" "$@"
    [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null || true
    cleanup
    stub_pid=
    trap - EXIT
}
# BENCH_RATE=R BENCH_SECONDS=S: one paced run (open-loop, realistic command mix) instead of the saturation workload.
if [ "${BENCH_CRASH:-}" = 1 ] || [ "${BENCH_DRILL:-}" = 1 ]; then
    one_run
    exit
fi
if [ -n "${BENCH_RATE:-}" ]; then
    one_run --rate "$BENCH_RATE" --seconds "${BENCH_SECONDS:-120}"
    exit
fi
clean=$(one_run)
traced=$(one_run --strace)
python3 -c 'import json,sys; a,b=(json.loads(x) for x in sys.argv[1:]); a["lock_wait_ms"],a["flock_calls_by_kind"]=b["lock_wait_ms"],b["flock_calls_by_kind"]; print(json.dumps(a,indent=1))' "$clean" "$traced"
