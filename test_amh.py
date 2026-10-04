"""End-to-end checks of `amh` on a temporary copy of the work-log root; prints only failures.

Run: `/usr/bin/python3 test_amh.py`. Nothing real is touched: Omnigent, IMAP, and SMTP are replaced by fakes,
and settings come from a temporary `env.toml`.
"""

from __future__ import annotations

import contextlib
import imaplib
import io
import os
import random
import re
import shutil
import smtplib
import subprocess
import sys
import tempfile
import time
import traceback
from email import message_from_bytes, policy
from email.message import EmailMessage
from pathlib import Path

REPO = Path(__file__).resolve().parent
BINARY = Path(os.environ.get("AMH_BINARY") or REPO / "target/release/amh")
if "amh_rs" not in sys.builtin_module_names:
    # Run inside the binary, so the Rust parts are exercised too.
    os.execve(BINARY, [str(BINARY)], {**os.environ, "AMH_RUN_SCRIPT": __file__})

import amh_rs  # noqa: E402

# The task files to copy come from the live root named in the user's own settings; nothing else of them is used.
LIVE_ROOT = Path(os.environ.get("AMH_TEST_SOURCE_ROOT") or amh_rs.settings()["work_logs"])
TEMP = Path(tempfile.mkdtemp(prefix="amh-test-"))
ROOT, STATE, WORKDIR = TEMP / "root", TEMP / "state", TEMP / "work"
MAIN, GUEST, HUMAN, AGENT = "omnigent://main", "guest@example.org", "human@example.com", "agent@example.com"
PNG = b"\x89PNG\r\n\x1a\n" + bytes(40)

for key in [key for key in os.environ if key.startswith(("OMO_", "AMH_", "OMNIGENT_", "TMUX"))]:
    del os.environ[key]
os.environ |= {"AMH_STATE_DIR": str(STATE), "AMH_CFG_DIR": str(TEMP / "config")}
for folder in (ROOT / "manager_mail", STATE, WORKDIR):
    folder.mkdir(parents=True)
for source in LIVE_ROOT.glob("*.md"):
    _ = shutil.copy(source, ROOT / source.name)
(ROOT / "guest_visitor.md").unlink(missing_ok=True)
(STATE / "env.toml").write_text(
    f"""work_logs = "{ROOT}"
main_manager = "{MAIN}"

[omnigent]
url = "http://127.0.0.1:9"

[mail]
agent_address = "{AGENT}"
agent_password = "none"
human_address = "{HUMAN}"
human_mailbox_config = "{TEMP}/himalaya.toml"
stored_prefix = "m"

[guest]
address = "{GUEST}"
name = "visitor"
workdir = "{WORKDIR}"
""",
    encoding="utf-8",
)
(TEMP / "himalaya.toml").write_text(f'host = "imap.human.test"\nlogin = "{HUMAN}"\ncmd = "echo secret"\n', encoding="utf-8")

from amh import agents, cli, guest, mail, taskfile, watch  # noqa: E402
from amh import config as configuration  # noqa: E402

CONFIG = configuration.load()
assert CONFIG.root == ROOT and CONFIG.state_dir == STATE, "the tests must never run on the live root"
failures: list[str] = []


def check(ok: object, label: str, detail: object = "") -> None:
    if not ok:
        failures.append(f"{label}: {detail}"[:1500])


def amh(*argv: str, me: str | None = None) -> tuple[int | str | None, str]:
    """Run the real command-line entry as the agent at `me`; return its exit status (or refusal text) and output."""
    os.environ.pop("OMNIGENT_RUNNER_PRIMARY_SESSION_ID", None)
    if me:
        os.environ["OMNIGENT_RUNNER_PRIMARY_SESSION_ID"] = me.removeprefix("omnigent://")
    parse = subprocess.run([BINARY, *argv], env={**os.environ, "AMH_PARSE_ONLY": "1"}, capture_output=True, text=True, check=False)
    if parse.returncode or not parse.stdout.startswith("{"):
        return parse.returncode, parse.stdout + parse.stderr
    out = io.StringIO()
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            return cli.run(parse.stdout), out.getvalue()
    except SystemExit as stop:
        return stop.code, out.getvalue()


def task(name: str) -> taskfile.Task:
    return taskfile.load(CONFIG, name)


def text_of(name: str) -> str:
    return (ROOT / name).read_text(encoding="utf-8")


def todo_section(name: str) -> str:
    rows = text_of("TODO.md").split("\n")
    at = [i for i, row in enumerate(rows) if row.split(" ", 1)[0] == name]
    return next(rows[i].strip().lower() for i in range(at[0], -1, -1) if rows[i].strip().lower() in taskfile.SECTIONS) if len(at) == 1 else f"{len(at)} rows"


def email(sender: str, subject: str, body: str, message_id: str, image: bool = False, auth: str = "") -> bytes:
    message = EmailMessage()
    message["From"], message["Subject"], message["Message-ID"] = f"Someone <{sender}>", subject, message_id
    if auth:
        message["Authentication-Results"] = auth
    message.set_content(body)
    if image:
        message.add_attachment(PNG, maintype="image", subtype="png", filename="shot.png")
    return message.as_bytes()


class FakeBox:
    """An IMAP mailbox holding raw emails by uid; answers the few commands `amh` uses."""

    def __init__(self, raws: dict[str, bytes]) -> None:
        self.raws, self.seen, self.moved = raws, set[str](), list[str]()

    def select(self, _folder: str = "INBOX", readonly: bool = False) -> tuple[str, list[bytes]]:
        self.readonly = readonly
        return "OK", [b""]

    def uid(self, command: str, *args: str | bytes) -> tuple[str, list[object]]:
        if command == "search":
            words = [arg for arg in args if isinstance(arg, str)]
            sender = words[words.index("FROM") + 1].strip('"') if "FROM" in words else ""
            tags = [found for word in words for found in re.findall(r"\[[^\]]+\]", word)]
            hits = []
            for uid, raw in self.raws.items():
                parsed = message_from_bytes(raw, policy=policy.default)
                if ("UNSEEN" not in words or uid not in self.seen) and sender in str(parsed["From"]) and all(tag in str(parsed["Subject"]) for tag in tags):
                    hits.append(uid)
            return "OK", [" ".join(hits).encode()]
        uids = (args[0].decode() if isinstance(args[0], bytes) else args[0]).split(",")
        if command == "fetch":
            parts: list[object] = []
            for uid in uids:
                parts += [(f"1 (UID {uid} BODY[] {{{len(self.raws[uid])}}}".encode(), self.raws[uid]), b")"]
            return "OK", parts
        if command == "store":
            self.seen.update(uids)
        elif command == "move":
            self.moved += uids
        return "OK", [b""]

    def login(self, user: str, password: str) -> None:
        self.user = (user, password)

    def shutdown(self) -> None:
        pass


