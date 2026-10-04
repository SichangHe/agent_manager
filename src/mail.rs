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

/// Why an email to the human was not sent.
pub enum SendError {
    /// The request itself is wrong; trying again unchanged cannot work.
    Refused(String),
    /// The network or a file failed.
    Failed(String),
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
}

/// The uids in ascending order as an IMAP set.
fn uid_set(uids: impl IntoIterator<Item = u32>) -> String {
    let mut uids: Vec<u32> = uids.into_iter().collect();
    uids.sort_unstable();
    let names: Vec<String> = uids.iter().map(u32::to_string).collect();
    names.join(",")
}

/// Read the files to attach, such as a figure the human asked to see; the type is guessed from the file name.
fn attached(paths: &[PathBuf]) -> Result<Vec<Attached>, SendError> {
    let cannot = |path: &Path, error: std::io::Error| {
        SendError::Failed(format!("cannot read {}: {error}", path.display()))
    };
    let mut n_bytes = 0;
    for path in paths {
        n_bytes += fs::metadata(path)
            .map_err(|error| cannot(path, error))?
            .len();
    }
    if n_bytes > ATTACHED_BYTES_MAX {
        return Err(SendError::Refused(format!(
            "the attachments exceed {} MB in total; send a path instead",
            ATTACHED_BYTES_MAX >> 20
        )));
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
) -> Result<String, SendError> {
    let found = Mailbox::agent(settings)
        .and_then(|mut mailbox| mailbox.search(ALL_MAIL, true, &thread_query(tag)))
        .map_err(SendError::Failed)?;
    let plan = plan(tag, subject, &found).map_err(SendError::Refused)?;
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
    let raw = compose(&draft, &message_id).map_err(SendError::Refused)?;
    deliver(settings, to, &raw).map_err(SendError::Failed)?;
    if sender != WATCHER {
        let epoch_s = SystemTime::now().duration_since(UNIX_EPOCH);
        let line = format!("{} {tag}\n", epoch_s.unwrap_or_default().as_secs());
        let mut log = fs::OpenOptions::new();
        log.create(true)
            .append(true)
            .open(state_dir.join(SENT_LOG))
            .and_then(|mut log| log.write_all(line.as_bytes()))
            .map_err(|error| {
                SendError::Failed(format!(
                    "the email {message_id} is sent, but recording it in {SENT_LOG} failed: {error}"
                ))
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
    let (doomed, kept): (Vec<Header>, Vec<Header>) = found
        .into_iter()
        .partition(|header| trash_ids.contains(&header.message_id));
    if !doomed.is_empty() {
        let uids: Vec<&str> = doomed.iter().map(|header| header.uid.as_str()).collect();
        let moved = mailbox.session.uid_mv(uids.join(","), TRASH);
        moved.map_err(|error| format!("moving replaced mail to the trash failed: {error}"))?;
    }
    Ok(kept)
}
