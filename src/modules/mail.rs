//! MAIL module: inbox list + reader on top of the Himalaya CLI.
//!
//! Everything comes from `himalaya --json …` run as a child process on a
//! background thread; the field names below follow the JSON Schemas the CLI
//! generates itself (`himalaya json-schema <dir>`): `envelope list` →
//! `{"envelopes":[…]}`, `mailbox list` → `{"mailboxes":[…]}`, and any failure
//! → `{"error":"…"}` on stdout. Mail is untrusted input, so every string that
//! reaches the screen goes through [`sanitize`].

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::widgets::truncate;
use chrono::{DateTime, Local};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use serde::Deserialize;
use std::borrow::Cow;
use std::cell::Cell;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Shortest accepted `interval`, so a typo cannot hammer the IMAP server.
const MIN_INTERVAL: u64 = 30;
/// A himalaya call that is still running after this is killed.
const CMD_TIMEOUT: Duration = Duration::from_secs(20);
const NOT_FOUND: &str = "himalaya not found — install it and run 'himalaya configure'";
const DATE_COLS: usize = 11;
/// List column widths, mirroring `himalaya envelope list`.
const FLAGS_W: usize = 5;
const FROM_W: usize = 24;
/// `FROM` width in the narrow layout (no DATE/SIZE columns).
const FROM_NARROW_W: usize = 16;
const DATE_W: usize = 16;
const SIZE_W: usize = 9;
/// Below this width the list drops down to FLAGS + SUBJECT + FROM only.
const NARROW: u16 = 80;
/// At/above this width the list shows the full date form and the SIZE column.
const WIDE: u16 = 100;
/// Largest amount of child stdout kept in memory. A run-away or hostile CLI
/// cannot flood the reader past this; a JSON payload cut off mid-stream just
/// fails to parse, which is already reported as a normal error.
const MAX_STDOUT: u64 = 8 << 20; // 8 MiB
/// Largest message body kept in memory / shown; nothing legitimate needs more
/// and the reader would otherwise clone unbounded text every draw.
const MAX_BODY: usize = 200 * 1024;

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MailCfg {
    /// Executable name or path; looked up on `PATH` when it is a bare name.
    pub command: String,
    /// Himalaya account name; empty = the account marked default.
    pub account: String,
    pub mailbox: String,
    /// Refresh period in seconds, clamped to at least [`MIN_INTERVAL`].
    pub interval: u64,
    pub page_size: u32,
}

impl Default for MailCfg {
    fn default() -> Self {
        Self { command: "himalaya".into(), account: String::new(), mailbox: "INBOX".into(), interval: 300, page_size: 30 }
    }
}

// ---- wire types (himalaya's own JSON Schema) --------------------------------

#[derive(Deserialize)]
struct WireEnvelopes {
    #[serde(default)]
    envelopes: Vec<WireEnvelope>,
}

#[derive(Deserialize)]
struct WireEnvelope {
    #[serde(deserialize_with = "de_id")]
    id: String,
    #[serde(default)]
    flags: Vec<WireFlag>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    from: WireFrom,
    #[serde(default)]
    date: Option<String>,
    /// Himalaya emits `null` here for some backends: `Option` absorbs it.
    #[serde(default, rename = "has-attachment")]
    has_attachment: Option<bool>,
    #[serde(default)]
    size: Option<u64>,
}

/// The schema says `id` is a string; some backends hand out numbers, so accept both.
fn de_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawId {
        S(String),
        N(i64),
    }
    Ok(match RawId::deserialize(d)? {
        RawId::S(s) => s,
        RawId::N(n) => n.to_string(),
    })
}

/// A flag is `{"raw":"\\Seen","iana":"seen"}`; a plain string is accepted too.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireFlag {
    Obj {
        #[serde(default)]
        raw: String,
        #[serde(default)]
        iana: Option<String>,
    },
    Name(String),
}

impl WireFlag {
    fn is_seen(&self) -> bool {
        let s = match self {
            WireFlag::Obj { raw, iana } => iana.as_deref().unwrap_or(raw),
            WireFlag::Name(s) => s,
        };
        s.trim_start_matches('\\').eq_ignore_ascii_case("seen")
    }
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum WireFrom {
    Many(Vec<WireAddress>),
    One(WireAddress),
    Text(String),
    #[default]
    None,
}

#[derive(Deserialize, Default)]
struct WireAddress {
    #[serde(default)]
    name: Option<String>,
    /// `email` in the schema; `addr` is accepted as an alias.
    #[serde(default, alias = "addr")]
    email: String,
}

impl WireAddress {
    fn display(&self) -> String {
        match self.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => n.to_string(),
            None => self.email.clone(),
        }
    }
}

impl WireFrom {
    fn display(&self) -> String {
        match self {
            WireFrom::Many(v) => v.first().map(WireAddress::display).unwrap_or_default(),
            WireFrom::One(a) => a.display(),
            WireFrom::Text(s) => s.clone(),
            WireFrom::None => String::new(),
        }
    }
}

#[derive(Deserialize)]
struct WireMailboxes {
    #[serde(default)]
    mailboxes: Vec<WireMailbox>,
}

#[derive(Deserialize)]
struct WireMailbox {
    name: String,
}

// ---- display types ----------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Envelope {
    id: String,
    seen: bool,
    subject: String,
    from: String,
    /// Short form (`MM-DD HH:MM`), used in the reader and the list below
    /// [`WIDE`].
    date: String,
    /// Full form (`YYYY-MM-DD HH:MM`), used in the list at/above [`WIDE`].
    date_full: String,
    has_attachment: bool,
    /// Message size in bytes, as reported by the envelope list.
    size: u64,
}

impl From<WireEnvelope> for Envelope {
    fn from(w: WireEnvelope) -> Self {
        let (date, date_full) = format_dates(w.date.as_deref());
        Envelope {
            id: sanitize(&w.id),
            seen: w.flags.iter().any(WireFlag::is_seen),
            subject: sanitize(&w.subject),
            from: sanitize(&w.from.display()),
            date,
            date_full,
            has_attachment: w.has_attachment.unwrap_or(false),
            size: w.size.unwrap_or(0),
        }
    }
}

/// RFC 3339 → local `(MM-DD HH:MM, YYYY-MM-DD HH:MM)`; anything else is shown
/// as-is (trimmed) in both.
fn format_dates(raw: Option<&str>) -> (String, String) {
    let Some(raw) = raw else { return (String::new(), String::new()) };
    match DateTime::parse_from_rfc3339(raw.trim()) {
        Ok(dt) => {
            let dt = dt.with_timezone(&Local);
            (dt.format("%m-%d %H:%M").to_string(), dt.format("%Y-%m-%d %H:%M").to_string())
        }
        Err(_) => {
            let s = truncate(&sanitize(raw.trim()), DATE_COLS);
            (s.clone(), s)
        }
    }
}

/// Formats a byte count the way `himalaya envelope list` does: whole bytes
/// under 1 KiB, one decimal place in KiB/MiB/GiB above that.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

// ---- sanitising untrusted mail text ----------------------------------------