smtp_out: list[EmailMessage] = []


class FakeSmtp:
    def __init__(self, *_args: object, **_kwargs: object) -> None:
        pass

    def __enter__(self) -> FakeSmtp:
        return self

    def __exit__(self, *_args: object) -> None:
        pass

    def login(self, *_args: object) -> None:
        pass

    def send_message(self, message: EmailMessage) -> None:
        smtp_out.append(message)


def forbidden(*_args: object, **_kwargs: object) -> None:
    raise AssertionError("a test reached a real agent or mailbox")


agent_inbox = FakeBox({})
human_inbox = FakeBox({})
smtplib.SMTP_SSL = FakeSmtp  # type: ignore[misc,assignment]
imaplib.IMAP4_SSL = lambda host, timeout: {"imap.gmail.com": agent_inbox, "imap.human.test": human_inbox}[host]  # type: ignore[misc,assignment]
time.sleep = lambda _s: None


def test_round_trip() -> None:
    """Every copied task file parses and re-renders to the same record; a stable file stays byte-identical."""
    n_parsed = 0
    for path in sorted(ROOT.glob("*.md")):
        text = path.read_text(encoding="utf-8")
        try:
            first = taskfile.parse(path.name, text)
        except ValueError:
            continue
        n_parsed += 1
        again = taskfile.parse(path.name, first.render())
        check((again.fields, again.items, again.body) == (first.fields, first.items, first.body), f"round trip {path.name}")
        check(again.render() == first.render(), f"render is stable {path.name}")
    check(n_parsed > 100, "most copied task files parse", n_parsed)
    tricky = ["🧑 fix: the `a: b` case", "- dash", "true", "yes", "it's #1", "ends:", "plain words", "'quoted'", '"double"', " # x"]
    made = taskfile.Task("x.md", {"status": "running", "runat": "h:1", "blocked_on": "a: b"}, tricky, "body\n")
    again = taskfile.parse("x.md", made.render())
    check((again.fields, again.items, again.body) == (made.fields, made.items, made.body), "tricky scalars round trip", again)
    check(taskfile.parse("x.md", '---\nstatus: "running"\nrunat: \'a\'\'b\'\npending_task_items: []\n---\n').fields == {"status": "running", "runat": "a'b"}, "quoted scalars")
    for bad in ("no frontmatter", "---\nstatus: running\n---\n", "---\nstatus: running\nrunat: x\n  stray\n---\n"):
        try:
            _ = taskfile.parse("x.md", bad)
            check(False, "bad task file is refused", bad)
        except ValueError:
            pass


def test_pending_blocks() -> None:
    body = "intro\n(pending)\nfirst a\nfirst b\n\n```\n(pending)\nin code\n```\n  (pending)  \nsecond\n(pending)\n\n(pending)\nlast"
    check(taskfile.pending_blocks(body) == [(1, ["first a", "first b"]), (9, ["second"]), (11, []), (13, ["last"])], "pending blocks", taskfile.pending_blocks(body))
    check(taskfile.pending_blocks("text (pending) inline\n") == [], "an inline marker is not a block")


def test_transport() -> None:
    """The real mail senders, on fake IMAP and SMTP; the Omnigent client is Rust and is exercised in production."""
    agent_inbox.raws = {"5": email(AGENT, "[t_work] Progress", "old", "<a@x>"), "6": email(AGENT, "Re: [t_work] Progress", "old", "<b@x>"), "7": email(AGENT, "[t_worker] Other", "old", "<c@x>")}
    figure = WORKDIR / "figure.png"
    _ = figure.write_bytes(PNG)
    _ = mail.send(CONFIG, "t_work", "", "see the figure", "omnigent://w", [figure])
    attached = [(part.get_filename(), part.get_content_type(), part.get_content()) for part in smtp_out[-1].iter_attachments()]
    check(attached == [("figure.png", "image/png", PNG)] and smtp_out[-1].get_body(("plain",)).get_content() == "see the figure\n", "a file is attached to the email", attached)
    sent_id = mail.send(CONFIG, "t_work", "", "body text", "omnigent://w")
    message = smtp_out[-1]
    check(message["Subject"] == "Re: [t_work] Progress" and message["In-Reply-To"] == "<b@x>" and message["Message-ID"] == sent_id, "reply reuses the tag's last subject", message.items())
    check((message["From"], message["To"], message["X-AMH-From"]) == (AGENT, HUMAN, "omnigent://w") and message.get_content() == "body text\n", "email headers", message.items())
    _ = mail.send(CONFIG, "t_work", "Re: [t_work] New topic", "b", "w")
    check(smtp_out[-1]["Subject"] == "[t_work] New topic" and smtp_out[-1]["In-Reply-To"] is None, "new subject starts a thread", smtp_out[-1].items())
    _ = mail.send(CONFIG, "fresh", "", "b", "w")
    check(smtp_out[-1]["Subject"] == "[fresh] fresh", "first email of a tag is titled by the tag", smtp_out[-1]["Subject"])
    try:
        _ = mail.send(CONFIG, "t_work", "s" * 60, "b", "w")
        check(False, "long subject is refused")
    except SystemExit:
        pass
    human_inbox.raws = {"1": email(AGENT, "[t_work] Progress", "x", "<a@x>"), "2": email(AGENT, "[t_work] Progress", "x", "<b@x>"), "3": email(AGENT, "[other] Progress", "x", "<c@x>")}
    check([h.message_id for h in mail.unread(CONFIG, "t_work", [])] == ["<a@x>", "<b@x>"] and human_inbox.readonly and human_inbox.user == (HUMAN, "secret"), "unread lists the tag's emails read-only, logged in as the human")
    check([h.message_id for h in mail.unread(CONFIG, "t_work", ["<a@x>"])] == ["<b@x>"] and human_inbox.moved == ["1"], "replaced unread email goes to the trash", human_inbox.moved)
    check(mail.tag_of("Re: Fwd: [a_b-1] x [y]") == "a_b-1" and mail.tag_of("x [a]") is None and mail.bare("Re: [a] RE: [b] Title [c]") == "Title [c]", "subject parsing")


sent_mail: list[tuple[str, str, str, str]] = []
messages: list[tuple[str, str]] = []
states: dict[str, tuple[str, str]] = {}
launched: list[tuple[object, ...]] = []
stopped: list[str] = []
down: set[str] = set()
unread_args: list[tuple[str, list[str]]] = []
unread_now: list[mail.Header] = []
logs: list[str] = []


