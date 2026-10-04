"""What the `amh` actions do to task files, agents, and mail."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

from amh import agents, mail, taskfile
from amh.config import LOCAL_ENV, Config
from amh.taskfile import HUMAN_MARK, Task

# 🧑 "make the manager reporting sound more like messaging the manager, to discourage agents to dump info to manager"
MANAGER_MESSAGE_MAX_CHARS = 600
MANAGER_GUIDES = ("agent_work", "agent_manager", "agent_manager_core")


def envelope(sender: str, text: str) -> str:
    """Mark text as coming from an agent, so the reader does not take it for the human's words."""
    return f'<agent_message from="{sender}">\n{text.strip()}\n</agent_message>'


def notify_human(config: Config, task: Task, change: str, items: list[str]) -> None:
    """Email the human that their requests were created or deleted on a task's list; call it outside the lock."""
    # 🧑 “Pending items originated from the human need emails, ones from agents do not.”
    theirs = [item.removeprefix(HUMAN_MARK) for item in items if item.startswith(HUMAN_MARK)]
    if theirs:
        _ = mail.send(config, task.tag, "", f"pending item {change}:\n" + "".join(f"- {item}\n" for item in theirs), task.address)


def add_item(config: Config, task: Task, item: str, from_human: bool) -> list[str]:
    """Append one open item to a freshly loaded task; return it in a list, or nothing when the task already has it."""
    if task.fields["status"] == "done":
        raise SystemExit(f"amh: {task.name} is done; start a new task instead")
    marked = taskfile.mark(item, from_human)
    if marked in task.items:
        return []
    task.items.append(marked)
    taskfile.save(config, task)
    return [marked]


def remove_item(config: Config, task: Task, item: str, evidence: str) -> None:
    """Remove one finished or cancelled item from a freshly loaded task, leaving a one-line record."""
    if item not in task.items:
        raise SystemExit(f"amh: {task.name} has no such open item; copy it exactly from `amh todo list`")
    task.items.remove(item)
    taskfile.note(task, f"removed open item: {evidence}")
    taskfile.save(config, task)


def tell_manager(config: Config, task: Task, text: str, state: str) -> None:
    if len(text) > MANAGER_MESSAGE_MAX_CHARS:
        raise SystemExit(f"amh: keep it under {MANAGER_MESSAGE_MAX_CHARS} characters; write details to a file and name its path")
    if "managerat" not in task.fields:
        raise SystemExit(f"amh: {task.name} records no manager")
    label = f"{task.name} is {state}: " if state else f"{task.name}: "
    agents.send(config, task.fields["managerat"], envelope(task.address, label + text))


def tell_human(config: Config, task: Task, subject: str, body: str, replaces: list[str]) -> None:
    message_id = mail.send(config, task.tag, subject, body, task.address)
    print(f"Email sent.\nMessage-ID: {message_id}")
    # 🧑 "sending email on a thread with unread emails from the same agent prompts the agent to replace unread emails with one single after the send"
    try:
        others = [h for h in mail.unread(config, task.tag, replaces) if h.message_id != message_id]
    except Exception as error:
        print(f"The email is sent; do not resend it. The check for your earlier unread emails failed: {error!r}")
        return
    if others:
        print("The human has not read these earlier emails of yours:")
        for header in others:
            print(f"  {header.message_id} {header.subject!r} {header.date}")
        print("If one email could now replace them and the one just sent, send it with `--replaces ID` for each; replaced unread emails go to the trash.")


def instructions(guides: tuple[str, ...], task_name: str) -> str:
    """Build the start of a first prompt: the task's name and the output of each `getagentsmd` command."""
    # 🧑 "put task file, without the ‘.md’ in all starter prompts and use that as tags"
    # 🧑 “makes its content available in the manager's initial prompt before the manager starts ... make it clear that it is the output of that command”
    parts = [f"Task tag: {Path(task_name).stem}\nTask file: {task_name}\nUse --task-file {task_name} with `amh` when it cannot tell who you are."]
    for guide in ("", *guides):
        command = ["getagentsmd", *(["get", guide] if guide else [])]
        shown = subprocess.run(command, capture_output=True, text=True, timeout=30, check=False)
        if shown.returncode or not shown.stdout.strip():
            raise SystemExit(f"amh: `{' '.join(command)}` failed: {shown.stderr.strip()}")
        parts.append(f"$ {' '.join(command)}\n{shown.stdout.strip()}")
    return "\n".join(parts)


