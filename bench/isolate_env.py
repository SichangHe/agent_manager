"""Print an isolated `env.toml`: a copy of SOURCE (may be missing) pointing every connector at the stub.

Usage: `isolate_env.py SOURCE WORK_LOGS STUB_URL`.
Keeps the `[mail]` table with `human_address` set to `agent_address`, so no mail can reach the human;
drops every other table (guest, mail routes, fallback, quota, zulip) and every top-level key, then sets
`work_logs`, `main_manager`, `omnigent.url`, and the proxy URLs to the stub.
"""

import json
import re
import sys

source, work_logs, url = sys.argv[1:4]
try:
    lines = open(source).read().splitlines()
except FileNotFoundError:
    lines = []
mail, table = {}, None
for line in lines:
    header = re.match(r"^\s*\[([^\]]+)\]\s*$", line)
    if header:
        table = header.group(1).strip()
        continue
    pair = re.match(r'^\s*([A-Za-z0-9_]+)\s*=\s*(.+?)\s*$', line)
    if table == "mail" and pair:
        mail[pair.group(1)] = pair.group(2)
agent = mail.get("agent_address", json.dumps("bench@localhost.invalid"))
mail.update(agent_address=agent, human_address=agent)
q = json.dumps
print(f"work_logs = {q(work_logs)}\nmain_manager = {q('omnigent://bench-mgr')}\n")
print("[mail]\n" + "".join(f"{k} = {v}\n" for k, v in mail.items()))
print(f"[omnigent]\nurl = {q(url)}\ntoken = \"\"\n")
print(f"[fallback]\ncodex_to_claude = false\nproxy_url = {q(url)}\n")
print(f"[quota]\nproxy_url = {q(url)}")