/// Drops C0/C1 control characters (keeping `\n` and `\t`) and ANSI `ESC[…` /
/// `ESC]…` sequences. A crafted subject or body would otherwise be able to
/// repaint the terminal.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.peek() {
                // CSI: runs to a final byte in 0x40..=0x7e.
                Some('[') => {
                    it.next();
                    for c in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: runs to BEL or ST (ESC \).
                Some(']') => {
                    it.next();
                    while let Some(c) = it.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            it.next();
                            break;
                        }
                    }
                }
                // Two-character escape.
                Some(_) => {
                    it.next();
                }
                None => {}
            }
            continue;
        }
        let code = c as u32;
        if c == '\n' || c == '\t' {
            out.push(c);
        } else if code >= 0x20
            && code != 0x7f
            && !(0x80..=0x9f).contains(&code)
            // bidi overrides and zero-width characters: no legitimate reason
            // for either in a subject/body, and both can be used to spoof
            // what is displayed.
            && !matches!(code, 0x200b..=0x200d | 0x202d | 0x202e | 0x2066..=0x2069 | 0xfeff)
        {
            out.push(c);
        }
    }
    out
}

/// Crude markup removal for the text/html fallback: everything between `<`
/// and `>` goes. ponytail: no entity decoding — plain-text parts are the
/// normal case and this is only the fallback.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

// ---- JSON parsing ----------------------------------------------------------

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

/// himalaya reports failures as `{"error":"…"}` on stdout (sometimes even with
/// exit code 0), so that shape is checked before the payload.
fn parse_json<T: serde::de::DeserializeOwned>(stdout: &str) -> Result<T, String> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| first_line(&e.to_string()))?;
    if let Some(msg) = v.get("error").and_then(|e| e.as_str()) {
        return Err(sanitize(&first_line(msg)));
    }
    serde_json::from_value(v).map_err(|e| first_line(&e.to_string()))
}

fn parse_envelopes(stdout: &str) -> Result<Vec<Envelope>, String> {
    let w: WireEnvelopes = parse_json(stdout)?;
    Ok(w.envelopes.into_iter().map(Envelope::from).collect())
}

fn parse_mailboxes(stdout: &str) -> Result<Vec<String>, String> {
    let w: WireMailboxes = parse_json(stdout)?;
    Ok(w.mailboxes.into_iter().map(|m| sanitize(&m.name)).collect())
}

/// A parsed `message read --json` result: the readable body text plus how
/// many attachments the message carries.
#[derive(Debug)]
struct ParsedMessage {
    body: String,
    attachments: usize,
}

/// `message read --json` emits the mail-parser message JSON: a flat `parts`
/// array, with `text_body`/`html_body`/`attachments` holding the indices into
/// it that the multipart tree actually resolved to. Those declared indices
/// are tried first; a value with no such shape (or an older/other CLI build)
/// falls back to a generic walk of the whole tree.
fn parse_message(stdout: &str) -> Result<ParsedMessage, String> {
    let v: serde_json::Value = parse_json(stdout)?;
    let attachments = v.get("attachments").and_then(serde_json::Value::as_array).map(Vec::len).unwrap_or(0);
    let body = extract_body(&v)
        .or_else(|| find_text(&v).map(|(_, t)| t))
        .or_else(|| v.as_str().map(str::to_string))
        .unwrap_or_default();
    Ok(ParsedMessage { body: cap_body(sanitize(&body)), attachments })
}

/// Follows `text_body`/`html_body` into `parts` (mail-parser's own choice of
/// readable part); falls back to the first part whose body is `Text`, then
/// the first whose body is `Html`. Ignores `Binary` bodies.
fn extract_body(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let parts = v.get("parts").and_then(Value::as_array)?;
    let text_of = |b: &Value| b.get("Text").and_then(Value::as_str).map(str::to_string);
    let html_of = |b: &Value| b.get("Html").and_then(Value::as_str).map(strip_tags);

    if let Some(t) = v.get("text_body").and_then(|idx| join_parts(parts, idx, text_of)) {
        return Some(t);
    }
    if let Some(h) = v.get("html_body").and_then(|idx| join_parts(parts, idx, html_of)) {
        return Some(h);
    }
    if let Some(t) = parts.iter().find_map(|p| p.get("body").and_then(text_of)) {
        return Some(t);
    }
    parts.iter().find_map(|p| p.get("body").and_then(html_of))
}

/// Resolves an array of part indices to their bodies via `get`, joining
/// non-empty results with a blank line (the same way separate MIME parts of
/// the same declared body read as separate paragraphs).
fn join_parts(
    parts: &[serde_json::Value],
    indices: &serde_json::Value,
    get: impl Fn(&serde_json::Value) -> Option<String>,
) -> Option<String> {
    let out: Vec<String> = indices
        .as_array()?
        .iter()
        .filter_map(|i| {
            let i = i.as_u64()? as usize;
            get(parts.get(i)?.get("body")?)
        })
        .collect();
    if out.is_empty() { None } else { Some(out.join("\n\n")) }
}

/// Truncates a body to [`MAX_BODY`] bytes (on a char boundary) with a marker,
/// so neither memory nor the per-frame render cost is unbounded.
fn cap_body(s: String) -> String {
    if s.len() <= MAX_BODY {
        return s;
    }
    let mut end = MAX_BODY;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s;
    out.truncate(end);
    out.push_str("… [truncated]");
    out
}

/// Best readable text in a parsed-message value: rank 0 = text/plain,
/// 1 = text/html, 2 = an untyped body string.
fn find_text(v: &serde_json::Value) -> Option<(u8, String)> {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            // mail-parser style tagged body: {"Text": "…"} / {"Html": "…"}.
            if let Some(s) = m.get("Text").and_then(Value::as_str) {
                return Some((0, s.to_string()));
            }
            if let Some(s) = m.get("Html").and_then(Value::as_str) {
                return Some((1, strip_tags(s)));
            }
            let mime = ["content_type", "content-type", "mime", "type"]
                .iter()
                .find_map(|k| m.get(*k).and_then(Value::as_str))
                .unwrap_or("")
                .to_ascii_lowercase();
            let own = ["body", "text", "content", "value", "message"].iter().find_map(|k| m.get(*k).and_then(Value::as_str));
            if let Some(s) = own {
                if mime.contains("text/plain") {
                    return Some((0, s.to_string()));
                }
                if mime.contains("text/html") {
                    return Some((1, strip_tags(s)));
                }
            }
            best(m.values()).or_else(|| own.map(|s| (2, s.to_string())))
        }
        Value::Array(a) => best(a.iter()),
        _ => None,
    }
}

fn best<'a>(it: impl Iterator<Item = &'a serde_json::Value>) -> Option<(u8, String)> {
    it.filter_map(find_text).min_by_key(|(rank, _)| *rank)
}

// ---- running the CLI --------------------------------------------------------

fn args_envelopes(cfg: &MailCfg, mailbox: &str) -> Vec<String> {
    let mut a: Vec<String> =
        ["--json", "envelope", "list", "-s"].iter().map(|s| s.to_string()).collect();
    a.push(cfg.page_size.max(1).to_string());
    push_account(&mut a, cfg);
    push_mailbox(&mut a, mailbox);
    a
}

fn args_mailboxes(cfg: &MailCfg) -> Vec<String> {
    let mut a: Vec<String> = ["--json", "mailbox", "list"].iter().map(|s| s.to_string()).collect();
    push_account(&mut a, cfg);
    a
}

