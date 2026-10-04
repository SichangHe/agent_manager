"""Email between agents and the human.

Agents send from the agent Gmail account to the human's address. The human's replies arrive in the agent account's inbox.
Whether the human has read an agent's email is only visible in the human's own mailbox, which is read through the himalaya config.
"""

from __future__ import annotations

import imaplib
import re
import smtplib
import time
import subprocess
from dataclasses import dataclass
from email import message_from_bytes, policy
from email.message import EmailMessage
from email.utils import make_msgid
from pathlib import Path

from amh.config import Config

ALL_MAIL = '"[Gmail]/All Mail"'
TRASH = '"[Gmail]/Trash"'
HEADERS = "(BODY.PEEK[HEADER.FIELDS (SUBJECT MESSAGE-ID REFERENCES DATE)])"


@dataclass(frozen=True)
class Header:
    uid: str
    subject: str
    message_id: str
    references: str
    date: str


def connect(config: Config, human: bool = False) -> imaplib.IMAP4_SSL:
    """Log in to the agent account, which sends agent mail and receives the human's replies.

    `human` logs in to the human's mailbox instead, where agent mail is unread until the human opens it.
    """
    host, user, password = "imap.gmail.com", config.get("OMO_AGENT_GMAIL_ADDRESS"), config.get("OMO_AGENT_GMAIL_APP_PASSWORD")
    if human:
        path = Path(config.get("OMO_EMAIL_CONFIG_PATH", str(Path.home() / ".config/himalaya/config.toml")))
        host, user, command = (re.search(rf'^{key} = "(.*)"$', path.read_text(encoding="utf-8"), re.MULTILINE)[1] for key in ("host", "login", "cmd"))
        password = subprocess.run(command, shell=True, capture_output=True, text=True, check=True).stdout.strip()
    box = imaplib.IMAP4_SSL(host, timeout=60)
    _ = box.login(user, password)
    return box


def search(box: imaplib.IMAP4_SSL, folder: str, readonly: bool, *criteria: str) -> list[Header]:
    """Return the headers of matching mail in `folder`, oldest first."""
    _ = box.select(folder, readonly=readonly)
    uids = box.uid("search", *criteria)[1][0].split()
    if not uids:
        return []
    found = []
    for part in box.uid("fetch", b",".join(uids), HEADERS)[1]:
        if not isinstance(part, tuple):
            continue
        parsed = message_from_bytes(part[1], policy=policy.default)
        uid = re.search(rb"UID (\d+)", part[0])
        found.append(Header(uid[1].decode() if uid else "", str(parsed["Subject"] or ""), str(parsed["Message-ID"] or "").strip(), " ".join(str(parsed["References"] or "").split()), str(parsed["Date"] or "")))
    return found


def raw(query: str) -> tuple[str, str]:
    """Build a Gmail search criterion."""
    return "X-GM-RAW", '"' + query.replace("\\", "\\\\").replace('"', '\\"') + '"'


def bare(subject: str) -> str:
    """Strip reply prefixes and `[tag]`s from the front of a subject."""
    return re.sub(r"^(\s*(re|fwd?):\s*|\s*\[[^\]]*\]\s*)+", "", subject, flags=re.IGNORECASE).strip()


def tag_of(subject: str) -> str | None:
    """Return the first `[tag]` at the front of a subject, ignoring reply prefixes."""
    match = re.match(r"^(?:\s*(?:re|fwd?):\s*)*\[([A-Za-z0-9_-]+)\]", subject, re.IGNORECASE)
    return match[1] if match else None


def compose(config: Config, to: str, subject: str, body: str, reply_to: str, references: str) -> EmailMessage:
    """Build an email from the agent account, as a reply to Message-ID `reply_to` when that is given."""
    message = EmailMessage()
    message["From"] = config.get("OMO_AGENT_GMAIL_ADDRESS")
    message["To"] = to
    message["Subject"] = subject
    message["Message-ID"] = make_msgid(domain="gmail.com")
    if reply_to:
        message["In-Reply-To"] = reply_to
        message["References"] = f"{references} {reply_to}".strip()
    message.set_content(body)
    return message


