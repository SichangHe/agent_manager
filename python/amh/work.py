"""What the `amh` actions do to task files, agents, and mail."""

from __future__ import annotations

import re
import subprocess
from collections.abc import Sequence
from pathlib import Path

from amh import agents, mail, taskfile
from amh.config import Config
from amh.taskfile import Task

# 🧑 "make the manager reporting sound more like messaging the manager, to discourage agents to dump info to manager"
MANAGER_MESSAGE_MAX_CHARS = 600
MANAGER_GUIDES = ("agent_work", "agent_manager", "agent_manager_core")


def envelope(sender: str, text: str) -> str:
    """Mark text as coming from an agent, so the reader does not take it for the human's words."""
    return f'<agent_message from="{sender}">\n{text.strip()}\n</agent_message>'


def tell_manager(config: Config, task: Task, text: str, state: str) -> None:
    if len(text) > MANAGER_MESSAGE_MAX_CHARS:
        raise SystemExit(f"amh: keep it under {MANAGER_MESSAGE_MAX_CHARS} characters; write details to a file and name its path")
    if "managerat" not in task.fields:
        raise SystemExit(f"amh: {task.name} records no manager")
    label = f"{task.name} is {state}: " if state else f"{task.name}: "
    agents.send(config, task.fields["managerat"], envelope(task.address, label + text))


def tell_human(config: Config, task: Task, subject: str, body: str, replaces: list[str], attachments: Sequence[Path] = ()) -> None:
    message_id = mail.send(config, task.tag, subject, body, task.address, attachments) if attachments else mail.send(config, task.tag, subject, body, task.address)
    print(f"Email sent.\nMessage-ID: {message_id}")
    # 🧑 "sending email on a thread with unread emails from the same agent prompts the agent to replace unread emails with one single after the send"
    # 🧑 "this process should ignore emails about pending tasks being created and removed"
    try:
        others = [h for h in mail.unread(config, task.tag, replaces) if h.message_id != message_id and not h.is_notice]
    except Exception as error:
        print(f"The email is sent; do not resend it. The check for your earlier unread emails failed: {error!r}")
        return
    if others:
        print(f"The human has {len(others)} earlier unread email(s) of yours besides the one just sent. The human wants at most 1 unread email from you:")
        for header in others:
            print(f"  {header.message_id} {header.subject!r} {header.date}")
        flags = " ".join(f"--replaces '{header.message_id}'" for header in others)
        print("Do this now: write ONE email that keeps everything the human still needs from those emails and from the one just sent, and send it with")
        print(f"  amh tell human --subject SAME_SUBJECT --file MERGED_FILE {flags} --replaces '{message_id}'")
        print("The replaced unread emails go to the trash. Skip this only if those earlier emails must stay separate.")


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


def start_agent(config: Config, name: str, fields: dict[str, str], tool: str, model: str, effort: str, workdir: Path, guides: tuple[str, ...], request: str, record: str) -> Task:
    """Launch an agent for task `name`, record where it runs plus `fields` and `record` in the task file, and give it its first prompt."""
    prompt = f"{instructions(guides, name)}\n{request}"
    address = agents.launch(config, tool, model, effort, workdir, Path(name).stem)
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
    config: Config, name: str, workdir: Path, goal: str, manager: str, tool: str | None = None, model: str | None = None, effort: str | None = None, as_manager: bool = False, email: str | None = None, lines: str | None = None
) -> str:
    """Create or reuse a task file, launch its agent with `goal` as its manager's request, and return the agent's address.

    `email` and `lines` name the stored human email lines that caused the task; they are quoted to the agent.
    """
    if (config.root / name).exists():
        old = taskfile.load(config, name)
        if old.fields["status"] != "done" and old.address != "retired" and agents.status(config, old.address)[0] != "missing":
            raise SystemExit(f"amh: {name} already has a live agent at {old.address}; message it, or stop it first")
    tool = tool or ("claude" if as_manager else "codex")
    default_model, default_effort = agents.launch_default(config, tool, as_manager)
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
    return start_agent(config, name, fields, tool, model or default_model, effort or default_effort, workdir, guides, request, request).address


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


def problems(config: Config) -> list[tuple[Task, str, str]]:
    """Return active tasks whose agent is gone, failed, or idle with open items, as (task, problem, evidence)."""
    found = []
    for task in taskfile.active_tasks(config):
        if task.fields["status"] == "blocked" or task.address == "retired":
            continue
        state, evidence = agents.status(config, task.address)
        waiting = task.fields["status"] == "long_running" and "blocked_on" in task.fields
        if state in ("missing", "error", "stuck") or (state == "ready" and task.items and not waiting):
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
    session = agents.api_get(agents.session_path(old) + "?include_items=false")
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
            env = config.state_dir / "env.toml"
            taskfile.write(env, env.read_text(encoding="utf-8").replace(old, new))
    agents.stop(config, old)
    return new
