"""End-to-end checks of `amh` on a temporary copy of the work-log root; prints only failures.

Run: `/usr/bin/python3 test_amh.py`. Nothing real is touched: Omnigent and the mail network calls are replaced by fakes,
and settings come from a temporary `env.toml`. Emails are built and parsed by the real Rust code.
Only what still runs through the Python is checked here; the watcher, the guest mailbox, and starting agents are Rust and checked by `cargo test`.
"""

from __future__ import annotations

import contextlib
import io
import os
import shutil
import subprocess
import sys
import tempfile
import traceback
from email import message_from_bytes, policy
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

from amh import agents, cli, mail, taskfile  # noqa: E402
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


def forbidden(*_args: object, **_kwargs: object) -> None:
    raise AssertionError("a test reached a real agent or mailbox")


amh_rs.mail_send = amh_rs.mail_unread = amh_rs.start_task = amh_rs.start_agent = forbidden


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


def test_mail() -> None:
    """The pure mail parts, which are Rust: thread choice, building, parsing. The network code is exercised in production."""
    found = [mail.Header("5", "[t_work] Progress", "<a@x>", "", ""), mail.Header("6", "Re: [t_work] Progress", "<b@x>", "<a@x>", ""), mail.Header("7", "[t_worker] Other", "<c@x>", "", "")]
    check(amh_rs.mail_plan("t_work", "", found) == ("Re: [t_work] Progress", "<b@x>", "<a@x>"), "reply reuses the tag's last subject", amh_rs.mail_plan("t_work", "", found))
    check(amh_rs.mail_plan("t_work", "Re: [t_work] New topic", found) == ("[t_work] New topic", "", ""), "new subject starts a thread")
    check(amh_rs.mail_plan("fresh", "", found) == ("[fresh] fresh", "", ""), "first email of a tag is titled by the tag")
    try:
        _ = amh_rs.mail_plan("t_work", "s" * 60, found)
        check(False, "long subject is refused")
    except ValueError:
        pass
    long_title = "a subject the human's thread already has, longer than sixty characters"
    found.append(mail.Header("8", f"Re: [t_work] {long_title}", "<d@x>", "", ""))
    check(amh_rs.mail_plan("t_work", f"Re: [t_work] {long_title}", found) == (f"Re: [t_work] {long_title}", "<d@x>", ""), "a reply may reuse a long existing subject")
    message_id, raw = amh_rs.mail_compose(HUMAN, "Re: [t_work] Progrès", "see the figure", "<b@x>", "<a@x>", "omnigent://w", [("figure.png", "image/png", PNG)])
    message = message_from_bytes(raw, policy=policy.default)
    attached = [(part.get_filename(), part.get_content_type(), part.get_content()) for part in message.iter_attachments()]
    check(attached == [("figure.png", "image/png", PNG)] and message.get_body(("plain",)).get_content() == "see the figure\r\n", "a file is attached to the email", attached)  # type: ignore[union-attr]
    headers = [message[name] for name in ("From", "To", "X-AMH-From", "Message-ID", "In-Reply-To", "References", "Subject")]
    check(headers == [AGENT, HUMAN, "omnigent://w", message_id, "<b@x>", "<a@x> <b@x>", "Re: [t_work] Progrès"], "email headers", headers)
    want = (AGENT, "", message_id, "<b@x>", "<a@x> <b@x>", "Re: [t_work] Progrès", "see the figure\r\n", [("figure.png", "image/png", PNG)])
    check(amh_rs.mail_parse(raw) == want, "a built email parses back", amh_rs.mail_parse(raw)[:7])
    plain = message_from_bytes(amh_rs.mail_compose(HUMAN, "[t_work] New", "body text\n", "", "", "", [])[1], policy=policy.default)
    check(plain.get_content() == "body text\r\n" and plain["In-Reply-To"] is None and plain["X-AMH-From"] is None, "a new thread has no reply headers", plain.items())
    odd = "From: =?utf-8?b?5a6i?= <G@Example.org>\r\nAuthentication-Results: mx; spf=pass\r\n smtp.mailfrom=g@example.org\r\nAuthentication-Results: forged\r\nSubject: =?utf-8?q?caf=C3=A9?=\r\n  twice\r\nContent-Type: text/html; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<p>=E9</p>\r\n"
    check(amh_rs.mail_parse(odd.encode()) == ("G@Example.org", "mx; spf=pass smtp.mailfrom=g@example.org", "", "", "", "café twice", "<p>é</p>\r\n", []), "headers are decoded, the first of a repeated header counts, HTML stays as written", amh_rs.mail_parse(odd.encode()))
    check(amh_rs.mail_tag_of("Re: Fwd: [a_b-1] x [y]") == "a_b-1" and amh_rs.mail_tag_of("x [a]") is None and amh_rs.mail_tag_of(" [a]") is None and amh_rs.mail_bare("Re: [a] RE: [b] Title [c] ") == "Title [c]", "subject parsing")


