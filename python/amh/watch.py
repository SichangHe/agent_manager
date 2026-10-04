"""The one background watcher: takes in the human's email, delivers `(pending)` blocks, and nudges managers and idle agents."""

from __future__ import annotations

import imaplib
import random
import sys
import time
from pathlib import Path

from amh import agents, config as configuration, guest, mail, taskfile, work
from amh.config import Config
from amh.taskfile import MARKER, Task

MAIL_SOURCE = "(record and delegate manager_mail/"
# 🧑 "Why so slow ... Improve it"
MAIL_POLL_S = 5
RECONNECT_S = 30
RETRY_S = 600
ANSWER_S = 900
AWAITING = "amh-awaiting.txt"
REMINDER_S = 1800
ACCEPT = "Immediately email the Human to acknowledge acceptance. Record every open item with `amh todo add --from-human`, quoting the human's words:"
ADD_REMINDER = "\nAdd every still-open request to your open items."


def log(text: str) -> None:
    print(f"{time.strftime('%Y-%m-%d %H:%M:%S')} {text}", flush=True)


def for_manager(body: str) -> bool:
    """Whether the human's own text starts or ends with `for manager`, or starts by asking to terminate or replace the agent."""
    # 🧑 “We should only dispatch “for manager” type and replace/terminate type messages to managers.”
    own = [line.strip(" \t.,:;!()[]*_").lower() for line in body.splitlines() if line.strip() and not line.lstrip().startswith(">")]
    return bool(own) and ("for manager" in (own[0], own[-1]) or own[0].startswith(("terminate this agent", "replace this agent")))


def route(config: Config, subject: str, body: str) -> Task:
    """Pick the task a human email belongs to: its `[tag]`, else that task's manager, else the main manager."""
    # 🧑 "it did not get routed to the correct Omnigent agent or even its task file"
    tasks = {task.name: task for task in taskfile.active_tasks(config) if task.address != "retired"}
    by_address = {task.address: task for task in tasks.values()}
    tag = mail.tag_of(subject)
    if tag and f"{tag}.md" in tasks:
        tagged = tasks[f"{tag}.md"]
        return by_address.get(tagged.fields.get("managerat", ""), tagged) if for_manager(body) else tagged
    if tag and (config.root / f"{tag}.md").is_file():
        try:
            manager = taskfile.load(config, f"{tag}.md").fields.get("managerat", "")
        except ValueError:
            manager = ""
        if manager in by_address:
            return by_address[manager]
    if config.main_manager not in by_address:
        raise LookupError("no active task file runs at the main manager address")
    return by_address[config.main_manager]


def take_in_mail(config: Config, box: imaplib.IMAP4_SSL) -> None:
    """Store each new human email and append a `(pending)` block naming it to the right task file."""
    for uid, parsed in mail.unseen_from(box, config.get("OMO_HUMAN_EMAIL_ADDRESS")):
        name = f"{config.get('AMH_MAIL_PREFIX', 'mail')}-{uid}.txt"
        stored = config.mail_dir / name
        subject, body = " ".join(str(parsed["Subject"] or "").split()), mail.text_of(parsed)
        if not stored.exists():
            try:
                with taskfile.locked(config):
                    task = route(config, subject, body)
                    stored.write_text(f"Subject: {subject}\n\n{body}", encoding="utf-8")
                    stored.chmod(0o600)
                    path = config.root / task.name
                    taskfile.write(path, path.read_text(encoding="utf-8").rstrip("\n") + f"\n\n{MARKER}\n{MAIL_SOURCE}{name})\n")
            except (LookupError, ValueError, OSError) as error:
                stored.unlink(missing_ok=True)
                log(f"mail {name} {subject!r} left unread, not taken in: {error!r}")
                continue
            log(f"mail {name} {subject!r} -> {task.name}")
        mail.mark_read(box, uid)


def delivery_text(config: Config, block: list[str]) -> str:
    """Turn a pending block into the message for its agent; a stored human email is inlined verbatim."""
    # 🧑 "Pending blocks in task files should not be `manager_delegation`, they should be dispatched naked, randomly followed by a reminder to add to pending task items"
    # 🧑 "human requests should be sent verbatim"
    if block[0].startswith(guest.SOURCE):
        return guest.delivery_text(config, block[0])
    reminder = ADD_REMINDER if random.random() < 1 / 8 else ""
    if block[0].startswith(MAIL_SOURCE):
        name = block[0].removeprefix(MAIL_SOURCE).rstrip(")")
        content = (config.mail_dir / Path(name).name).read_text(encoding="utf-8").strip()
        return f'{ACCEPT}\n<human_instruction authoritative="true" source="manager_mail/{name}">\n{content}\n</human_instruction>{reminder}'
    return "\n".join(block).strip() + reminder


