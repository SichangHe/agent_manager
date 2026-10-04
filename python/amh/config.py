"""Machine-local settings, read from the environment and then from `local.env`."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path

LOCAL_ENV = Path(os.environ.get("OMO_MANAGER_LOCAL_ENV") or Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config") / "amh/local.env")


@dataclass(frozen=True)
class Config:
    values: dict[str, str]

    def get(self, key: str, default: str = "") -> str:
        return os.environ.get(key) or self.values.get(key, default)

    @property
    def root(self) -> Path:
        """The work-log root holding task files, `TODO.md`, and stored human mail."""
        return Path(self.get("OMO_WORK_LOGS_ROOT", str(Path.home() / "work_logs")))

    @property
    def state_dir(self) -> Path:
        return Path(self.get("OMO_MANAGER_STATE_DIR", str(Path.home() / ".local/state/omo-manager")))

    @property
    def mail_dir(self) -> Path:
        return self.root / "manager_mail"

    @property
    def main_manager(self) -> str:
        """The address of the main manager."""
        return self.get("OMO_MANAGER_TMUX_TARGET")


def load() -> Config:
    """Parse `export KEY="value"` lines, expanding `$VAR` references to earlier keys and the environment."""
    values: dict[str, str] = {}
    for key, raw in re.findall(r'^export (\w+)="(.*)"$', LOCAL_ENV.read_text(encoding="utf-8"), re.MULTILINE):
        expanded = re.sub(r"\$\{(\w+):-([^}]*)\}", lambda m: os.environ.get(m[1]) or values.get(m[1]) or m[2], raw)
        values[key] = re.sub(r"\$(\w+)", lambda m: os.environ.get(m[1]) or values.get(m[1], ""), expanded)
    return Config(values)
