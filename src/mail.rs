//! Email between agents and the human.
//!
//! Agents send from the agent Gmail account to the human's address. The human's replies arrive in the agent account's inbox.
//! Whether the human has read an agent's email is only visible in the human's own mailbox, which is read through the himalaya config.
//!
//! Subject and thread decisions, parsing, and message building are pure functions; `Mailbox`, `deliver`, `send`, and `unread` do the network input and output around them.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lettre::message::header::{ContentType, HeaderName as SentName, HeaderValue as SentValue};
use lettre::message::{Attachment, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{SmtpTransport, Transport};
use mail_parser::{HeaderName, HeaderValue, Message, MessageParser, MimeHeaders, PartType};

use crate::decide::quoted;
use crate::settings;

/// The sender name of the watcher's own emails, which are not recorded as an agent's answer.
pub const WATCHER: &str = "amh watcher";
/// The record in the state folder of every email an agent sent: lines `EPOCH TAG`.
pub const SENT_LOG: &str = "amh-sent.txt";
const ALL_MAIL: &str = "[Gmail]/All Mail";
const TRASH: &str = "[Gmail]/Trash";
/// The start of `X-AMH-From` on the automatic "pending item created/deleted" emails.
pub const NOTICE_SENDER: &str = "amh notice";
const HEADERS: &str =
    "(UID BODY.PEEK[HEADER.FIELDS (SUBJECT MESSAGE-ID REFERENCES DATE X-AMH-FROM)])";
const ATTACHED_BYTES_MAX: u64 = 20 << 20;
const NEW_TITLE_CHARS_MAX: usize = 60;
const TIMEOUT: Duration = Duration::from_secs(60);

/// What a search returns of each email.
#[derive(Debug, PartialEq)]
pub struct Header {
    pub uid: String,
    /// Decoded, as the reader sees it.
    pub subject: String,
    /// With its angle brackets.
    pub message_id: String,
    /// On one line.
    pub references: String,
    /// As written.
    pub date: String,
    /// The `X-AMH-From` header: who sent it through amh; starts with `NOTICE_SENDER` for an item notice.
    pub sender: String,
}

/// A file carried by an email: its name, MIME type, and content.
pub struct Attached {
    pub name: String,
    pub kind: String,
    pub data: Vec<u8>,
}

/// What the callers read of a received email; every header is on one line.
#[derive(Default)]
pub struct Parsed {
    /// The address in `From`, without the display name.
    pub sender: String,
    /// The topmost `Authentication-Results`, which is the one the receiving server added.
    pub authentication: String,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub subject: String,
    /// The plain part, else the HTML part as written.
    pub body: String,
    /// Every `image/*` part in order.
    pub images: Vec<Attached>,
}

/// The subject and thread of an email about to be sent.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub subject: String,
    /// The Message-ID answered; empty for a new thread.
    pub reply_to: String,
    /// The `References` of the email answered.
    pub references: String,
}

/// An email to build; an empty `reply_to` starts a thread, an empty `sender` adds no `X-AMH-From`.
pub struct Draft<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
    pub reply_to: &'a str,
    pub references: &'a str,
    pub sender: &'a str,
    pub attachments: &'a [Attached],
}

/// Join text on one line with single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drop one reply prefix (`Re:`, `Fw:`, `Fwd:`, any case) from the front.
fn after_prefix(subject: &str) -> Option<&str> {
    let colon = subject.find(':')?;
    ["re", "fw", "fwd"]
        .iter()
        .any(|prefix| subject[..colon].eq_ignore_ascii_case(prefix))
        .then(|| &subject[colon + 1..])
}

/// Strip reply prefixes and `[tag]`s from the front of a subject.
pub fn bare(subject: &str) -> &str {
    let mut rest = subject.trim_start();
    loop {
        let bracketed = rest
            .strip_prefix('[')
            .and_then(|inner| Some(&inner[inner.find(']')? + 1..]));
        match after_prefix(rest).or(bracketed) {
            Some(shorter) => rest = shorter.trim_start(),
            None => return rest.trim_end(),
        }
    }
}

/// The first `[tag]` at the front of a subject, ignoring reply prefixes.
pub fn tag_of(subject: &str) -> Option<&str> {
    let mut rest = subject;
    while let Some(shorter) = after_prefix(rest.trim_start()) {
        rest = shorter.trim_start();
    }
    let inner = rest.strip_prefix('[')?;
    let tag = &inner[..inner.find(']')?];
    let allowed = |letter: char| letter.is_ascii_alphanumeric() || letter == '_' || letter == '-';
    (!tag.is_empty() && tag.chars().all(allowed)).then_some(tag)
}

/// The Gmail search for every email whose subject carries `[tag]`.
fn thread_query(tag: &str) -> String {
    format!("X-GM-RAW \"subject:\\\"[{tag}]\\\"\"")
}