def fake_mail_send(_config: object, tag: str, subject: str, body: str, sender: str) -> str:
    if "mail" in down:
        raise OSError("smtp is down")
    sent_mail.append((tag, subject, body, sender))
    return f"<sent-{len(sent_mail)}@x>"


def fake_send(_config: object, address: str, text: str) -> None:
    if address in down:
        raise agents.AgentError(f"{address} is down")
    messages.append((address, text))


def fake_launch(_config: object, *args: object) -> str:
    launched.append(args)
    return f"omnigent://s{len(launched)}"


def fake_stop(_config: object, address: str) -> None:
    if states.get(address, ("running", ""))[0] == "missing":
        raise agents.AgentError(f"nothing runs at {address}")
    stopped.append(address)


def install_fakes() -> None:
    mail.send = fake_mail_send  # type: ignore[assignment]
    mail.unread = lambda _config, tag, trash: unread_args.append((tag, trash)) or list(unread_now)  # type: ignore[assignment]
    agents.send = fake_send  # type: ignore[assignment]
    agents.status = lambda _config, address: states.get(address, ("running", "fake"))  # type: ignore[assignment]
    agents.launch = fake_launch  # type: ignore[assignment]
    agents.stop = fake_stop  # type: ignore[assignment]
    agents.saw_message = lambda *_args: True  # type: ignore[assignment]
    agents.api_get = forbidden  # type: ignore[assignment]
    watch.log = logs.append  # type: ignore[assignment]


def to(address: str) -> list[str]:
    return [text for target, text in messages if target == address]


def test_task_start() -> None:
    (ROOT / "t_main.md").write_text(f"---\nstatus: long_running\nrunat: {MAIN}\nis_manager: true\npending_task_items: []\n---\n", encoding="utf-8")
    (ROOT / "TODO.md").write_text(text_of("TODO.md").replace("current:\n", f"current:\nt_main.md {MAIN}\n", 1), encoding="utf-8")
    prompt = WORKDIR / "prompt.md"
    prompt.write_text("Build the thing.\n- part one\n", encoding="utf-8")
    (ROOT / "manager_mail/m-7.txt").write_text("Subject: [t_main] Please\n\nline three\nline four\nline five\n", encoding="utf-8")
    code, out = amh("task", "start", "t_work.md", "--dir", str(WORKDIR), "--prompt", str(prompt), "--email", "m-7.txt", "--lines", "3-4", me=MAIN)
    worker = task("t_work.md")
    check((code, out) == (0, "omnigent://s1\n"), "task start prints the address", (code, out))
    check(worker.fields == {"version": "v1.0.0", "status": "running", "managerat": MAIN, "is_manager": "false", "runat": "omnigent://s1", "tool": "codex", "session_id": "s1"}, "worker task fields", worker.fields)
    check(launched[-1] == ("codex", "gpt-6.1-sol", "low", WORKDIR, "t_work"), "worker launch defaults", launched[-1])
    first = to("omnigent://s1")[0]
    delegation = f'<manager_delegation from="{MAIN}">\nBuild the thing.\n- part one\n</manager_delegation>\n<human_instruction authoritative="true" source="manager_mail/m-7.txt:3-4">\nline three\nline four\n</human_instruction>\n'
    check(first.startswith("Task tag: t_work\nTask file: t_work.md\nUse --task-file t_work.md") and "$ getagentsmd\n" in first and "$ getagentsmd get agent_work\n" in first and first.endswith(delegation), "first prompt", first[-600:])
    check(worker.body == delegation, "the request is recorded in the task file", worker.body)
    check(todo_section("t_work.md") == "current:" and "\nt_work.md omnigent://s1\n" in text_of("TODO.md"), "new task is listed under current")
    code, out = amh("task", "start", "t_sub.md", "--dir", str(WORKDIR), "--prompt", str(prompt), "--as-manager", "--manager", "omnigent://s1", "--tool", "codex", "--effort", "medium")
    sub = task("t_sub.md")
    check(code == 0 and (sub.fields["status"], sub.fields["is_manager"], sub.fields["managerat"]) == ("long_running", "true", "omnigent://s1"), "manager task fields", sub.fields)
    check(launched[-1][:3] == ("codex", "gpt-6-luna", "medium") and "$ getagentsmd get submanager\n" in to(sub.address)[0], "manager launch", launched[-1])
    # A task left over from before Omnigent: its address is not a session, so no agent can be reached there.
    (ROOT / "t_tmux.md").write_text(f"---\nversion: v1.0.0\nstatus: running\nmanagerat: {MAIN}\nis_manager: false\nrunat: pb:3\ntool: codex\npending_task_items: []\n---\n", encoding="utf-8")
    (ROOT / "TODO.md").write_text(text_of("TODO.md").replace("current:\n", "current:\nt_tmux.md pb:3\n", 1), encoding="utf-8")
    launched.append(("codex", "x", "low", WORKDIR, "t_tmux"))
    code, out = amh("task", "start", "t_work.md", "--dir", str(WORKDIR), "--prompt", str(prompt), me=MAIN)
    check("already has a live agent" in str(code) and len(launched) == 3, "a live task is not started twice", code)
    check("go together" in str(amh("task", "start", "t_x.md", "--dir", ".", "--prompt", str(prompt), "--email", "m-7.txt", me=MAIN)[0]), "--email needs --lines")
    check("START-END" in str(amh("task", "start", "t_x.md", "--dir", ".", "--prompt", str(prompt), "--email", "m-7.txt", "--lines", "0-2", me=MAIN)[0]), "--lines is validated")
    check("cannot tell which agent" in str(amh("task", "start", "t_x.md", "--dir", ".", "--prompt", str(prompt))[0]) and not (ROOT / "t_x.md").exists(), "an unknown caller cannot be the manager")


