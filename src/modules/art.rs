//! ART module: a holotape gallery — ANSI/ASCII artwork from 16colo.rs and
//! the streamed terminal animations of ascii.live.
//!
//! Both sources are plain terminal byte streams, so both are rendered the
//! same way TERM renders its child process: bytes into a `vt100::Parser`,
//! then `ui::widgets::screen_line` turns the emulated screen into ratatui
//! spans on the 16 ANSI colors. The only extra step for a file is decoding
//! CP437 and cutting off its SAUCE record.
//!
//! Nothing is written to disk: an image lives in the parser and is dropped
//! when the next one loads.

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::widgets::{screen_line, truncate};
use chrono::Datelike;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde::Deserialize;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stream_download::http::reqwest;

/// Hard cap on a downloaded artwork; the biggest 16colo.rs `.ans` files are
/// a few hundred kB.
const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Tallest picture we emulate — an ANSI file is one screen row per line.
const MAX_ROWS: usize = 1000;
/// The oldest year 16colo.rs has packs for.
const FIRST_YEAR: i32 = 1990;
/// Consecutive random packs with no renderable art before `random` gives up.
const MAX_RANDOM_RETRIES: u32 = 5;
/// Fallback animation size when the pane hasn't drawn yet (`view` is still
/// `(0, 0)`), so the very first stream chunk still has somewhere to land.
const FALLBACK_ANIM_SIZE: (u16, u16) = (24, 80);
/// Rest time on a file before it is fetched automatically.
const AUTOLOAD_DELAY: Duration = Duration::from_millis(300);
/// A running animation is cancelled when the tab has not been drawn for this
/// long — that is how "the tab went away" reaches the background task.
const IDLE_STOP: Duration = Duration::from_secs(1);
/// Width at which the file/animation list gets its own column.
const LIST_AT: u16 = 100;
const LIST_W: u16 = 30;

const HELP_PICTURES: &str = "r random   ↑/↓ file (auto-loads)   [ ] pack   enter load now   a animations   pgup/pgdn scroll   1-9 tabs   q quit";
const HELP_ANIMATION: &str = "↑/↓ animation   space play/stop   a pictures   1-9 tabs   q quit";

// ---- CP437 and SAUCE -------------------------------------------------------

/// CP437 → Unicode. Index = byte value.
///
/// 0x00–0x1F are the DOS *graphic* glyphs (☺☻♥…), which is what ANSI art
/// expects — except the four control codes an ANSI file actually uses as
/// controls: TAB, LF, CR and ESC. 0x00 is padding, so it becomes a space.
pub const CP437: [char; 256] = [
    ' ', '☺', '☻', '♥', '♦', '♣', '♠', '•', '◘', '\t', '\n', '♂', '♀', '\r', '♫', '☼', '►', '◄', '↕', '‼', '¶',
    '§', '▬', '↨', '↑', '↓', '→', '\u{1b}', '∟', '↔', '▲', '▼', ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')',
    '*', '+', ',', '-', '.', '/', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?',
    '@', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U',
    'V', 'W', 'X', 'Y', 'Z', '[', '\\', ']', '^', '_', '`', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k',
    'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', '{', '|', '}', '~', '', 'Ç', 'ü',
    'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù',
    'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡',
    '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├',
    '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘',
    '┌', '█', '▄', '▌', '▐', '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩',
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}'
];

/// Decode CP437 bytes to a UTF-8 string.
pub fn cp437_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| CP437[b as usize]).collect()
}

/// What a SAUCE record tells us about the artwork.
#[derive(Debug, PartialEq)]
pub struct Sauce {
    /// Character columns (TInfo1); `None` when the record is absent or says 0.
    pub width: Option<u16>,
}

/// Strips a trailing SAUCE record (with its COMNT block and the `\x1a` EOF
/// marker before it) from `data`, returning the artwork bytes and what the
/// record said. Files without SAUCE come back untouched.
pub fn strip_sauce(data: &[u8]) -> (&[u8], Sauce) {
    if data.len() < 128 || &data[data.len() - 128..data.len() - 123] != b"SAUCE" {
        return (data, Sauce { width: None });
    }
    let rec = &data[data.len() - 128..];
    // id[5] version[2] title[35] author[20] group[20] date[8] filesize[4]
    // datatype[1] filetype[1] tinfo1[2] … → tinfo1 at 96, comment count at 104.
    let width = u16::from_le_bytes([rec[96], rec[97]]);
    let comments = rec[104] as usize;
    let mut end = data.len() - 128;
    if comments > 0 {
        let block = 5 + comments * 64;
        if end >= block && &data[end - block..end - block + 5] == b"COMNT" {
            end -= block;
        }
    }
    if end > 0 && data[end - 1] == 0x1a {
        end -= 1;
    }
    (&data[..end], Sauce { width: if width == 0 { None } else { Some(width) } })
}