sent_mail: list[tuple[str, str, str, str]] = []
messages: list[tuple[str, str]] = []
states: dict[str, tuple[str, str]] = {}
started: list[tuple[object, ...]] = []
stopped: list[str] = []
down: set[str] = set()
unread_args: list[tuple[str, list[str]]] = []
unread_now: list[mail.Header] = []


def fake_mail_send(_config: object, tag: str, subject: str, body: str, sender: str) -> str:
    if "mail" in down:
        raise OSError("smtp is down")
    sent_mail.append((tag, subject, body, sender))
    return f"<sent-{len(sent_mail)}@x>"


def fake_send(_config: object, address: str, text: str) -> None:
    if address in down:
        raise agents.AgentError(f"{address} is down")
    messages.append((address, text))


def fake_start_agent(name: str, *args: object) -> str:
    """Stands for the Rust start of an agent: records the request and writes the new address into the task file."""
    started.append((name, *args))
    with taskfile.locked(CONFIG):
        fresh = task(name)
        fresh.fields["runat"] = f"omnigent://new{len(started)}"
        taskfile.save(CONFIG, fresh)
        taskfile.place_in_list(CONFIG, fresh)
    return fresh.address


def fake_stop(_config: object, address: str) -> None:
    if states.get(address, ("running", ""))[0] == "missing":
        raise agents.AgentError(f"nothing runs at {address}")
    stopped.append(address)


def install_fakes() -> None:
    mail.send = fake_mail_send  # type: ignore[assignment]
    mail.unread = lambda _config, tag, trash: unread_args.append((tag, trash)) or list(unread_now)  # type: ignore[assignment]
    agents.send = fake_send  # type: ignore[assignment]
    agents.status = lambda _config, address: states.get(address, ("running", "fake"))  # type: ignore[assignment]
    agents.stop = fake_stop  # type: ignore[assignment]
    agents.api_get = forbidden  # type: ignore[assignment]


def to(address: str) -> list[str]:
    return [text for target, text in messages if target == address]


def test_task_start() -> None:
    """Lay out a main manager, a worker under it, a submanager under the worker, and a task left over from before Omnigent; starting itself is Rust."""
    delegation = f'<manager_delegation from="{MAIN}">\nBuild the thing.\n</manager_delegation>\n<human_instruction authoritative="true" source="manager_mail/m-7.txt:3-4">\nline three\nline four\n</human_instruction>\n'
    made = [
        taskfile.Task("t_main.md", {"status": "long_running", "runat": MAIN, "is_manager": "true"}, [], ""),
        taskfile.Task("t_work.md", {"version": "v1.0.0", "status": "running", "managerat": MAIN, "is_manager": "false", "runat": "omnigent://s1", "tool": "codex", "session_id": "s1"}, [], delegation),
        taskfile.Task("t_sub.md", {"version": "v1.0.0", "status": "long_running", "managerat": "omnigent://s1", "is_manager": "true", "runat": "omnigent://s2", "tool": "codex", "session_id": "s2"}, [], delegation),
        # Its address is not a session, so no agent can be reached there.
        taskfile.Task("t_tmux.md", {"version": "v1.0.0", "status": "running", "managerat": MAIN, "is_manager": "false", "runat": "pb:3", "tool": "codex"}, [], ""),
    ]
    for new in made:
        taskfile.save(CONFIG, new)
        taskfile.place_in_list(CONFIG, new)
    check(todo_section("t_work.md") == "current:" and "\nt_work.md omnigent://s1\n" in text_of("TODO.md"), "a task is listed under current")
    prompt = WORKDIR / "prompt.md"
    prompt.write_text("Build the thing.\n- part one\n", encoding="utf-8")
    check("go together" in str(amh("task", "start", "t_x.md", "--dir", ".", "--prompt", str(prompt), "--email", "m-7.txt", me=MAIN)[0]), "--email needs --lines")
    check("cannot tell which agent" in str(amh("task", "start", "t_x.md", "--dir", ".", "--prompt", str(prompt))[0]) and not (ROOT / "t_x.md").exists(), "an unknown caller cannot be the manager")


