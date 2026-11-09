"""Merged per-message E2E latency timeline keyed on the Zulip message id.

Usage: `python3 -I timeline.py <zulip_msg_id> [--topic T]`, `python3 -I timeline.py --last N [--topic T]`,
`python3 -I timeline.py --summary N [--topic T]`. Read-only; design in `timeline.md`.
"""
import argparse, base64, configparser, json, re, statistics, subprocess, sys, urllib.parse, urllib.request
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

HOME = Path.home()
CHANNEL = "AMH"
SERVER_LOGS = HOME / ".omnigent/logs/server"
PROBE_LOG = HOME / ".local/state/zulip-voice/latency.jsonl"
# Inferred server join: POST arrival within this many s of delivery (negative = before). Arrival precedes the delivered log.
INFER_WINDOW_S = (-60, 2)
CLIENT_LOG = HOME / ".local/state/zulip-voice/relay/client.log"
# Segments in order: (name, from-mark, to-mark). Marks are server-clock epoch seconds.
SEGMENTS = [
    ("human_post>amh_queued", "post", "queued"),
    ("amh_queued>received", "queued", "received"),
    ("amh_received>delivered", "received", "delivered"),
    ("delivered>reply_posted", "delivered", "reply"),
    ("reply>tts_ready", "reply", "tts_ready"),
]
TOTAL = "human_post>tts_ready"


@dataclass
class Trip:
    msg_id: int
    topic: str
    marks: dict[str, float] = field(default_factory=dict)
    notes: list[str] = field(default_factory=list)
    phone: list[str] = field(default_factory=list)
    attempts: dict[str, list[float]] = field(default_factory=dict)  # later queued/received/delivered times

    def spans(self) -> dict[str, float | None]:
        m = self.marks
        out = {n: (m[b] - m[a] if a in m and b in m else None) for n, a, b in SEGMENTS}
        out[TOTAL] = m["tts_ready"] - m["post"] if "tts_ready" in m else None
        return out


