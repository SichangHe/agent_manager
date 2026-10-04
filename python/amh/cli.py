"""Agent manager helper: one command for agents and managers.

Start at `amh --help` and follow the groups down; every level has its own `--help`.
"""
# 🧑 "Consolidate the manager helpers. Make them into one command `amh` ... Make the subcommands nested and with sane names ... anyone can start from the root --help page and work their way naturally"
# 🧑 "Don't keep calling the old helper commands. Integrate them into a single program. Later we will rewrite it in Rust."

from __future__ import annotations

import json
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace

from amh import agents, config as configuration, guest, mail, taskfile, watch, work
from amh.config import Config

Run = Callable[[Config, SimpleNamespace], int | None]
UNIT = "amh-watch.service"


def todo_list(config: Config, args: SimpleNamespace) -> None:
    print("\n".join(taskfile.own_task(config, args.task_file).items))


def add_item(config: Config, args: SimpleNamespace) -> None:
    """`todo add` to the caller's own task, or `task give` to the named task."""
    named = getattr(args, "task", None)
    with taskfile.locked(config):
        task = taskfile.load(config, named) if named else taskfile.own_task(config, args.task_file)
        new = work.add_item(config, task, args.item, args.from_human)
    work.notify_human(config, task, "created", new)
    print(f"added {len(new)} open item(s)" + (f" to {named}" if named else ""))


def todo_done(config: Config, args: SimpleNamespace) -> None:
    with taskfile.locked(config):
        task = taskfile.own_task(config, args.task_file)
        work.remove_item(config, task, args.item, args.evidence)
    work.notify_human(config, task, "deleted", [args.item])
    print("removed 1 open item")


def todo_replace(config: Config, args: SimpleNamespace) -> None:
    with taskfile.locked(config):
        task = taskfile.own_task(config, args.task_file)
        if args.old not in task.items:
            raise SystemExit("amh: no such open item; copy it exactly from `amh todo list`")
        task.items[task.items.index(args.old)] = taskfile.mark(args.new, args.old.startswith(taskfile.HUMAN_MARK))
        taskfile.save(config, task)
    print("replaced 1 open item")


def tell_manager(config: Config, args: SimpleNamespace) -> None:
    work.tell_manager(config, taskfile.own_task(config, args.task_file), args.text, "blocked" if args.blocked else "done" if args.done else "")
    print("message sent to your manager; do not send it again")


def tell_human(config: Config, args: SimpleNamespace) -> None:
    body = Path(args.file).read_text(encoding="utf-8") if args.file else sys.stdin.read()
    work.tell_human(config, taskfile.own_task(config, args.task_file), args.subject, body, args.replaces)


def tell_agent(config: Config, args: SimpleNamespace) -> None:
    if bool(args.text) == bool(args.file):
        raise SystemExit("amh: give the message as TEXT or as --file, not both")
    text = Path(args.file).read_text(encoding="utf-8") if args.file else args.text
    agents.send(config, address(config, args.who), work.envelope(agents.own_address(), text))


def tell_guest(config: Config, args: SimpleNamespace) -> None:
    print(f"Email sent to the guest.\nMessage-ID: {guest.reply(config, args.mail, Path(args.file).read_text(encoding='utf-8'), [Path(image) for image in args.image])}")


def address(config: Config, who: str) -> str:
    """Resolve a task file name to where its agent runs; pass an address through."""
    return taskfile.load(config, who).address if who.endswith(".md") else who


def task_start(config: Config, args: SimpleNamespace) -> None:
    if bool(args.email) != bool(args.lines):
        raise SystemExit("amh: --email and --lines go together")
    goal = Path(args.prompt).read_text(encoding="utf-8")
    print(work.start_task(config, args.task, Path(args.dir), goal, args.manager or agents.own_address(), args.tool, args.model, args.effort, args.as_manager, args.tmux, args.proxy, args.email, args.lines))


def task_show(config: Config, args: SimpleNamespace) -> None:
    for name in args.task:
        task = taskfile.load(config, name)
        print(f"{task.name}\n" + "".join(f"  {key}: {value}\n" for key, value in task.fields.items()) + "".join(f"  - {item}\n" for item in task.items), end="")


def task_close(config: Config, args: SimpleNamespace) -> None:
    name = args.task or taskfile.own_task(config, args.task_file).name
    print(f"closed {name} at {work.close_task(config, name, args.agent_gone, not args.no_email, own=args.task is None)}")


def task_status(config: Config, args: SimpleNamespace) -> None:
    work.set_status(config, args.task, args.status, args.on)