/// Decide the subject and thread of an email on a tag, from the emails found carrying the tag, oldest first.
///
/// It answers the newest one with the same tag and, when `subject` is given, the same bare title; an empty `subject` reuses that one's title, else the tag.
// 🧑 "Let email_me.py prefer the task file name in the subject tag. I find it easier"
// 🧑 "pending item created/deleted messages should reuse subject of the agent’s previous email"
pub fn plan(tag: &str, subject: &str, found: &[Header]) -> Result<Plan, String> {
    let title = bare(subject);
    let last = found.iter().rfind(|header| {
        tag_of(&header.subject) == Some(tag) && (title.is_empty() || bare(&header.subject) == title)
    });
    match last {
        Some(last) => {
            let title = if title.is_empty() {
                bare(&last.subject)
            } else {
                title
            };
            Ok(Plan {
                subject: format!("Re: [{tag}] {title}"),
                reply_to: last.message_id.clone(),
                references: last.references.clone(),
            })
        }
        // The length limit is for new subjects only: a reply must be able to reuse the subject of the thread it answers, whatever its length.
        None if title.chars().count() >= NEW_TITLE_CHARS_MAX => Err(format!(
            "a new subject must be under {NEW_TITLE_CHARS_MAX} characters"
        )),
        None => Ok(Plan {
            subject: format!("[{tag}] {}", if title.is_empty() { tag } else { title }),
            reply_to: String::new(),
            references: String::new(),
        }),
    }
}

impl Header {
    /// Whether an agent wrote this email and sent it through amh, so that agent can replace it.
    ///
    /// Not its own: the automatic "pending item created/deleted" notices and the watcher's emails (senders starting `amh `), and emails sent outside amh, such as the summaries of a mail-compression job, which carry no sender header.
    pub fn is_own(&self) -> bool {
        !self.sender.is_empty() && !self.sender.starts_with("amh ")
    }

    /// The email as the unread listings show it.
    pub fn listed(&self) -> String {
        format!(
            "{} {} {}",
            self.message_id,
            quoted(&self.subject),
            self.date
        )
    }
}

/// Every unread email from the agent account in the human's inbox, oldest first.
pub fn unread_all(settings: &BTreeMap<String, String>) -> Result<Vec<Header>, String> {
    let agent = settings::get(settings, "mail.agent_address");
    Mailbox::human(settings)?.search("INBOX", true, &format!("UNSEEN FROM \"{agent}\""))
}

/// What `tell human` prints after sending `message_id`, given the task's emails the human has not opened: nothing, or the request to replace the earlier ones with one email.
// 🧑 "sending email on a thread with unread emails from the same agent prompts the agent to replace unread emails with one single after the send"
// 🧑 "this process should ignore emails about pending tasks being created and removed"
// Emails the agent did not write, such as a mail-compression job's summaries, are left out too: it cannot merge text it never saw.
pub fn replace_reminder(message_id: &str, unread: &[Header]) -> String {
    let others: Vec<&Header> = unread
        .iter()
        .filter(|header| header.message_id != message_id && header.is_own())
        .collect();
    if others.is_empty() {
        return String::new();
    }
    let listed: String = others
        .iter()
        .map(|header| format!("  {}\n", header.listed()))
        .collect();
    let flags: Vec<String> = others
        .iter()
        .map(|header| format!("--replaces '{}'", header.message_id))
        .collect();
    format!(
        "The human has {} earlier unread email(s) of yours besides the one just sent. The human wants at most 1 unread email from you:\n{listed}Do this now: write ONE email that keeps everything the human still needs from those emails and from the one just sent, and send it with\n  amh tell human --subject SAME_SUBJECT --file MERGED_FILE {} --replaces '{message_id}'\nThe replaced unread emails go to the trash. Skip this only if those earlier emails must stay separate.\n",
        others.len(),
        flags.join(" ")
    )
}

/// What `tell human` prints when the email is sent but the human's unread emails could not be listed.
pub fn unread_check_failed(error: &str) -> String {
    format!(
        "The email is sent; do not resend it. The check for your earlier unread emails failed: OSError({})\n",
        quoted(error)
    )
}

/// The first header of that name as written, on one line; the parser's own lookup returns the last one.
fn written(message: &Message, name: &str) -> String {
    message
        .headers_raw()
        .find(|(found, _)| found.eq_ignore_ascii_case(name))
        .map(|(_, value)| one_line(value))
        .unwrap_or_default()
}

/// The first header of that name, decoded.
fn decoded<'a>(message: &'a Message, name: HeaderName) -> Option<&'a HeaderValue<'a>> {
    let found = message.headers().iter().find(|header| header.name == name);
    found.map(|header| &header.value)
}