def deliver_pending(config: Config, failed_at: dict[str, float]) -> None:
    """Send every pending block to its task's agent, then delete the marker line.

    A block in a task that has no agent (done or retired) goes to that task's manager, else to the main manager.
    """
    tasks = taskfile.all_tasks(config)
    agent_at = {task.address for task in tasks if task.fields["status"] != "done" and task.address != "retired"}
    for task in tasks:
        if MARKER not in task.body or time.monotonic() - failed_at.get(task.name, -RETRY_S) < RETRY_S:
            continue
        orphan = task.address not in agent_at or task.fields["status"] == "done"
        target = task.address if not orphan else task.fields.get("managerat") if task.fields.get("managerat") in agent_at else config.main_manager
        for _, block in taskfile.pending_blocks(task.body):
            if not block:
                continue
            try:
                text = (f"This was left in {task.name}, a task that has no agent; it comes to you as its manager.\n" if orphan else "") + delivery_text(config, block)
            except OSError as error:
                failed_at[task.name] = time.monotonic()
                log(f"a block in {task.name} names a stored email that cannot be read: {error}")
                break
            try:
                state, evidence = agents.status(config, target)
                if state == "missing":
                    raise agents.AgentError(f"the agent is missing: {evidence}")
                agents.send(config, target, text)
            except (agents.AgentError, OSError) as error:
                log(f"delivery of a block in {task.name} to {target} failed: {error}")
                fallback = next((t for t in (task.fields.get("managerat"), config.main_manager) if t in agent_at and t != target), None)
                try:
                    if fallback is None:
                        raise agents.AgentError("no manager to fall back to")
                    agents.send(config, fallback, f"This could not be delivered to the agent of {task.name} ({error}); it comes to you as its manager. Handle it or get that agent working again.\n" + text)
                except (agents.AgentError, OSError) as second:
                    failed_at[task.name] = time.monotonic()
                    log(f"fallback delivery for {task.name} failed too: {second}")
                    tell_human_once(config, task, f"undelivered {block[0]}", f"Your message for {task.name} has not been delivered to anyone: its agent could not be reached ({error}), and neither could a manager. The watcher retries every 10 minutes.")
                    break
                log(f"delivered {block[0][:80]!r} in {task.name} to the manager at {fallback} instead")
                tell_human_once(config, task, f"rerouted {block[0]}", f"Your message for {task.name} could not be delivered to its agent ({error}). I gave it to its manager instead, who is to handle it or get that agent working again.")
            else:
                log(f"delivered {block[0][:80]!r} in {task.name} to {target}")
                if block[0].startswith(MAIL_SOURCE) and not orphan:
                    with (config.state_dir / AWAITING).open("a", encoding="utf-8") as record:
                        _ = record.write(f"{time.time():.0f} {task.name} {block[0].removeprefix(MAIL_SOURCE).rstrip(')')} 0\n")
            with taskfile.locked(config):
                fresh = taskfile.load(config, task.name)
                lines = fresh.body.split("\n")
                at = next((i for i, found in taskfile.pending_blocks(fresh.body) if found[:1] == block[:1]), None)
                if at is None:
                    failed_at[task.name] = time.monotonic()
                    log(f"delivered block is no longer in {task.name}; its marker was not deleted")
                    break
                del lines[at]
                fresh.body = "\n".join(lines)
                taskfile.save(config, fresh)


def tell_human_once(config: Config, task: Task, key: str, text: str) -> None:
    """Email the human about one of their messages, at most once per `key`."""
    # 🧑 "if I message an agent and somehow they are not whatever state they are, I should get some sort of response"
    told = config.state_dir / "amh-told-human.txt"
    key = f"{task.name} {key}"
    if told.exists() and key in told.read_text(encoding="utf-8").splitlines():
        return
    try:
        _ = mail.send(config, task.tag, "", text + "\n", mail.WATCHER)
    except Exception as failure:
        log(f"email to the human about {task.name} was not sent: {failure!r}")
        return
    with told.open("a", encoding="utf-8") as record:
        _ = record.write(key + "\n")


def chase_answers(config: Config) -> None:
    """Make sure each delivered human email gets an answer: remind the agent after `ANSWER_S`, then tell its manager and the human."""
    # 🧑 "if I message an agent and somehow they are not whatever state they are, I should get some sort of response"
    path, sent_log = config.state_dir / AWAITING, config.state_dir / mail.SENT_LOG
    if not path.exists():
        return
    sent = [line.split(" ", 1) for line in sent_log.read_text(encoding="utf-8").splitlines()] if sent_log.exists() else []
    tasks = {task.name: task for task in taskfile.active_tasks(config)}
    keep = []
    for row in path.read_text(encoding="utf-8").splitlines():
        at, name, mail_name, stage = row.split(" ")
        task = tasks.get(name)
        if task is None or any(tag == task.tag and float(when) >= float(at) for when, tag in sent):
            continue
        if time.time() - float(at) < ANSWER_S * (int(stage) + 1):
            keep.append(row)
            continue
        given = time.strftime("%H:%M", time.localtime(float(at)))
        try:
            if stage == "0":
                agents.send(config, task.address, f"The human's email manager_mail/{mail_name} was given to you at {given} and you have not emailed the human since. Email the human now with `amh tell human`, even if only to say you got it and what you are doing.")
                keep.append(f"{at} {name} {mail_name} 1")
                continue
            agents.send(config, task.fields.get("managerat", ""), f"The agent of {name} was given the human's email manager_mail/{mail_name} at {given}, was reminded, and still has not emailed the human. Find out why and make sure the human gets an answer.")
        except agents.AgentError as error:
            log(f"chasing an answer from {name} failed: {error}")
        if stage == "1":
            tell_human_once(config, task, f"unanswered {mail_name}", f"The agent of {name} was given your message at {given} and was reminded once, but has not emailed you since. Its manager has been told to find out why.")
    taskfile.write(path, "".join(f"{row}\n" for row in keep))