fn args_read(cfg: &MailCfg, mailbox: &str, id: &str) -> Vec<String> {
    let mut a: Vec<String> = ["--json", "message", "read"].iter().map(|s| s.to_string()).collect();
    a.push(id.to_string());
    a.push("--seen".to_string());
    push_account(&mut a, cfg);
    push_mailbox(&mut a, mailbox);
    a
}

fn push_account(a: &mut Vec<String>, cfg: &MailCfg) {
    if !cfg.account.trim().is_empty() {
        a.push("-a".into());
        a.push(cfg.account.clone());
    }
}

fn push_mailbox(a: &mut Vec<String>, mailbox: &str) {
    if !mailbox.trim().is_empty() {
        a.push("-m".into());
        a.push(mailbox.to_string());
    }
}

/// Drains a child pipe on its own thread, so a large body cannot fill the pipe
/// buffer and deadlock the timeout loop below.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(r) = pipe {
            // Capped at MAX_STDOUT: a run-away child cannot grow this
            // unboundedly. Read stops silently at the cap; a JSON payload
            // truncated mid-stream just fails to parse, reported as usual.
            let _ = r.take(MAX_STDOUT).read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Runs `<command> <args…>` with no shell and no stdin, killing it after
/// [`CMD_TIMEOUT`]. `Ok` holds stdout when it looks like JSON; otherwise the
/// first stderr line (or the exit status) is the error.
fn capture(cfg: &MailCfg, args: &[String]) -> Result<String, String> {
    let mut child = match Command::new(&cfg.command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(NOT_FOUND.to_string()),
        Err(e) => return Err(format!("{}: {e}", cfg.command)),
    };
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + CMD_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let Some(status) = status else {
        return Err(format!("himalaya timed out after {}s", CMD_TIMEOUT.as_secs()));
    };
    if stdout.trim_start().starts_with(['{', '[']) {
        return Ok(stdout);
    }
    let msg = sanitize(&first_line(&stderr));
    Err(if msg.is_empty() { format!("himalaya exited with {status}") } else { msg })
}

// ---- background thread ------------------------------------------------------

enum MailCmd {
    Refresh,
    Mailbox(String),
    Read(String),
}

enum MailEvent {
    /// Tagged with the mailbox it was fetched for, so a result that arrives
    /// after the user has switched away can be told apart from a fresh one.
    Envelopes(String, Vec<Envelope>),
    Mailboxes(Vec<String>),
    /// Tagged with the mailbox and message id it was fetched for, same
    /// reason. `Ok` holds `(body, attachment count)`.
    Message(String, String, Result<(String, usize), String>),
    Error(String),
}

fn load_envelopes(cfg: &MailCfg, mailbox: &str) -> MailEvent {
    match capture(cfg, &args_envelopes(cfg, mailbox)).and_then(|o| parse_envelopes(&o)) {
        Ok(v) => MailEvent::Envelopes(mailbox.to_string(), v),
        Err(e) => MailEvent::Error(e),
    }
}