fn subject_of(message: &Message) -> String {
    let subject = decoded(message, HeaderName::Subject).and_then(HeaderValue::as_text);
    subject.unwrap_or_default().to_owned()
}

/// Read the header fields a search fetched.
pub fn header_of(uid: String, raw: &[u8]) -> Header {
    let Some(message) = MessageParser::default().parse_headers(raw) else {
        return Header {
            uid,
            subject: String::new(),
            message_id: String::new(),
            references: String::new(),
            date: String::new(),
            sender: String::new(),
        };
    };
    Header {
        uid,
        subject: subject_of(&message),
        message_id: written(&message, "Message-ID"),
        references: written(&message, "References"),
        date: written(&message, "Date"),
        sender: written(&message, "X-AMH-From"),
    }
}

/// Read a raw email into what the callers need; bytes that are no email give an empty record.
pub fn parse(raw: &[u8]) -> Parsed {
    let Some(message) = MessageParser::default().parse(raw) else {
        return Parsed::default();
    };
    let sender = decoded(&message, HeaderName::From)
        .and_then(HeaderValue::as_address)
        .and_then(|address| address.first()?.address());
    let texts = || {
        let ids = message.text_body.iter().chain(&message.html_body);
        ids.filter_map(|id| message.parts.get(*id as usize))
    };
    let body = texts()
        .find(|part| matches!(part.body, PartType::Text(_)))
        .or_else(|| texts().next())
        .and_then(|part| part.text_contents());
    let images = message.parts.iter().filter_map(|part| {
        let kind = part.content_type()?;
        kind.ctype()
            .eq_ignore_ascii_case("image")
            .then(|| Attached {
                name: part.attachment_name().unwrap_or_default().to_owned(),
                kind: format!("image/{}", kind.subtype().unwrap_or_default()).to_ascii_lowercase(),
                data: part.contents().to_vec(),
            })
    });
    Parsed {
        sender: sender.unwrap_or_default().to_owned(),
        authentication: written(&message, "Authentication-Results"),
        message_id: written(&message, "Message-ID"),
        in_reply_to: written(&message, "In-Reply-To"),
        references: written(&message, "References"),
        subject: one_line(&subject_of(&message)),
        body: body.unwrap_or_default().to_owned(),
        images: images.collect(),
    }
}

/// The header naming which agent sent an email from the shared agent account.
#[derive(Clone)]
struct AmhFrom(String);

impl lettre::message::header::Header for AmhFrom {
    fn name() -> SentName {
        SentName::new_from_ascii_str("X-AMH-From")
    }

    fn parse(text: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(text.to_owned()))
    }

    fn display(&self) -> SentValue {
        SentValue::new(Self::name(), self.0.clone())
    }
}

/// Build an email as the bytes to send, with the given Message-ID.
pub fn compose(draft: &Draft, message_id: &str) -> Result<Vec<u8>, String> {
    let mailbox = |address: &str| {
        let parsed = address.parse::<lettre::Address>();
        parsed
            .map(|address| lettre::message::Mailbox::new(None, address))
            .map_err(|error| format!("bad email address {address:?}: {error}"))
    };
    let mut message = lettre::Message::builder()
        .from(mailbox(draft.from)?)
        .to(mailbox(draft.to)?)
        .subject(draft.subject)
        .message_id(Some(message_id.to_owned()));
    if !draft.reply_to.is_empty() {
        let references = format!("{} {}", draft.references, draft.reply_to);
        message = message
            .in_reply_to(draft.reply_to.to_owned())
            .references(references.trim().to_owned());
    }
    if !draft.sender.is_empty() {
        message = message.header(AmhFrom(draft.sender.to_owned()));
    }
    // The body ends with exactly one line break: a lone part gets one added when formatted, a part among others does not.
    let text = draft.body.strip_suffix('\n').unwrap_or(draft.body);
    let built = if draft.attachments.is_empty() {
        message.singlepart(SinglePart::plain(text.to_owned()))
    } else {
        let mut parts = MultiPart::mixed().singlepart(SinglePart::plain(format!("{text}\n")));
        for file in draft.attachments {
            let kind = ContentType::parse(&file.kind)
                .map_err(|error| format!("bad file type {:?}: {error}", file.kind))?;
            parts =
                parts.singlepart(Attachment::new(file.name.clone()).body(file.data.clone(), kind));
        }
        message.multipart(parts)
    };
    built
        .map(|message| message.formatted())
        .map_err(|error| format!("cannot build the email: {error}"))
}

/// A fresh Message-ID in the agent account's domain, which Gmail keeps as given.
pub fn new_id() -> String {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let ns = since.unwrap_or_default().as_nanos();
    format!("<{ns}.{}@gmail.com>", std::process::id())
}

/// The folder in the state folder holding emails to the human that could not be sent yet.
pub const OUTBOX: &str = "outbox";

