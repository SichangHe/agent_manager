"""Stage-3 upgrade drills: switch the daemon between two builds under load, as `amh release` and `amh release --rollback` do (SIGTERM the running daemon, start the other build).

Usage: `drills.py AMH STUB_URL WORKDIR` under the isolated environment of `run.sh` (BENCH_DRILL=1 with BENCH_DRILL_OLD=OLD_BINARY).
AMH is the new build (HEAD), BENCH_DRILL_OLD the previous release. 10 upgrades (old -> new) and 10 rollbacks (new -> old), one every
DRILL_EVERY_S seconds, while clients send open-loop load (DRILL_RATE commands/s: 30 % `tell agent`, 70 % `agent status`).
Gates (docs/rework-and-upgrades.md, stage 3): zero refused requests, restart gap p99 < 1 s (SIGTERM to the new daemon accepting),
no request applied twice, message duplicate rate <= 1 per 10,000.
"""

import json
import os
import random
import signal
import socket
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import N_SEEDS, Runner, stub, pct  # noqa: E402

SWITCHES, EVERY_S = 20, float(os.environ.get("DRILL_EVERY_S", "6"))
RATE = float(os.environ.get("DRILL_RATE", "54"))
TELL_SHARE = float(os.environ.get("DRILL_TELL_SHARE", "0.3"))
STATE = os.environ["AMH_STATE_DIR"]
SOCKET = os.path.join(STATE, "amhd.sock")


def accepting():
    s = socket.socket(socket.AF_UNIX)
    try:
        s.connect(SOCKET)
        return True
    except OSError:
        return False
    finally:
        s.close()


class Daemon:
    def __init__(self, workdir):
        self.log, self.proc = open(f"{workdir}/daemon.log", "a"), None

    def start(self, binary):
        env = dict(os.environ, AMH_DAEMON="1", AMH_CLIENT="0")
        self.proc = subprocess.Popen([binary, "daemon"], env=env, stdout=self.log, stderr=self.log)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()


def main():
    new, url, workdir = sys.argv[1:4]
    old = os.environ["BENCH_DRILL_OLD"]
    runner = Runner(new, workdir, False)
    daemon = Daemon(workdir)
    daemon.start(old)
    while not accepting():
        time.sleep(0.01)
    with open(f"{workdir}/prompt.md", "w") as f:
        f.write("Benchmark task: do nothing.\n")
    for i in range(N_SEEDS):
        _, _, rc = runner.run("start", ["task", "start", f"seed{i}.md", "--tool", "claude", "--dir", workdir, "--prompt", f"{workdir}/prompt.md", "--manager", "omnigent://bench-mgr"])
        if rc:
            sys.exit(f"seeding failed: {runner.failures[-1]}")
    runner.failures.clear()
    stop = threading.Event()
    sent, codes, lat = [], [], {"tell": [], "status": []}
    seq = [0]

    def one(kind, n):
        if kind == "tell":
            text = f"drill-{n}"
            s, e, rc = runner.run("tell", ["tell", "agent", f"seed{n % N_SEEDS}.md", text, "--from", "bench"])
            sent.append(text)
        else:
            s, e, rc = runner.run("status", ["agent", "status", f"seed{n % N_SEEDS}.md"])
        codes.append(rc)
        lat[kind].append((e - s) / 1e6)

    def load():
        rng = random.Random(3)
        threads = []
        due = time.time()
        while not stop.is_set():
            due += rng.expovariate(RATE)
            time.sleep(max(0, due - time.time()))
            seq[0] += 1
            t = threading.Thread(target=one, args=("tell" if rng.random() < TELL_SHARE else "status", seq[0]))
            t.start()
            threads.append(t)
        for t in threads:
            t.join()

    loader = threading.Thread(target=load)
    loader.start()
    gaps, binaries = [], [new, old] * (SWITCHES // 2)
    for to in binaries:
        time.sleep(EVERY_S)
        began = time.time()
        daemon.stop()
        daemon.start(to)
        while not accepting():
            time.sleep(0.002)
        gaps.append((time.time() - began) * 1000)
    time.sleep(EVERY_S)
    stop.set()
    loader.join()
    deadline = time.time() + 120
    counts = {}
    while time.time() < deadline:
        counts = {}
        for m in stub(url, "/stub/messages"):
            for word in m["text"].split():
                if word.startswith("drill-"):
                    counts[word] = counts.get(word, 0) + 1
        if all(counts.get(t) for t in sent):
            break
        time.sleep(0.5)
    daemon.stop()
    import sqlite3
    db = sqlite3.connect(os.path.join(STATE, "amh.sqlite3"))
    rows_per_body = db.execute("SELECT MAX(n) FROM (SELECT COUNT(*) n FROM outbox WHERE body LIKE '%drill-%' GROUP BY body)").fetchone()[0]
    dups = sum(max(0, counts.get(t, 0) - 1) for t in sent)
    out = {
        "switches": len(gaps), "requests": len(codes), "n_tells": len(sent),
        "refused_or_failed": sum(rc != 0 for rc in codes),
        "restart_gap_ms": {"p50": pct(gaps, 50), "p99": pct(gaps, 99), "max": pct(gaps, 100)},
        "tell_ms": {"p50": pct(lat["tell"], 50), "p99": pct(lat["tell"], 99)},
        "status_ms": {"p50": pct(lat["status"], 50), "p99": pct(lat["status"], 99)},
        "lost": sum(not counts.get(t) for t in sent),
        "requests_applied_twice": max(0, (rows_per_body or 1) - 1),
        "duplicates": dups, "duplicate_rate_per_10k": round(dups * 10000 / max(1, len(sent)), 2),
        "failures": runner.failures[:5],
    }
    out["pass"] = (out["refused_or_failed"] == 0 and out["restart_gap_ms"]["p99"] < 1000 and out["lost"] == 0
                   and out["requests_applied_twice"] == 0 and out["duplicate_rate_per_10k"] <= 1)
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