def test_todo() -> None:
    me = "omnigent://s1"
    check(amh("todo", "add", "write   the\n code", me=me) == (0, "added 1 open item(s)\n") and not sent_mail, "todo add")
    check(amh("todo", "add", "write the code", me=me) == (0, "added 0 open item(s)\n"), "a duplicate item is not added")
    check(amh("todo", "add", "--from-human", "🧑 ship it", "--task-file", "t_work.md") == (0, "added 1 open item(s)\n"), "todo add --from-human by --task-file")
    check(sent_mail == [("t_work", "", "pending item created:\n- ship it\n", me)], "the human is told their item was created", sent_mail)
    check(amh("todo", "list", me=me) == (0, "write the code\n🧑 ship it\n"), "todo list", amh("todo", "list", me=me))
    os.environ["OMO_AGENT_TASK_FILE"] = "t_work.md"
    check(amh("todo", "list")[1] == "write the code\n🧑 ship it\n", "the task file named at launch identifies the caller")
    del os.environ["OMO_AGENT_TASK_FILE"]
    check("0 active tasks run at omnigent://nobody" in str(amh("todo", "list", me="omnigent://nobody")[0]), "an unknown caller is refused")
    check("empty" in str(amh("todo", "add", " 🧑 ", me=me)[0]), "an empty item is refused")
    check(amh("todo", "replace", "🧑 ship it", "ship it today", me=me) == (0, "replaced 1 open item\n") and task("t_work.md").items == ["write the code", "🧑 ship it today"], "replace keeps the human mark", task("t_work.md").items)
    check("no such open item" in str(amh("todo", "replace", "absent", "x", me=me)[0]), "replace refuses an unknown item")
    check("no such open item" in str(amh("todo", "done", "absent", "--evidence", "e", me=me)[0]), "done refuses an unknown item")
    check(amh("todo", "done", "write the code", "--evidence", "tests\npass", me=me) == (0, "removed 1 open item\n") and len(sent_mail) == 1, "todo done of an agent item sends no email")
    check(task("t_work.md").body.endswith("</human_instruction>\n\n(removed open item: tests pass)\n"), "the removal note follows a blank line", task("t_work.md").body[-80:])
    check(amh("todo", "done", "🧑 ship it today", "--evidence", "shipped", me=me)[0] == 0 and sent_mail[-1] == ("t_work", "", "pending item deleted:\n- ship it today\n", me), "the human is told their item was deleted", sent_mail[-1])
    check(task("t_work.md").items == [] and "pending_task_items: []\n" in text_of("t_work.md"), "an empty list is written inline")


def test_task_actions() -> None:
    check(amh("task", "give", "t_work.md", "do x") == (0, "added 1 open item(s) to t_work.md\n"), "task give")
    n_mail = len(sent_mail)
    check(amh("task", "give", "t_work.md", "do y", "--from-human")[0] == 0 and sent_mail[n_mail:] == [("t_work", "", "pending item created:\n- do y\n", "omnigent://s1")], "task give --from-human emails", sent_mail[n_mail:])
    check(amh("task", "show", "t_work.md", "t_sub.md")[1].startswith("t_work.md\n  version: v1.0.0\n  status: running\n  managerat: omnigent://main\n  is_manager: false\n  runat: omnigent://s1\n  tool: codex\n  session_id: s1\n  - do x\n  - 🧑 do y\nt_sub.md\n"), "task show", amh("task", "show", "t_work.md")[1])
    check("no task file" in str(amh("task", "show", "absent.md")[0]) and "no task file" in str(amh("task", "show", "../x.md")[0]), "an unknown task file is refused")
    check(amh("task", "move", "t_work.md", "t_sub.md", "do x") == (0, "") and task("t_work.md").items == ["🧑 do y"] and task("t_sub.md").items == ["do x"], "task move")
    check("no such open item" in str(amh("task", "move", "t_work.md", "t_sub.md", "absent")[0]) and "same task" in str(amh("task", "move", "t_sub.md", "t_sub.md", "do x")[0]), "task move refusals")
    check(amh("task", "note", "t_sub.md", "checked\n twice") == (0, "") and task("t_sub.md").body.endswith("\n\n(checked twice)\n"), "task note", task("t_sub.md").body[-40:])
    check(amh("task", "status", "t_work.md", "blocked", "--on", "human") == (0, "") and task("t_work.md").fields["blocked_on"] == "human" and todo_section("t_work.md") == "human pending:", "blocked on the human moves to human pending", todo_section("t_work.md"))
    check(amh("task", "status", "t_work.md", "blocked", "--on", "t_sub.md")[0] == 0 and todo_section("t_work.md") == "current:", "blocked on a task stays current")
    check("needs --on" in str(amh("task", "status", "t_work.md", "blocked")[0]) and "needs --on" in str(amh("task", "status", "t_work.md", "running", "--on", "x")[0]), "status refusals")
    check(amh("task", "status", "t_work.md", "running")[0] == 0 and "blocked_on" not in task("t_work.md").fields, "running clears what it waited on")
    check(amh("task", "status", "t_sub.md", "long_running", "--on", "human")[0] == 0 and task("t_sub.md").fields["blocked_on"] == "human", "a long-running task may wait")
    mine = [line for line in amh("task", "check")[1].splitlines() if line.startswith(("t_", "omnigent://s", "pb:"))]
    check(not mine, "task check finds nothing wrong with the test tasks", mine)
    todo = text_of("TODO.md")
    (ROOT / "TODO.md").write_text(todo.replace("t_work.md omnigent://s1", "t_work.md omnigent://old").replace("previous:\n", "previous:\nt_sub.md x\n", 1), encoding="utf-8")
    code, out = amh("task", "check")
    check(code == 1 and "t_work.md: TODO.md has it under `current:` at omnigent://old, the task file says `current:` at omnigent://s1" in out and "t_sub.md: 2 rows in TODO.md, want 1" in out, "task check reports disagreements", out[-400:])
    (ROOT / "TODO.md").write_text(todo, encoding="utf-8")


