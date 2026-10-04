"""Email between agents and the human; the work is done by the Rust side (`src/mail.rs`).

Agents send from the agent Gmail account to the human's address. The human's replies arrive in the agent account's inbox.
Whether the human has read an agent's email is only visible in the human's own mailbox, which is read through the himalaya config.
"""

from __future__ import annotations

from collections.abc import Sequence
from pathlib import Path
from typing import NamedTuple

import amh_rs

from amh.config import Config


class Header(NamedTuple):
    uid: str
    subject: str
    message_id: str
    references: str
    date: str
    sender: str = ""

    @property
    def is_notice(self) -> bool:
        """Whether this is an automatic "pending item created/deleted" email."""
        return self.sender.startswith(amh_rs.MAIL_NOTICE_SENDER)


def send(_config: Config, tag: str, subject: str, body: str, sender: str, attachments: Sequence[Path] = ()) -> str:
    """Email the human on the tag's thread and return the new Message-ID.

    An empty `subject` reuses the subject of the newest email carrying the tag. Files are attached by path, such as a figure the human asked to see.
    """
    return amh_rs.mail_send(tag, subject, body, sender, list(attachments))


def unread(_config: Config, tag: str, trash_ids: list[str]) -> list[Header]:
    """List the tag's emails the human has not opened; first move those named in `trash_ids` to the trash."""
    return [Header(*row) for row in amh_rs.mail_unread(tag, trash_ids)]