/// Where the email with this Message-ID waits when it could not be sent.
fn queued_path(state_dir: &Path, message_id: &str) -> PathBuf {
    let name: String = message_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect();
    state_dir.join(OUTBOX).join(format!("{name}.eml"))
}

/// Whether the email with this Message-ID is waiting to be sent.
pub fn is_queued(state_dir: &Path, message_id: &str) -> bool {
    queued_path(state_dir, message_id).is_file()
}

/// Keep superseded queued mail until its replacement is sent, including a later retry.
// 🧑 "a replacement ... was sent with --replaces. ... check that the blocked message is no longer in the retry queue."
pub fn replace_queued(
    state_dir: &Path,
    message_id: &str,
    replaces: &[String],
) -> Result<(), String> {
    let path = queued_path(state_dir, message_id);
    if path.is_file() {
        if !replaces.is_empty() {
            let raw = serde_json::to_vec(replaces).map_err(|error| error.to_string())?;
            fs::write(path.with_extension("json"), raw).map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    for replaced in replaces {
        if queued_path(state_dir, replaced) == path {
            continue;
        }
        let old = queued_path(state_dir, replaced);
        for path in [old.clone(), old.with_extension("json")] {
            match fs::remove_file(&path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(format!("removing replaced queued mail failed: {error}")),
            }
        }
    }
    Ok(())
}

fn bare_id(message_id: &str) -> &str {
    message_id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
}

/// Whether a send failure says the account's sending limit is reached, so trying more emails now is pointless. Pure.
pub fn is_limit(error: &str) -> bool {
    error.contains("5.4.5") || error.to_lowercase().contains("sending limit")
}

/// Send the queued emails, oldest first; returns one log line per email sent or refused. Stops at the sending limit.
pub fn flush_outbox(settings: &BTreeMap<String, String>, state_dir: &Path) -> Vec<String> {
    let mut lines = retry_replacements(state_dir);
    let entries = fs::read_dir(state_dir.join(OUTBOX)).into_iter().flatten();
    let mut waiting: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.extension().is_some_and(|kind| kind == "eml"))
        .collect();
    waiting.sort();
    let to = settings::get(settings, "mail.human_address");
    for (at, path) in waiting.iter().enumerate() {
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let sent = fs::read(path)
            .map_err(|error| error.to_string())
            .and_then(|raw| deliver(settings, to, &raw));
        match sent {
            Ok(()) => {
                if let Err(error) = fs::remove_file(path) {
                    lines.push(format!(
                        "sent queued email {name}, but removing it failed: {error}"
                    ));
                    continue;
                }
                let metadata = path.with_extension("json");
                if metadata.is_file() {
                    let replaced = fs::read(&metadata)
                        .map_err(|error| error.to_string())
                        .and_then(|raw| {
                            serde_json::from_slice::<Vec<String>>(&raw)
                                .map_err(|error| error.to_string())
                        })
                        .and_then(|ids| {
                            replace_queued(
                                state_dir,
                                &path.file_stem().unwrap_or_default().to_string_lossy(),
                                &ids,
                            )
                        });
                    match replaced {
                        Ok(()) => {
                            let _ = fs::remove_file(metadata);
                        }
                        Err(error) => lines.push(format!(
                            "sent queued email {name}, but replacing queued mail failed: {error}"
                        )),
                    }
                }
                lines.push(format!("sent queued email {name}"));
            }
            Err(error) if is_limit(&error) => {
                lines.push(format!(
                    "{} queued email(s) still wait: {error}",
                    waiting.len() - at
                ));
                break;
            }
            Err(error) => lines.push(format!("queued email {name} was refused: {error}")),
        }
    }
    lines
}

/// Retry supersession cleanup after successful sends without resending those emails.
fn retry_replacements(state_dir: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in fs::read_dir(state_dir.join(OUTBOX)).into_iter().flatten() {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().is_none_or(|kind| kind != "json") || path.with_extension("eml").exists()
        {
            continue;
        }
        let result = fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|raw| {
                serde_json::from_slice::<Vec<String>>(&raw).map_err(|error| error.to_string())
            })
            .and_then(|ids| {
                replace_queued(
                    state_dir,
                    &path.file_stem().unwrap_or_default().to_string_lossy(),
                    &ids,
                )
            })
            .and_then(|()| fs::remove_file(&path).map_err(|error| error.to_string()));
        if let Err(error) = result {
            lines.push(format!("replacing queued mail cleanup failed: {error}"));
        }
    }
    lines
}