def test_tell() -> None:
    me = "omnigent://s1"
    n = len(to(MAIN))
    check(amh("tell", "manager", "need a decision", "--blocked", me=me) == (0, "message sent to your manager; do not send it again\n"), "tell manager")
    check(to(MAIN)[n:] == [f'<agent_message from="{me}">\nt_work.md is blocked: need a decision\n</agent_message>'], "the manager message is wrapped and labelled", to(MAIN)[n:])
    check(amh("tell", "manager", "all good", "--done", me=me)[0] == 0 and "t_work.md is done: all good" in to(MAIN)[-1] and amh("tell", "manager", "hi", me=me)[0] == 0 and "\nt_work.md: hi\n" in to(MAIN)[-1], "manager message labels")
    check("under 600 characters" in str(amh("tell", "manager", "x" * 601, me=me)[0]), "a long manager message is refused")
    check("records no manager" in str(amh("tell", "manager", "x", me=MAIN)[0]), "the main manager has no manager")
    body = WORKDIR / "body.txt"
    body.write_text("Report body\n", encoding="utf-8")
    unread_now[:] = [mail.Header("4", "[t_work] Earlier", "<old@x>", "", "Mon")]
    code, out = amh("tell", "human", "--subject", "Progress", "--file", str(body), "--replaces", "<gone@x>", me=me)
    check(code == 0 and sent_mail[-1] == ("t_work", "Progress", "Report body\n", me) and unread_args[-1] == ("t_work", ["<gone@x>"]), "tell human sends on the task's tag", (sent_mail[-1], unread_args[-1]))
    check(out.startswith(f"Email sent.\nMessage-ID: <sent-{len(sent_mail)}@x>\nThe human has not read these earlier emails of yours:\n  <old@x> '[t_work] Earlier' Mon\n") and "--replaces ID" in out, "tell human lists earlier unread emails", out)
    check(amh("mail", "unread", me=me) == (0, "<old@x> '[t_work] Earlier' Mon\n"), "mail unread")
    unread_now.clear()
    check(amh("tell", "human", "--subject", "Progress", "--file", str(body), me=me)[1] == f"Email sent.\nMessage-ID: <sent-{len(sent_mail)}@x>\n", "tell human with nothing unread")
    check(amh("tell", "agent", "t_work.md", "do\nthis", me=MAIN)[0] == 0 and to(me)[-1] == f'<agent_message from="{MAIN}">\ndo\nthis\n</agent_message>', "tell agent by task file", to(me)[-1])
    check(amh("tell", "agent", "pb:9", "--file", str(body), me=MAIN)[0] == 0 and to("pb:9") == [f'<agent_message from="{MAIN}">\nReport body\n</agent_message>'], "tell agent by address from a file")
    check("not both" in str(amh("tell", "agent", "pb:9", "x", "--file", str(body), me=MAIN)[0]) and "not both" in str(amh("tell", "agent", "pb:9", me=MAIN)[0]), "tell agent needs exactly one message source")
    down.add("pb:9")
    check(amh("tell", "agent", "pb:9", "x", me=MAIN)[0] == "amh: pb:9 is down", "an agent error becomes a refusal")
    down.clear()
    check(amh("agent", "compact", "t_work.md")[0] == 0 and to(me)[-1] == "/compact", "agent compact")
    code, out = amh("--help")
    check(code == 0 and all(group in out for group in ("tell", "todo", "task", "agent", "mail", "manager")), "root help names every group", out)


def test_agent_views() -> None:
    states.update({"omnigent://s1": ("ready", "idle evidence"), "omnigent://s2": ("error", "session_status=failed last_task_error=boom"), "pb:3": ("missing", "no such tmux window")})
    code, out = amh("agent", "problems")
    mine = [line for line in out.splitlines() if line.startswith("t_")]
    check(code == 3 and mine == ["t_sub.md at omnigent://s2: error (session_status=failed last_task_error=boom)", "t_tmux.md at pb:3: missing (no such tmux window)", "t_work.md at omnigent://s1: idle with open items (idle evidence)"], "agent problems", mine)
    _ = amh("task", "status", "t_tmux.md", "blocked", "--on", "t_work.md")
    check("t_tmux.md" not in amh("agent", "problems")[1], "a blocked task is no problem")
    listed = amh("agent", "list")[1].splitlines()
    check("ready: t_work.md [running] omnigent://s1 idle evidence" in listed and "missing: t_tmux.md [blocked] pb:3 no such tmux window" in listed and "running: t_main.md [long_running] omnigent://main fake" in listed, "agent list", [line for line in listed if " t_" in line])
    check(amh("agent", "status", "t_work.md") == (0, "ready: idle evidence\n") and amh("agent", "status", "pb:3")[1] == "missing: no such tmux window\n", "agent status")
    tree = amh("agent", "tree")[1]
    want = "t_main.md [manager, long_running] omnigent://main\n  t_tmux.md [worker, blocked] pb:3\n  t_work.md [worker, running] omnigent://s1\n    - 🧑 do y\n    t_sub.md [manager, long_running] omnigent://s2\n      - do x\nnot under the main manager:\n"
    check(tree.startswith(want), "agent tree", tree[:400])
    check(amh("agent", "stop", "t_sub.md") == (0, "") and stopped[-1] == "omnigent://s2" and task("t_sub.md").fields["status"] == "long_running", "agent stop leaves the task open")
    states.clear()
    check(amh("agent", "problems")[0] == 0, "no problems when every agent runs")


def test_routing() -> None:
    (ROOT / "t_done.md").write_text("---\nstatus: done\nrunat: omnigent://gone\nmanagerat: omnigent://s1\npending_task_items: []\n---\n", encoding="utf-8")
    cases = [
        ("Re: [t_sub] hello", "do it", "t_sub.md"),
        ("[t_sub] hello", "For manager.\nplease", "t_work.md"),
        ("[t_sub] hello", "please\n> quoted\n**for manager**\n> for worker", "t_work.md"),
        ("[t_sub] hello", "Terminate this agent now", "t_work.md"),
        ("[t_sub] hello", "replace this agent, it is stuck", "t_work.md"),
        ("[t_sub] hello", "not for manager really\nok", "t_sub.md"),
        ("[t_main] hello", "for manager", "t_main.md"),
        ("[t_done] hello", "more work", "t_work.md"),
        ("[absent_tag] hello", "x", "t_main.md"),
        ("no tag", "x", "t_main.md"),
        ("[TODO] hello", "x", "t_main.md"),
    ]
    for subject, body, want in cases:
        check(watch.route(CONFIG, subject, body).name == want, f"route {subject!r} {body[:20]!r}", watch.route(CONFIG, subject, body).name)