def zulip_get(path: str, params: dict[str, str]) -> dict:
    cfg = configparser.ConfigParser()
    cfg.read(HOME / ".config/zulip-voice/zuliprc")
    a = cfg["api"]
    auth = base64.b64encode(f"{a['email']}:{a['key']}".encode()).decode()
    req = urllib.request.Request(f"{a['site']}/api/v1/{path}?{urllib.parse.urlencode(params)}",
                                 headers={"Authorization": f"Basic {auth}"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.load(r)


def iso2epoch(s: str) -> float:
    s = re.sub(r"(\.\d{6})\d*", r"\1", s)
    return datetime.fromisoformat(s).timestamp()


def fetch_zulip(topic: str, anchor: str, before: int, after: int) -> list[dict]:
    narrow = json.dumps([{"operator": "channel", "operand": CHANNEL}, {"operator": "topic", "operand": topic}])
    d = zulip_get("messages", {"anchor": anchor, "num_before": str(before), "num_after": str(after),
                               "narrow": narrow, "apply_markdown": "false"})
    return d["messages"]


def pair_trips(msgs: list[dict], human_id: int) -> list[Trip]:
    """Each human message paired with the first non-human message after it, if before the next human message."""
    trips = []
    for i, m in enumerate(msgs):
        if m["sender_id"] != human_id:
            continue
        t = Trip(m["id"], m["subject"], {"post": float(m["timestamp"])})
        for r in msgs[i + 1:]:
            if r["sender_id"] == human_id:
                break
            t.marks["reply"] = float(r["timestamp"])
            t.notes.append(f"reply_id={r['id']}")
            break
        trips.append(t)
    return trips


def amh_events(topic: str) -> list[dict]:
    out = subprocess.run(["amh", "log", f"{topic}.md", "-n", "5000"], capture_output=True, text=True, timeout=60).stdout
    evs = []
    for line in out.splitlines():
        if line.startswith("{"):
            try:
                evs.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return evs


def add_amh(t: Trip, evs: list[dict]) -> str | None:
    """Fills queued/received/delivered marks with the first occurrence; later ones go to `attempts`.

    Returns the session id of the first delivery.
    """
    tag = f"zulip-{t.msg_id}.txt"
    sid = None
    for e in evs:
        d = json.dumps(e.get("data", {}))
        if tag not in d:
            continue
        k = {"message queued": "queued", "human mail received": "received", "message delivered": "delivered"}.get(e["kind"])
        if k:
            ts = iso2epoch(e["time"])
            if k in t.marks:
                t.attempts.setdefault(k, []).append(ts)
                continue
            t.marks[k] = ts
            if k == "delivered":
                sid = e["data"]["to"].rsplit("/", 1)[-1]
    return sid


def server_records(sids: set[str], t0: float) -> list[dict]:
    """Latency JSON records for the sessions, from server logs modified since t0 (greps; logs can be GBs)."""
    files = [f for f in SERVER_LOGS.glob("server-*.log") if f.stat().st_mtime >= t0]
    if not files or not sids:
        return []
    pat = '"session_id": "(' + "|".join(sids) + ')"'
    recs = []
    for f in sorted(files):
        p = subprocess.run(f"grep -E 'latency \\{{\"stage\"' {f} | grep -E '{pat}'", shell=True, capture_output=True,
                           text=True, timeout=300)
        for line in p.stdout.splitlines():
            recs.append(json.loads(line[line.index("latency {") + 8:]))
    return recs


def add_server(t: Trip, sid: str, recs: list[dict]) -> None:
    """Joins on `zulip_message_id` if the server logs it (measured), else guesses (inferred, not measured).

    The guess is the latest events POST arriving within `INFER_WINDOW_S` of delivery; it only adds a note, no marks.
    """
    mine = [r for r in recs if r["session_id"] == sid]
    exact = {r["request_id"] for r in mine if str(r.get("zulip_message_id")) == str(t.msg_id)}
    d = t.marks.get("delivered")
    if exact:
        rid, how = next(iter(exact)), "measured"
    else:
        near = [r for r in mine if r["stage"] == "http.arrival" and r.get("method") == "POST"
                and d is not None and INFER_WINDOW_S[0] <= r["time_ns"] / 1e9 - d <= INFER_WINDOW_S[1]]
        if not near:
            t.notes.append("server: no records")
            return
        rid, how = max(near, key=lambda r: r["time_ns"])["request_id"], "inferred, not measured"
    st = {r["stage"]: r["time_ns"] / 1e9 for r in mine if r["request_id"] == rid}
    ack = st.get("runner.dispatch_ack") or st.get("http.response_headers")
    arr = st.get("http.arrival")
    if how == "measured":
        if arr:
            t.marks["server_arrival"] = arr
        if ack:
            t.marks["server_ack"] = ack
    t.notes.append(f"server join={how} req={rid[:8]}"
                   + (f" arrival>ack={(ack - arr) * 1000:.0f}ms" if arr and ack else "")
                   + (f" arrival-delivered={arr - d:+.1f}s" if arr and d else ""))


def add_tts(trips: list[Trip]) -> None:
    """Audio-ready = journal time of the relay's last `tts id=proactive_<reply>_<n>` line (logged on completion)."""
    replies = [t for t in trips if "reply" in t.marks]
    if not replies:
        return
    since = datetime.fromtimestamp(min(t.marks["reply"] for t in replies) - 5).strftime("%Y-%m-%d %H:%M:%S")
    out = subprocess.run(["journalctl", "--user", "-u", "zulip-voice-relay", "--since", since, "--no-pager",
                          "-o", "short-unix"], capture_output=True, text=True, timeout=60).stdout
    rx = re.compile(r"^(\d+\.\d+) .*tts id=proactive_(\d+)_(\d+) .*?work=(\d+)ms")
    by_reply: dict[str, list[tuple[float, str]]] = {}
    for line in out.splitlines():
        m = rx.match(line)
        if m:
            by_reply.setdefault(m[2], []).append((float(m[1]), f"chunk{m[3]} work={m[4]}ms"))
    for t in replies:
        hits = by_reply.get(next(n.split("=")[1] for n in t.notes if n.startswith("reply_id=")))
        if hits:
            t.marks["tts_ready"] = max(h[0] for h in hits)
            t.notes.append("tts " + ", ".join(h[1] for h in hits))


def add_phone(t: Trip) -> None:
    """Phone clock is untrusted: only relative spans between phone lines of the same reply."""
    rid = next((n.split("=")[1] for n in t.notes if n.startswith("reply_id=")), None)
    if not rid or not CLIENT_LOG.exists():
        return
    p = subprocess.run(["grep", "-E", f"(bundle active_{rid}_|audio  start {rid}:|send id={t.msg_id})", str(CLIENT_LOG)],
                       capture_output=True, text=True, timeout=30)
    ts = []
    for line in p.stdout.splitlines()[:6]:
        h, mi, s = line.split(" ", 1)[0].split(":")
        ts.append(int(h) * 3600 + int(mi) * 60 + float(s))
        t.phone.append(line[:90])
    if len(ts) > 1:
        t.notes.append(f"phone(relative) first>last line = {ts[-1] - ts[0]:.2f}s")


def build(trips: list[Trip], topic: str) -> None:
    evs = amh_events(topic)
    sess = {t.msg_id: s for t in trips if (s := add_amh(t, evs))}
    sids = set(sess.values())
    recs = server_records(sids, min(t.marks["post"] for t in trips)) if trips else []
    for t in trips:
        if t.msg_id in sess:
            add_server(t, sess[t.msg_id], recs)
    add_tts(trips)
    for t in trips:
        add_phone(t)


def fmt_abs(e: float) -> str:
    return datetime.fromtimestamp(e).strftime("%H:%M:%S.%f")[:-3]


def show(t: Trip) -> None:
    print(f"msg {t.msg_id} topic {t.topic} (server clock; Zulip times are whole seconds)")
    for k in ["post", "queued", "received", "delivered", "server_arrival", "server_ack", "reply", "tts_ready"]:
        if k in t.marks:
            print(f"  {fmt_abs(t.marks[k])}  {k}")
    for n, v in t.spans().items():
        print(f"  {n:26} {'-' if v is None else f'{v:7.2f}s'}")
    for k, v in t.attempts.items():
        print(f"  later {k} attempts: {len(v)} at " + ", ".join(fmt_abs(x) for x in v))
    for line in t.notes:
        print(f"  note: {line}")
    for line in t.phone:
        print(f"  phone(relative only): {line}")
    print("JSON " + json.dumps({"msg_id": t.msg_id, "marks": t.marks, "spans_s": t.spans(), "attempts": t.attempts,
                       "notes": t.notes}))


def summary(trips: list[Trip]) -> None:
    rows = {}
    for n in [s[0] for s in SEGMENTS] + [TOTAL]:
        v = sorted(x for t in trips if (x := t.spans()[n]) is not None)
        if v:
            rows[n] = (len(v), statistics.median(v), v[min(len(v) - 1, int(0.9 * len(v)))])
    print(f"{len(trips)} round trips; seconds")
    print(f"  {'segment':26} {'n':>3} {'median':>7} {'p90':>7}")
    for n, (c, med, p90) in rows.items():
        print(f"  {n:26} {c:3} {med:7.2f} {p90:7.2f}")
    segs = {n: r for n, r in rows.items() if n != TOTAL}
    worst = max(segs, key=lambda n: segs[n][1])
    print(f"worst segment by median: {worst} ({segs[worst][1]:.2f}s); by p90: {max(segs, key=lambda n: segs[n][2])}")
    print("JSON " + json.dumps({n: {"n": c, "median_s": m, "p90_s": p} for n, (c, m, p) in rows.items()}))


def probe_summary(path: Path, n: int) -> None:
    """Separate section: last `n` voice probe records (synthetic probe, not Zulip round trips); ms fields."""
    if not path.exists():
        print("probe: no log")
        return
    recs = [json.loads(l) for l in path.read_text().splitlines() if l.startswith("{")][-n:]
    print(f"probe ({len(recs)} records from {path.name}; synthetic, separate from round trips); ms")
    print(f"  {'field':32} {'n':>3} {'median':>7} {'p90':>7} {'max':>7}")
    out = {}
    flat = [{**{k: v for k, v in r.items() if k.endswith("_ms")},
             **{f"{p}.{k}": v for p in ("sw_relay", "play_relay") for k, v in (r.get(p) or {}).items()}} for r in recs]
    for k in dict.fromkeys(k for r in flat for k in r):
        v = sorted(x for r in flat if isinstance(x := r.get(k), (int, float)))
        if v:
            out[k] = {"n": len(v), "median": statistics.median(v), "p90": v[min(len(v) - 1, int(0.9 * len(v)))], "max": v[-1]}
            print(f"  {k:32} {len(v):3} {out[k]['median']:7.0f} {out[k]['p90']:7.0f} {v[-1]:7.0f}")
    bad = sum(not r.get("play_bytes_ok", True) for r in recs)
    print(f"  play_bytes_ok failures: {bad}")
    print("JSON " + json.dumps({"probe": out, "play_bytes_failures": bad}))


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("msg_id", nargs="?", type=int)
    ap.add_argument("--last", type=int)
    ap.add_argument("--summary", type=int)
    ap.add_argument("--topic", default=None)
    ap.add_argument("--human-id", type=int, default=1212497)
    a = ap.parse_args()
    topic = a.topic
    if a.msg_id:
        topic = topic or zulip_get(f"messages/{a.msg_id}", {})["message"]["subject"]
        msgs = fetch_zulip(topic, str(a.msg_id), 0, 30)
        trips = [t for t in pair_trips(msgs, a.human_id) if t.msg_id == a.msg_id]
    else:
        n = a.last or a.summary
        if not n:
            ap.error("give a message id, --last N or --summary N")
        topic = topic or "human_mgr"
        msgs = fetch_zulip(topic, "newest", 40 * n, 0)
        trips = [t for t in pair_trips(msgs, a.human_id) if "reply" in t.marks][-n:]
    if not trips:
        sys.exit("no round trips found")
    build(trips, topic)
    if a.summary:
        summary(trips)
        probe_summary(PROBE_LOG, a.summary)
    else:
        for t in trips:
            show(t)


if __name__ == "__main__":
    main()