/// Send built email bytes to one address through the agent account.
pub fn deliver(settings: &BTreeMap<String, String>, to: &str, raw: &[u8]) -> Result<(), String> {
    let from = settings::get(settings, "mail.agent_address");
    let failed = |error: &dyn std::fmt::Display| format!("the email was not sent: {error}");
    let address = |text: &str| {
        text.parse::<lettre::Address>()
            .map_err(|error| failed(&error))
    };
    let envelope = lettre::address::Envelope::new(Some(address(from)?), vec![address(to)?])
        .map_err(|error| failed(&error))?;
    let password = settings::get(settings, "mail.agent_password").to_owned();
    let smtp = SmtpTransport::relay("smtp.gmail.com")
        .map_err(|error| failed(&error))?
        .credentials(Credentials::new(from.to_owned(), password))
        .timeout(Some(TIMEOUT))
        .build();
    smtp.send_raw(&envelope, raw)
        .map(|_| ())
        .map_err(|error| failed(&error))
}

/// One logged-in IMAP connection.
pub struct Mailbox {
    session: imap::Session<native_tls::TlsStream<TcpStream>>,
}

impl Mailbox {
    /// Log in to the agent account, which sends agent mail and receives the human's replies.
    pub fn agent(settings: &BTreeMap<String, String>) -> Result<Self, String> {
        Self::login(
            "imap.gmail.com",
            settings::get(settings, "mail.agent_address"),
            settings::get(settings, "mail.agent_password"),
        )
    }

    /// Log in to the human's mailbox, where agent mail is unread until the human opens it.
    ///
    /// The himalaya config names the host, the login, and the shell command that prints the password.
    pub fn human(settings: &BTreeMap<String, String>) -> Result<Self, String> {
        let path = match settings::get(settings, "mail.human_mailbox_config") {
            "" => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".config/himalaya/config.toml"),
            path => PathBuf::from(path),
        };
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let value = |key: &str| {
            let start = format!("{key} = \"");
            text.lines()
                .find_map(|line| line.strip_prefix(&start)?.strip_suffix('"'))
                .ok_or_else(|| format!("{} has no line `{key} = \"...\"`", path.display()))
        };
        let (host, user, command) = (value("host")?, value("login")?, value("cmd")?);
        let output = Command::new("sh").args(["-c", command]).output();
        match output {
            Ok(output) if output.status.success() => {
                Self::login(host, user, String::from_utf8_lossy(&output.stdout).trim())
            }
            Ok(output) => Err(format!(
                "the password command of {} failed: {}",
                path.display(),
                output.status
            )),
            Err(error) => Err(format!(
                "the password command of {} did not run: {error}",
                path.display()
            )),
        }
    }

    fn login(host: &str, user: &str, password: &str) -> Result<Self, String> {
        let failed = |error: &dyn std::fmt::Display| format!("cannot log in to {host}: {error}");
        let mut addresses = (host, 993)
            .to_socket_addrs()
            .map_err(|error| failed(&error))?;
        let stream = addresses
            .find_map(|address| TcpStream::connect_timeout(&address, TIMEOUT).ok())
            .ok_or_else(|| failed(&"no connection"))?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
            .map_err(|error| failed(&error))?;
        let tls = native_tls::TlsConnector::new().map_err(|error| failed(&error))?;
        let mut client =
            imap::Client::new(tls.connect(host, stream).map_err(|error| failed(&error))?);
        client.read_greeting().map_err(|error| failed(&error))?;
        let session = client.login(user, password);
        session
            .map(|session| Self { session })
            .map_err(|(error, _)| failed(&error))
    }

    /// The headers of matching mail in `folder`, oldest first.
    pub fn search(
        &mut self,
        folder: &str,
        readonly: bool,
        query: &str,
    ) -> Result<Vec<Header>, String> {
        let failed = |error: imap::Error| format!("mail search in {folder} failed: {error}");
        let opened = if readonly {
            self.session.examine(folder)
        } else {
            self.session.select(folder)
        };
        opened.map_err(failed)?;
        let uids = self.session.uid_search(query).map_err(failed)?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let fetched = self.session.uid_fetch(uid_set(uids), HEADERS);
        let fetched = fetched.map_err(failed)?;
        let mut found: Vec<_> = fetched
            .iter()
            .filter_map(|mail| Some((mail.uid?, mail.header()?)))
            .collect();
        found.sort_unstable_by_key(|(uid, _)| *uid);
        let headers = found
            .iter()
            .map(|(uid, raw)| header_of(uid.to_string(), raw));
        Ok(headers.collect())
    }

    /// Each unread inbox email from `sender` as its uid and raw bytes, oldest first, without marking it read.
    pub fn unseen_from(&mut self, sender: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
        if sender.contains(['"', '\\', '\r', '\n']) {
            return Err("invalid sender in unread mail search".to_owned());
        }
        let failed = |error: imap::Error| format!("reading unread mail failed: {error}");
        self.session.select("INBOX").map_err(failed)?;
        let query = format!("UNSEEN FROM \"{sender}\"");
        let uids = self.session.uid_search(query).map_err(failed)?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let fetched = self.session.uid_fetch(uid_set(uids), "(UID BODY.PEEK[])");
        let fetched = fetched.map_err(failed)?;
        let mut found: Vec<_> = fetched
            .iter()
            .filter_map(|mail| Some((mail.uid?, mail.body()?.to_vec())))
            .collect();
        found.sort_unstable_by_key(|(uid, _)| *uid);
        let named = found.into_iter().map(|(uid, raw)| (uid.to_string(), raw));
        Ok(named.collect())
    }

    /// Mark one email of the selected folder as read.
    pub fn mark_read(&mut self, uid: &str) -> Result<(), String> {
        let stored = self.session.uid_store(uid, "+FLAGS (\\Seen)");
        stored
            .map(|_| ())
            .map_err(|error| format!("marking mail {uid} read failed: {error}"))
    }

    /// Move an accepted notification out of the inbox after its durable handoff.
    pub fn trash(&mut self, uid: &str) -> Result<(), String> {
        self.session
            .uid_mv(uid, TRASH)
            .map_err(|error| format!("moving routed mail {uid} to trash failed: {error}"))
    }

    /// UID generation distinguishes a reused inbox UID from the previously routed email.
    pub fn uid_validity(&mut self) -> Result<u32, String> {
        self.session
            .select("INBOX")
            .map_err(|error| format!("reading mailbox UID generation failed: {error}"))?
            .uid_validity
            .ok_or_else(|| "mailbox has no UIDVALIDITY".to_owned())
    }
}