/// Row count of the `vt100::Parser` that renders the decoded text, capped at
/// `cap`: the line count, or the rows a single long line wraps into at `cols`
/// (one-line ANSI art drives the cursor itself), and never below 2 — vt100's
/// grid underflows on a wrap in a 1-row screen (`grid.rs col_wrap`, panicked
/// live on 2026-09-10).
pub fn line_count(text: &str, cols: u16, cap: usize) -> u16 {
    let lines = text.lines().count().max(text.matches('\n').count() + 1);
    let longest = text.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    let wrapped = longest.div_ceil(cols.max(1) as usize).max(1);
    lines.max(wrapped).clamp(2, cap.max(2)) as u16
}

/// The tty layer's ONLCR: a bare `\n` becomes `\r\n`, so a stream that only
/// sends line feeds (ascii.live, LF-only art files) does not staircase across
/// the screen. `\r\n` is left alone.
pub fn onlcr(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 16);
    let mut prev = 0u8;
    for &b in data {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

/// CP437 art bytes → a `vt100::Parser` holding the whole picture.
pub fn parse_art(data: &[u8], max_rows: usize) -> vt100::Parser {
    let (art, sauce) = strip_sauce(data);
    let text = cp437_to_string(art);
    let cols = sauce.width.unwrap_or(80).clamp(2, 512);
    let rows = line_count(&text, cols, max_rows);
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(&onlcr(text.as_bytes()));
    parser
}

// ---- 16colo.rs API ---------------------------------------------------------

/// Confirmed against the live service: `/v1/year/<year>` lists packs,
/// `/v1/pack/<name>` lists a pack's files, and the bytes of a file are at
/// `https://16colo.rs/pack/<pack>/raw/<FILE>`.
const API: &str = "https://api.16colo.rs/v1";

#[derive(Deserialize)]
struct YearResp {
    results: Vec<YearPack>,
}

#[derive(Deserialize)]
struct YearPack {
    /// A pack called "1991" comes back as the integer 1991 (seen live in
    /// the 1991 and 1992 listings), so accept both.
    #[serde(deserialize_with = "string_or_number")]
    name: String,
}

fn string_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        S(String),
        N(i64),
    }
    Ok(match V::deserialize(d)? {
        V::S(s) => s,
        V::N(n) => n.to_string(),
    })
}

/// Pack names of a year, in API order.
pub fn parse_year(json: &str) -> Result<Vec<String>, String> {
    let resp: YearResp = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(resp.results.into_iter().map(|p| p.name).collect())
}

#[derive(Deserialize)]
struct PackResp {
    results: Vec<PackEntry>,
}

#[derive(Deserialize)]
struct PackEntry {
    /// An object keyed by file name; the values carry metadata we don't need.
    #[serde(default)]
    files: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Renderable artwork files of a pack, sorted; archives and images are dropped.
pub fn parse_pack(json: &str) -> Result<Vec<String>, String> {
    let resp: PackResp = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut names: Vec<String> =
        resp.results.into_iter().flat_map(|e| e.files.into_keys()).filter(|n| is_art_file(n)).collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// `.ans`, `.asc`, `.nfo` and `.diz` are the CP437 text formats we can render.
pub fn is_art_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".ans", ".asc", ".nfo", ".diz"].iter().any(|ext| lower.ends_with(ext))
}

pub fn raw_url(pack: &str, file: &str) -> String {
    format!("https://16colo.rs/pack/{pack}/raw/{file}")
}

/// Clamps a scroll offset so at least one row/column of content stays visible.
pub fn clamp_offset(offset: u16, content: u16, view: u16) -> u16 {
    offset.min(content.saturating_sub(view.max(1)))
}

/// Whether a random walk has re-rolled through enough empty packs that it
/// should stop and report failure instead of trying yet another one.
pub fn random_retries_exhausted(tries: u32) -> bool {
    tries >= MAX_RANDOM_RETRIES
}

/// ponytail: a xorshift on the clock — "surprise me" needs no rand crate, and
/// the calls are separated by network round trips anyway.
fn rand_below(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x % n as u64) as usize
}

// ---- HTTP ------------------------------------------------------------------

fn client(agent: &str) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(agent)
        .build()
        .map_err(|e| e.to_string())
}

/// GET with a hard byte cap: the body is read chunk by chunk and the transfer
/// is abandoned as soon as `cap` is reached.
async fn get_capped(url: &str, cap: usize) -> Result<Vec<u8>, String> {
    let client = client("PipBoyCRT")?;
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    let mut resp = resp.error_for_status().map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        buf.extend_from_slice(&chunk);
        if buf.len() >= cap {
            buf.truncate(cap);
            break;
        }
    }
    Ok(buf)
}

// ---- state -----------------------------------------------------------------

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ArtCfg {
    pub animations: Vec<String>,
}