def failure_text(config: Config, task: Task, problem: str, error: str) -> str:
    """Say in plain words which task's agent has which problem."""
    # 🧑 “This message sucks. I don’t know the task file. Has been recurring.”
    manager = next((other.name for other in taskfile.active_tasks(config) if other.address == task.fields.get("managerat")), task.fields.get("managerat", "none"))
    what = "has failed and is not working any more" if problem == "error" else "is gone: nothing runs at its address"
    return f"The agent working on task file {task.name} {what}.\nTool: {task.fields.get('tool', 'unknown')}, running at {task.address}\nIts manager: {manager}\nOpen items on the task: {len(task.items)}\n" + (f"The error its harness reported:\n{error}\n" if error else "")


def nudge(config: Config, told_at: dict[tuple[str, str], float]) -> None:
    """Remind idle agents of their open items; report each agent failure once to its manager and, for harness errors, to the human."""
    notified = config.state_dir / "amh-problem-notices.txt"
    before = set(notified.read_text(encoding="utf-8").splitlines()) if notified.exists() else set()
    now: set[str] = set()
    by_manager: dict[str, list[str]] = {}
    for task, problem, evidence in work.problems(config):
        if "unreachable" in evidence:
            return
        if problem == "idle with open items":
            if time.monotonic() - told_at.get((task.name, problem), -REMINDER_S) >= REMINDER_S:
                told_at[task.name, problem] = time.monotonic()
                try:
                    agents.send(config, task.address, f"You have {len(task.items)} open items. See them with `amh todo list`. Continue until each is finished or cancelled.")
                except agents.AgentError as error:
                    log(f"reminder to {task.name} failed: {error}")
            continue
        error = " ".join(evidence.partition("last_task_error=")[2].split("; pane_tail:", 1)[0].split()) if problem == "error" else ""
        key = f"{task.name} {task.address} {problem} {error[:200]}"
        now.add(key)
        if key in before:
            continue
        log(f"agent problem: {task.name} at {task.address}: {problem} ({evidence})")
        text = failure_text(config, task, problem, error)
        by_manager.setdefault(task.fields.get("managerat", ""), []).append(text)
        # 🧑 "Harness errors should directly email me too"
        if problem == "error":
            try:
                _ = mail.send(config, task.tag, "", text + "\nIts manager has been told to handle it.\n", "amh watcher")
            except Exception as failure:
                log(f"failure email for {task.name} was not sent: {failure!r}")
                now.discard(key)
    for manager, texts in by_manager.items():
        try:
            agents.send(config, manager, "Handle these agent problems; only email the human if you cannot:\n\n" + "\n".join(texts))
        except agents.AgentError as error:
            log(f"problem notice to {manager} failed: {error}")
    # A problem that went away is forgotten, so it is reported again if it comes back.
    taskfile.write(notified, "".join(f"{key}\n" for key in sorted(now)))


def run() -> int:
    """Loop forever; the service manager restarts the process if it dies."""
    failed_at: dict[str, float] = {}
    told_at: dict[tuple[str, str], float] = {}
    last_nudge = 0.0
    box = None
    retry_login_at = 0.0
    log("watcher started")
    while True:
        config = configuration.load()
        try:
            if box is None and time.monotonic() >= retry_login_at:
                box = mail.connect(config)
            if box is not None:
                take_in_mail(config, box)
                guest.take_in(config, box)
        except (imaplib.IMAP4.error, OSError) as error:
            log(f"mail connection failed, reconnecting in {RECONNECT_S} s: {error!r}")
            box = None
            retry_login_at = time.monotonic() + RECONNECT_S
        except Exception as error:
            log(f"mail intake failed: {error!r}")
        try:
            deliver_pending(config, failed_at)
            if time.monotonic() - last_nudge > 60:
                last_nudge = time.monotonic()
                nudge(config, told_at)
                chase_answers(config)
        except (Exception, SystemExit) as error:
            log(f"task scan failed: {error!r}")
        time.sleep(MAIL_POLL_S)


if __name__ == "__main__":
    sys.exit(run())