def test_todo() -> None:
    me = "omnigent://s1"
    check(amh("todo", "add", "write   the\n code", me=me) == (0, "added 1 open item(s)\n") and not sent_mail, "todo add")
    check(amh("todo", "add", "write the code", me=me) == (0, "added 0 open item(s)\n"), "a duplicate item is not added")
    check(amh("todo", "add", "--from-human", "🧑 ship it", "--task-file", "t_work.md") == (0, "added 1 open item(s)\n"), "todo add --from-human by --task-file")
    check(sent_mail == [("t_work", "", "pending item created:\n- ship it\n", f"amh notice {me}")], "the human is told their item was created", sent_mail)
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
    check(amh("todo", "done", "🧑 ship it today", "--evidence", "shipped", me=me)[0] == 0 and sent_mail[-1] == ("t_work", "", "pending item deleted:\n- ship it today\n", f"amh notice {me}"), "the human is told their item was deleted", sent_mail[-1])
    unread_now.append(mail.Header("9", "[t_work] x", f"<sent-{len(sent_mail)}@x>", "", "", "amh notice x"))
    n_trash = len(unread_args)
    _ = amh("todo", "add", "one more", "--from-human", me=me)
    check(sent_mail[-1][2] == "pending item deleted:\n- ship it today\npending item created:\n- one more\n" and unread_args[-1] == ("t_work", [f"<sent-{len(sent_mail) - 1}@x>"]) and len(unread_args) == n_trash + 2, "an unread earlier notice is folded into the new one and trashed", (sent_mail[-1], unread_args[n_trash:]))
    unread_now.clear()
    _ = amh("todo", "done", "🧑 one more", "--evidence", "test", me=me)
    check(sent_mail[-1][2] == "pending item deleted:\n- one more\n", "a notice the human has read is not repeated", sent_mail[-1])
    check(task("t_work.md").items == [] and "pending_task_items: []\n" in text_of("t_work.md"), "an empty list is written inline")


def test_task_actions() -> None:
    check(amh("task", "give", "t_work.md", "do x") == (0, "added 1 open item(s) to t_work.md\n"), "task give")
    n_mail = len(sent_mail)
    check(amh("task", "give", "t_work.md", "do y", "--from-human")[0] == 0 and sent_mail[n_mail:] == [("t_work", "", "pending item created:\n- do y\n", "amh notice omnigent://s1")], "task give --from-human emails", sent_mail[n_mail:])
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
    unread_now[:] = [mail.Header("4", "[t_work] Earlier", "<old@x>", "", "Mon"), mail.Header("5", "[t_work] Earlier", "<notice@x>", "", "Mon", "amh notice omnigent://s1")]
    code, out = amh("tell", "human", "--subject", "Progress", "--file", str(body), "--replaces", "<gone@x>", me=me)
    check(code == 0 and sent_mail[-1] == ("t_work", "Progress", "Report body\n", me) and unread_args[-1] == ("t_work", ["<gone@x>"]), "tell human sends on the task's tag", (sent_mail[-1], unread_args[-1]))
    check(out.startswith(f"Email sent.\nMessage-ID: <sent-{len(sent_mail)}@x>\nThe human has 1 earlier unread email(s) of yours besides the one just sent.") and "  <old@x> '[t_work] Earlier' Mon\n" in out and f"--replaces '<old@x>' --replaces '<sent-{len(sent_mail)}@x>'" in out, "tell human lists earlier unread emails", out)
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
    _ = amh("task", "status", "t_sub.md", "blocked", "--on", "t_work.md")
    code, out = amh("agent", "problems")
    mine = [line for line in out.splitlines() if line.startswith("t_")]
    # The real Rust check; no Omnigent answers at the test address, so every agent it asks about is missing.
    want = ["t_main.md at omnigent://main: missing (Omnigent is unreachable at http://127.0.0.1:9/v1/sessions/main?", "t_tmux.md at pb:3: missing (pb:3 is not an Omnigent session; amh reaches agents on Omnigent only)", "t_work.md at omnigent://s1: missing (Omnigent is unreachable at http://127.0.0.1:9/v1/sessions/s1?"]
    check(code == 3 and len(mine) == 3 and all(line.startswith(start) for line, start in zip(mine, want)), "agent problems lists unreachable agents and skips a blocked task", mine)
    _ = amh("task", "status", "t_sub.md", "long_running", "--on", "human")
    _ = amh("task", "status", "t_tmux.md", "blocked", "--on", "t_work.md")
    listed = amh("agent", "list")[1].splitlines()
    check("ready: t_work.md [running] omnigent://s1 idle evidence" in listed and "missing: t_tmux.md [blocked] pb:3 no such tmux window" in listed and "running: t_main.md [long_running] omnigent://main fake" in listed, "agent list", [line for line in listed if " t_" in line])
    check(amh("agent", "status", "t_work.md") == (0, "ready: idle evidence\n") and amh("agent", "status", "pb:3")[1] == "missing: no such tmux window\n", "agent status")
    tree = amh("agent", "tree")[1]
    want = "t_main.md [manager, long_running] omnigent://main\n  t_tmux.md [worker, blocked] pb:3\n  t_work.md [worker, running] omnigent://s1\n    - 🧑 do y\n    t_sub.md [manager, long_running] omnigent://s2\n      - do x\nnot under the main manager:\n"
    check(tree.startswith(want), "agent tree", tree[:400])
    check(amh("agent", "stop", "t_sub.md") == (0, "") and stopped[-1] == "omnigent://s2" and task("t_sub.md").fields["status"] == "long_running", "agent stop leaves the task open")
    states.clear()