impl Default for ArtCfg {
    fn default() -> Self {
        Self {
            animations: ["parrot", "nyan", "donut", "dvd", "batman", "forrest", "knot", "coin", "playstation", "spidyswing"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pictures,
    Animation,
}

#[derive(PartialEq, Eq)]
enum Load {
    Idle,
    Loading,
    Error,
}

/// What a background task sends back to `poll`.
enum Ev {
    /// Pack list of a year; `pick` = continue a random walk.
    Packs { year: i32, packs: Vec<String>, pick: bool },
    /// File list of a pack; `pick` = continue a random walk.
    Files { pack: String, files: Vec<String>, pick: bool },
    Image { pack: String, file: String, data: Vec<u8> },
    /// One chunk of an ascii.live stream.
    Frame(Vec<u8>),
    /// The stream ended or failed on its own.
    Stopped,
    Error(String),
}

pub struct Art {
    cfg: ArtCfg,
    mode: Mode,
    // 16colo.rs browsing state
    year: i32,
    packs: Vec<String>,
    pack_sel: usize,
    files: Vec<String>,
    file_sel: usize,
    /// Pack lists already fetched, keyed by year (memory only).
    cache: HashMap<i32, Vec<String>>,
    /// Consecutive random packs in a row with no renderable art; capped by
    /// `random_retries_exhausted` so an unlucky streak can't spin forever.
    random_tries: u32,
    /// The picture on screen: `(pack, file)` and its emulated screen.
    shown: Option<(String, String)>,
    picture: Option<vt100::Parser>,
    // ascii.live state
    anim_sel: usize,
    anim: Option<vt100::Parser>,
    cancel: Option<Arc<AtomicBool>>,
    /// Size the animation parser was built for; a resized pane rebuilds it.
    anim_size: (u16, u16),
    // shared
    load: Load,
    /// Set by ↑/↓ in pictures mode: the selected file loads on its own once
    /// the selection has rested for `AUTOLOAD_DELAY` (no Enter needed, and a
    /// fast scroll does not fetch every file on the way).
    autoload_at: Option<Instant>,
    err: Option<String>,
    scroll: (u16, u16),
    /// Content size of what `draw` last showed, for clamping the offsets.
    content: Cell<(u16, u16)>,
    /// `(height, width)` of the body area `draw` last used, for paging and
    /// for deciding whether ←/→ scroll or switch tabs.
    view: Cell<(u16, u16)>,
    /// When `draw` last ran — an animation whose tab went away stops itself.
    seen: Cell<Instant>,
    tx: Option<Sender<Ev>>,
    rx: Option<Receiver<Ev>>,
}

impl Art {
    pub fn new() -> Self {
        Self {
            cfg: ArtCfg::default(),
            mode: Mode::Pictures,
            year: FIRST_YEAR,
            packs: Vec::new(),
            pack_sel: 0,
            files: Vec::new(),
            file_sel: 0,
            cache: HashMap::new(),
            random_tries: 0,
            shown: None,
            picture: None,
            anim_sel: 0,
            anim: None,
            cancel: None,
            anim_size: (0, 0),
            load: Load::Idle,
            autoload_at: None,
            err: None,
            scroll: (0, 0),
            content: Cell::new((0, 0)),
            view: Cell::new((10, 80)),
            seen: Cell::new(Instant::now()),
            tx: None,
            rx: None,
        }
    }

    fn name(&self) -> String {
        match self.mode {
            Mode::Pictures => match &self.shown {
                Some((pack, file)) => format!("{pack}/{file}"),
                None => self.packs.get(self.pack_sel).cloned().unwrap_or_else(|| "—".to_string()),
            },
            Mode::Animation => self.cfg.animations.get(self.anim_sel).cloned().unwrap_or_else(|| "—".to_string()),
        }
    }

    fn fail(&mut self, msg: String) {
        self.err = Some(msg);
        self.load = Load::Error;
    }

    /// Fetches the pack list of `year` (from the cache when possible).
    fn want_packs(&mut self, ctx: &Ctx, year: i32, pick: bool) {
        if let Some(packs) = self.cache.get(&year) {
            let packs = packs.clone();
            self.apply_packs(ctx, year, packs, pick);
            return;
        }
        let Some(tx) = self.tx.clone() else { return };
        self.load = Load::Loading;
        let url = format!("{API}/year/{year}?pagesize=500");
        ctx.rt.spawn(async move {
            let ev = match get_capped(&url, MAX_BYTES).await {
                Ok(body) => match parse_year(&String::from_utf8_lossy(&body)) {
                    Ok(packs) => Ev::Packs { year, packs, pick },
                    Err(e) => Ev::Error(e),
                },
                Err(e) => Ev::Error(e),
            };
            let _ = tx.send(ev);
        });
    }

    fn apply_packs(&mut self, ctx: &Ctx, year: i32, packs: Vec<String>, pick: bool) {
        if packs.is_empty() {
            self.fail(format!("no packs for {year}"));
            return;
        }
        self.cache.insert(year, packs.clone());
        self.year = year;
        self.pack_sel = if pick { rand_below(packs.len()) } else { self.pack_sel.min(packs.len() - 1) };
        self.packs = packs;
        self.want_files(ctx, pick);
    }

    /// Fetches the file list of the selected pack.
    fn want_files(&mut self, ctx: &Ctx, pick: bool) {
        let Some(pack) = self.packs.get(self.pack_sel).cloned() else { return };
        let Some(tx) = self.tx.clone() else { return };
        self.load = Load::Loading;
        self.files.clear();
        self.file_sel = 0;
        let url = format!("{API}/pack/{pack}");
        ctx.rt.spawn(async move {
            let ev = match get_capped(&url, MAX_BYTES).await {
                Ok(body) => match parse_pack(&String::from_utf8_lossy(&body)) {
                    Ok(files) => Ev::Files { pack, files, pick },
                    Err(e) => Ev::Error(e),
                },
                Err(e) => Ev::Error(e),
            };
            let _ = tx.send(ev);
        });
    }

    /// Downloads the selected file and shows it.
    fn want_image(&mut self, ctx: &Ctx) {
        let (Some(pack), Some(file)) = (self.packs.get(self.pack_sel).cloned(), self.files.get(self.file_sel).cloned())
        else {
            return;
        };
        let Some(tx) = self.tx.clone() else { return };
        self.load = Load::Loading;
        self.err = None;
        let url = raw_url(&pack, &file);
        ctx.rt.spawn(async move {
            let ev = match get_capped(&url, MAX_BYTES).await {
                Ok(data) => Ev::Image { pack, file, data },
                Err(e) => Ev::Error(e),
            };
            let _ = tx.send(ev);
        });
    }

    /// Random walk: a random year → a random pack → a random file.
    fn random(&mut self, ctx: &Ctx) {
        let last = chrono::Local::now().year();
        let year = FIRST_YEAR + rand_below((last - FIRST_YEAR + 1).max(1) as usize) as i32;
        self.err = None;
        self.want_packs(ctx, year, true);
    }

    // ---- ascii.live --------------------------------------------------------

    fn stop_stream(&mut self) {
        if let Some(flag) = self.cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }

    fn start_stream(&mut self, ctx: &Ctx) {
        self.stop_stream();
        let Some(name) = self.cfg.animations.get(self.anim_sel).cloned() else { return };
        let Some(tx) = self.tx.clone() else { return };
        let flag = Arc::new(AtomicBool::new(false));
        self.cancel = Some(flag.clone());
        self.anim = None;
        self.load = Load::Loading;
        self.err = None;
        self.seen.set(Instant::now());
        ctx.rt.spawn(async move {
            // ascii.live only serves the animation to a terminal client, so
            // the User-Agent has to look like curl's.
            let client = match client("curl/8.6.0 PipBoyCRT") {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Ev::Error(e));
                    return;
                }
            };
            let resp = match client.get(format!("https://ascii.live/{name}")).send().await.and_then(|r| r.error_for_status())
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Ev::Error(e.to_string()));
                    return;
                }
            };
            let mut resp = resp;
            loop {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        if tx.send(Ev::Frame(chunk.to_vec())).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Ev::Error(e.to_string()));
                        return;
                    }
                }
            }
            let _ = tx.send(Ev::Stopped);
        });
    }

    fn running(&self) -> bool {
        self.cancel.is_some()
    }

    // ---- drawing helpers ---------------------------------------------------

    fn title_line(&self, t: Theme) -> Line<'static> {
        let mut spans = vec![Span::styled(" ART ", t.title)];
        match self.mode {
            Mode::Pictures => spans.push(Span::styled(self.name(), t.value)),
            Mode::Animation => {
                spans.push(Span::styled("animation ", t.text));
                spans.push(Span::styled(self.name(), t.value));
            }
        }
        match self.load {
            Load::Loading => spans.push(Span::styled("  loading…", t.warn)),
            Load::Error => spans.push(Span::styled(
                format!("  {}", truncate(self.err.as_deref().unwrap_or("failed"), 60)),
                t.danger,
            )),
            Load::Idle => {}
        }
        Line::from(spans)
    }

    /// The list column: pack files, or the animation names.
    fn list_lines(&self, height: u16, t: Theme) -> Vec<Line<'static>> {
        let (items, sel): (&[String], usize) = match self.mode {
            Mode::Pictures => (&self.files, self.file_sel),
            Mode::Animation => (&self.cfg.animations, self.anim_sel),
        };
        let h = height.max(1) as usize;
        let start = sel.saturating_sub(h / 2).min(items.len().saturating_sub(h));
        let mut lines = Vec::with_capacity(h);
        if self.mode == Mode::Pictures {
            let pack = self.packs.get(self.pack_sel).map(String::as_str).unwrap_or("—");
            lines.push(Line::from(Span::styled(
                format!(" {} {}", self.year, truncate(pack, LIST_W as usize - 8)),
                t.title,
            )));
        }
        for (i, item) in items.iter().enumerate().skip(start).take(h.saturating_sub(lines.len())) {
            let mark = if i == sel { "▶ " } else { "  " };
            let style = if i == sel { t.value } else { t.text };
            lines.push(Line::from(Span::styled(
                format!("{mark}{}", truncate(item, LIST_W as usize - 3)),
                style,
            )));
        }
        if items.is_empty() {
            lines.push(Line::from(Span::styled("  (empty)", t.frame)));
        }
        lines
    }

    /// The visible rows of the emulated screen, honouring the vertical offset.
    fn screen_lines(&self, screen: &vt100::Screen, area: Rect, t: Theme) -> Vec<Line<'static>> {
        let (rows, cols) = screen.size();
        self.content.set((rows, cols));
        let top = clamp_offset(self.scroll.0, rows, area.height);
        (top..rows.min(top.saturating_add(area.height))).map(|r| screen_line(screen, r, cols, t)).collect()
    }
}