def deliver(config: Config, message: EmailMessage) -> str:
    """Send an email through the agent account and return its Message-ID."""
    with smtplib.SMTP_SSL("smtp.gmail.com", 465, timeout=60) as smtp:
        _ = smtp.login(config.get("OMO_AGENT_GMAIL_ADDRESS"), config.get("OMO_AGENT_GMAIL_APP_PASSWORD"))
        _ = smtp.send_message(message)
    return str(message["Message-ID"])


WATCHER = "amh watcher"
SENT_LOG = "amh-sent.txt"


def send(config: Config, tag: str, subject: str, body: str, sender: str) -> str:
    """Email the human on the tag's thread and return the new Message-ID.

    An empty `subject` reuses the subject of the newest email carrying the tag.
    """
    # 🧑 "Let email_me.py prefer the task file name in the subject tag. I find it easier"
    # 🧑 "pending item created/deleted messages should reuse subject of the agent’s previous email"
    title = bare(subject)
    if len(title) >= 60:
        raise SystemExit("amh: the subject must be under 60 characters")
    box = connect(config)
    try:
        thread = [h for h in search(box, ALL_MAIL, True, *raw(f'subject:"[{tag}]"')) if tag_of(h.subject) == tag and (not title or bare(h.subject) == title)]
    finally:
        box.shutdown()
    last = thread[-1] if thread else Header("", "", "", "", "")
    message = compose(config, config.get("OMO_HUMAN_EMAIL_ADDRESS"), f"{'Re: ' if thread else ''}[{tag}] {title or (bare(last.subject) if thread else tag)}", body, last.message_id, last.references)
    message["X-AMH-From"] = sender
    sent = deliver(config, message)
    if sender != WATCHER:
        with (config.state_dir / SENT_LOG).open("a", encoding="utf-8") as record:
            _ = record.write(f"{time.time():.0f} {tag}\n")
    return sent


def unread(config: Config, tag: str, trash_ids: list[str]) -> list[Header]:
    """List the tag's emails the human has not opened; first move those named in `trash_ids` to the trash."""
    # 🧑 “Trash the emails that I no longer need to read that are not read yet.”
    box = connect(config, human=True)
    try:
        found = search(box, "INBOX", not trash_ids, "UNSEEN", "FROM", f'"{config.get("OMO_AGENT_GMAIL_ADDRESS")}"', "SUBJECT", f'"[{tag}]"')
        doomed = [h for h in found if h.message_id in trash_ids]
        if doomed:
            _ = box.uid("move", ",".join(h.uid for h in doomed), TRASH)
        return [h for h in found if h not in doomed]
    finally:
        box.shutdown()


def unseen_from(box: imaplib.IMAP4_SSL, sender: str) -> list[tuple[str, EmailMessage]]:
    """Return each unread inbox email from `sender` with its uid, without marking it read."""
    _ = box.select("INBOX")
    uids = box.uid("search", "UNSEEN", "FROM", f'"{sender}"')[1][0].split()
    return [(uid.decode(), message_from_bytes(box.uid("fetch", uid, "(BODY.PEEK[])")[1][0][1], policy=policy.default)) for uid in uids]


def text_of(parsed: EmailMessage) -> str:
    """Return the readable body of an email: its plain part, else its HTML part."""
    part = parsed.get_body(preferencelist=("plain", "html"))
    try:
        return part.get_content() if part else ""
    except (LookupError, ValueError):
        return (part.get_payload(decode=True) or b"").decode(errors="replace")


def mark_read(box: imaplib.IMAP4_SSL, uid: str) -> None:
    _ = box.uid("store", uid, "+FLAGS", "(\\Seen)")