def test_intake_and_delivery() -> None:
    random.random = lambda: 0.5
    box = FakeBox({"11": email(HUMAN, "Re: [t_sub] do more", "Please do Z.\n> old", "<h1@x>"), "12": email("stranger@example.com", "[t_sub] spam", "x", "<s@x>"), "13": email(HUMAN, "[t_done] again", "more", "<h2@x>")})
    before = text_of("t_sub.md")
    watch.take_in_mail(CONFIG, box)  # type: ignore[arg-type]
    stored = ROOT / "manager_mail/m-11.txt"
    check(stored.read_text(encoding="utf-8") == "Subject: Re: [t_sub] do more\n\nPlease do Z.\n> old\n" and stored.stat().st_mode & 0o777 == 0o600, "human email is stored privately")
    check(text_of("t_sub.md") == before.rstrip("\n") + "\n\n(pending)\n(record and delegate manager_mail/m-11.txt)\n", "a pending block naming the email is appended", text_of("t_sub.md")[-120:])
    check(text_of("t_work.md").endswith("\n\n(pending)\n(record and delegate manager_mail/m-13.txt)\n"), "mail for a done task goes to its manager")
    check(box.seen == {"11", "13"}, "only the human's emails are marked read", box.seen)
    box.seen.clear()
    watch.take_in_mail(CONFIG, box)  # type: ignore[arg-type]
    check(text_of("t_sub.md").count("m-11.txt") == 1 and box.seen == {"11", "13"}, "a stored email is not taken in twice")
    _ = amh("task", "note", "t_sub.md", "a manager note")
    with taskfile.locked(CONFIG):
        sub = task("t_sub.md")
        sub.body += "\n(pending)\nplain block line one\nline two\n"
        taskfile.save(CONFIG, sub)
    failed_at: dict[str, float] = {}
    down.update({"omnigent://s2", "omnigent://s1", MAIN})
    n, n_mail = len(to("omnigent://s2")), len(sent_mail)
    watch.deliver_pending(CONFIG, failed_at)
    watch.deliver_pending(CONFIG, {})
    check("t_sub.md" in failed_at and text_of("t_sub.md").count("(pending)") == 2, "a delivery that reaches nobody keeps the marker")
    check([m[0] for m in sent_mail[n_mail:]] == ["t_sub", "t_work"] and "not been delivered to anyone" in sent_mail[-1][2] and sent_mail[-1][3] == mail.WATCHER, "the human is told once that nobody got their message", sent_mail[n_mail:])
    down.clear()
    watch.deliver_pending(CONFIG, failed_at)
    check(len(to("omnigent://s2")) == n, "a failed target is not retried at once")
    failed_at.clear()
    watch.deliver_pending(CONFIG, failed_at)
    got = to("omnigent://s2")[n:]
    want = f'{watch.ACCEPT}\n<human_instruction authoritative="true" source="manager_mail/m-11.txt">\nSubject: Re: [t_sub] do more\n\nPlease do Z.\n> old\n</human_instruction>'
    check(got == [want, "plain block line one\nline two"], "blocks are delivered in order, the email verbatim", got)
    body = task("t_sub.md").body
    check("(pending)" not in body and body.endswith("\n\n(record and delegate manager_mail/m-11.txt)\n\n(a manager note)\n\nplain block line one\nline two\n"), "markers are removed, source lines stay", body[-200:])
    check(len(to("omnigent://s1")) and "m-13.txt" in to("omnigent://s1")[-1] and "(pending)" not in text_of("t_work.md"), "the manager's block is delivered too")
    watch.deliver_pending(CONFIG, failed_at)
    check(len(to("omnigent://s2")) == n + 2, "a delivered block is not sent again")
    done = task("t_done.md")
    done.body += "\n(pending)\nnote left in a closed task\n"
    taskfile.save(CONFIG, done)
    watch.deliver_pending(CONFIG, {})
    check("note left in a closed task" in to("omnigent://s1")[-1] and "t_done.md" in to("omnigent://s1")[-1] and "(pending)" not in text_of("t_done.md"), "a block in a task with no agent goes to its manager", to("omnigent://s1")[-1:])
    awaiting = STATE / watch.AWAITING
    check(" t_sub.md m-11.txt 0\n" in awaiting.read_text(encoding="utf-8"), "a delivered human email awaits an answer", awaiting.read_text(encoding="utf-8"))
    with taskfile.locked(CONFIG):
        sub = task("t_sub.md")
        sub.body += "\n(pending)\nsecond note\n"
        taskfile.save(CONFIG, sub)
    states["omnigent://s2"] = ("missing", "gone")
    n, n_mail = len(to("omnigent://s2")), len(sent_mail)
    watch.deliver_pending(CONFIG, {})
    check(len(to("omnigent://s2")) == n and "second note" in to("omnigent://s1")[-1] and "could not be delivered to the agent of t_sub.md" in to("omnigent://s1")[-1] and "(pending)" not in text_of("t_sub.md"), "a message for an unreachable agent goes to its manager", to("omnigent://s1")[-1:])
    check(len(sent_mail) == n_mail + 1 and "gave it to its manager" in sent_mail[-1][2], "and the human is told", sent_mail[n_mail:])
    states.clear()
    old = time.time() - watch.ANSWER_S - 5
    awaiting.write_text(f"{old:.0f} t_sub.md m-11.txt 0\n", encoding="utf-8")
    n, n_mail = len(to("omnigent://s2")), len(sent_mail)
    watch.chase_answers(CONFIG)
    check(len(to("omnigent://s2")) == n + 1 and "m-11.txt" in to("omnigent://s2")[-1] and awaiting.read_text(encoding="utf-8").endswith(" 1\n"), "a silent agent is reminded of the human's email", to("omnigent://s2")[n:])
    awaiting.write_text(f"{old - watch.ANSWER_S:.0f} t_sub.md m-11.txt 1\n", encoding="utf-8")
    watch.chase_answers(CONFIG)
    check("has not emailed the human" in to("omnigent://s1")[-1] and len(sent_mail) == n_mail + 1 and "has not emailed you" in sent_mail[-1][2] and awaiting.read_text(encoding="utf-8") == "", "then its manager and the human are told", sent_mail[n_mail:])
    awaiting.write_text(f"{old:.0f} t_sub.md m-11.txt 0\n", encoding="utf-8")
    (STATE / mail.SENT_LOG).write_text(f"{time.time():.0f} t_sub\n", encoding="utf-8")
    n = len(messages)
    watch.chase_answers(CONFIG)
    check(len(messages) == n and awaiting.read_text(encoding="utf-8") == "", "an answered email is not chased")
    random.random = lambda: 0.0
    check(watch.delivery_text(CONFIG, ["do it"]) == "do it" + watch.ADD_REMINDER, "the reminder is appended at random")
    random.random = lambda: 0.5
    (ROOT / "t_main.md").rename(ROOT / "t_main.off")
    unrouted = FakeBox({"21": email(HUMAN, "untagged", "x", "<h3@x>")})
    watch.take_in_mail(CONFIG, unrouted)  # type: ignore[arg-type]
    (ROOT / "t_main.off").rename(ROOT / "t_main.md")
    check(not unrouted.seen and not (ROOT / "manager_mail/m-21.txt").exists(), "an unroutable email stays unread and unstored")


