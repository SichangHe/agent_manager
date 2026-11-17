"""The load workload of docs/daemon-design.md against one amh binary and the Omnigent stub; prints a JSON summary.

Usage: `load.py AMH STUB_URL WORKDIR [--strace]`; the caller has isolated the environment (see `run.sh`).
Seeds `N_SEEDS` tasks, then at once: `N_STARTS` concurrent `amh task start`,
`N_TELLERS` clients each sending `N_TELLS` `amh tell agent`, and `N_STATUS` clients looping `amh agent status`.
With `--strace`, every amh run is traced for `flock` calls to measure lock waits.
"""

import glob
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.request

N_SEEDS, N_STARTS, N_TELLERS, N_TELLS, N_STATUS = 10, 100, 10, 100, 20
START_TIMEOUT_S = 900
TELL_ID = re.compile(r"bench-tell \d+-\d+")
BUSY = re.compile(r"SQLITE_BUSY|database is locked")
FLOCK = re.compile(r"flock\(\d+, (LOCK_\w+(?:\|LOCK_\w+)?)\)\s+= (-?\d+).*<([\d.]+)>")


def pct(values, p):
    if not values:
        return None
    s = sorted(values)
    return round(s[min(len(s) - 1, int(len(s) * p / 100))], 1)


def stub(url, path):
    with urllib.request.urlopen(url + path, timeout=30) as r:
        return json.load(r)


class Runner:
    """Runs amh commands, recording wall time and failures; shared across client threads."""

    def __init__(self, amh, workdir, use_strace):
        self.amh, self.workdir, self.use_strace = amh, workdir, use_strace
        self.lock = threading.Lock()
        self.failures = []
        self.n_busy = 0
        self.n_runs = 0

    def run(self, kind, args):
        with self.lock:
            self.n_runs += 1
            n = self.n_runs
        cmd = [self.amh, *args]
        if self.use_strace:
            cmd = ["strace", "-f", "-qq", "-T", "-e", "trace=flock", "-o", f"{self.workdir}/strace/{kind}.{n}", *cmd]
        start_ns = time.time_ns()
        done = subprocess.run(cmd, capture_output=True, text=True, timeout=1200)
        end_ns = time.time_ns()
        with self.lock:
            self.n_busy += bool(BUSY.search(done.stderr))
            if done.returncode != 0:
                self.failures.append(f"{kind} {' '.join(args)}: exit {done.returncode}: {done.stderr.strip()[:300]}")
        return start_ns, end_ns, done.returncode


def lock_waits(workdir):
    """Milliseconds each blocking `flock(LOCK_EX)` took, and how many `flock` calls ran per command kind."""
    waits, n_calls = [], {}
    for path in glob.glob(f"{workdir}/strace/*"):
        kind = os.path.basename(path).split(".")[0]
        for line in open(path):
            found = FLOCK.search(line)
            if not found:
                continue
            n_calls[kind] = n_calls.get(kind, 0) + 1
            if found.group(1) == "LOCK_EX":
                waits.append(float(found.group(3)) * 1000)
    return waits, n_calls


def received(url):
    """When the stub first received each bench tell, and how many arrived more than once."""
    recv, n_dup = {}, 0
    for m in stub(url, "/stub/messages"):
        if found := TELL_ID.search(m["text"]):
            n_dup += found.group(0) in recv
            recv.setdefault(found.group(0), m["recv_ns"])
    return recv, n_dup


# Realistic mix from amh-actions.log since 2026-11-05 (67k commands): agent status 41 %, tell 26 % (agent 13, manager 9, human 5),
# todo list 6 %, task start 5 %; rounded to the four kinds this harness drives.
MIX = (("status", 0.55), ("tell", 0.30), ("todo", 0.10), ("start", 0.05))


def paced(runner, url, workdir, start, rate, seconds):
    """Open-loop Poisson arrivals at `rate` commands/s for `seconds`; prints per-kind latencies."""
    import random
    random.seed(1)
    lat = {kind: [] for kind, _ in MIX}
    tells, threads = [], []
    seq = [0]

    def one(kind, n):
        if kind == "status":
            s, e, rc = runner.run("status", ["agent", "status", f"seed{n % N_SEEDS}.md"])
        elif kind == "todo":
            s, e, rc = runner.run("todo", ["--task-file", f"seed{n % N_SEEDS}.md", "todo", "list"])
        elif kind == "start":
            s, e, rc = start(f"paced{n}.md")
        else:
            text = f"bench-tell 0-{n}"
            s, e, rc = runner.run("tell", ["tell", "agent", f"seed{n % N_SEEDS}.md", text, "--from", "bench"])
            tells.append((text, s, e, rc))
        lat[kind].append((e - s) / 1e6)

    began = time.time()
    due = began
    late = 0
    while due < began + seconds:
        due += random.expovariate(rate)
        now = time.time()
        if due > now:
            time.sleep(due - now)
        else:
            late += 1
        r, acc = random.random(), 0.0
        for kind, share in MIX:
            acc += share
            if r < acc:
                break
        seq[0] += 1
        t = threading.Thread(target=one, args=(kind, seq[0]))
        t.start()
        threads.append(t)
    for t in threads:
        t.join()
    wall = time.time() - began
    time.sleep(5)
    recv, n_dup = received(url)
    delivered = [(t, s, e) for t, s, e, rc in tells if rc == 0 and t in recv]
    every = [v for vs in lat.values() for v in vs]
    print(json.dumps({
        "profile": {"rate_per_s": rate, "seconds": seconds, "wall_s": round(wall, 1), "n_commands": len(every),
                    "achieved_per_s": round(len(every) / wall, 1), "n_dispatched_late": late},
        "all_ms": {"p50": pct(every, 50), "p99": pct(every, 99)},
        **{f"{k}_ms": {"p50": pct(v, 50), "p99": pct(v, 99), "n": len(v)} for k, v in lat.items()},
        "delivery_from_tell_start_ms": {"p50": pct([(recv[t] - s) / 1e6 for t, s, _ in delivered], 50),
                                        "p99": pct([(recv[t] - s) / 1e6 for t, s, _ in delivered], 99)},
        "n_tells": len(tells), "n_delivered": len(delivered), "n_double_sends": n_dup,
        "n_sqlite_busy": runner.n_busy, "failures": runner.failures[:20], "n_failures": len(runner.failures),
    }, indent=1))


