"""Talk to running agents: Omnigent sessions (`omnigent://ID`) and tmux windows (`SESSION:WINDOW`)."""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from pathlib import Path

from amh.config import Config

OMNIGENT_PREFIX = "omnigent://"
# 🧑 "Default should be opus 5.5 low" ... "gpt-6-luna high for managers and gpt-6.1-sol low for workers" ... "Cursor should use the latest Grok with high by default and not enable fast, Antigravity should use the latest Gemini"
DEFAULTS = {"claude": ("claude-opus-5-5", "low"), "codex": ("gpt-6.1-sol", "low"), "cursor": ("grok-4.7", "high"), "antigravity": ("gemini-3.8-flash-high", "high")}
MANAGER_DEFAULTS = DEFAULTS | {"codex": ("gpt-6-luna", "high")}
# 🧑 "make Omnigent run every harness without sandbox and with no permission asking"
UNRESTRICTED = {
    "claude": ["--dangerously-skip-permissions"],
    "antigravity": ["--dangerously-skip-permissions"],
    "cursor": ["--force", "--sandbox", "disabled", "--trust", "--approve-mcps"],
    "codex": ["--dangerously-bypass-approvals-and-sandbox", "--config", "shell_environment_policy.inherit=all"],
}
FOLLOW_UP = "The message above reached you through `amh`, the helper that carries the human's and your manager's requests to you; act on it."
CODEX_COMMAND = "bunx @openai/codex@latest --no-daemon --dangerously-bypass-approvals-and-sandbox"
WAIT_S = 90


class AgentError(Exception):
    """An agent could not be reached or refused the request."""


def is_omnigent(address: str) -> bool:
    return address.startswith(OMNIGENT_PREFIX)


def session_path(address: str) -> str:
    """The Omnigent API path of the session at `address`."""
    return "/v1/sessions/" + address.removeprefix(OMNIGENT_PREFIX)


def api(config: Config, method: str, path: str, body: dict[str, object] | None = None) -> dict[str, object]:
    """Call the Omnigent server and return its JSON answer."""
    url = config.get("OMO_MANAGER_OMNIGENT_URL", "http://127.0.0.1:6767") + path
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(url, data=data, method=method, headers={"Content-Type": "application/json"})
    if token := config.get("OMO_MANAGER_OMNIGENT_TOKEN"):
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=30) as answer:
            return json.loads(answer.read() or b"{}")
    except urllib.error.HTTPError as error:
        raise AgentError(f"Omnigent answered {error.code} for {method} {path}: {error.read().decode(errors='replace')[:300]}") from error
    except OSError as error:
        raise AgentError(f"Omnigent is unreachable at {url}: {error}") from error


def post_event(config: Config, address: str, kind: str, data: dict[str, object]) -> dict[str, object]:
    return api(config, "POST", session_path(address) + "/events", {"type": kind, "data": data})


def post_message(config: Config, address: str, text: str) -> dict[str, object]:
    return post_event(config, address, "message", {"role": "user", "content": [{"type": "input_text", "text": text}]})


def tmux(*args: str, text_in: str | None = None) -> subprocess.CompletedProcess[str]:
    """Run tmux on the user's default server; an Omnigent session's own `TMUX` points at a private one."""
    env = {key: value for key, value in os.environ.items() if key != "TMUX"}
    return subprocess.run(["tmux", *args], input=text_in, capture_output=True, text=True, check=False, env=env)


def pane_text(address: str, n_lines: int = 40) -> str | None:
    """Return the last visible lines of a tmux window, or `None` when it does not exist."""
    shown = tmux("capture-pane", "-p", "-t", address, "-S", f"-{n_lines}")
    return shown.stdout if shown.returncode == 0 else None


def own_address() -> str:
    """Where the calling agent runs: its Omnigent session, else its tmux window."""
    if session := os.environ.get("OMNIGENT_RUNNER_PRIMARY_SESSION_ID"):
        return OMNIGENT_PREFIX + session
    if pane := os.environ.get("TMUX_PANE"):
        shown = tmux("display-message", "-p", "-t", pane, "#{session_name}:#{window_index}")
        if shown.returncode == 0:
            return shown.stdout.strip()
    raise SystemExit("amh: cannot tell which agent is calling; pass --task-file NAME.md")


def wait_for(done: Callable[[], bool]) -> bool:
    """Poll `done` every second for up to `WAIT_S`; say whether it came true."""
    deadline = time.monotonic() + WAIT_S
    while not done():
        if time.monotonic() > deadline:
            return False
        time.sleep(1)
    return True


def send(config: Config, address: str, text: str) -> None:
    """Deliver `text` to an agent as its next user message."""
    if is_omnigent(address):
        answer = post_message(config, address, text)
        if answer.get("queued") is not True:
            raise AgentError(f"{address} did not queue the message: {answer}")
        if "\n" in text.strip():
            # Claude Code shows a multi-line message as pasted text and acts on it only when a typed line says to.
            try:
                _ = post_message(config, address, FOLLOW_UP)
            except AgentError:
                pass
        return
    state, evidence = status(config, address)
    if state == "missing":
        raise AgentError(f"no agent runs in tmux window {address}: {evidence}")
    buffer = f"amh-{os.getpid()}"
    loaded = tmux("load-buffer", "-b", buffer, "-", text_in=text)
    pasted = tmux("paste-buffer", "-d", "-p", "-b", buffer, "-t", address)
    if loaded.returncode or pasted.returncode:
        raise AgentError(f"tmux refused the paste into {address}: {loaded.stderr}{pasted.stderr}")
    # 🧑 "don’t need to check if the pasted message renders for any harness and just send them"
    time.sleep(1)
    submitted = tmux("send-keys", "-t", address, "Enter")
    if submitted.returncode:
        raise AgentError(f"tmux refused to submit into {address}: {submitted.stderr}")