fn run(cfg: MailCfg, tx: Sender<MailEvent>, crx: Receiver<MailCmd>) {
    let interval = Duration::from_secs(cfg.interval.max(MIN_INTERVAL));
    let mut mailbox = cfg.mailbox.clone();
    let list_mailboxes = |tx: &Sender<MailEvent>| {
        // A failing mailbox list is not reported separately: the envelope
        // fetch right after it surfaces the very same error.
        if let Ok(names) = capture(&cfg, &args_mailboxes(&cfg)).and_then(|o| parse_mailboxes(&o)) {
            let _ = tx.send(MailEvent::Mailboxes(names));
        }
    };
    list_mailboxes(&tx);
    let mut next = Instant::now();
    loop {
        if Instant::now() >= next {
            if tx.send(load_envelopes(&cfg, &mailbox)).is_err() {
                return;
            }
            next = Instant::now() + interval;
        }
        let wait = next.saturating_duration_since(Instant::now());
        match crx.recv_timeout(wait) {
            Ok(MailCmd::Refresh) => {
                list_mailboxes(&tx);
                next = Instant::now();
            }
            Ok(MailCmd::Mailbox(m)) => {
                mailbox = m;
                // `[`/`]` hammered five times queues five fetches of ~3 s each;
                // only the last mailbox matters, so swallow the rest first.
                while let Ok(MailCmd::Mailbox(later)) = crx.try_recv() {
                    mailbox = later;
                }
                next = Instant::now();
            }
            Ok(MailCmd::Read(id)) => {
                let body = capture(&cfg, &args_read(&cfg, &mailbox, &id))
                    .and_then(|o| parse_message(&o))
                    .map(|m| (m.body, m.attachments));
                if tx.send(MailEvent::Message(mailbox.clone(), id, body)).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---- the module -------------------------------------------------------------

/// Open message: the headers come from the envelope that was selected, the
/// body arrives later on the channel.
struct Reader {
    env: Envelope,
    /// `Ok` holds `(body, attachment count)`.
    body: Option<Result<(String, usize), String>>,
    scroll: u16,
}

pub struct Mail {
    cfg: MailCfg,
    envelopes: Vec<Envelope>,
    mailboxes: Vec<String>,
    mailbox: String,
    sel: usize,
    /// `Some` = reader view, `None` = list view.
    reader: Option<Reader>,
    err: Option<String>,
    /// A list fetch is in flight (start, `r`, mailbox switch): the list area
    /// says LOADING… instead of looking empty for the ~3 s a Himalaya call takes.
    loading: bool,
    updated: Option<DateTime<Local>>,
    /// Height last given to the reader body, for PgUp/PgDn paging and the
    /// wrapped-row scroll limit.
    reader_height: Cell<u16>,
    /// Width last given to the reader body, for the wrapped-row scroll limit.
    reader_width: Cell<u16>,
    rx: Option<Receiver<MailEvent>>,
    tx: Option<Sender<MailCmd>>,
}

impl Mail {
    pub fn new() -> Self {
        let cfg = MailCfg::default();
        Self {
            mailbox: cfg.mailbox.clone(),
            cfg,
            envelopes: Vec::new(),
            mailboxes: Vec::new(),
            sel: 0,
            reader: None,
            err: None,
            loading: false,
            updated: None,
            reader_height: Cell::new(10),
            reader_width: Cell::new(80),
            rx: None,
            tx: None,
        }
    }

    fn unread(&self) -> usize {
        self.envelopes.iter().filter(|e| !e.seen).count()
    }

    fn move_sel(&mut self, delta: i32) {
        if self.envelopes.is_empty() {
            self.sel = 0;
            return;
        }
        self.sel = (self.sel as i32 + delta).clamp(0, self.envelopes.len() as i32 - 1) as usize;
    }

    /// Steps to the next/previous known mailbox and asks for a refresh.
    /// No-op while the mailbox list is empty (nothing to cycle through).
    fn cycle_mailbox(&mut self, delta: i32) {
        let n = self.mailboxes.len();
        if n == 0 {
            return;
        }
        let cur = self.mailboxes.iter().position(|m| m.eq_ignore_ascii_case(&self.mailbox)).unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n as i32) as usize;
        self.set_mailbox(self.mailboxes[next].clone());
    }

    /// Switches to `name`: clears the stale list and tells the worker, so the
    /// fetch that follows and the label shown in the title agree.
    fn set_mailbox(&mut self, name: String) {
        self.mailbox = name;
        self.sel = 0;
        self.envelopes.clear();
        self.loading = true;
        if let Some(tx) = &self.tx {
            let _ = tx.send(MailCmd::Mailbox(self.mailbox.clone()));
        }
    }

    fn open_selected(&mut self) {
        let Some(env) = self.envelopes.get_mut(self.sel) else { return };
        env.seen = true; // the CLI is called with --seen; mirror it locally
        let env = env.clone();
        if let Some(tx) = &self.tx {
            let _ = tx.send(MailCmd::Read(env.id.clone()));
        }
        self.reader = Some(Reader { env, body: None, scroll: 0 });
    }

    fn scroll_reader(&mut self, delta: i32) {
        let width = self.reader_width.get();
        let height = self.reader_height.get();
        let Some(r) = &mut self.reader else { return };
        let max = r
            .body
            .as_ref()
            .and_then(|b| b.as_ref().ok())
            .map(|(b, _)| wrapped_rows(b, width))
            .unwrap_or(1)
            .saturating_sub(height);
        r.scroll = (r.scroll as i32 + delta).clamp(0, max as i32) as u16;
    }

    fn title_line(&self, width: u16, t: Theme) -> Line<'static> {
        let mut head = format!("MAIL · {} · {} unread", self.mailbox, self.unread());
        if let Some(u) = self.updated {
            head.push_str(&format!(" · updated {}", u.format("%H:%M")));
        }
        let mut spans = vec![Span::styled(truncate(&head, width as usize), t.title)];
        if let Some(e) = &self.err {
            let rest = (width as usize).saturating_sub(head.chars().count());
            if rest > 3 {
                spans.push(Span::styled(truncate(&format!(" · {e}"), rest), t.warn));
            }
        }
        Line::from(spans)
    }

    fn draw_list(&self, f: &mut Frame, area: Rect, t: Theme) {
        let rows =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).split(area);
        f.render_widget(Paragraph::new(self.title_line(rows[0].width, t)), rows[0]);
        f.render_widget(Paragraph::new(header_line(rows[1].width, t)), rows[1]);
        if self.envelopes.is_empty() && (self.loading || self.err.is_none()) {
            // Centred status instead of an empty list: LOADING… while a fetch
            // runs, otherwise the mailbox really is empty.
            let (msg, style) = if self.loading {
                ("LOADING\u{2026}".to_string(), t.title)
            } else {
                (format!("no messages in {}", self.mailbox), t.frame)
            };
            let y = rows[2].y + rows[2].height / 2;
            if rows[2].height > 0 {
                let line = Rect { x: rows[2].x, y, width: rows[2].width, height: 1 };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(truncate(&msg, rows[2].width as usize), style)))
                        .alignment(ratatui::layout::Alignment::Center),
                    line,
                );
            }
            return;
        }
        let items: Vec<ListItem> = if self.envelopes.is_empty() {
            let msg = match &self.err {
                Some(e) => format!("n/a: {e}"),
                None => "(no messages)".to_string(),
            };
            vec![ListItem::new(truncate(&msg, rows[2].width as usize)).style(t.frame)]
        } else {
            self.envelopes.iter().map(|e| ListItem::new(row_line(e, rows[2].width, t))).collect()
        };
        let mut state = ListState::default();
        if !self.envelopes.is_empty() {
            state.select(Some(self.sel.min(self.envelopes.len() - 1)));
        }
        f.render_stateful_widget(List::new(items).highlight_style(t.tab_active), rows[2], &mut state);
    }

    fn draw_reader(&self, f: &mut Frame, area: Rect, t: Theme, r: &Reader) {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
        let w = area.width as usize;
        f.render_widget(Paragraph::new(Line::from(Span::styled(truncate(&r.env.subject, w), t.title))), rows[0]);
        f.render_widget(Paragraph::new(Line::from(Span::styled(truncate(&r.env.from, w), t.value))), rows[1]);
        let date_line = match &r.body {
            Some(Ok((_, n))) => format!("{}  ·  attachments: {n}", r.env.date),
            _ => r.env.date.clone(),
        };
        f.render_widget(Paragraph::new(Line::from(Span::styled(truncate(&date_line, w), t.frame))), rows[2]);
        self.reader_height.set(rows[3].height);
        self.reader_width.set(rows[3].width);
        if r.body.is_none() {
            // Same centred LOADING… as the list, in place of the body.
            let y = rows[3].y + rows[3].height / 2;
            if rows[3].height > 0 {
                let line = Rect { x: rows[3].x, y, width: rows[3].width, height: 1 };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled("LOADING\u{2026}", t.title)))
                        .alignment(ratatui::layout::Alignment::Center),
                    line,
                );
            }
            return;
        }
        // Cow: the common (large) body case borrows instead of cloning every
        // frame; only the small synthetic strings below ever allocate.
        let (body, style): (Cow<str>, _) = match &r.body {
            None => unreachable!("handled above"),
            Some(Ok((b, _))) if b.trim().is_empty() => (Cow::Borrowed("(empty message)"), t.frame),
            Some(Ok((b, _))) => (Cow::Borrowed(b.as_str()), t.text),
            Some(Err(e)) => (Cow::Owned(format!("n/a: {e}")), t.warn),
        };
        // Clamp defensively here too, in case the stored scroll predates a
        // resize or a shorter body replaced a longer one.
        let max_scroll = wrapped_rows(&body, rows[3].width).saturating_sub(rows[3].height);
        let scroll = r.scroll.min(max_scroll);
        let p = Paragraph::new(body.as_ref()).style(style).wrap(Wrap { trim: false }).scroll((scroll, 0));
        f.render_widget(p, rows[3]);
    }
}

/// Resolved column widths for the list, for a given terminal width.
struct Cols {
    subject: usize,
    from: usize,
    show_date: bool,
    full_date: bool,
    show_size: bool,
}

/// FLAGS · SUBJECT (flex) · FROM · [DATE] · [SIZE], mirroring
/// `himalaya envelope list`. Below [`NARROW`] only FLAGS/SUBJECT/FROM show;
/// at/above [`WIDE`] the date grows to its full form and SIZE appears.
fn cols(width: u16) -> Cols {
    let w = width as usize;
    if width < NARROW {
        let fixed = FLAGS_W + 1 + FROM_NARROW_W;
        return Cols { subject: w.saturating_sub(fixed).max(10), from: FROM_NARROW_W, show_date: false, full_date: false, show_size: false };
    }
    let show_size = width >= WIDE;
    let mut fixed = FLAGS_W + 1 + FROM_W + 1 + DATE_W;
    if show_size {
        fixed += 1 + SIZE_W;
    }
    Cols { subject: w.saturating_sub(fixed).max(10), from: FROM_W, show_date: true, full_date: show_size, show_size }
}

