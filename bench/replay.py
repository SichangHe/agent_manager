"""Replay an hour of agents' amh commands from amh-actions.log against an isolated copy (run through `run.sh`-style env).

Usage: replay.py AMH ACTIONS_LOG START END [SPEEDUP]
Each logged command whose arguments can be rebuilt from its JSON is run at its logged offset (divided by SPEEDUP), as its
logged caller (OMNIGENT_RUNNER_PRIMARY_SESSION_ID), through the daemon (AMH_CLIENT=1). Commands that reach people or
agents go to the Omnigent stub or fail harmlessly (no Zulip, no mail credentials in the copy). Prints what ran and skipped.
"""

import json
import os
import re
import subprocess
import sys
import threading
import time
from datetime import datetime

amh, log, start, end = sys.argv[1:5]
speedup = float(sys.argv[5]) if len(sys.argv) > 5 else 1.0


def flag(argv, name, value):
    if value not in (None, False, "", []):
        argv += [name] if value is True else [name, str(value)]


def build(v):
    """argv for a logged command, or None when it cannot be rebuilt."""
    g, a = v.get("group"), v.get("action")
    tf = v.get("task_file")
    pre = ["--task-file", tf] if tf else []
    if g == "tell" and a == "human":
        argv = pre + ["tell", "human"]
        for d in v.get("done") or []:
            argv += ["--done", d]
        for x in v.get("add") or []:
            argv += ["--add", x]
        if v.get("text"):
            argv.append(v["text"])
        elif v.get("file"):
            argv += ["--file", v["file"]]
        else:
            return None
        return argv
    if g == "tell" and a == "manager":
        argv = pre + ["tell", "manager"]
        flag(argv, "--done", v.get("done"))
        flag(argv, "--blocked", v.get("blocked"))
        if not v.get("text"):
            return None
        return argv + [v["text"]]
    if g == "tell" and a == "agent":
        if not v.get("text") or not v.get("who"):
            return None
        return pre + ["tell", "agent", v["who"], v["text"]]
    if g == "todo" and a in ("add",):
        items = v.get("text") or v.get("items") or []
        items = items if isinstance(items, list) else [items]
        return pre + ["todo", "add", *items] if items else None
    if g == "todo" and a == "done":
        items = v.get("items") or v.get("text") or []
        items = items if isinstance(items, list) else [items]
        if not items:
            return None
        argv = pre + ["todo", "done", *items]
        flag(argv, "--evidence", v.get("evidence"))
        return argv
    if g == "todo" and a == "wait":
        argv = pre + ["todo", "wait", v["id"]]
        for b in v.get("blocker") or []:
            argv += ["--for", b]
        flag(argv, "--until", v.get("until"))
        return argv
    if g == "todo" and a in ("list",):
        return pre + ["todo", "list"]
    if g == "todo" and a in ("approve", "reject"):
        return pre + ["todo", a, v["id"]] + (["--reason", v["reason"]] if v.get("reason") else [])
    if g == "todo" and a == "ask-human":
        return pre + ["todo", "ask-human", v["text"]] if v.get("text") else None
    if g == "task" and a == "status":
        argv = pre + ["task", "status", v["task"], v["status"]]
        flag(argv, "--on", v.get("on"))
        flag(argv, "--until", v.get("until"))
        return argv
    if g == "task" and a == "note":
        return pre + ["task", "note", v["task"], v["text"]]
    if g == "task" and a == "priority":
        return pre + ["task", "priority", v["task"], str(v["priority"])]
    if g == "agent" and a in ("status", "list"):
        return ["agent", a] + ([v["who"]] if v.get("who") else [])
    if g == "short" and a in ("ls", "human"):
        return pre + [a] if a == "ls" else None
    return None


rows = []
for line in open(log, errors="replace"):
    t = line[:19]
    if not (start <= t < end) or "{" not in line:
        continue
    m = re.match(r"\S+ \S+ (\S+) ", line)
    try:
        v = json.loads(line[line.index("{"):])
    except ValueError:
        continue
    argv = build(v)
    rows.append((t, m.group(1) if m else "", argv, f'{v.get("group")} {v.get("action")}'))

t0 = datetime.strptime(start, "%Y-%m-%d %H:%M:%S")
began = time.time()
ran, skipped, failed = {}, {}, {}
threads = []


def one(caller, argv, kind):
    env = dict(os.environ, AMH_CLIENT="1")
    if caller.startswith("omnigent://"):
        env["OMNIGENT_RUNNER_PRIMARY_SESSION_ID"] = caller.removeprefix("omnigent://")
    done = subprocess.run([amh, *argv], env=env, capture_output=True, text=True, timeout=300)
    ran[kind] = ran.get(kind, 0) + 1
    if done.returncode:
        failed[kind] = failed.get(kind, 0) + 1


for t, caller, argv, kind in rows:
    if argv is None:
        skipped[kind] = skipped.get(kind, 0) + 1
        continue
    due = (datetime.strptime(t, "%Y-%m-%d %H:%M:%S") - t0).total_seconds() / speedup
    time.sleep(max(0, due - (time.time() - began)))
    th = threading.Thread(target=one, args=(caller, argv, kind))
    th.start()
    threads.append(th)
for th in threads:
    th.join()
print(json.dumps({"ran": ran, "failed": failed, "skipped": skipped, "seconds": round(time.time() - began)}, indent=1))
