"""Crash test: `amh tell agent` load while the daemon is killed (SIGKILL, or aborted at a fault point) and restarted.

Usage: `crash.py AMH STUB_URL WORKDIR`, under the isolated environment of `run.sh` (BENCH_CRASH=1).
Checks: every command answers 0 (no request lost), every message arrives (none lost), and counts duplicates,
which the outbox allows only for a send whose result was not yet stored when the daemon died.
"""

import json
import os
import random
import signal
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import N_SEEDS, Runner, stub  # noqa: E402

N_TELLERS, N_TELLS = 10, 40
SOCKET = os.path.join(os.environ["AMH_STATE_DIR"], "amhd.sock")


class Daemon:
    def __init__(self, amh, workdir):
        self.amh, self.log, self.proc, self.n = amh, open(f"{workdir}/daemon.log", "a"), None, 0

    def start(self, crash=None):
        env = dict(os.environ, AMH_DAEMON="1", AMH_CLIENT="0")
        env.pop("AMHD_CRASH", None)
        if crash:
            env["AMHD_CRASH"] = crash
        self.proc = subprocess.Popen([self.amh, "daemon"], env=env, stdout=self.log, stderr=self.log)
        self.n += 1
        for _ in range(400):
            if os.path.exists(SOCKET) and self.proc.poll() is None:
                try:
                    import socket
                    s = socket.socket(socket.AF_UNIX)
                    s.connect(SOCKET)
                    s.close()
                    return
                except OSError:
                    pass
            time.sleep(0.01)
        raise SystemExit("daemon did not start")

    def kill(self):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGKILL)
        self.proc.wait()


def received(url):
    counts = {}
    for m in stub(url, "/stub/messages"):
        for word in m["text"].split():
            if word.startswith("crash-"):
                counts[word] = counts.get(word, 0) + 1
    return counts


def scenario(name, amh, url, workdir, runner, daemon, crash=None, killer=False):
    """Run tells while the daemon dies; returns the scenario's result."""
    sent, codes, crashes = [], [], [0]
    stop = threading.Event()

    def teller(k):
        for i in range(N_TELLS):
            text = f"crash-{name}-{k}-{i}"
            _, _, rc = runner.run("tell", ["tell", "agent", f"seed{k % N_SEEDS}.md", text, "--from", "bench"])
            sent.append(text)
            codes.append(rc)

    def kill_loop():
        rng = random.Random(7)
        while not stop.is_set():
            time.sleep(rng.uniform(0.3, 1.5))
            if stop.is_set():
                break
            daemon.kill()
            crashes[0] += 1
            daemon.start()

    def watch_abort():
        # A fault point aborts the daemon by itself; restart it without the fault.
        while not stop.is_set():
            if daemon.proc.poll() is not None:
                crashes[0] += 1
                daemon.start()
            time.sleep(0.01)

    daemon.kill()
    daemon.start(crash)
    helper = threading.Thread(target=kill_loop if killer else watch_abort)
    helper.start()
    threads = [threading.Thread(target=teller, args=(k,)) for k in range(N_TELLERS)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    stop.set()
    helper.join()
    deadline = time.time() + 60
    while time.time() < deadline:
        got = received(url)
        if all(got.get(t, 0) for t in sent):
            break
        time.sleep(0.5)
    got = received(url)
    return {
        "scenario": name, "crashes": crashes[0], "n_tells": len(sent),
        "n_requests_failed": sum(rc != 0 for rc in codes),
        "n_lost": sum(not got.get(t, 0) for t in sent),
        "n_duplicates": sum(max(0, got.get(t, 0) - 1) for t in sent),
        # Documented duplicates: only rows in flight (`sending`, at most one per recipient) when the daemon died.
        "duplicates_within_bound": sum(max(0, got.get(t, 0) - 1) for t in sent) <= crashes[0] * min(N_TELLERS, N_SEEDS),
    }


def main():
    amh, url, workdir = sys.argv[1:4]
    runner = Runner(amh, workdir, False)
    daemon = Daemon(amh, workdir)
    daemon.start()
    prompt = f"{workdir}/prompt.md"
    with open(prompt, "w") as f:
        f.write("Benchmark task: do nothing.\n")
    for i in range(N_SEEDS):
        _, _, rc = runner.run("start", ["task", "start", f"seed{i}.md", "--tool", "claude", "--dir", workdir, "--prompt", prompt, "--manager", "omnigent://bench-mgr"])
        if rc:
            sys.exit(f"seeding failed: {runner.failures[-1]}")
    results = []
    for point, n in (("before-commit", 40), ("after-commit", 40), ("after-send", 25)):
        results.append(scenario(point, amh, url, workdir, runner, daemon, crash=f"{point}:{n}"))
    results.append(scenario("sigkill", amh, url, workdir, runner, daemon, killer=True))
    daemon.kill()
    import sqlite3
    db = sqlite3.connect(os.path.join(os.environ["AMH_STATE_DIR"], "amh.sqlite3"))
    states = dict(db.execute("SELECT state, COUNT(*) FROM outbox GROUP BY state").fetchall())
    unanswered = db.execute("SELECT COUNT(*) FROM request_outcomes WHERE code IS NULL").fetchone()[0]
    ok = all(r["n_requests_failed"] == 0 and r["n_lost"] == 0 and r["duplicates_within_bound"] for r in results)
    ok = ok and states.get("dead", 0) == 0 and states.get("sending", 0) == 0 and states.get("pending", 0) == 0
    print(json.dumps({
        "pass": ok,
        "outbox_states": states,
        "requests_without_outcome": unanswered,
        "results": results,
        "failures": runner.failures[:10],
    }, indent=1))


if __name__ == "__main__":
    main()