def test_nudge() -> None:
    told: dict[tuple[str, str], float] = {}
    notices = STATE / "amh-problem-notices.txt"
    n_main, n_work, n_mail = len(to(MAIN)), len(to("omnigent://s1")), len(sent_mail)
    states.update({"omnigent://s1": ("ready", "idle"), "omnigent://s2": ("error", "session_status=failed last_task_error=model  overloaded; pane_tail: junk")})
    watch.nudge(CONFIG, told)
    watch.nudge(CONFIG, told)
    reminders = [text for text in to("omnigent://s1")[n_work:] if not text.startswith("Handle these")]
    check(reminders == ["You have 1 open items. See them with `amh todo list`. Continue until each is finished or cancelled."], "an idle agent is reminded once per period", reminders)
    failure = "The agent working on task file t_sub.md has failed and is not working any more.\nTool: codex, running at omnigent://s2\nIts manager: t_work.md\nOpen items on the task: 1\nThe error its harness reported:\nmodel overloaded\n"
    got = [text for target, text in messages if "task file t_sub.md" in text]
    check(got == ["Handle these agent problems; only email the human if you cannot:\n\n" + failure] and not to(MAIN)[n_main:], "the failure is reported once, to the task's manager", got)
    check(sent_mail[n_mail:] == [("t_sub", "", failure + "\nIts manager has been told to handle it.\n", "amh watcher")], "a harness error is emailed to the human once", sent_mail[n_mail:])
    check(notices.read_text(encoding="utf-8") == "t_sub.md omnigent://s2 error model overloaded\n", "the reported problem is remembered", notices.read_text(encoding="utf-8"))
    states["omnigent://s2"] = ("running", "fine")
    watch.nudge(CONFIG, told)
    check(notices.read_text(encoding="utf-8") == "", "a problem that went away is forgotten")
    states["omnigent://s2"] = ("missing", "runner offline")
    watch.nudge(CONFIG, told)
    watch.nudge(CONFIG, told)
    gone = "The agent working on task file t_sub.md is gone: nothing runs at its address.\nTool: codex, running at omnigent://s2\nIts manager: t_work.md\nOpen items on the task: 1\n"
    got = [text for target, text in messages if "task file t_sub.md" in text]
    check(len(got) == 2 and got[1].endswith(gone) and len(sent_mail) == n_mail + 1, "a new problem after recovery is reported again, without email", got[1:])
    states["omnigent://s2"] = ("missing", "Omnigent is unreachable at http://x")
    watch.nudge(CONFIG, told)
    check(notices.read_text(encoding="utf-8") != "", "nothing changes while Omnigent is unreachable")
    states["omnigent://s2"] = ("error", "session_status=failed last_task_error=second")
    down.add("mail")
    watch.nudge(CONFIG, told)
    check("error second" not in notices.read_text(encoding="utf-8"), "a failure whose email failed is not remembered")
    down.clear()
    watch.nudge(CONFIG, told)
    check("error second" in notices.read_text(encoding="utf-8") and sent_mail[-1][0] == "t_sub", "it is reported again once email works")
    states.clear()


def test_guest() -> None:
    n_launch = len(launched)
    auth = f"mx.google.com; dkim=pass header.i=@example.org; spf=pass (sender is designated) smtp.mailfrom={GUEST}; dmarc=pass"
    box = FakeBox({"31": email(GUEST, "回复: Question", "What is 2+2?", "<g1@qq>", image=True, auth=auth), "32": email(GUEST, "forged", "x", "<g2@qq>", auth="mx.google.com; spf=fail smtp.mailfrom=evil@example.com")})
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        guest.take_in(CONFIG, box)  # type: ignore[arg-type]
    stored = ROOT / "guest_visitor_manager_mail/guest-31.txt"
    want = "Message-ID: <g1@qq>\nIn-Reply-To: \nReferences: \nSubject: 回复: Question\n\nWhat is 2+2?\n\n\nGuest images:\n- guest_visitor_manager_mail/guest-31-0.png\n"
    check(stored.read_text(encoding="utf-8") == want and stored.stat().st_mode & 0o777 == 0o600, "guest email is stored with its headers", stored.read_text(encoding="utf-8"))
    check(stored.with_name("guest-31-0.png").read_bytes() == PNG, "guest image is stored")
    check("uid 32 rejected" in out.getvalue() and not stored.with_name("guest-32.txt").exists() and box.seen == {"31", "32"}, "an unverified sender is rejected and marked read", out.getvalue())
    agent = task("guest_visitor.md")
    check(len(launched) == n_launch + 1 and launched[-1][:5] == ("codex", "gpt-6.1-sol", "low", WORKDIR, "guest_visitor") and agent.fields["managerat"] == MAIN, "a guest agent is started under the main manager", launched[-1])
    check(f"writes to the agent mailbox from {GUEST}" in to(agent.address)[0], "the guest agent gets its standing goal")
    check(agent.items == ["Answer the guest email guest_visitor_manager_mail/guest-31.txt"] and agent.body.endswith("\n\n(pending)\n(guest mail guest_visitor_manager_mail/guest-31.txt)\n"), "the guest email is queued", agent.body[-90:])
    box.raws["33"] = email(GUEST, "Second", "more", "<g3@qq>", auth=auth)
    guest.take_in(CONFIG, box)  # type: ignore[arg-type]
    check(len(launched) == n_launch + 1 and len(task("guest_visitor.md").items) == 2, "a live guest agent is reused")
    watch.deliver_pending(CONFIG, {})
    told = f"A guest email arrived. Read {stored}, answer what it asks, and send the answer with `amh tell guest --mail guest-31.txt --file ANSWER_FILE`. Then remove its open item with `amh todo done`."
    check(to(agent.address)[1:] == [told, told.replace("guest-31", "guest-33")] and "(pending)" not in text_of("guest_visitor.md"), "guest emails are delivered to the guest agent", to(agent.address)[1:])
    answer, picture = WORKDIR / "answer.txt", WORKDIR / "pic.JPEG"
    answer.write_text("It is 4.\n", encoding="utf-8")
    _ = picture.write_bytes(b"jpegdata")
    code, out_text = amh("tell", "guest", "--mail", "guest-31.txt", "--file", str(answer), "--image", str(picture))
    message = smtp_out[-1]
    check(code == 0 and out_text == f"Email sent to the guest.\nMessage-ID: {message['Message-ID']}\n", "tell guest prints the Message-ID", out_text)
    check((message["From"], message["To"], message["Subject"], message["In-Reply-To"], message["References"]) == (AGENT, GUEST, "Re: Question", "<g1@qq>", "<g1@qq>"), "guest reply headers", message.items())
    attached = list(message.iter_attachments())
    check(message.get_body(("plain",)).get_content() == "It is 4.\n" and [(part.get_content_type(), part.get_filename()) for part in attached] == [("image/jpeg", "pic.JPEG")], "guest reply body and image")  # type: ignore[union-attr]
    n = len(smtp_out)
    check("not a png" in str(amh("tell", "guest", "--mail", "guest-31.txt", "--file", str(answer), "--image", str(answer))[0]) and len(smtp_out) == n, "a non-image attachment is refused")
    _ = answer.write_text(" \n", encoding="utf-8")
    check("empty" in str(amh("tell", "guest", "--mail", "guest-31.txt", "--file", str(answer))[0]) and len(smtp_out) == n, "an empty answer is refused")