def main():
    amh, url, workdir = sys.argv[1:4]
    runner = Runner(amh, workdir, "--strace" in sys.argv)
    os.makedirs(f"{workdir}/strace", exist_ok=True)
    prompt = f"{workdir}/prompt.md"
    with open(prompt, "w") as f:
        f.write("Benchmark task: do nothing.\n")

    def start(name):
        return runner.run("start", ["task", "start", name, "--tool", "claude", "--dir", workdir, "--prompt", prompt, "--manager", "omnigent://bench-mgr"])

    for i in range(N_SEEDS):
        if start(f"seed{i}.md")[2] != 0:
            sys.exit(f"seeding failed: {runner.failures[-1]}")
    runner.failures.clear()
    if "--rate" in sys.argv:
        rate = float(sys.argv[sys.argv.index("--rate") + 1])
        seconds = float(sys.argv[sys.argv.index("--seconds") + 1])
        return paced(runner, url, workdir, start, rate, seconds)
    seeded = stub(url, "/stub/stats")["sessions_with_messages"]
    tells, starts, statuses = [], [], []
    tells_done = threading.Event()

    def teller(k):
        for i in range(N_TELLS):
            text = f"bench-tell {k}-{i}"
            s, e, rc = runner.run("tell", ["tell", "agent", f"seed{k % N_SEEDS}.md", text, "--from", "bench"])
            tells.append((text, s, e, rc))

    def status_client(k):
        while not tells_done.is_set():
            s, e, _ = runner.run("status", ["agent", "status", f"seed{k % N_SEEDS}.md"])
            statuses.append((e - s) / 1e6)

    began_ns = time.time_ns()
    threads = [threading.Thread(target=lambda i=i: starts.append(start(f"load{i}.md"))) for i in range(N_STARTS)]
    tellers = [threading.Thread(target=teller, args=(k,)) for k in range(N_TELLERS)]
    threads += tellers + [threading.Thread(target=status_client, args=(k,)) for k in range(N_STATUS)]
    for t in threads:
        t.start()
    for t in tellers:
        t.join()
    tells_done.set()
    for t in threads:
        t.join()
    deadline = time.time() + START_TIMEOUT_S
    while (stats := stub(url, "/stub/stats"))["sessions_with_messages"] < seeded + N_STARTS and time.time() < deadline:
        time.sleep(0.5)
    started_ns = time.time_ns()
    # The outbox delivers after the command returns: wait (at most 120 s) for every accepted tell to arrive.
    want = sum(rc == 0 for *_, rc in tells)
    drain = time.time() + 120
    while len(received(url)[0]) < want and time.time() < drain:
        time.sleep(0.5)
    drained_ns = time.time_ns()
    recv, n_dup = received(url)
    tell_ms = [(e - s) / 1e6 for _, s, e, _ in tells]
    delivered = [(t, s, e) for t, s, e, rc in tells if rc == 0 and t in recv]
    waits, n_flock = lock_waits(workdir)
    print(json.dumps({
        "tell_ms": {"p50": pct(tell_ms, 50), "p99": pct(tell_ms, 99), "n": len(tell_ms)},
        "delivery_after_return_ms": {"p50": pct([max(0, recv[t] - e) / 1e6 for t, _, e in delivered], 50),
                                     "p99": pct([max(0, recv[t] - e) / 1e6 for t, _, e in delivered], 99)},
        "delivery_from_tell_start_ms": {"p50": pct([(recv[t] - s) / 1e6 for t, s, _ in delivered], 50),
                                        "p99": pct([(recv[t] - s) / 1e6 for t, s, _ in delivered], 99)},
        "n_tells_ok": sum(rc == 0 for *_, rc in tells), "n_delivered": len(delivered),
        "status_ms": {"p50": pct(statuses, 50), "p99": pct(statuses, 99), "n": len(statuses)},
        "start_cmd_ms": {"p50": pct([(e - s) / 1e6 for s, e, _ in starts], 50), "p99": pct([(e - s) / 1e6 for s, e, _ in starts], 99)},
        "start_wall_s": round((started_ns - began_ns) / 1e9, 1),
        "drain_after_starts_s": round((drained_ns - started_ns) / 1e9, 1),
        "sessions_started": stats["sessions_with_messages"] - seeded,
        "max_create_in_flight": stats["max_create_in_flight"],
        "lock_wait_ms": {"p50": pct(waits, 50), "p99": pct(waits, 99), "max": pct(waits, 100), "n": len(waits)} if runner.use_strace else None,
        "flock_calls_by_kind": n_flock if runner.use_strace else None,
        "n_double_sends": n_dup,
        "n_sqlite_busy": runner.n_busy,
        "failures": runner.failures[:20], "n_failures": len(runner.failures),
    }, indent=1))


if __name__ == "__main__":
    main()
