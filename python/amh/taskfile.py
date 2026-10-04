"""Task files and the task list `TODO.md`: the only persistent records of who works on what."""

from __future__ import annotations

import fcntl
import os
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

import amh_rs

from amh import agents
from amh.config import Config

HUMAN_MARK: str = amh_rs.HUMAN_MARK
SECTIONS: tuple[str, ...] = tuple(amh_rs.SECTIONS)
MARKER: str = amh_rs.MARKER


@dataclass
class Task:
    """One task file: frontmatter `fields`, open `items`, and free-text `body`."""

    name: str
    fields: dict[str, str]
    items: list[str]
    body: str

    @property
    def address(self) -> str:
        return self.fields["runat"]

    @property
    def tag(self) -> str:
        """The file name without `.md`: the `[tag]` of the task's email subjects."""
        return Path(self.name).stem

    def render(self) -> str:
        return amh_rs.render_task(list(self.fields.items()), self.items, self.body)


def parse(name: str, text: str) -> Task:
    """Split a task file into frontmatter and body; raise `ValueError` when it is not a task file."""
    fields, items, body = amh_rs.parse_task(name, text)
    return Task(name, dict(fields), items, body)


@contextmanager
def locked(config: Config) -> Iterator[None]:
    """Serialize every writer of the work-log root through one lock file."""
    config.state_dir.mkdir(parents=True, exist_ok=True)
    with open(config.state_dir / "amh.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def write(path: Path, text: str) -> None:
    """Replace `path` atomically, keeping its permissions."""
    amh_rs.write_file(path, text)


def load(config: Config, name: str) -> Task:
    path = config.root / name
    if path.parent != config.root or path.suffix != ".md" or not path.is_file():
        raise SystemExit(f"amh: no task file {name} in the work-log root")
    return parse(name, path.read_text(encoding="utf-8"))


def save(config: Config, task: Task) -> None:
    write(config.root / task.name, task.render())


def note(task: Task, text: str) -> None:
    """Append a one-line `(note)` to the body, after a blank line so it never joins a pending block."""
    task.body = task.body.rstrip("\n") + f"\n\n({' '.join(text.split())})\n"


def pending_blocks(body: str) -> list[tuple[int, list[str]]]:
    """Find each `(pending)` line outside code fences, with the lines after it up to the next blank line."""
    return amh_rs.pending_blocks(body)


def all_tasks(config: Config) -> list[Task]:
    """Every task file in the root."""
    tasks = []
    for path in sorted(config.root.glob("*.md")):
        try:
            tasks.append(parse(path.name, path.read_text(encoding="utf-8")))
        except ValueError:
            continue
    return tasks


def active_tasks(config: Config) -> list[Task]:
    """Every task file in the root that is not done."""
    return [task for task in all_tasks(config) if task.fields["status"] in ("running", "long_running", "blocked")]


def own_task(config: Config, named: str | None) -> Task:
    """Find the caller's task: the named file, else the one active task whose agent runs at the caller's address."""
    # 🧑 "We need to pass in the task file name at launch and let the agent pass in that when they use any helper command."
    if named := named or os.environ.get("OMO_AGENT_TASK_FILE"):
        return load(config, named)
    address = agents.own_address()
    matches = [task for task in active_tasks(config) if task.address == address]
    if len(matches) != 1:
        raise SystemExit(f"amh: {len(matches)} active tasks run at {address}; pass --task-file NAME.md")
    return matches[0]


def mark(item: str, from_human: bool) -> str:
    """Normalize one item to a single line carrying exactly the right authorship mark."""
    try:
        return amh_rs.mark_item(item, from_human)
    except ValueError as error:
        raise SystemExit(str(error)) from error


def section_of(task: Task) -> str:
    """Name the task-list section a task belongs in."""
    status = task.fields["status"]
    if status == "done":
        return "previous:"
    # 🧑 "None of the currently listed as human pending tasks is actually pending on the human."
    waits_on_human = status == "blocked" and task.fields.get("blocked_on", "").lower() == "human"
    return "human pending:" if waits_on_human else "current:"


def place_in_list(config: Config, task: Task) -> None:
    """Put the task's row at the top of its section of `TODO.md`, removing any other row for it."""
    path = config.root / "TODO.md"
    try:
        moved = amh_rs.placed_in_list(path.read_text(encoding="utf-8"), task.name, task.address, section_of(task))
    except ValueError as error:
        raise SystemExit(str(error)) from error
    if moved is not None:
        write(path, moved)