impl Module for Art {
    fn id(&self) -> &'static str {
        "art"
    }
    fn title(&self) -> &'static str {
        "ART"
    }
    fn describe(&self) -> &'static str {
        "ANSI art from 16colo.rs and ascii.live animations"
    }
    fn help(&self) -> &'static str {
        match self.mode {
            Mode::Pictures => HELP_PICTURES,
            Mode::Animation => HELP_ANIMATION,
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<ArtCfg>(self.id());
        self.cfg = cfg;
        if self.cfg.animations.is_empty() {
            self.cfg.animations = ArtCfg::default().animations;
        }
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        let (tx, rx) = mpsc::channel();
        self.tx = Some(tx);
        self.rx = Some(rx);
        // One random picture up front, so the tab is never empty.
        self.random(ctx);
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        if self.autoload_at.is_some_and(|at| at.elapsed() >= AUTOLOAD_DELAY) {
            self.autoload_at = None;
            self.want_image(ctx);
        }
        let mut n = 0;
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            n += 1;
            match ev {
                Ev::Packs { year, packs, pick } => self.apply_packs(ctx, year, packs, pick),
                Ev::Files { pack, files, pick } => {
                    if self.packs.get(self.pack_sel).map(String::as_str) != Some(pack.as_str()) {
                        continue; // the user moved on while this was in flight
                    }
                    if files.is_empty() {
                        self.load = Load::Idle;
                        self.files.clear();
                        if pick {
                            // this pack has no renderable art — re-roll, but
                            // only up to a point.
                            self.random_tries += 1;
                            if random_retries_exhausted(self.random_tries) {
                                self.random_tries = 0;
                                self.fail("no art found in 5 random packs — press r again".to_string());
                            } else {
                                self.random(ctx);
                            }
                        }
                        continue;
                    }
                    self.random_tries = 0;
                    self.file_sel = if pick { rand_below(files.len()) } else { 0 };
                    self.files = files;
                    self.load = Load::Idle;
                    if pick {
                        self.want_image(ctx);
                    }
                }
                Ev::Image { pack, file, data } => {
                    self.picture = Some(parse_art(&data, MAX_ROWS));
                    self.shown = Some((pack, file));
                    self.scroll = (0, 0);
                    self.load = Load::Idle;
                    self.err = None;
                }
                Ev::Frame(chunk) => {
                    let (rows, cols) = self.view.get();
                    // vt100 cannot take a 1-row/1-col grid (wrap underflow).
                    let size = if rows < 2 || cols < 2 { FALLBACK_ANIM_SIZE } else { (rows, cols) };
                    if self.anim.is_none() || self.anim_size != size {
                        self.anim = Some(vt100::Parser::new(size.0, size.1, 0));
                        self.anim_size = size;
                    }
                    let parser = self.anim.as_mut().expect("just built above");
                    parser.process(&onlcr(&chunk));
                    self.load = Load::Idle;
                }
                Ev::Stopped => {
                    self.cancel = None;
                    self.load = Load::Idle;
                }
                Ev::Error(msg) => self.fail(msg),
            }
        }
        // An animation only makes sense while its tab is on screen.
        if self.running() && self.seen.get().elapsed() > IDLE_STOP {
            self.stop_stream();
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        let page = self.view.get().0.max(1);
        match (self.mode, key.code) {
            (_, KeyCode::Char('a')) => {
                self.stop_stream();
                self.mode = if self.mode == Mode::Pictures { Mode::Animation } else { Mode::Pictures };
                self.load = Load::Idle;
                self.err = None;
                true
            }
            (Mode::Animation, KeyCode::Char(' ')) => {
                if self.running() {
                    self.stop_stream();
                } else {
                    self.start_stream(ctx);
                }
                true
            }
            (Mode::Animation, KeyCode::Up | KeyCode::Down) => {
                let n = self.cfg.animations.len();
                if n > 0 {
                    let up = key.code == KeyCode::Up;
                    self.anim_sel = if up { (self.anim_sel + n - 1) % n } else { (self.anim_sel + 1) % n };
                    if self.running() {
                        self.start_stream(ctx);
                    }
                }
                true
            }
            (Mode::Pictures, KeyCode::Char('r')) => {
                self.random_tries = 0;
                self.random(ctx);
                true
            }
            (Mode::Pictures, KeyCode::Up | KeyCode::Down) => {
                let n = self.files.len();
                if n > 0 {
                    let up = key.code == KeyCode::Up;
                    self.file_sel = if up { (self.file_sel + n - 1) % n } else { (self.file_sel + 1) % n };
                    self.autoload_at = Some(Instant::now());
                }
                true
            }
            (Mode::Pictures, KeyCode::Char('[') | KeyCode::Char(']')) => {
                let n = self.packs.len();
                if n > 0 {
                    let back = key.code == KeyCode::Char('[');
                    self.pack_sel = if back { (self.pack_sel + n - 1) % n } else { (self.pack_sel + 1) % n };
                    self.want_files(ctx, false);
                }
                true
            }
            (Mode::Pictures, KeyCode::Enter) => {
                self.autoload_at = None;
                self.want_image(ctx);
                true
            }
            (Mode::Pictures, KeyCode::PageUp) => {
                self.scroll.0 = self.scroll.0.saturating_sub(page);
                true
            }
            (Mode::Pictures, KeyCode::PageDown) => {
                self.scroll.0 = clamp_offset(self.scroll.0.saturating_add(page), self.content.get().0, page);
                true
            }
            // ←/→ belong to the shell (tab switching); they only scroll while
            // the picture really is wider than the pane.
            (Mode::Pictures, KeyCode::Left) if self.scroll.1 > 0 => {
                self.scroll.1 = self.scroll.1.saturating_sub(8);
                true
            }
            (Mode::Pictures, KeyCode::Right) if self.content.get().1 > self.view.get().1 => {
                self.scroll.1 = clamp_offset(self.scroll.1.saturating_add(8), self.content.get().1, self.view.get().1);
                true
            }
            _ => false,
        }
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        self.seen.set(Instant::now());
        if area.width == 0 || area.height == 0 {
            return;
        }
        let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        f.render_widget(Paragraph::new(self.title_line(t)), head);
        if body.height == 0 {
            return;
        }
        let (list, pic) = if body.width >= LIST_AT {
            let [l, p] = Layout::horizontal([Constraint::Length(LIST_W), Constraint::Min(0)]).areas(body);
            (Some(l), p)
        } else {
            (None, body)
        };
        if let Some(list) = list {
            f.render_widget(Paragraph::new(self.list_lines(list.height, t)), list);
        }
        self.view.set((pic.height.max(1), pic.width.max(1)));
        let screen = match self.mode {
            Mode::Pictures => self.picture.as_ref().map(|p| p.screen()),
            Mode::Animation => self.anim.as_ref().map(|p| p.screen()),
        };
        let Some(screen) = screen else {
            let hint = match (self.mode, &self.load) {
                (_, Load::Loading) => "",
                (Mode::Animation, _) => " press space to play",
                (Mode::Pictures, _) => " press r for a random picture",
            };
            f.render_widget(Paragraph::new(Line::from(Span::styled(hint, t.frame))), pic);
            return;
        };
        let lines = self.screen_lines(screen, pic, t);
        let left = clamp_offset(self.scroll.1, self.content.get().1, pic.width);
        f.render_widget(Paragraph::new(lines).scroll((0, left)), pic);
    }

    fn overview(&self, width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(" ART", t.title)),
            Line::from(Span::styled(
                format!("  {}", truncate(&self.name(), width.saturating_sub(3).max(1) as usize)),
                t.value,
            )),
        ]
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(6)
    }

    fn wants_fast_frames(&self, active: bool) -> bool {
        active && self.running()
    }

    fn status(&self) -> String {
        let mode = match self.mode {
            Mode::Pictures => "pictures",
            Mode::Animation => "animation",
        };
        let load = match self.load {
            Load::Idle => "loaded",
            Load::Loading => "loading",
            Load::Error => "error",
        };
        format!("art {mode} {} {load}", self.name())
    }
}

