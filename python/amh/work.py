"""What the `amh` actions do to task files, agents, and mail."""

from __future__ import annotations

from collections.abc import Sequence
from pathlib import Path

import amh_rs

from amh import agents, mail, taskfile
from amh.config import Config
from amh.taskfile import Task

# 🧑 "make the manager reporting sound more like messaging the manager, to discourage agents to dump info to manager"
MANAGER_MESSAGE_MAX_CHARS = 600


def tell_manager(config: Config, task: Task, text: str, state: str) -> None:
    if len(text) > MANAGER_MESSAGE_MAX_CHARS:
        raise SystemExit(f"amh: keep it under {MANAGER_MESSAGE_MAX_CHARS} characters; write details to a file and name its path")
    if "managerat" not in task.fields:
        raise SystemExit(f"amh: {task.name} records no manager")
    label = f"{task.name} is {state}: " if state else f"{task.name}: "
    agents.send(config, task.fields["managerat"], amh_rs.envelope(task.address, label + text))


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
            agents.send(config, task.fields.get("managerat", ""), amh_rs.envelope(task.address, f"{name} had no open work left and closed itself."))
        except agents.AgentError:
            pass
    try:
        agents.stop(config, task.address)
    except agents.AgentError as error:
        if not agent_gone:
            raise SystemExit(f"amh: {name} is marked done, but its agent was not stopped: {error}") from error
    return task.address


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
    guides = [*amh_rs.MANAGER_GUIDES, "main_manager" if is_main else "submanager"]
    request = f"You replace the manager that ran at {old}. Its task file {name} in {config.root} holds its open work and history; take over from there.\n"
    record = f"(manager replaced: {old} handed over to a fresh agent)\n"
    new = amh_rs.start_agent(name, [], tool or str(session["harness"]).removesuffix("-native"), model or str(session["model_override"]), effort or str(session["reasoning_effort"]), str(session["workspace"]), guides, request, record)
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