fn header_line(width: u16, t: Theme) -> Line<'static> {
    let c = cols(width);
    let mut s = pad("FLAGS", FLAGS_W);
    s.push(' ');
    s.push_str(&pad("SUBJECT", c.subject));
    s.push(' ');
    s.push_str(&pad("FROM", c.from));
    if c.show_date {
        s.push(' ');
        s.push_str(&pad("DATE", DATE_W));
    }
    if c.show_size {
        s.push(' ');
        s.push_str(&pad("SIZE", SIZE_W));
    }
    Line::from(Span::styled(truncate(&s, width as usize), t.title.add_modifier(Modifier::UNDERLINED)))
}

/// One list row: `FLAGS SUBJECT FROM [DATE] [SIZE]`, colour-matched to
/// `himalaya envelope list`; unseen rows get a bold subject and a `*` flag.
fn row_line(e: &Envelope, width: u16, t: Theme) -> Line<'static> {
    let c = cols(width);
    let mut flags = String::new();
    if !e.seen {
        flags.push('*');
    }
    if e.has_attachment {
        flags.push('!');
    }
    let flags_style = if e.seen { t.text } else { t.title };
    let subject_style = if e.seen { t.value } else { t.value.add_modifier(Modifier::BOLD) };

    let mut spans = vec![
        Span::styled(pad(&flags, FLAGS_W), flags_style),
        Span::raw(" "),
        Span::styled(pad(&e.subject, c.subject), subject_style),
        Span::raw(" "),
        Span::styled(pad(&e.from, c.from), t.text),
    ];
    if c.show_date {
        let date = if c.full_date { &e.date_full } else { &e.date };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pad(date, DATE_W), t.warn));
    }
    if c.show_size {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pad(&format_size(e.size), SIZE_W), t.frame));
    }
    Line::from(spans)
}

/// Wrapped-row count for a body shown at `width` columns: every line takes at
/// least one row, more if it is wider than `width` (matches the `Wrap`
/// widget's line-wrapping, not its word-wrapping, closely enough for a
/// scroll limit). Character count, like [`truncate`]/[`pad`] elsewhere in
/// this file — not full Unicode display width.
fn wrapped_rows(body: &str, width: u16) -> u16 {
    let width = (width as usize).max(1);
    let rows: usize = body
        .lines()
        .map(|l| {
            let w = l.chars().count();
            if w == 0 { 1 } else { (w + width - 1) / width }
        })
        .sum();
    rows.clamp(1, u16::MAX as usize) as u16
}

/// Truncates to `n` characters, then pads to exactly `n` with spaces.
fn pad(s: &str, n: usize) -> String {
    let t = truncate(s, n);
    let fill = n.saturating_sub(t.chars().count());
    format!("{t}{}", " ".repeat(fill))
}