def task_note(config: Config, args: SimpleNamespace) -> None:
    with taskfile.locked(config):
        task = taskfile.load(config, args.task)
        taskfile.note(task, args.text)
        taskfile.save(config, task)


def task_move(config: Config, args: SimpleNamespace) -> None:
    with taskfile.locked(config):
        source, to = taskfile.load(config, args.source), taskfile.load(config, args.to)
        if args.item not in source.items or to.fields["status"] == "done" or args.source == args.to:
            raise SystemExit(f"amh: {args.source} has no such open item, or {args.to} is done or the same task")
        source.items.remove(args.item)
        to.items += [args.item] if args.item not in to.items else []
        taskfile.save(config, to)
        taskfile.save(config, source)


def task_check(config: Config, _args: SimpleNamespace) -> int:
    findings = work.check(config)
    print("\n".join(findings), end="\n" if findings else "")
    return 1 if findings else 0


def agent_problems(config: Config, _args: SimpleNamespace) -> int:
    found = work.problems(config)
    for task, problem, evidence in found:
        print(f"{task.name} at {task.address}: {problem} ({evidence})")
    return 3 if found else 0


def agent_list(config: Config, _args: SimpleNamespace) -> None:
    for task in taskfile.active_tasks(config):
        state, evidence = ("retired", "") if task.address == "retired" else agents.status(config, task.address)
        print(f"{state}: {task.name} [{task.fields['status']}] {task.address} {evidence}")


def agent_tree(config: Config, _args: SimpleNamespace) -> None:
    print(work.tree(config))


def agent_status(config: Config, args: SimpleNamespace) -> None:
    print(": ".join(agents.status(config, address(config, args.who))))


def agent_stop(config: Config, args: SimpleNamespace) -> None:
    agents.stop(config, address(config, args.who))


def agent_compact(config: Config, args: SimpleNamespace) -> None:
    agents.send(config, address(config, args.who), "/compact")


def manager_watchers(_config: Config, _args: SimpleNamespace) -> int:
    restarted = subprocess.run(["systemctl", "--user", "restart", UNIT], check=False).returncode
    return restarted or subprocess.run(["systemctl", "--user", "--no-pager", "--lines=3", "status", UNIT], check=False).returncode


def manager_rotate(config: Config, args: SimpleNamespace) -> None:
    print(work.rotate(config, args.who, args.tool, args.model, args.effort))


def manager_worktree(config: Config, _args: SimpleNamespace) -> None:
    for repo in (Path.home() / ".config", config.root):
        dirty = subprocess.run(["git", "-C", str(repo), "status", "--porcelain"], capture_output=True, text=True, check=False).stdout
        print(f"{repo}:\n{dirty}" if dirty else f"{repo}: clean")


def mail_unread(config: Config, args: SimpleNamespace) -> int:
    for header in mail.unread(config, taskfile.own_task(config, args.task_file).tag, []):
        print(f"{header.message_id} {header.subject!r} {header.date}")
    return 0


HANDLERS: dict[tuple[str, str], Run] = {
    ("tell", "manager"): tell_manager,
    ("tell", "human"): tell_human,
    ("tell", "agent"): tell_agent,
    ("tell", "guest"): tell_guest,
    ("todo", "list"): todo_list,
    ("todo", "add"): add_item,
    ("todo", "done"): todo_done,
    ("todo", "replace"): todo_replace,
    ("task", "start"): task_start,
    ("task", "show"): task_show,
    ("task", "close"): task_close,
    ("task", "status"): task_status,
    ("task", "note"): task_note,
    ("task", "give"): add_item,
    ("task", "move"): task_move,
    ("task", "check"): task_check,
    ("agent", "problems"): agent_problems,
    ("agent", "list"): agent_list,
    ("agent", "tree"): agent_tree,
    ("agent", "status"): agent_status,
    ("agent", "stop"): agent_stop,
    ("agent", "compact"): agent_compact,
    ("mail", "unread"): mail_unread,
    ("manager", "watchers"): manager_watchers,
    ("manager", "rotate"): manager_rotate,
    ("manager", "worktree"): manager_worktree,
    ("manager", "watch"): lambda _config, _args: watch.run(),
}


def run(parsed: str) -> int:
    """Run one action from the command line as parsed by the Rust front end: a JSON object of `group`, `action`, and the action's values."""
    values = json.loads(parsed)
    handler = HANDLERS[values.pop("group"), values.pop("action")]
    try:
        return handler(configuration.load(), SimpleNamespace(**values)) or 0
    except agents.AgentError as error:
        raise SystemExit(f"amh: {error}") from error
