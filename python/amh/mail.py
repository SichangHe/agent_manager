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

WATCHER: str = amh_rs.MAIL_WATCHER
SENT_LOG: str = amh_rs.MAIL_SENT_LOG


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


class Mail(NamedTuple):
    """A received email; every header is on one line."""

    sender: str
    authentication: str
    message_id: str
    in_reply_to: str
    references: str
    subject: str
    body: str
    images: list[tuple[str, str, bytes]]


def connect(_config: Config) -> amh_rs.Mailbox:
    """Log in to the agent account, which sends agent mail and receives the human's replies; `OSError` when it fails."""
    return amh_rs.mail_connect()


def bare(subject: str) -> str:
    """Strip reply prefixes and `[tag]`s from the front of a subject."""
    return amh_rs.mail_bare(subject)


def tag_of(subject: str) -> str | None:
    """Return the first `[tag]` at the front of a subject, ignoring reply prefixes."""
    return amh_rs.mail_tag_of(subject)


def compose(to: str, subject: str, body: str, reply_to: str, references: str, attachments: Sequence[tuple[str, str, bytes]] = ()) -> tuple[str, bytes]:
    """Build an email from the agent account, as a reply to Message-ID `reply_to` when that is given; return its Message-ID and its bytes.

    Each attachment is a file name, a MIME type, and the content.
    """
    return amh_rs.mail_compose(to, subject, body, reply_to, references, "", list(attachments))


def deliver(to: str, raw: bytes) -> None:
    """Send a built email through the agent account; `OSError` when it fails."""
    amh_rs.mail_deliver(to, raw)


def send(_config: Config, tag: str, subject: str, body: str, sender: str, attachments: Sequence[Path] = ()) -> str:
    """Email the human on the tag's thread and return the new Message-ID.

    An empty `subject` reuses the subject of the newest email carrying the tag. Files are attached by path, such as a figure the human asked to see.
    """
    return amh_rs.mail_send(tag, subject, body, sender, list(attachments))


def unread(_config: Config, tag: str, trash_ids: list[str]) -> list[Header]:
    """List the tag's emails the human has not opened; first move those named in `trash_ids` to the trash."""
    return [Header(*row) for row in amh_rs.mail_unread(tag, trash_ids)]


def parse(raw: bytes) -> Mail:
    """Read a raw email: the headers the callers need, the readable body (its plain part, else its HTML part), and its images."""
    return Mail(*amh_rs.mail_parse(raw))


def unseen_from(box: amh_rs.Mailbox, sender: str) -> list[tuple[str, Mail]]:
    """Return each unread inbox email from `sender` with its uid, without marking it read."""
    return [(uid, parse(raw)) for uid, raw in box.unseen_from(sender)]


def mark_read(box: amh_rs.Mailbox, uid: str) -> None:
    box.mark_read(uid)