def test_guest() -> None:
    """`amh tell guest` is Rust; only its refusals are run here, which stop before any email is sent."""
    folder = ROOT / "guest_visitor_manager_mail"
    folder.mkdir()
    (folder / "guest-31.txt").write_text("Message-ID: <g1@qq>\nIn-Reply-To: \nReferences: \nSubject: Question\n\nWhat is 2+2?\n", encoding="utf-8")
    answer = WORKDIR / "answer.txt"
    answer.write_text("It is 4.\n", encoding="utf-8")
    check("not a png" in str(amh("tell", "guest", "--mail", "guest-31.txt", "--file", str(answer), "--image", str(answer))[0]), "a non-image attachment is refused")
    check("cannot read" in str(amh("tell", "guest", "--mail", "absent.txt", "--file", str(answer))[0]), "an unknown guest email is refused")
    _ = answer.write_text(" \n", encoding="utf-8")
    check("empty" in str(amh("tell", "guest", "--mail", "guest-31.txt", "--file", str(answer))[0]), "an empty answer is refused")


def test_close() -> None:
    me = "omnigent://s2"
    check("still has open items" in str(amh("task", "close", "t_sub.md")[0]) and task("t_sub.md").fields["status"] == "long_running", "close refuses open items")
    check(amh("todo", "done", "do x", "--evidence", "done", me=me)[0] == 0, "clear the last item")
    with taskfile.locked(CONFIG):
        sub = task("t_sub.md")
        sub.body += "\n(pending)\nundelivered\n"
        taskfile.save(CONFIG, sub)
    check("undelivered `(pending)` block" in str(amh("task", "close", me=me)[0]), "close refuses an undelivered block")
    with taskfile.locked(CONFIG):
        sub = task("t_sub.md")
        sub.body = sub.body.replace("\n(pending)\n", "\n")
        taskfile.save(CONFIG, sub)
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


def test_rotate() -> None:
    check("only Omnigent managers" in str(amh("manager", "rotate", "t_tmux.md")[0]), "a tmux manager is not rotated")
    agents.api_get = lambda *_args: {"harness": "claude-native", "model_override": "claude-opus-5-5", "reasoning_effort": "low", "workspace": str(WORKDIR)}  # type: ignore[assignment]
    amh_rs.start_agent = fake_start_agent
    taskfile.save(CONFIG, taskfile.Task("t_report.md", {"status": "running", "runat": "omnigent://s3", "managerat": MAIN}, [], ""))
    n_stop = len(stopped)
    code, out = amh("manager", "rotate", "t_main.md", "--effort", "high")
    new = out.strip()
    request = f"You replace the manager that ran at {MAIN}. Its task file t_main.md in {ROOT} holds its open work and history; take over from there.\n"
    want = ("t_main.md", [], "claude", "claude-opus-5-5", "high", str(WORKDIR), ["agent_work", "agent_manager", "agent_manager_core", "main_manager"], request, f"(manager replaced: {MAIN} handed over to a fresh agent)\n")
    check(code == 0 and new == "omnigent://new1" and started == [want], "rotate starts the same tool and model on the same task, with the main manager's guides", (code, out, started))
    check(task("t_report.md").fields["managerat"] == new and task("t_sub.md").fields["managerat"] == "omnigent://s1" and task("t_work.md").fields["managerat"] == MAIN, "active reports follow the new manager", task("t_report.md").fields)
    check(f'main_manager = "{new}"\n' in (STATE / "env.toml").read_text(encoding="utf-8") and stopped[n_stop:] == [MAIN], "the main manager address is updated and the old agent stopped", stopped[n_stop:])
    check(f"\nt_main.md {new}\n" in text_of("TODO.md"), "the task list names the new address")


def main() -> int:
    tests = [test_round_trip, test_pending_blocks, test_mail, install_fakes, test_task_start, test_todo, test_task_actions, test_tell, test_agent_views, test_guest, test_close, test_rotate]
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