def start_agent(config: Config, name: str, fields: dict[str, str], tool: str, model: str, effort: str, workdir: Path, guides: tuple[str, ...], request: str, record: str, tmux_session: str | None = None, proxy: str | None = None) -> Task:
    """Launch an agent for task `name`, record where it runs plus `fields` and `record` in the task file, and give it its first prompt."""
    prompt = f"{instructions(guides, name)}\n{request}"
    address = agents.launch(config, tool, model, effort, workdir, Path(name).stem, tmux_session, proxy)
    with taskfile.locked(config):
        task = taskfile.load(config, name) if (config.root / name).exists() else Task(name, {"version": "v1.0.0"}, [], "")
        task.fields |= {**fields, "runat": address, "tool": tool}
        _ = task.fields.pop("session_id", None)
        if agents.is_omnigent(address):
            task.fields["session_id"] = address.removeprefix(agents.OMNIGENT_PREFIX)
        if task.fields["status"] != "blocked":
            _ = task.fields.pop("blocked_on", None)
        task.body = (task.body.rstrip("\n") + "\n\n" if task.body.strip() else "") + record
        taskfile.save(config, task)
        taskfile.place_in_list(config, task)
    # 🧑 "Make the agent spawning script start the session and then inject the prompt, in 2 separate steps, and verify that it works"
    agents.send(config, address, prompt)
    if agents.is_omnigent(address) and not agents.wait_for(lambda: agents.saw_message(config, address, prompt)):
        raise SystemExit(f"amh: {address} runs and {name} records it, but the first prompt did not show up in its history; send it again with `amh tell agent`")
    return task


def start_task(
    config: Config, name: str, workdir: Path, goal: str, manager: str, tool: str | None = None, model: str | None = None, effort: str | None = None, as_manager: bool = False, tmux_session: str | None = None, proxy: str | None = None, email: str | None = None, lines: str | None = None
) -> str:
    """Create or reuse a task file, launch its agent with `goal` as its manager's request, and return the agent's address.

    `email` and `lines` name the stored human email lines that caused the task; they are quoted to the agent.
    """
    if (config.root / name).exists():
        old = taskfile.load(config, name)
        if old.fields["status"] != "done" and old.address != "retired" and agents.status(config, old.address)[0] != "missing":
            raise SystemExit(f"amh: {name} already has a live agent at {old.address}; message it, or stop it first")
    tool = tool or ("claude" if as_manager else "codex")
    default_model, default_effort = (agents.MANAGER_DEFAULTS if as_manager else agents.DEFAULTS)[tool]
    request = f'<manager_delegation from="{manager}">\n{goal.strip()}\n</manager_delegation>\n'
    if email:
        # 🧑 "human requests should be sent verbatim"
        span = re.fullmatch(r"([1-9]\d*)-([1-9]\d*)", lines or "")
        if span is None:
            raise SystemExit("amh: --lines must be START-END, counting from 1")
        excerpt = (config.mail_dir / Path(email).name).read_text(encoding="utf-8").splitlines()[int(span[1]) - 1 : int(span[2])]
        request += f'<human_instruction authoritative="true" source="manager_mail/{Path(email).name}:{lines}">\n' + "\n".join(excerpt) + "\n</human_instruction>\n"
    # 🧑 “Long running simply means that the agent will not be closed if they have zero pending item.”
    fields = {"status": "long_running" if as_manager else "running", "managerat": manager, "is_manager": str(as_manager).lower()}
    guides = (*MANAGER_GUIDES, "submanager") if as_manager else ("agent_work",)
    return start_agent(config, name, fields, tool, model or default_model, effort or default_effort, workdir, guides, request, request, tmux_session, proxy).address


def close_task(config: Config, name: str, agent_gone: bool, email: bool, own: bool) -> str:
    """Mark a task done, move it to `previous`, tell the human, stop its agent last, and return the agent's address.

    `own` means the agent closes itself: its manager is told, and stopping the session ends the caller.
    """
    with taskfile.locked(config):
        task = taskfile.load(config, name)
        if task.items or taskfile.pending_blocks(task.body):
            raise SystemExit(f"amh: {name} still has open items or an undelivered `(pending)` block; finish, cancel, or move them first")
        task.fields["status"] = "done"
        _ = task.fields.pop("blocked_on", None)
        taskfile.save(config, task)
        taskfile.place_in_list(config, task)
    # 🧑 "When closing an agent, use the last email chain the agent used to send an automatic email ‘Closed xx:n’ with the agent’s window."
    if email:
        _ = mail.send(config, task.tag, "", f"Closed {task.address}\n", task.address)
    # 🧑 "Agents should be able to close themselves. Have a command in amh for that"
    if own:
        print(f"closed {name}; stopping your own session now", flush=True)
        try:
            agents.send(config, task.fields.get("managerat", ""), envelope(task.address, f"{name} had no open work left and closed itself."))
        except agents.AgentError:
            pass
    try:
        agents.stop(config, task.address)
    except agents.AgentError as error:
        if not agent_gone:
            raise SystemExit(f"amh: {name} is marked done, but its agent was not stopped: {error}") from error
    return task.address


def set_status(config: Config, name: str, status: str, blocked_on: str | None) -> None:
    if (status == "blocked") != bool(blocked_on) and status != "long_running":
        raise SystemExit("amh: `blocked` needs --on saying what it waits for; `running` takes no --on")
    with taskfile.locked(config):
        task = taskfile.load(config, name)
        task.fields["status"] = status
        _ = task.fields.pop("blocked_on", None)
        if blocked_on:
            task.fields["blocked_on"] = " ".join(blocked_on.split())
        taskfile.save(config, task)
        taskfile.place_in_list(config, task)