/// The uids in ascending order as an IMAP set.
fn uid_set(uids: impl IntoIterator<Item = u32>) -> String {
    let mut uids: Vec<u32> = uids.into_iter().collect();
    uids.sort_unstable();
    let names: Vec<String> = uids.iter().map(u32::to_string).collect();
    names.join(",")
}

/// Read the files to attach, such as a figure the human asked to see; the type is guessed from the file name.
fn attached(paths: &[PathBuf]) -> Result<Vec<Attached>, String> {
    let cannot =
        |path: &Path, error: std::io::Error| format!("cannot read {}: {error}", path.display());
    let mut n_bytes = 0;
    for path in paths {
        n_bytes += fs::metadata(path)
            .map_err(|error| cannot(path, error))?
            .len();
    }
    if n_bytes > ATTACHED_BYTES_MAX {
        return Err(format!(
            "the attachments exceed {} MB in total; send a path instead",
            ATTACHED_BYTES_MAX >> 20
        ));
    }
    let read = |path: &PathBuf| {
        Ok(Attached {
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            kind: mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string(),
            data: fs::read(path).map_err(|error| cannot(path, error))?,
        })
    };
    paths.iter().map(read).collect()
}

/// Email the human on the tag's thread and return the new Message-ID.
///
/// An empty `subject` reuses the subject of the newest email carrying the tag. Unless the watcher sends, the email is recorded in `SENT_LOG` under `state_dir`.
pub fn send(
    settings: &BTreeMap<String, String>,
    state_dir: &Path,
    tag: &str,
    subject: &str,
    body: &str,
    sender: &str,
    attachments: &[PathBuf],
) -> Result<String, String> {
    let found = Mailbox::agent(settings)
        .and_then(|mut mailbox| mailbox.search(ALL_MAIL, true, &thread_query(tag)))?;
    let plan = plan(tag, subject, &found)?;
    let to = settings::get(settings, "mail.human_address");
    let draft = Draft {
        from: settings::get(settings, "mail.agent_address"),
        to,
        subject: &plan.subject,
        body,
        reply_to: &plan.reply_to,
        references: &plan.references,
        sender,
        attachments: &attached(attachments)?,
    };
    let message_id = new_id();
    let raw = compose(&draft, &message_id)?;
    if let Err(error) = deliver(settings, to, &raw) {
        // A refused email is kept and sent again by the watcher, so nothing the human should read is lost.
        let path = queued_path(state_dir, &message_id);
        fs::create_dir_all(state_dir.join(OUTBOX))
            .and_then(|()| fs::write(&path, &raw))
            .map_err(|failure| format!("{error}; and it could not be queued: {failure}"))?;
        eprintln!("amh: {error}; the email is queued and will be sent when Gmail allows");
    }
    if sender != WATCHER {
        let epoch_s = SystemTime::now().duration_since(UNIX_EPOCH);
        let line = format!("{} {tag}\n", epoch_s.unwrap_or_default().as_secs());
        let mut log = fs::OpenOptions::new();
        log.create(true)
            .append(true)
            .open(state_dir.join(SENT_LOG))
            .and_then(|mut log| log.write_all(line.as_bytes()))
            .map_err(|error| {
                format!(
                    "the email {message_id} is sent, but recording it in {SENT_LOG} failed: {error}"
                )
            })?;
    }
    Ok(message_id)
}