def status(config: Config, address: str) -> tuple[str, str]:
    """Classify an agent as `running`, `ready` (idle), `error`, or `missing`, with one line of evidence."""
    if is_omnigent(address):
        try:
            session = api(config, "GET", session_path(address) + "?include_items=false&refresh_state=true")
        except AgentError as error:
            return "missing", str(error)
        state = session.get("status")
        evidence = f"session_status={state} harness={session.get('harness')} runner_online={session.get('runner_online')}"
        if state == "failed":
            return "error", f"{evidence} last_task_error={str(session.get('last_task_error'))[:300]}"
        # Omnigent takes an idle session's runner offline and brings it back on the next message, so that session is `ready`.
        parked = state == "idle" and session.get("host_online") is True
        if session.get("runner_online") is not True and not parked:
            return "missing", evidence
        return ("running" if state in ("running", "waiting") else "ready" if state == "idle" else "missing"), evidence
    text = pane_text(address)
    if text is None:
        return "missing", "no such tmux window"
    last = " / ".join(line.strip() for line in text.splitlines() if line.strip())[-200:]
    if tmux("display-message", "-p", "-t", address, "#{pane_current_command}").stdout.strip() in ("zsh", "bash", "fish", "sh"):
        return "missing", f"only a shell runs there: {last}"
    return ("running" if "esc to interrupt" in text else "ready"), last


def stop(config: Config, address: str) -> None:
    """Stop an agent: end its Omnigent session, or kill its tmux window."""
    if is_omnigent(address):
        _ = post_event(config, address, "stop_session", {})
        return
    # 🧑 Tmux sessions whose names start with `h` are human-owned.
    if address.startswith("h"):
        raise AgentError(f"{address} is in a human-owned tmux session; ask the human to close it")
    if tmux("kill-window", "-t", address).returncode:
        raise AgentError(f"no tmux window {address}")


def launch(config: Config, tool: str, model: str, effort: str, workdir: Path, title: str, tmux_session: str | None, proxy: str | None) -> str:
    """Start an agent with no prompt and return its address."""
    # 🧑 "make Omnigent the default when launching agents and add `--tmux` for the old tmux"
    if tmux_session is not None:
        if tool != "codex":
            raise AgentError("only Codex can be launched in tmux")
        command = f'{CODEX_COMMAND} --model {model} --config model_reasoning_effort="{effort}" --config check_for_update_on_startup=false'
        if proxy:
            # The local proxy owns upstream login and account rotation, so Codex must not use its own saved login.
            settings = ('model_provider="amh_proxy"', 'model_providers.amh_proxy.name="Local proxy"', f"model_providers.amh_proxy.base_url={json.dumps(proxy)}", 'model_providers.amh_proxy.wire_api="responses"', "model_providers.amh_proxy.requires_openai_auth=false")
            command += "".join(f" --config {shlex.quote(setting)}" for setting in settings)
        # The window runs Codex directly, not a shell, so a prompt can never be typed into a shell.
        script = f"export OMO_AGENT_TASK_FILE={shlex.quote(title + '.md')} PATH={shlex.quote(str(Path.home() / '.config/bin'))}:$PATH; exec {command}"
        made = tmux("new-window", "-d", "-P", "-F", "#{session_name}:#{window_index}", "-t", f"{tmux_session}:", "-n", title, "-c", str(workdir), f"bash -lc {shlex.quote(script)}")
        if made.returncode:
            raise AgentError(f"tmux could not open a window in session {tmux_session}: {made.stderr.strip()}")
        address = made.stdout.strip()
        if not wait_for(lambda: "OpenAI Codex" in (pane_text(address, 200) or "")):
            raise AgentError(f"Codex did not come up in {address}: {(pane_text(address) or 'the window closed').strip()[-300:]}")
        return address
    found = [a for a in api(config, "GET", "/v1/agents?limit=1000").get("data", []) if a.get("name") == f"{tool}-native-ui"]
    hosts = [h for h in api(config, "GET", "/v1/hosts").get("hosts", []) if h.get("status") == "online"]
    if len(found) != 1 or len(hosts) != 1:
        raise AgentError(f"Omnigent has {len(found)} `{tool}-native-ui` agents and {len(hosts)} online hosts; need exactly one of each")
    # 🧑 "After the codex update, path no longer works, e.g. agents don't have getagentsmd on path"
    codex_args = ["--config", f'shell_environment_policy.set.PATH="{Path.home()}/.config/bin:{os.environ["PATH"]}"'] if tool == "codex" else []
    session = api(
        config,
        "POST",
        "/v1/sessions",
        {
            "agent_id": found[0]["id"],
            "host_id": hosts[0]["host_id"],
            "workspace": str(workdir),
            "title": title,
            "model_override": f"{model}-{effort}" if tool == "cursor" else model,
            "reasoning_effort": effort,
            "terminal_launch_args": UNRESTRICTED[tool] + codex_args,
            "labels": {"omnigent.codex_native.bypass_sandbox": "1"} if tool == "codex" else {},
        },
    )
    address = OMNIGENT_PREFIX + str(session["id"])
    if not wait_for(lambda: api(config, "GET", session_path(address) + "?include_items=false").get("runner_online") is True):
        raise AgentError(f"{address} was created but its runner never came online; stop it before retrying")
    return address


def saw_message(config: Config, address: str, text: str) -> bool:
    """Whether an Omnigent session's recent history holds the last line of `text`."""
    tail = json.dumps(text.strip().splitlines()[-1][-80:], ensure_ascii=False)[1:-1]
    items = api(config, "GET", session_path(address) + "/items?limit=100&order=desc").get("data", [])
    return any(tail in json.dumps(item, ensure_ascii=False) for item in items)