impl Drop for Art {
    fn drop(&mut self) {
        self.stop_stream();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// ascii.live frames use bare `\n`; without ONLCR every line starts
    /// where the previous one ended (seen live as a staircase, 2026-09-10).
    #[test]
    fn bare_line_feeds_get_a_carriage_return_but_crlf_stays() {
        assert_eq!(onlcr(b"a\nb"), b"a\r\nb");
        assert_eq!(onlcr(b"a\r\nb"), b"a\r\nb");
        assert_eq!(onlcr(b"\n\n"), b"\r\n\r\n");
        let mut p = vt100::Parser::new(3, 10, 0);
        p.process(&onlcr(b"ab\ncd"));
        assert_eq!(p.screen().contents(), "ab\ncd", "second line starts at column 0");
    }

    #[test]
    fn cp437_maps_blocks_and_keeps_controls() {
        assert_eq!(CP437[0xB0], '░');
        assert_eq!(CP437[0xB1], '▒');
        assert_eq!(CP437[0xDB], '█');
        assert_eq!(CP437[0x01], '☺');
        assert_eq!(CP437[0x1B], '\u{1b}', "ESC stays ESC so the art keeps its colors");
        assert_eq!(CP437[0x0A], '\n');
        assert_eq!(CP437[0x0D], '\r');
        assert_eq!(CP437[0x09], '\t');
        assert_eq!(CP437[0x00], ' ');
        assert_eq!(CP437[0xE1], 'ß');
        assert_eq!(CP437[0xFE], '■');
        assert_eq!(cp437_to_string(&[0xDB, 0xB0, b'A']), "█░A");
    }

    fn sauce_record(width: u16, comments: u8) -> Vec<u8> {
        let mut r = vec![0u8; 128];
        r[..5].copy_from_slice(b"SAUCE");
        r[5..7].copy_from_slice(b"00");
        r[96..98].copy_from_slice(&width.to_le_bytes());
        r[104] = comments;
        r
    }

    #[test]
    fn strip_sauce_removes_record_and_reads_width() {
        let mut data = b"ART\x1a".to_vec();
        data.extend_from_slice(&sauce_record(132, 0));
        let (art, s) = strip_sauce(&data);
        assert_eq!(art, b"ART");
        assert_eq!(s.width, Some(132));
    }

    #[test]
    fn strip_sauce_without_record_is_identity() {
        let (art, s) = strip_sauce(b"just art");
        assert_eq!(art, b"just art");
        assert_eq!(s.width, None);
        // TInfo1 = 0 means "unknown", not zero columns.
        let mut data = b"ART".to_vec();
        data.extend_from_slice(&sauce_record(0, 0));
        assert_eq!(strip_sauce(&data).1.width, None);
    }

    #[test]
    fn strip_sauce_drops_the_comment_block() {
        let mut data = b"ART\x1a".to_vec();
        data.extend_from_slice(b"COMNT");
        data.extend_from_slice(&[b' '; 128]); // two 64-byte comment lines
        data.extend_from_slice(&sauce_record(80, 2));
        assert_eq!(strip_sauce(&data).0, b"ART");
    }

    #[test]
    fn ans_fixture_becomes_colored_cells() {
        let parser = parse_art(b"\x1b[31mA\x1b[0m B", MAX_ROWS);
        let screen = parser.screen();
        assert_eq!(screen.size(), (2, 80), "no SAUCE → 80 columns; one line still gets the 2-row minimum");
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "A");
        assert_eq!(screen.cell(0, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
        assert_eq!(screen.cell(0, 2).unwrap().contents(), "B");
        assert_eq!(screen.cell(0, 2).unwrap().fgcolor(), vt100::Color::Default);
        let t = Theme::new(crate::config::ThemeKind::Color);
        let line = screen_line(screen, 0, 80, t);
        assert_eq!(line.spans[0].style.fg, Some(ratatui::style::Color::Indexed(1)));
    }

    #[test]
    fn sauce_width_sizes_the_screen() {
        let mut data = b"\x1b[32mhi\x1a".to_vec();
        data.extend_from_slice(&sauce_record(132, 0));
        assert_eq!(parse_art(&data, MAX_ROWS).screen().size(), (2, 132));
    }

    #[test]
    fn parser_height_follows_the_line_count() {
        assert_eq!(line_count("a\nb\nc", 80, 100), 3);
        assert_eq!(line_count("", 80, 100), 2, "never below 2 rows");
        assert_eq!(line_count(&"x\n".repeat(5000), 80, 1000), 1000, "capped");
        assert_eq!(line_count(&"x".repeat(200), 80, 100), 3, "one long line wraps into 3 rows");
        // Regression: a one-line picture wider than the screen used to build a
        // 1-row grid and panic inside vt100 on the first wrap.
        let p = parse_art("x".repeat(300).as_bytes(), 1000);
        assert!(p.screen().size().0 >= 2);
        assert_eq!(parse_art(b"one\r\ntwo\r\nthree", MAX_ROWS).screen().size().0, 3);
    }

    const YEAR_JSON: &str = r#"{"page":{"total":2},"results":[
        {"year":1996,"name":"acdu0196","groups":["acid"]},
        {"year":1996,"name":"twi-9703","groups":["twilight"]}]}"#;

