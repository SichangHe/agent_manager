"""Talk to running agents, which are Omnigent sessions (`omnigent://ID`); the work is done by the Rust side."""

from __future__ import annotations

import json
import time
from collections.abc import Callable
from pathlib import Path

import amh_rs

from amh.config import Config

OMNIGENT_PREFIX = "omnigent://"
WAIT_S = 90


class AgentError(Exception):
    """An agent could not be reached or refused the request."""


def is_omnigent(address: str) -> bool:
    return address.startswith(OMNIGENT_PREFIX)


def session_path(address: str) -> str:
    """The Omnigent API path of the session at `address`."""
    return "/v1/sessions/" + address.removeprefix(OMNIGENT_PREFIX)


def api_get(path: str) -> dict[str, object]:
    """Read from the Omnigent server."""
    try:
        return json.loads(amh_rs.omnigent_get(path))
    except ValueError as error:
        raise AgentError(str(error)) from error


def own_address() -> str:
    """Where the calling agent runs: its Omnigent session."""
    try:
        return amh_rs.own_address()
    except ValueError as error:
        raise SystemExit(str(error)) from error


def wait_for(done: Callable[[], bool]) -> bool:
    """Poll `done` every second for up to `WAIT_S`; say whether it came true."""
    deadline = time.monotonic() + WAIT_S
    while not done():
        if time.monotonic() > deadline:
            return False
        time.sleep(1)
    return True


def send(_config: Config, address: str, text: str) -> None:
    """Deliver `text` to an agent as its next user message."""
    try:
        amh_rs.agent_send(address, text)
    except ValueError as error:
        raise AgentError(str(error)) from error


def status(_config: Config, address: str) -> tuple[str, str]:
    """Classify an agent as `running`, `ready` (idle), `error`, or `missing`, with one line of evidence."""
    return amh_rs.agent_status(address)


def stop(_config: Config, address: str) -> None:
    """Stop an agent by ending its Omnigent session."""
    try:
        amh_rs.agent_stop(address)
    except ValueError as error:
        raise AgentError(str(error)) from error


def launch(_config: Config, tool: str, model: str, effort: str, workdir: Path, title: str) -> str:
    """Start an agent with no prompt and return its address."""
    try:
        return amh_rs.agent_launch(tool, model, effort, str(workdir), title)
    except ValueError as error:
        raise AgentError(str(error)) from error


def launch_default(_config: Config, tool: str, as_manager: bool) -> tuple[str, str]:
    """The model and effort a tool starts with."""
    return amh_rs.launch_default(tool, as_manager)


def saw_message(_config: Config, address: str, text: str) -> bool:
    """Whether an Omnigent session's recent history holds the last line of `text`."""
    try:
        return amh_rs.agent_saw(address, text)
    except ValueError as error:
        raise AgentError(str(error)) from error