impl Module for Mail {
    fn id(&self) -> &'static str {
        "mail"
    }
    fn title(&self) -> &'static str {
        "MAIL"
    }
    fn describe(&self) -> &'static str {
        "Your inbox through the Himalaya CLI (read-only)"
    }
    fn help(&self) -> &'static str {
        match self.reader {
            None => "↑/↓ select   enter read   [ ] mailbox   r refresh   1-9 tabs   q quit",
            Some(_) => "↑/↓ scroll   esc back   q quit",
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        self.loading = true;
        let (cfg, notice) = ctx.config.section::<MailCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.cfg = cfg;
        self.mailbox = self.cfg.mailbox.clone();
        let (tx_ev, rx_ev) = mpsc::channel();
        let (tx_cmd, rx_cmd) = mpsc::channel();
        self.rx = Some(rx_ev);
        self.tx = Some(tx_cmd);
        let cfg = self.cfg.clone();
        std::thread::spawn(move || run(cfg, tx_ev, rx_cmd));
    }

    fn poll(&mut self, _ctx: &Ctx) -> usize {
        let mut n = 0;
        // Taken out for the duration of the loop: self.set_mailbox() below
        // needs &mut self, which can't coexist with a borrow of self.rx.
        let Some(rx) = self.rx.take() else { return 0 };
        while let Ok(ev) = rx.try_recv() {
            n += 1;
            match ev {
                MailEvent::Envelopes(mailbox, v) => {
                    // Stale: fetched for a mailbox we've since switched away
                    // from (`[`/`]` or a second Enter beat this reply back).
                    if mailbox != self.mailbox {
                        continue;
                    }
                    self.loading = false;
                    self.err = None;
                    self.updated = Some(Local::now());
                    self.sel = self.sel.min(v.len().saturating_sub(1));
                    self.envelopes = v;
                }
                MailEvent::Mailboxes(v) => {
                    // Servers spell it `Inbox`, configs say `INBOX`: match
                    // case-insensitively and adopt the server's spelling, so
                    // the label, the cycle index and the fetch agree.
                    let wanted = self.mailbox.clone();
                    match v.iter().find(|m| m.eq_ignore_ascii_case(&wanted)) {
                        Some(name) if *name != wanted => self.set_mailbox(name.clone()),
                        Some(_) => {}
                        None => {
                            let fallback = v
                                .iter()
                                .find(|m| m.eq_ignore_ascii_case("inbox"))
                                .or_else(|| v.first())
                                .cloned();
                            if let Some(name) = fallback {
                                self.set_mailbox(name);
                            }
                        }
                    }
                    self.mailboxes = v;
                }
                MailEvent::Message(mailbox, id, b) => {
                    if let Some(r) = &mut self.reader {
                        if mailbox == self.mailbox && r.env.id == id {
                            r.body = Some(b);
                            r.scroll = 0;
                        }
                    }
                }
                MailEvent::Error(e) => {
                    self.err = Some(e);
                    self.loading = false;
                }
            }
        }
        self.rx = Some(rx);
        n
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> bool {
        if self.reader.is_some() {
            let page = self.reader_height.get().saturating_sub(2).max(1) as i32;
            match key.code {
                KeyCode::Up => self.scroll_reader(-1),
                KeyCode::Down => self.scroll_reader(1),
                KeyCode::PageUp => self.scroll_reader(-page),
                KeyCode::PageDown => self.scroll_reader(page),
                KeyCode::Esc | KeyCode::Backspace => self.reader = None,
                _ => return false,
            }
            return true;
        }
        match key.code {
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::Char('[') => self.cycle_mailbox(-1),
            KeyCode::Char(']') => self.cycle_mailbox(1),
            KeyCode::Char('r') => {
                self.loading = true;
                if let Some(tx) = &self.tx {
                    let _ = tx.send(MailCmd::Refresh);
                }
            }
            KeyCode::Enter => self.open_selected(),
            _ => return false,
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        match &self.reader {
            Some(r) => self.draw_reader(f, area, t, r),
            None => self.draw_list(f, area, t),
        }
    }

    fn header(&self, width: u16, t: Theme) -> Vec<Span<'static>> {
        let unread = self.unread();
        if unread == 0 {
            return vec![];
        }
        let s = format!("✉ {unread}");
        if s.chars().count() > width as usize {
            return vec![];
        }
        vec![Span::styled(s, t.warn)]
    }

    fn overview(&self, width: u16, height: u16, t: Theme) -> Vec<Line<'static>> {
        let w = (width as usize).saturating_sub(1);
        let mut lines =
            vec![Line::from(Span::styled(" MAIL", t.title)), Line::from(format!(" {} unread", self.unread()))];
        if let Some(e) = self.envelopes.iter().find(|e| !e.seen) {
            lines.push(Line::from(truncate(&format!(" {} – {}", e.from, e.subject), w)));
        } else if let Some(err) = &self.err {
            lines.push(Line::from(Span::styled(truncate(&format!(" {err}"), w), t.warn)));
        }
        lines.truncate(height.max(1) as usize);
        lines
    }

    fn overview_slot(&self) -> Slot {
        Slot::Left(3)
    }

    fn status(&self) -> String {
        let state = match &self.err {
            Some(e) => format!("error: {e}"),
            None => "ok".to_string(),
        };
        format!("mail {} {} envelopes, {} unread, {}", self.mailbox, self.envelopes.len(), self.unread(), state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const ENVELOPES: &str = r#"{"envelopes":[
      {"id":"42","flags":[{"raw":"\\Seen","iana":"seen"}],"subject":"Weekly report",
       "from":[{"name":"Alice","email":"alice@example.org"}],"date":"2026-09-11T08:30:00+02:00",
       "has-attachment":true,"size":123456},
      {"id":7,"flags":[],"subject":"Reactor status",
       "from":[{"email":"bob@example.org"}],"date":"2026-09-10T22:05:00+02:00",
       "has-attachment":null,"size":null},
      {"id":"x1","flags":["Seen"],"subject":"","from":[],"date":null}
    ]}"#;

    const MAILBOXES: &str = r#"{"mailboxes":[
      {"id":"INBOX","name":"INBOX","total":12,"unread":3},
      {"id":"Archive","name":"Archive","total":null,"unread":null},
      {"id":"Sent","name":"Sent"}
    ]}"#;

    /// Legacy/other-CLI shape (no `text_body`/`html_body` index arrays):
    /// exercises the generic-walk fallback in [`extract_body`]/[`find_text`].
    const MESSAGE_LEGACY: &str = r#"{"parts":[
      {"content_type":"multipart/alternative","parts":[
        {"content_type":"text/plain; charset=utf-8","body":"Hello\nfrom the vault"},
        {"content_type":"text/html","body":"<p>Hello</p>"}
      ]}
    ]}"#;

    /// The real mail-parser shape `himalaya --json message read` emits: a
    /// flat `parts` array, `text_body`/`html_body`/`attachments` holding the
    /// indices into it.
    const MESSAGE_TEXT: &str = r#"{"text_body":[1],"html_body":[],"attachments":[0],"parts":[
      {"headers":[],"is_encoding_problem":false,"body":{"Binary":[1,2,3]},"offset_header":0,"offset_body":0,"offset_end":3},
      {"headers":[],"is_encoding_problem":false,"body":{"Text":"Hello"},"offset_header":0,"offset_body":0,"offset_end":5}
    ]}"#;

    const MESSAGE_HTML_ONLY: &str = r#"{"text_body":[],"html_body":[0],"attachments":[],"parts":[
      {"headers":[],"is_encoding_problem":false,"body":{"Html":"<p>Hi <b>there</b></p>"},"offset_header":0,"offset_body":0,"offset_end":10}
    ]}"#;

    const ERROR: &str = r#"{"error":"invalid peer certificate: UnknownIssuer\nsecond line","sources":[],"backtrace":null}"#;

    fn env(seen: bool, from: &str, subject: &str) -> Envelope {
        Envelope {
            id: "1".into(),
            seen,
            subject: subject.into(),
            from: from.into(),
            date: "09-11 08:30".into(),
            date_full: "2026-09-11 08:30".into(),
            has_attachment: false,
            size: 0,
        }
    }

    /// Flattens a rendered [`Line`] back to plain text, for column-content
    /// assertions that don't care about styling.
    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn parses_envelope_list_ids_flags_and_addresses() {
        let v = parse_envelopes(ENVELOPES).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].id, "42");
        assert!(v[0].seen);
        assert_eq!(v[0].subject, "Weekly report");
        assert_eq!(v[0].from, "Alice");
        assert!(v[0].date.ends_with("08:30") || !v[0].date.is_empty());
        // numeric id, no flags, address without a display name
        assert_eq!(v[1].id, "7");
        assert!(!v[1].seen);
        assert_eq!(v[1].from, "bob@example.org");
        // plain-string flag, no date, no sender
        assert_eq!(v[2].id, "x1");
        assert!(v[2].seen);
        assert_eq!(v[2].from, "");
        assert_eq!(v[2].date, "");
    }

    #[test]
    fn parses_mailbox_list() {
        assert_eq!(parse_mailboxes(MAILBOXES).unwrap(), vec!["INBOX", "Archive", "Sent"]);
    }

    #[test]
    fn parses_mail_parser_text_body_by_index() {
        // The real shape: `text_body: [1]` points at `parts[1].body.Text`.
        let m = parse_message(MESSAGE_TEXT).unwrap();
        assert_eq!(m.body, "Hello");
        assert_eq!(m.attachments, 1); // `attachments: [0]`
    }

    #[test]
    fn parses_mail_parser_html_only_message() {
        let m = parse_message(MESSAGE_HTML_ONLY).unwrap();
        assert_eq!(m.body, "Hi there");
        assert_eq!(m.attachments, 0);
    }

    #[test]
    fn falls_back_to_a_generic_walk_for_shapes_without_text_body_html_body() {
        assert_eq!(parse_message(MESSAGE_LEGACY).unwrap().body, "Hello\nfrom the vault");
        let html = r#"{"parts":[{"content_type":"text/html","body":"<p>only <b>html</b></p>"}]}"#;
        assert_eq!(parse_message(html).unwrap().body, "only html");
        assert_eq!(parse_message(r#"{"message":"raw text"}"#).unwrap().body, "raw text");
        assert_eq!(parse_message(r#"{"body":"just a body"}"#).unwrap().body, "just a body");
    }

    #[test]
    fn error_object_is_an_error_with_the_first_line() {
        assert_eq!(parse_envelopes(ERROR).unwrap_err(), "invalid peer certificate: UnknownIssuer");
        assert_eq!(parse_mailboxes(ERROR).unwrap_err(), "invalid peer certificate: UnknownIssuer");
        assert_eq!(parse_message(ERROR).unwrap_err(), "invalid peer certificate: UnknownIssuer");
    }

    #[test]
    fn non_json_output_is_an_error_not_a_panic() {
        assert!(parse_envelopes("this is not json").is_err());
        assert!(parse_envelopes("").is_err());
        assert!(parse_mailboxes("{\"mailboxes\": 3}").is_err());
    }

    #[test]
    fn unread_counts_only_unseen_envelopes() {
        let mut m = Mail::new();
        assert_eq!(m.unread(), 0);
        m.envelopes = parse_envelopes(ENVELOPES).unwrap();
        assert_eq!(m.unread(), 1);
        assert!(m.status().contains("3 envelopes, 1 unread, ok"));
        m.err = Some("boom".into());
        assert!(m.status().ends_with("error: boom"));
    }

    #[test]
    fn format_size_matches_himalayas_iec_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(900), "900 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(6963), "6.8 KiB"); // 6963 / 1024 = 6.799...
        assert_eq!(format_size(120_730), "117.9 KiB"); // 120730 / 1024 = 117.900...
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn row_columns_present_at_each_breakpoint() {
        let t = Theme::new(ThemeKind::Color);
        let mut e = env(false, "Alice", "Weekly report");
        e.has_attachment = true;
        e.size = 6963;
        for w in [40u16, 79, 80, 100, 120] {
            let s = plain(&row_line(&e, w, t));
            assert!(s.starts_with('*'), "w={w}: {s:?}"); // unseen flag
            assert!(s.contains('!'), "w={w}: {s:?}"); // attachment flag
            assert!(s.contains("Alice"), "w={w}: {s:?}");
            assert!(s.contains("Weekly report") || s.contains('…'), "w={w}: {s:?}");
            if w < NARROW {
                assert!(!s.contains("09-11") && !s.contains("2026-"), "w={w}: date hidden: {s:?}");
                assert!(!s.contains("KiB"), "w={w}: size hidden: {s:?}");
            } else if w < WIDE {
                assert!(s.contains("09-11 08:30"), "w={w}: short date: {s:?}");
                assert!(!s.contains("KiB"), "w={w}: size still hidden: {s:?}");
            } else {
                assert!(s.contains("2026-09-11 08:30"), "w={w}: full date: {s:?}");
                assert!(s.contains("6.8 KiB"), "w={w}: size shown: {s:?}");
            }
        }
    }

    #[test]
    fn header_row_lists_all_columns_when_wide() {
        let t = Theme::new(ThemeKind::Color);
        let s = plain(&header_line(120, t));
        for label in ["FLAGS", "SUBJECT", "FROM", "DATE", "SIZE"] {
            assert!(s.contains(label), "{s:?} missing {label}");
        }
        assert!(!plain(&header_line(40, t)).contains("SIZE"));
    }

    #[test]
    fn row_line_handles_unicode_subjects_at_tiny_and_huge_widths() {
        let t = Theme::new(ThemeKind::Color);
        let e = env(false, "Nyírcsák Zoltán ügyvezető", "Árvíztűrő tükörfúrógép és még sok minden más");
        for w in [0u16, 1, 12, 40, 79, 80, 100, 120] {
            plain(&row_line(&e, w, t)); // must not panic
        }
        assert!(plain(&row_line(&e, 40, t)).contains("Nyírcsák"));
    }

    #[test]
    fn sanitize_drops_escapes_and_controls_but_keeps_newlines() {
        assert_eq!(sanitize("a\x1b[31mb\x07c"), "abc");
        assert_eq!(sanitize("line1\nline2\tend"), "line1\nline2\tend");
        assert_eq!(sanitize("t\x1b]0;pwn\x07x"), "tx");
        assert_eq!(sanitize("a\x1b]8;;http://x\x1b\\b"), "ab");
        assert_eq!(sanitize("a\u{9b}31mb"), "a31mb"); // C1 CSI byte dropped
        assert_eq!(sanitize("héllo"), "héllo");
        assert_eq!(sanitize("a\x1b"), "a");
        // bidi overrides and zero-width characters: can spoof what is shown.
        assert_eq!(sanitize("a\u{202e}b\u{200b}c\u{feff}d\u{2066}e"), "abcde");
    }

    #[test]
    fn body_is_capped_with_a_truncation_marker() {
        let short = "hello".to_string();
        assert_eq!(cap_body(short.clone()), short);
        let long = "a".repeat(MAX_BODY + 1000);
        let capped = cap_body(long);
        assert!(capped.ends_with("… [truncated]"));
        assert!(capped.len() <= MAX_BODY + "… [truncated]".len());
    }

    /// Seen live: the server lists `Inbox`, the config says `INBOX`, and the
    /// exact match fell back to `Deleted Items` (first alphabetically).
    #[test]
    fn list_shows_loading_until_the_fetch_answers_and_empty_afterwards() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let (tx, rx) = mpsc::channel();
        let mut m = Mail::new();
        m.rx = Some(rx);
        m.mailbox = "Inbox".into();
        m.set_mailbox("Sent Items".into());
        assert!(m.loading, "a mailbox switch is a fetch in flight");
        let t = Theme::new(ThemeKind::Color);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).unwrap();
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let screen: String = (0..12u16).map(|y| (0..60u16).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect();
        assert!(screen.contains("LOADING"), "{screen}");

        tx.send(MailEvent::Envelopes("Sent Items".into(), Vec::new())).unwrap();
        m.poll(&ctx);
        assert!(!m.loading);
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let screen: String = (0..12u16).map(|y| (0..60u16).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect();
        assert!(screen.contains("no messages in Sent Items"), "{screen}");
    }

    #[test]
    fn configured_mailbox_matches_the_server_spelling_case_insensitively() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let (tx, rx) = mpsc::channel();
        let mut m = Mail::new();
        m.rx = Some(rx);
        m.mailbox = "INBOX".into();
        tx.send(MailEvent::Mailboxes(vec!["Deleted Items".into(), "Drafts".into(), "Inbox".into()])).unwrap();
        m.poll(&ctx);
        assert_eq!(m.mailbox, "Inbox", "adopts the server's spelling");

        // Unknown configured name: prefer the inbox over the first entry.
        m.mailbox = "Nope".into();
        tx.send(MailEvent::Mailboxes(vec!["Deleted Items".into(), "Inbox".into()])).unwrap();
        m.poll(&ctx);
        assert_eq!(m.mailbox, "Inbox");
    }

    #[test]
    fn poll_drops_events_fetched_for_a_mailbox_or_message_no_longer_current() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let (tx, rx) = mpsc::channel();
        let mut m = Mail::new();
        m.rx = Some(rx);
        m.mailbox = "Archive".into();

        // An INBOX result arriving after the switch to Archive is stale.
        tx.send(MailEvent::Envelopes("INBOX".into(), parse_envelopes(ENVELOPES).unwrap())).unwrap();
        m.poll(&ctx);
        assert!(m.envelopes.is_empty(), "stale-mailbox envelopes must not replace the current list");

        // A fresh Archive result is accepted.
        tx.send(MailEvent::Envelopes("Archive".into(), parse_envelopes(ENVELOPES).unwrap())).unwrap();
        m.poll(&ctx);
        assert_eq!(m.envelopes.len(), 3);

        // A message reply for a ended/different id is ignored.
        m.reader = Some(Reader { env: env(true, "a", "b"), body: None, scroll: 0 }); // id "1"
        tx.send(MailEvent::Message("Archive".into(), "999".into(), Ok(("stale body".into(), 0)))).unwrap();
        m.poll(&ctx);
        assert!(m.reader.as_ref().unwrap().body.is_none(), "reply for a different message must be ignored");

        // The matching mailbox + id is accepted.
        tx.send(MailEvent::Message("Archive".into(), "1".into(), Ok(("real body".into(), 2)))).unwrap();
        m.poll(&ctx);
        assert_eq!(m.reader.as_ref().unwrap().body.as_ref().unwrap().as_ref().unwrap(), &("real body".to_string(), 2));
    }

    #[test]
    fn sanitize_runs_on_parsed_fields() {
        // JSON-escaped ESC and BEL, exactly how a hostile header arrives over the wire.
        let json = r#"{"envelopes":[{"id":"1","subject":"ev\u001b[2Jil","from":[{"name":"A\u0007B"}]}]}"#;
        let v = parse_envelopes(json).unwrap();
        assert_eq!(v[0].subject, "evil");
        assert_eq!(v[0].from, "AB");
    }

    #[test]
    fn mailbox_cycling_wraps_and_is_a_noop_when_empty() {
        let mut m = Mail::new();
        m.cycle_mailbox(1);
        assert_eq!(m.mailbox, "INBOX", "no known mailboxes yet: nothing to cycle");
        m.mailboxes = vec!["INBOX".into(), "Archive".into(), "Sent".into()];
        m.cycle_mailbox(1);
        assert_eq!(m.mailbox, "Archive");
        m.cycle_mailbox(1);
        assert_eq!(m.mailbox, "Sent");
        m.cycle_mailbox(1);
        assert_eq!(m.mailbox, "INBOX");
        m.cycle_mailbox(-1);
        assert_eq!(m.mailbox, "Sent");
    }

    #[test]
    fn selection_stays_in_bounds() {
        let mut m = Mail::new();
        m.move_sel(1);
        assert_eq!(m.sel, 0);
        m.envelopes = parse_envelopes(ENVELOPES).unwrap();
        m.move_sel(10);
        assert_eq!(m.sel, 2);
        m.move_sel(-10);
        assert_eq!(m.sel, 0);
    }

    #[test]
    fn args_are_a_list_with_the_optional_account_and_mailbox() {
        let mut cfg = MailCfg::default();
        assert_eq!(args_envelopes(&cfg, "INBOX"), ["--json", "envelope", "list", "-s", "30", "-m", "INBOX"]);
        assert_eq!(args_mailboxes(&cfg), ["--json", "mailbox", "list"]);
        assert_eq!(args_read(&cfg, "INBOX", "42"), ["--json", "message", "read", "42", "--seen", "-m", "INBOX"]);
        cfg.account = "dev".into();
        cfg.page_size = 0;
        assert_eq!(args_envelopes(&cfg, ""), ["--json", "envelope", "list", "-s", "1", "-a", "dev"]);
    }

    #[test]
    fn enter_opens_the_reader_and_marks_the_row_seen_locally() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut m = Mail::new();
        m.envelopes = parse_envelopes(ENVELOPES).unwrap();
        m.sel = 1;
        assert!(m.on_key(KeyEvent::from(KeyCode::Enter), &ctx));
        assert!(m.envelopes[1].seen);
        assert_eq!(m.unread(), 0);
        let r = m.reader.as_ref().unwrap();
        assert_eq!(r.env.subject, "Reactor status");
        assert!(r.body.is_none());
        assert!(m.help().contains("scroll"));
        assert!(m.on_key(KeyEvent::from(KeyCode::Esc), &ctx));
        assert!(m.reader.is_none());
        assert!(m.help().contains("enter read"));
    }

    #[test]
    fn reader_scroll_is_clamped_to_the_visible_wrapped_rows() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut m = Mail::new();
        m.reader = Some(Reader { env: env(true, "a", "b"), body: Some(Ok(("l1\nl2\nl3".into(), 0))), scroll: 0 });
        m.reader_width.set(80);
        m.reader_height.set(2); // 3 rows of content, only 2 visible -> max scroll 1
        for _ in 0..10 {
            m.on_key(KeyEvent::from(KeyCode::Down), &ctx);
        }
        assert_eq!(m.reader.as_ref().unwrap().scroll, 1);
        for _ in 0..10 {
            m.on_key(KeyEvent::from(KeyCode::Up), &ctx);
        }
        assert_eq!(m.reader.as_ref().unwrap().scroll, 0);
    }

    #[test]
    fn reader_scroll_limit_accounts_for_line_wrapping() {
        // A single long line used to report zero wrapped rows (raw line
        // count == 1), making a wrapped body unreachable past the fold.
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut m = Mail::new();
        m.reader = Some(Reader { env: env(true, "a", "b"), body: Some(Ok(("x".repeat(500), 0))), scroll: 0 });
        m.reader_width.set(40);
        m.reader_height.set(5);
        m.on_key(KeyEvent::from(KeyCode::Down), &ctx);
        assert!(m.reader.as_ref().unwrap().scroll > 0, "a wrapped single-line body must be scrollable");
    }

    #[test]
    fn wrapped_rows_counts_wrap_and_empty_lines() {
        assert_eq!(wrapped_rows("", 40), 1);
        assert_eq!(wrapped_rows("hello", 40), 1);
        assert_eq!(wrapped_rows(&"x".repeat(80), 40), 2);
        assert_eq!(wrapped_rows("a\n\nb", 40), 3);
    }

    #[test]
    fn header_and_overview_react_to_unread() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = Mail::new();
        assert!(m.header(20, t).is_empty(), "nothing to show at zero unread");
        m.envelopes = parse_envelopes(ENVELOPES).unwrap();
        assert_eq!(m.header(20, t).len(), 1);
        assert!(m.header(2, t).is_empty(), "dropped when it does not fit");
        let ov = m.overview(60, 3, t);
        assert_eq!(ov.len(), 3);
        assert!(ov[2].spans.iter().any(|s| s.content.contains("Reactor status")));
        assert_eq!(m.overview(30, 1, t).len(), 1);
    }

    #[test]
    fn draw_does_not_panic_in_any_state_at_tiny_sizes() {
        let t = Theme::new(ThemeKind::Color);
        let states = || {
            let empty = Mail::new();
            let mut errored = Mail::new();
            errored.err = Some(NOT_FOUND.to_string());
            let mut list = Mail::new();
            list.envelopes = parse_envelopes(ENVELOPES).unwrap();
            list.mailboxes = vec!["INBOX".into()];
            list.updated = Some(Local::now());
            list.sel = 2;
            let mut reader = Mail::new();
            reader.reader = Some(Reader {
                env: env(false, "Alice", "Árvíztűrő"),
                body: Some(Ok(("line one\nline two\nline three".into(), 2))),
                scroll: 1,
            });
            let mut loading = Mail::new();
            loading.reader = Some(Reader { env: env(false, "a", "b"), body: None, scroll: 0 });
            let mut failed = Mail::new();
            failed.reader = Some(Reader { env: env(false, "a", "b"), body: Some(Err("nope".into())), scroll: 0 });
            vec![empty, errored, list, reader, loading, failed]
        };
        for (w, h) in [(40u16, 12u16), (1, 1), (120, 40)] {
            for m in states() {
                let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                term.draw(|f| m.draw(f, f.area(), t)).unwrap();
            }
        }
    }

    #[test]
    fn config_defaults_and_interval_floor() {
        let cfg = MailCfg::default();
        assert_eq!(cfg.command, "himalaya");
        assert_eq!(cfg.mailbox, "INBOX");
        assert_eq!((cfg.interval, cfg.page_size), (300, 30));
        assert!(cfg.account.is_empty());
        let table: toml::Table = toml::from_str("[mail]\ninterval = 5\n").unwrap();
        let (parsed, notice) = crate::module::ModuleConfig(table).section::<MailCfg>("mail");
        assert!(notice.is_none());
        assert_eq!(parsed.interval.max(MIN_INTERVAL), MIN_INTERVAL);
        assert_eq!(parsed.command, "himalaya", "missing keys keep their defaults");
    }
}
