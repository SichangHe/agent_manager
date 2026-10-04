"""Task files and the task list `TODO.md`: the only persistent records of who works on what."""

from __future__ import annotations

import fcntl
import json
import os
import re
import stat
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

from amh import agents
from amh.config import Config

HUMAN_MARK = "🧑 "
SECTIONS = ("current:", "human pending:", "low priority:", "previous:")
ITEMS_KEY = "pending_task_items"
MARKER = "(pending)"


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
        head = "".join(f"{key}: {scalar(value)}\n" for key, value in self.fields.items())
        listing = f"{ITEMS_KEY}:\n" + "".join(f"  - {scalar(item)}\n" for item in self.items) if self.items else f"{ITEMS_KEY}: []\n"
        return f"---\n{head}{listing}---\n{self.body}"


def scalar(value: str) -> str:
    """Write a one-line YAML string: plain when that is unambiguous, else single-quoted."""
    plain = re.fullmatch(r"[^\s\-?:,\[\]{}#&*!|>'\"%@`][^\n]*", value) and ": " not in value and " #" not in value and not value.endswith(":")
    return value if plain and value.lower() not in ("true", "false", "null", "yes", "no", "~") or value in ("true", "false") else "'" + value.replace("'", "''") + "'"


def unscalar(text: str) -> str:
    """Read a one-line YAML string: plain, single-quoted, or double-quoted."""
    text = text.strip()
    if len(text) > 1 and text[0] == text[-1] == "'":
        return text[1:-1].replace("''", "'")
    if len(text) > 1 and text[0] == text[-1] == '"':
        return json.loads(text)
    return text


def parse(name: str, text: str) -> Task:
    """Split a task file into frontmatter and body; raise `ValueError` when it is not a task file."""
    match = re.match(r"---\n(.*?\n)---\n(.*)\Z", text, re.DOTALL)
    if match is None:
        raise ValueError(f"{name} has no frontmatter")
    fields: dict[str, str] = {}
    items: list[str] = []
    key = ""
    for line in match[1].splitlines():
        pair = re.fullmatch(r"(\w+):(?: (.*))?", line)
        if line.startswith("  - ") and key == ITEMS_KEY:
            items.append(unscalar(line[4:]))
        elif pair is None:
            raise ValueError(f"{name} has a frontmatter line that is neither `key: value` nor an open item: {line[:80]}")
        else:
            key = pair[1]
            if key != ITEMS_KEY:
                fields[key] = unscalar(pair[2] or "")
    if "status" not in fields or "runat" not in fields:
        raise ValueError(f"{name} records no status or no agent address")
    return Task(name, fields, items, match[2])


@contextmanager
def locked(config: Config) -> Iterator[None]:
    """Serialize every writer of the work-log root through one lock file."""
    config.state_dir.mkdir(parents=True, exist_ok=True)
    with open(config.state_dir / "amh.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def write(path: Path, text: str) -> None:
    """Replace `path` atomically, keeping its permissions."""
    temp = path.with_name(f".{path.name}.amh-tmp")
    temp.write_text(text, encoding="utf-8")
    temp.chmod(stat.S_IMODE(path.stat().st_mode) if path.exists() else 0o660)
    os.replace(temp, path)


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
    lines = body.split("\n")
    fences = [i for i, line in enumerate(lines) if line.startswith(("```", "~~~"))]
    fenced = {i for start, end in zip(fences[::2], fences[1::2]) for i in range(start, end)}
    found = []
    for index, line in enumerate(lines):
        if index not in fenced and line.strip() == MARKER:
            end = next((i for i in range(index + 1, len(lines)) if lines[i].strip() in ("", MARKER)), len(lines))
            found.append((index, lines[index + 1 : end]))
    return found


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
    # 🧑 "agents’ pending task item list distinguishes human requests from agent-made ones ... prepend a human emoji"
    text = " ".join(item.split())
    while text.startswith(HUMAN_MARK.strip()):
        text = text.removeprefix(HUMAN_MARK.strip()).lstrip()
    if not text:
        raise SystemExit("amh: the item is empty")
    return HUMAN_MARK + text if from_human else text


def section_of(task: Task) -> str:
    """Name the task-list section a task belongs in."""
    status = task.fields["status"]
    if status == "done":
        return "previous:"
    # 🧑 "None of the currently listed as human pending tasks is actually pending on the human."
    waits_on_human = status == "blocked" and task.fields.get("blocked_on", "").lower() == "human"
    return "human pending:" if waits_on_human else "current:"


def section_at(rows: list[str], index: int) -> str:
    """Name the section that row `index` of the task list lies in."""
    return next((rows[i].strip().lower() for i in range(index, -1, -1) if rows[i].strip().lower() in SECTIONS), "")


def place_in_list(config: Config, task: Task) -> None:
    """Put the task's row at the top of its section of `TODO.md`, removing any other row for it."""
    # 🧑 "the task reference moves from `TODO.md` `current` to the top of `previous`"
    path = config.root / "TODO.md"
    section = section_of(task)
    rows = path.read_text(encoding="utf-8").split("\n")
    old = [i for i, row in enumerate(rows) if row.split(" ", 1)[0] == task.name]
    if len(old) == 1 and section_at(rows, old[0]) == section and rows[old[0]].split()[1:2] == [task.address]:
        return
    rows = [row for i, row in enumerate(rows) if i not in old]
    at = next((i + 1 for i, row in enumerate(rows) if row.strip().lower() == section), None)
    if at is None:
        raise SystemExit(f"amh: TODO.md has no `{section}` section")
    while section != "previous:" and at < len(rows) and not rows[at].strip():
        at += 1
    rows.insert(at, f"{task.name} {task.address}")
    write(path, "\n".join(rows))
