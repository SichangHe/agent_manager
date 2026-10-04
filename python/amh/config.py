"""Settings, loaded by the Rust side from the committed `config.toml` and the machine-local `env.toml`."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import amh_rs


@dataclass(frozen=True)
class Config:
    values: dict[str, str]

    def get(self, key: str, default: str = "") -> str:
        """One setting by its dotted key, e.g. `mail.agent_address`."""
        return self.values.get(key) or default

    @property
    def root(self) -> Path:
        """The work-log root holding task files, `TODO.md`, and stored human mail."""
        return Path(self.get("work_logs", str(Path.home() / "work_logs")))

    @property
    def state_dir(self) -> Path:
        """The folder of `env.toml` and of everything the program records while running."""
        return Path(amh_rs.state_dir())

    @property
    def mail_dir(self) -> Path:
        return self.root / "manager_mail"

    @property
    def main_manager(self) -> str:
        """The address of the main manager."""
        return self.get("main_manager")


def load() -> Config:
    return Config(amh_rs.settings())