def test_close() -> None:
    me = "omnigent://s2"
    check("still has open items" in str(amh("task", "close", "t_sub.md")[0]) and task("t_sub.md").fields["status"] == "long_running", "close refuses open items")
    check(amh("todo", "done", "do x", "--evidence", "done", me=me)[0] == 0, "clear the last item")
    with taskfile.locked(CONFIG):
        sub = task("t_sub.md")
        sub.body += "\n(pending)\nundelivered\n"
        taskfile.save(CONFIG, sub)
    check("undelivered `(pending)` block" in str(amh("task", "close", me=me)[0]), "close refuses an undelivered block")
    watch.deliver_pending(CONFIG, {})
    n_mail, n_work, n_stop = len(sent_mail), len(to("omnigent://s1")), len(stopped)
    code, out = amh("task", "close", me=me)
    check(code == 0 and out == "closed t_sub.md; stopping your own session now\nclosed t_sub.md at omnigent://s2\n", "self-close output", (code, out))
    check(task("t_sub.md").fields["status"] == "done" and "blocked_on" not in task("t_sub.md").fields, "self-close marks the task done", task("t_sub.md").fields)
    check(sent_mail[n_mail:] == [("t_sub", "", f"Closed {me}\n", me)], "the human is told the task closed", sent_mail[n_mail:])
    check(to("omnigent://s1")[n_work:] == [f'<agent_message from="{me}">\nt_sub.md had no open work left and closed itself.\n</agent_message>'], "the manager is told of a self-close", to("omnigent://s1")[n_work:])
    check(stopped[n_stop:] == [me], "the agent's own session is stopped last", stopped[n_stop:])
    rows = text_of("TODO.md").split("\n")
    check(rows[rows.index("previous:") + 1] == f"t_sub.md {me}" and todo_section("t_sub.md") == "previous:", "a closed task tops `previous`")
    check("is done; start a new task" in str(amh("task", "give", "t_sub.md", "x")[0]), "a done task takes no items")
    check("0 active tasks" in str(amh("task", "close", me=me)[0]), "a closed agent cannot close again")
    states["pb:3"] = ("missing", "gone")
    code, out = amh("task", "close", "t_tmux.md", "--no-email", me=MAIN)
    check("marked done, but its agent was not stopped" in str(code) and task("t_tmux.md").fields["status"] == "done" and len(sent_mail) == n_mail + 1, "a failed stop is reported", code)
    _ = amh("todo", "done", "🧑 do y", "--evidence", "done", "--task-file", "t_work.md")
    n_mail, n_main = len(sent_mail), len(to(MAIN))
    states["omnigent://s1"] = ("missing", "gone")
    check(amh("task", "close", "t_work.md", "--agent-gone", me=MAIN) == (0, "closed t_work.md at omnigent://s1\n"), "a manager closes a task whose agent is gone")
    check(sent_mail[n_mail:] == [("t_work", "", "Closed omnigent://s1\n", "omnigent://s1")] and len(to(MAIN)) == n_main, "a manager's close emails the human and messages nobody")
    states.clear()
    prompt = WORKDIR / "prompt.md"
    check(amh("task", "start", "t_work.md", "--dir", str(WORKDIR), "--prompt", str(prompt), "--tool", "claude", me=MAIN)[0] == 0, "a done task can be started again")
    again = task("t_work.md")
    check(again.fields["status"] == "running" and again.fields["tool"] == "claude" and again.address == f"omnigent://s{len(launched)}" and again.fields["session_id"] == f"s{len(launched)}" and launched[-1][:3] == ("claude", "claude-opus-5-5", "low"), "restart records the new agent", again.fields)
    check(todo_section("t_work.md") == "current:" and again.body.count("<manager_delegation") == 2, "restart moves the task back to current and keeps history")


def test_rotate() -> None:
    check("only Omnigent managers" in str(amh("manager", "rotate", "t_tmux.md")[0]), "a tmux manager is not rotated")
    agents.api_get = lambda *_args: {"harness": "claude-native", "model_override": "claude-opus-5-5", "reasoning_effort": "low", "workspace": str(WORKDIR)}  # type: ignore[assignment]
    n_stop = len(stopped)
    code, out = amh("manager", "rotate", "t_main.md", "--effort", "high")
    new = out.strip()
    check(code == 0 and new == f"omnigent://s{len(launched)}" and launched[-1] == ("claude", "claude-opus-5-5", "high", WORKDIR, "t_main"), "rotate launches the same tool and model", (code, out, launched[-1]))
    first = to(new)[0]
    check("$ getagentsmd get main_manager\n" in first and first.endswith(f"You replace the manager that ran at {MAIN}. Its task file t_main.md in {ROOT} holds its open work and history; take over from there.\n"), "the new manager is told to take over", first[-300:])
    main_task = task("t_main.md")
    check(main_task.address == new and main_task.fields["status"] == "long_running" and main_task.body.endswith(f"(manager replaced: {MAIN} handed over to a fresh agent)\n"), "the task file records the replacement", main_task.fields)
    check(task("t_work.md").fields["managerat"] == new and task("guest_visitor.md").fields["managerat"] == new and task("t_sub.md").fields["managerat"] == "omnigent://s1", "active reports follow the new manager")
    check(f'main_manager = "{new}"\n' in (STATE / "env.toml").read_text(encoding="utf-8") and stopped[n_stop:] == [MAIN], "the main manager address is updated and the old agent stopped", stopped[n_stop:])
    check(f"\nt_main.md {new}\n" in text_of("TODO.md"), "the task list names the new address")


def main() -> int:
    tests = [test_round_trip, test_pending_blocks, test_transport, install_fakes, test_task_start, test_todo, test_task_actions, test_tell, test_agent_views, test_routing, test_intake_and_delivery, test_nudge, test_guest, test_close, test_rotate]
    try:
        for test in tests:
            try:
                test()
            except Exception:
                failures.append(f"{test.__name__} crashed:\n{traceback.format_exc()}")
    finally:
        shutil.rmtree(TEMP, ignore_errors=True)
    print("\n".join(failures), end="\n" if failures else "")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