    const PACK_JSON: &str = r#"{"page":{},"results":[{"year":1996,"files":{
        "ACID0196.ANS":{"file":{"raw":"ACID0196.ANS"}},
        "ACIDVIEW.EXE":{"file":{"raw":"ACIDVIEW.EXE"}},
        "ACID-TEE.GIF":{"file":{"raw":"ACID-TEE.GIF"}},
        "ACDU0196.NFO":{"file":{"raw":"ACDU0196.NFO"}}}}]}"#;

    #[test]
    fn api_json_parses() {
        assert_eq!(parse_year(YEAR_JSON).unwrap(), vec!["acdu0196", "twi-9703"]);
        // A numeric pack name (the real 1991 listing) must not fail the whole year.
        let numeric = r#"{"page":{},"results":[{"year":1991,"name":1991},{"year":1991,"name":"acid-91"}]}"#;
        assert_eq!(parse_year(numeric).unwrap(), vec!["1991", "acid-91"]);
        assert_eq!(parse_pack(PACK_JSON).unwrap(), vec!["ACDU0196.NFO", "ACID0196.ANS"]);
        assert!(parse_year("not json").is_err());
        assert!(parse_pack(r#"{"results":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn art_files_and_raw_url() {
        assert!(is_art_file("X.ans") && is_art_file("x.NFO") && is_art_file("a.diz"));
        assert!(!is_art_file("x.gif") && !is_art_file("x.exe") && !is_art_file("ans"));
        assert_eq!(raw_url("acdu0196", "ACID0196.ANS"), "https://16colo.rs/pack/acdu0196/raw/ACID0196.ANS");
    }

    #[test]
    fn scroll_offsets_clamp() {
        assert_eq!(clamp_offset(50, 100, 20), 50);
        assert_eq!(clamp_offset(95, 100, 20), 80);
        assert_eq!(clamp_offset(5, 10, 40), 0, "content smaller than the view");
        assert_eq!(clamp_offset(5, 0, 0), 0);
    }

    #[test]
    fn random_stays_in_range() {
        for n in [1usize, 2, 7, 500] {
            assert!(rand_below(n) < n);
        }
        assert_eq!(rand_below(0), 0);
    }

    #[test]
    fn random_retry_limit_stops_at_five() {
        for tries in 0..5 {
            assert!(!random_retries_exhausted(tries), "try {tries} should still re-roll");
        }
        assert!(random_retries_exhausted(5));
        assert!(random_retries_exhausted(6));
    }

    #[test]
    fn frame_builds_the_parser_lazily_and_on_resize() {
        let mut art = Art::new();
        let (tx, rx) = mpsc::channel();
        art.tx = Some(tx.clone());
        art.rx = Some(rx);
        let (ctx, _notices) = crate::shell::test_ctx(toml::Table::new());

        // No pane size drawn yet (a fresh pane records (0, 0)) → falls back
        // to a sane default instead of building a zero-sized parser.
        art.view.set((0, 0));
        tx.send(Ev::Frame(b"\x1b[2J\x1b[Hhello".to_vec())).unwrap();
        art.poll(&ctx);
        let parser = art.anim.as_ref().expect("Ev::Frame must build the parser");
        assert_eq!(parser.screen().size(), FALLBACK_ANIM_SIZE);
        assert_eq!(art.anim_size, FALLBACK_ANIM_SIZE);
        assert!(parser.screen().contents().starts_with("hello"));
        assert!(art.load == Load::Idle, "a delivered frame must clear Loading");

        // A second chunk at the same size keeps appending to the same parser.
        tx.send(Ev::Frame(b" world".to_vec())).unwrap();
        art.poll(&ctx);
        assert!(art.anim.as_ref().unwrap().screen().contents().starts_with("hello world"));

        // The pane resized (as `draw` would record) → the parser is rebuilt.
        art.view.set((10, 40));
        tx.send(Ev::Frame(b"\x1b[2J\x1b[Hresized".to_vec())).unwrap();
        art.poll(&ctx);
        let parser = art.anim.as_ref().unwrap();
        assert_eq!(parser.screen().size(), (10, 40));
        assert_eq!(art.anim_size, (10, 40));
        assert!(parser.screen().contents().starts_with("resized"));
    }

    fn draw_at(art: &Art, w: u16, h: u16) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term
            .draw(|f| {
                let area = f.area();
                art.draw(f, area, Theme::new(crate::config::ThemeKind::Color));
            })
            .unwrap();
    }

    #[test]
    fn draws_without_data_on_tiny_areas() {
        let mut art = Art::new();
        draw_at(&art, 40, 12);
        draw_at(&art, 1, 1);
        draw_at(&art, 120, 40); // wide enough for the list column
        art.mode = Mode::Animation;
        draw_at(&art, 40, 12);
        draw_at(&art, 1, 1);
        art.mode = Mode::Pictures;
        art.picture = Some(parse_art(b"\x1b[31mhello\r\nworld", MAX_ROWS));
        art.shown = Some(("acdu0196".into(), "ACID0196.ANS".into()));
        art.scroll = (500, 500); // clamped, not a panic
        draw_at(&art, 40, 12);
        draw_at(&art, 120, 40);
        assert_eq!(art.status(), "art pictures acdu0196/ACID0196.ANS loaded");
        assert_eq!(art.overview(20, 2, Theme::new(crate::config::ThemeKind::Mono)).len(), 2);
    }
}