def check(config: Config) -> list[str]:
    """Return every disagreement between task files and the task list."""
    # 🧑 "Maybe we should have a validation script that agents should run after editing task file/TODO file?"
    findings = []
    rows: dict[str, list[tuple[str, str]]] = {}
    section = ""
    for row in (config.root / "TODO.md").read_text(encoding="utf-8").splitlines():
        if row.strip().lower() in taskfile.SECTIONS:
            section = row.strip().lower()
        elif section and re.match(r"\S+\.md( |$)", row):
            name, _, address = row.partition(" ")
            rows.setdefault(name, []).append((section, address.split(" ")[0]))
    active = {task.name: task for task in taskfile.active_tasks(config)}
    for name, task in active.items():
        placed = rows.get(name, [])
        if not placed and task.fields["status"] == "blocked":
            continue
        if len(placed) != 1:
            findings.append(f"{name}: {len(placed)} rows in TODO.md, want 1")
        elif placed[0] != (taskfile.section_of(task), task.address):
            findings.append(f"{name}: TODO.md has it under `{placed[0][0]}` at {placed[0][1]}, the task file says `{taskfile.section_of(task)}` at {task.address}")
    # 🧑 "You have current tasks in \"previous\" in TODO.md ... Act to prevent that in the future"
    for name, placed in rows.items():
        if name not in active and any(section != "previous:" for section, _ in placed):
            findings.append(f"{name}: listed as open work in TODO.md but it is done or not a task file")
    shared: dict[str, list[str]] = {}
    for task in active.values():
        shared.setdefault(task.address, []).append(task.name)
    findings += [f"{address}: several active tasks run there: {', '.join(names)}" for address, names in shared.items() if len(names) > 1 and address != "retired"]
    return findings


def problems(config: Config) -> list[tuple[Task, str, str]]:
    """Return active tasks whose agent is gone, failed, or idle with open items, as (task, problem, evidence)."""
    found = []
    for task in taskfile.active_tasks(config):
        if task.fields["status"] == "blocked" or task.address == "retired":
            continue
        state, evidence = agents.status(config, task.address)
        waiting = task.fields["status"] == "long_running" and "blocked_on" in task.fields
        if state in ("missing", "error") or (state == "ready" and task.items and not waiting):
            found.append((task, "idle with open items" if state == "ready" else state, evidence))
    return found


def tree(config: Config) -> str:
    """Draw who reports to whom among active tasks, starting from the main manager."""
    tasks = taskfile.active_tasks(config)
    reports: dict[str, list[Task]] = {}
    for task in tasks:
        reports.setdefault(task.fields.get("managerat", ""), []).append(task)
    lines: list[str] = []
    drawn: set[str] = set()

    def draw(task: Task, depth: int) -> None:
        if task.name in drawn:
            return
        drawn.add(task.name)
        role = "manager" if task.fields.get("is_manager") == "true" else "worker"
        lines.append(f"{'  ' * depth}{task.name} [{role}, {task.fields['status']}] {task.address}")
        lines.extend(f"{'  ' * depth}  - {item}" for item in task.items)
        for report in reports.get(task.address, []):
            draw(report, depth + 1)

    for task in tasks:
        if task.address == config.main_manager:
            draw(task, 0)
    orphans = [task for task in tasks if task.name not in drawn]
    if orphans:
        lines.append("not under the main manager:")
        for task in orphans:
            draw(task, 1)
    return "\n".join(lines)


def rotate(config: Config, name: str, tool: str | None, model: str | None, effort: str | None) -> str:
    """Replace a manager with a fresh agent that takes over its task file and its reports."""
    # 🧑 "Make it s.t. rotation works for Omnigent overall"
    old = taskfile.load(config, name).address
    if not agents.is_omnigent(old):
        raise SystemExit("amh: only Omnigent managers can be rotated")
    session = agents.api(config, "GET", agents.session_path(old) + "?include_items=false")
    is_main = old == config.main_manager
    guides = (*MANAGER_GUIDES, "main_manager" if is_main else "submanager")
    request = f"You replace the manager that ran at {old}. Its task file {name} in {config.root} holds its open work and history; take over from there.\n"
    new = start_agent(
        config, name, {}, tool or str(session["harness"]).removesuffix("-native"), model or str(session["model_override"]), effort or str(session["reasoning_effort"]), Path(str(session["workspace"])), guides, request, f"(manager replaced: {old} handed over to a fresh agent)\n"
    ).address
    with taskfile.locked(config):
        for other in taskfile.active_tasks(config):
            if other.fields.get("managerat") == old:
                other.fields["managerat"] = new
                taskfile.save(config, other)
        if is_main:
            taskfile.write(LOCAL_ENV, LOCAL_ENV.read_text(encoding="utf-8").replace(old, new))
    agents.stop(config, old)
    return new