/// List the tag's emails the human has not opened; first move those named in `trash_ids` to the trash.
// 🧑 “Trash the emails that I no longer need to read that are not read yet.”
pub fn unread(
    settings: &BTreeMap<String, String>,
    tag: &str,
    trash_ids: &[String],
) -> Result<Vec<Header>, String> {
    let mut mailbox = Mailbox::human(settings)?;
    let agent = settings::get(settings, "mail.agent_address");
    let query = format!("UNSEEN FROM \"{agent}\" SUBJECT \"[{tag}]\"");
    let found = mailbox.search("INBOX", trash_ids.is_empty(), &query)?;
    let (doomed, kept): (Vec<Header>, Vec<Header>) = found.into_iter().partition(|header| {
        trash_ids
            .iter()
            .any(|id| bare_id(id) == bare_id(&header.message_id))
    });
    if !doomed.is_empty() {
        let uids: Vec<&str> = doomed.iter().map(|header| header.uid.as_str()).collect();
        let moved = mailbox.session.uid_mv(uids.join(","), TRASH);
        moved.map_err(|error| format!("moving replaced mail to the trash failed: {error}"))?;
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUMAN: &str = "human@example.com";
    const AGENT: &str = "agent@example.com";

    fn header(uid: &str, subject: &str, message_id: &str, references: &str) -> Header {
        Header {
            uid: uid.to_owned(),
            subject: subject.to_owned(),
            message_id: message_id.to_owned(),
            references: references.to_owned(),
            date: "Mon".to_owned(),
            sender: "omnigent://s1".to_owned(),
        }
    }

    fn planned(subject: &str, reply_to: &str, references: &str) -> Result<Plan, String> {
        Ok(Plan {
            subject: subject.to_owned(),
            reply_to: reply_to.to_owned(),
            references: references.to_owned(),
        })
    }

    #[test]
    fn an_email_joins_the_thread_of_its_tag_and_title() {
        let mut found = vec![
            header("5", "[t_work] Progress", "<a@x>", ""),
            header("6", "Re: [t_work] Progress", "<b@x>", "<a@x>"),
            header("7", "[t_worker] Other", "<c@x>", ""),
        ];
        assert_eq!(
            plan("t_work", "", &found),
            planned("Re: [t_work] Progress", "<b@x>", "<a@x>")
        );
        assert_eq!(
            plan("t_work", "Re: [t_work] New topic", &found),
            planned("[t_work] New topic", "", "")
        );
        assert_eq!(plan("fresh", "", &found), planned("[fresh] fresh", "", ""));
        assert_eq!(
            plan("t_work", &"s".repeat(60), &found),
            Err("a new subject must be under 60 characters".to_owned())
        );
        let long =
            "Re: [t_work] a subject the human's thread already has, longer than sixty characters";
        found.push(header("8", long, "<d@x>", ""));
        assert_eq!(plan("t_work", long, &found), planned(long, "<d@x>", ""));
        assert_eq!(tag_of("Re: Fwd: [a_b-1] x [y]"), Some("a_b-1"));
        assert_eq!((tag_of("x [a]"), tag_of(" [a]")), (None, None));
        assert_eq!(bare("Re: [a] RE: [b] Title [c] "), "Title [c]");
    }

    #[test]
    fn a_built_email_parses_back() {
        let png = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0; 40]].concat();
        let figure = [Attached {
            name: "figure.png".to_owned(),
            kind: "image/png".to_owned(),
            data: png.clone(),
        }];
        let mut draft = Draft {
            from: AGENT,
            to: HUMAN,
            subject: "Re: [t_work] Progrès",
            body: "see the figure",
            reply_to: "<b@x>",
            references: "<a@x>",
            sender: "omnigent://w",
            attachments: &figure,
        };
        let raw = compose(&draft, "<new@x>").unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.contains("To: human@example.com\r\n")
                && text.contains("X-AMH-From: omnigent://w\r\n")
        );
        let mail = parse(&raw);
        assert_eq!(
            [
                &mail.sender,
                &mail.authentication,
                &mail.message_id,
                &mail.in_reply_to,
                &mail.references,
                &mail.subject,
                &mail.body
            ],
            [
                AGENT,
                "",
                "<new@x>",
                "<b@x>",
                "<a@x> <b@x>",
                "Re: [t_work] Progrès",
                "see the figure\r\n"
            ]
        );
        let images: Vec<_> = mail
            .images
            .iter()
            .map(|image| (image.name.as_str(), image.kind.as_str(), &image.data))
            .collect();
        assert_eq!(images, [("figure.png", "image/png", &png)]);
        (
            draft.subject,
            draft.body,
            draft.reply_to,
            draft.sender,
            draft.attachments,
        ) = ("[t_work] New", "body text\n", "", "", &[]);
        let raw = compose(&draft, "<new@x>").unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            !text.contains("In-Reply-To")
                && !text.contains("References")
                && !text.contains("X-AMH-From")
        );
        assert_eq!(parse(&raw).body, "body text\r\n");
    }

    #[test]
    fn a_received_email_is_decoded_and_its_first_header_counts() {
        let odd = "From: =?utf-8?b?5a6i?= <G@Example.org>\r\nAuthentication-Results: mx; spf=pass\r\n smtp.mailfrom=g@example.org\r\nAuthentication-Results: forged\r\nSubject: =?utf-8?q?caf=C3=A9?=\r\n  twice\r\nContent-Type: text/html; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<p>=E9</p>\r\n";
        let mail = parse(odd.as_bytes());
        assert_eq!(
            [
                &mail.sender,
                &mail.authentication,
                &mail.message_id,
                &mail.subject,
                &mail.body
            ],
            [
                "G@Example.org",
                "mx; spf=pass smtp.mailfrom=g@example.org",
                "",
                "café twice",
                "<p>é</p>\r\n"
            ]
        );
        assert!(mail.images.is_empty());
    }

    #[test]
    fn earlier_unread_emails_are_listed_for_replacement() {
        let mut notice = header("6", "[t_work] Earlier", "<notice@x>", "");
        notice.sender = "amh notice omnigent://s1".to_owned();
        let mut outside = header("8", "[t_work] Summary", "<summary@mail.x>", "");
        outside.sender = String::new();
        assert!(!notice.is_own() && !outside.is_own());
        let unread = [
            header("4", "[t_work] Earlier", "<old@x>", ""),
            header("5", "[t_work] it's \"x\"", "<older@x>", ""),
            notice,
            outside,
            header("7", "[t_work] Progress", "<sent@x>", ""),
        ];
        assert_eq!(unread[0].listed(), "<old@x> '[t_work] Earlier' Mon");
        assert_eq!(
            replace_reminder("<sent@x>", &unread),
            "The human has 2 earlier unread email(s) of yours besides the one just sent. The human wants at most 1 unread email from you:\n  <old@x> '[t_work] Earlier' Mon\n  <older@x> '[t_work] it\\'s \"x\"' Mon\nDo this now: write ONE email that keeps everything the human still needs from those emails and from the one just sent, and send it with\n  amh tell human --subject SAME_SUBJECT --file MERGED_FILE --replaces '<old@x>' --replaces '<older@x>' --replaces '<sent@x>'\nThe replaced unread emails go to the trash. Skip this only if those earlier emails must stay separate.\n"
        );
        assert_eq!(replace_reminder("<sent@x>", &unread[2..]), "");
        assert!(NOTICE_SENDER.starts_with("amh ") && WATCHER.starts_with("amh "));
        assert_eq!(
            unread_check_failed("mail search in INBOX failed: it's down"),
            "The email is sent; do not resend it. The check for your earlier unread emails failed: OSError(\"mail search in INBOX failed: it's down\")\n"
        );
    }

    #[test]
    fn the_sending_limit_is_recognised() {
        assert!(is_limit(
            "the email was not sent: permanent error (550): 5.4.5 Daily user sending limit exceeded."
        ));
        assert!(!is_limit("the email was not sent: connection refused"));
        let path = queued_path(Path::new("/state"), "<1791.42@gmail.com>");
        assert_eq!(path, Path::new("/state/outbox/1791.42gmail.com.eml"));
    }

    #[test]
    fn replacements_keep_queued_mail_until_sent() -> Result<(), String> {
        let state = std::env::temp_dir().join(format!("amh-replaces-{}", new_id()));
        fs::create_dir_all(state.join(OUTBOX)).map_err(|error| error.to_string())?;
        let old = "<blocked@gmail.com>";
        let replacement = "<replacement@gmail.com>";
        let unrelated = "<other@gmail.com>";
        for id in [old, replacement, unrelated] {
            fs::write(queued_path(&state, id), b"queued").map_err(|error| error.to_string())?;
        }
        let ids = vec!["blocked@gmail.com".to_owned()];
        replace_queued(&state, replacement, &ids)?;
        assert!(is_queued(&state, old));
        let metadata = queued_path(&state, replacement).with_extension("json");
        let saved: Vec<String> =
            serde_json::from_slice(&fs::read(&metadata).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        assert_eq!(saved, ids);
        fs::remove_file(queued_path(&state, replacement)).map_err(|error| error.to_string())?;
        assert!(retry_replacements(&state).is_empty());
        assert!(!metadata.exists());
        assert!(!is_queued(&state, old));
        assert!(is_queued(&state, unrelated));
        replace_queued(&state, replacement, &ids)?;
        assert_eq!(bare_id(old), bare_id(" blocked@gmail.com "));
        fs::remove_dir_all(state).map_err(|error| error.to_string())
    }
}
