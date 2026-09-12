//! QUEST module: a gamebook reader/player on a holotape.
//!
//! Two kinds of book:
//!
//! * **Lone Wolf** by Joe Dever, Internet Edition by [Project Aon]. The app
//!   ships **no book text**: the Project Aon licence allows a personal copy
//!   but not redistribution, so the user presses `d` and the pages are
//!   downloaded from the official site into `vault/quests/lw/<code>/`.
//! * **Your own** gamebooks in a tiny plain-text format
//!   (`vault/quests/<name>.txt`), documented in the README.
//!
//! Both parse into the same [`Book`] model, so the play view, the adventure
//! sheet and the combat engine do not care where a section came from.
//!
//! [Project Aon]: https://www.projectaon.org

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use stream_download::http::reqwest;

/// Shown once, the first time a book is downloaded.
const LICENSE_NOTICE: &str = "Lone Wolf \u{a9} Joe Dever, Internet Edition by Project Aon \u{b7} personal copy only \u{b7} https://www.projectaon.org/en/Main/License";

const BASE_URL: &str = "https://www.projectaon.org/en/xhtml/lw";
const USER_AGENT: &str = "PipBoyCRT";
const CONCURRENCY: usize = 20;
const PAGE_TIMEOUT: u64 = 15;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_HISTORY: usize = 50;
const DICE_SPIN: Duration = Duration::from_millis(600);

/// The hint that lives at the bottom of the Action Chart panel. Two lines
/// because the chart column is 30 cells wide.
const SHEET_HINT: &str = " tab next \u{b7} +/- change \u{b7} i add item\n x remove selected \u{b7} a hide";

/// Shown once at the start of a new game: the Pip-Boy is the dice, the player
/// is the chart.
const INTRO: [&str; 6] = [
    "This is a paper gamebook on a holotape.",
    "The book tells you what happens; you keep the",
    "Action Chart yourself: tab picks a field, +/-",
    "changes it, i adds an item, x removes one (a",
    "hides or shows the chart). The Pip-Boy only rolls",
    "the dice and runs the combat. Every move is saved.",
];

/// The caption above the spinning digit.
const DICE_TITLE: &str = "Random Number Table";

/// The same reminder, once per session, on the first section you open.
const CHART_REMINDER: &str = "\u{25b6} Remember: you keep the chart yourself \u{2014} tab, +/-, i, x edit it";

/// The Lone Wolf books the library knows out of the box: title, Project Aon
/// code, number of numbered sections. Verified against
/// `https://www.projectaon.org/en/xhtml/lw/<code>/sect<N>.htm` (N+1 is 404).
/// More can be added with `[quest] books = ["Title|code|sections", ...]`.
const BOOKS: [(&str, &str, u32); 5] = [
    ("Flight from the Dark", "01fftd", 350),
    ("Fire on the Water", "02fotw", 350),
    ("The Caverns of Kalte", "03tcok", 350),
    ("The Chasm of Doom", "04tcod", 350),
    ("Shadow on the Sand", "05sots", 400),
];

/// Frontmatter pages fetched with a book: `(file stem, label)`.
const FRONT: [(&str, &str); 10] = [
    ("tssf", "The Story So Far"),
    ("gamerulz", "The Game Rules"),
    ("discplnz", "Kai Disciplines"),
    ("equipmnt", "Equipment"),
    ("cmbtrulz", "Combat Rules"),
    ("crtable", "Combat Results Table"),
    ("random", "Random Number Table"),
    ("action", "Action Chart"),
    ("map", "Map"),
    ("toc", "Table of Contents"),
];

/// Endurance loss that means "killed outright" in the Combat Results Table.
pub const KILL: u8 = 255;

/// The official Lone Wolf **Combat Results Table**, a rules mechanic (not book
/// text), transcribed from `crtneg.png` + `crtpos.png` of `crtable.htm`.
///
/// Indexed `CRT[random number 0..=9][combat-ratio column]`; each entry is
/// `(ENDURANCE lost by the enemy, ENDURANCE lost by Lone Wolf)`, with [`KILL`]
/// for the table's `K`. The 13 columns are the ratio buckets
/// `<=-11, -10/-9, -8/-7, -6/-5, -4/-3, -2/-1, 0, +1/+2, +3/+4, +5/+6, +7/+8, +9/+10, >=+11`
/// (see [`crt_col`]). The `0` column appears on both halves of the printed
/// table and agrees, which is the cross-check that the two images line up.
#[rustfmt::skip]
pub const CRT: [[(u8, u8); 13]; 10] = [
    // random number 0
    [(6,0),(7,0),(8,0),(9,0),(10,0),(11,0),(12,0),(14,0),(16,0),(18,0),(KILL,0),(KILL,0),(KILL,0)],
    // 1
    [(0,KILL),(0,KILL),(0,8),(0,6),(1,6),(2,5),(3,5),(4,5),(5,4),(6,4),(7,4),(8,3),(9,3)],
    // 2
    [(0,KILL),(0,8),(0,7),(1,6),(2,5),(3,5),(4,4),(5,4),(6,3),(7,3),(8,3),(9,3),(10,2)],
    // 3
    [(0,8),(0,7),(1,6),(2,5),(3,5),(4,4),(5,4),(6,3),(7,3),(8,3),(9,2),(10,2),(11,2)],
    // 4
    [(0,8),(1,7),(2,6),(3,5),(4,4),(5,4),(6,3),(7,3),(8,2),(9,2),(10,2),(11,2),(12,2)],
    // 5
    [(1,7),(2,6),(3,5),(4,4),(5,4),(6,3),(7,2),(8,2),(9,2),(10,2),(11,2),(12,2),(14,1)],
    // 6
    [(2,6),(3,6),(4,5),(5,4),(6,3),(7,2),(8,2),(9,2),(10,2),(11,1),(12,1),(14,1),(16,1)],
    // 7
    [(3,5),(4,5),(5,4),(6,3),(7,2),(8,2),(9,1),(10,1),(11,1),(12,0),(14,0),(16,0),(18,0)],
    // 8
    [(4,4),(5,4),(6,3),(7,2),(8,1),(9,1),(10,0),(11,0),(12,0),(14,0),(16,0),(18,0),(KILL,0)],
    // 9
    [(5,3),(6,3),(7,2),(8,0),(9,0),(10,0),(11,0),(12,0),(14,0),(16,0),(18,0),(KILL,0),(KILL,0)],
];

/// Combat Ratio (your COMBAT SKILL minus the enemy's) to a [`CRT`] column.
pub fn crt_col(ratio: i32) -> usize {
    match ratio {
        i if i <= -11 => 0,
        -10 | -9 => 1,
        -8 | -7 => 2,
        -6 | -5 => 3,
        -4 | -3 => 4,
        -2 | -1 => 5,
        0 => 6,
        1 | 2 => 7,
        3 | 4 => 8,
        5 | 6 => 9,
        7 | 8 => 10,
        9 | 10 => 11,
        _ => 12,
    }
}

/// One combat round: `(enemy ENDURANCE loss, Lone Wolf ENDURANCE loss)`,
/// either possibly [`KILL`]. `random` outside `0..=9` is clamped.
pub fn crt_lookup(ratio: i32, random: u8) -> (u8, u8) {
    CRT[(random as usize).min(9)][crt_col(ratio)]
}

// ---------------------------------------------------------------- model ----

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Choice {
    pub text: String,
    pub target: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Enemy {
    pub enemy: String,
    pub combat_skill: i32,
    pub endurance: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Section {
    pub number: u32,
    /// Paragraphs, already plain text (choice sentences included).
    pub text: Vec<String>,
    pub choices: Vec<Choice>,
    pub combat: Vec<Enemy>,
    /// The section asks for a pick from the Random Number Table.
    pub random: bool,
}

impl Section {
    /// The section allows running away instead of fighting to the death.
    pub fn evadable(&self) -> bool {
        self.text.iter().any(|p| p.to_ascii_lowercase().contains("evade"))
    }
}

/// A frontmatter page kept as plain text (rules, map, ...).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub key: String,
    pub title: String,
    pub text: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Book {
    pub title: String,
    /// `01fftd` for a Lone Wolf book, the file stem for a custom one.
    pub key: String,
    pub sections: Vec<Section>,
    /// Names of the Kai Disciplines offered at the start (empty for a custom book).
    pub disciplines: Vec<String>,
    pub pages: Vec<Page>,
}

impl Book {
    pub fn section(&self, n: u32) -> Option<&Section> {
        self.sections.iter().find(|s| s.number == n)
    }
    fn page(&self, key: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.key == key)
    }
}

/// The Action Chart.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Sheet {
    pub combat_skill: i32,
    pub endurance: i32,
    pub endurance_max: i32,
    pub gold: i32,
    pub meals: i32,
    pub weapons: Vec<String>,
    pub backpack: Vec<String>,
    pub special: Vec<String>,
    pub disciplines: Vec<String>,
    pub notes: String,
}

impl Default for Sheet {
    fn default() -> Self {
        Self {
            combat_skill: 10,
            endurance: 20,
            endurance_max: 20,
            gold: 0,
            meals: 2,
            weapons: vec![String::new(); 2],
            backpack: vec![String::new(); 8],
            special: Vec::new(),
            disciplines: Vec::new(),
            notes: String::new(),
        }
    }
}

impl Sheet {
    fn has(&self, discipline: &str) -> bool {
        self.disciplines.iter().any(|d| d.eq_ignore_ascii_case(discipline))
    }
}

/// One book in progress: where we are, how we got here, the sheet, the log.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Game {
    pub key: String,
    pub title: String,
    pub section: u32,
    pub history: Vec<u32>,
    pub sheet: Sheet,
    pub log: Vec<String>,
}

impl Game {
    fn goto(&mut self, target: u32) {
        self.history.push(self.section);
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        self.section = target;
    }
    fn back(&mut self) -> bool {
        match self.history.pop() {
            Some(prev) => {
                self.section = prev;
                true
            }
            None => false,
        }
    }
    fn log_line(&mut self, s: String) {
        self.log.push(s);
        if self.log.len() > 40 {
            self.log.remove(0);
        }
    }
}

/// `save.json`: every book's progress in one file, keyed by [`Book::key`].
#[derive(Serialize, Deserialize, Default)]
struct SaveFile {
    #[serde(default)]
    games: BTreeMap<String, Game>,
}

// ------------------------------------------------------------- html bits ---

/// `&amp;` and friends, plus the numeric forms; `&nbsp;` becomes a plain space
/// so the combat line's `COMBAT&nbsp;SKILL&nbsp;16` parses like normal text.
fn decode_entities(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            if let Some(rel) = chars[i + 1..].iter().take(12).position(|&c| c == ';') {
                let semi = i + 1 + rel;
                let ent: String = chars[i + 1..semi].iter().collect();
                let named = match ent.as_str() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" | "#39" => Some('\''),
                    "nbsp" | "#160" => Some(' '),
                    "ndash" => Some('\u{2013}'),
                    "mdash" => Some('\u{2014}'),
                    "lsquo" | "rsquo" => Some('\''),
                    "ldquo" | "rdquo" => Some('"'),
                    "eacute" => Some('\u{e9}'),
                    "copy" => Some('\u{a9}'),
                    "hellip" => Some('\u{2026}'),
                    _ => None,
                };
                let decoded = named.or_else(|| {
                    let n = if let Some(hex) = ent.strip_prefix('#').and_then(|r| r.strip_prefix(['x', 'X'])) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        ent.strip_prefix('#').and_then(|r| r.parse::<u32>().ok())
                    };
                    n.and_then(char::from_u32)
                });
                if let Some(c) = decoded {
                    out.push(if c == '\u{a0}' { ' ' } else { c });
                    i = semi + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Drops every tag, decodes entities and collapses whitespace to single spaces.
fn text_of(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut depth = 0u32;
    for c in html.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Value of `attr` in a tag's attribute soup (`class="choice"` → `choice`).
/// Single or double quotes; an unquoted or malformed attribute yields `""`.
fn attr(tag: &str, name: &str) -> String {
    let bytes: Vec<char> = tag.chars().collect();
    let lower: Vec<char> = tag.to_ascii_lowercase().chars().collect();
    let want: Vec<char> = name.to_ascii_lowercase().chars().collect();
    let mut i = 0usize;
    while i + want.len() <= lower.len() {
        if lower[i..i + want.len()] == want[..] {
            let mut j = i + want.len();
            while j < lower.len() && lower[j].is_whitespace() {
                j += 1;
            }
            if lower.get(j) == Some(&'=') {
                j += 1;
                while j < lower.len() && lower[j].is_whitespace() {
                    j += 1;
                }
                if let Some(&q @ ('"' | '\'')) = lower.get(j) {
                    let start = j + 1;
                    if let Some(rel) = lower[start..].iter().position(|&c| c == q) {
                        return bytes[start..start + rel].iter().collect();
                    }
                }
            }
        }
        i += 1;
    }
    String::new()
}

/// Every `<tag ...>inner</tag>` pair at the top level of `html`, as
/// `(attribute soup, inner html)`. Not nesting-aware — the Project Aon
/// paragraphs never nest a `<p>` inside a `<p>`.
fn elements<'a>(html: &'a str, tag: &str) -> Vec<(String, &'a str)> {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(rel) = lower[at..].find(&open) {
        let start = at + rel;
        let after = start + open.len();
        // `<p` must not match `<pre`
        if !matches!(lower[after..].chars().next(), None | Some('>' | '/' | ' ' | '\t' | '\n' | '\r')) {
            at = after;
            continue;
        }
        let Some(gt) = lower[after..].find('>') else { break };
        let inner_start = after + gt + 1;
        // self-closing `<p id="x"/>`
        if lower[after..inner_start].trim_end_matches('>').ends_with('/') {
            at = inner_start;
            continue;
        }
        let Some(crel) = lower[inner_start..].find(&close) else { break };
        let inner_end = inner_start + crel;
        out.push((html[start..inner_start].to_string(), &html[inner_start..inner_end]));
        at = inner_end + close.len();
    }
    out
}

/// The readable part of a Project Aon page: the `<div class="maintext">`
/// block, or the whole document if that class is missing (unknown markup must
/// still show *something*). Unlike [`elements`] this counts nesting, because
/// the wanted div sits three `<div>`s deep and the licence footer sits outside
/// it — taking the wrong `</div>` is how boilerplate leaks into a section.
fn maintext(html: &str) -> &str {
    let lower = html.to_ascii_lowercase();
    let mut at = 0usize;
    while let Some(rel) = lower[at..].find("<div") {
        let start = at + rel;
        let Some(gt) = lower[start..].find('>') else { break };
        let open_end = start + gt + 1;
        if !attr(&html[start..open_end], "class").contains("maintext") {
            at = open_end;
            continue;
        }
        // Walk forward with a depth counter to the matching close tag.
        let mut depth = 1usize;
        let mut i = open_end;
        while depth > 0 {
            let next_open = lower[i..].find("<div").map(|r| i + r);
            let next_close = lower[i..].find("</div").map(|r| i + r);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    i = o + 4;
                }
                (_, Some(c)) => {
                    depth -= 1;
                    if depth == 0 {
                        return &html[open_end..c];
                    }
                    i = c + 5;
                }
                _ => return &html[open_end..],
            }
        }
        at = open_end;
    }
    html
}

/// The first `src=`/`href=` inside a `<figure>`, i.e. the illustration's file name.
fn figure_src(inner: &str) -> String {
    let src = attr(inner, "src");
    if src.is_empty() {
        attr(inner, "href")
    } else {
        src
    }
}

/// `sect141.htm` → `141`.
fn sect_target(href: &str) -> Option<u32> {
    let name = href.rsplit('/').next()?;
    name.strip_prefix("sect")?.strip_suffix(".htm")?.parse().ok()
}

/// Parses one `sectNNN.htm` page into a [`Section`]. Robust by design: an
/// unrecognised page still yields its text, just with no choices.
pub fn parse_section(html: &str, fallback_number: u32) -> Section {
    let body = maintext(html);
    let number = elements(body, "h3")
        .first()
        .map(|(_, inner)| text_of(inner))
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(fallback_number);

    let mut text = Vec::new();
    let mut choices = Vec::new();
    let mut combat = Vec::new();
    for (a, inner) in elements(body, "p") {
        let class = attr(&a, "class");
        let line = text_of(inner);
        if line.is_empty() {
            continue;
        }
        if class.contains("combat") {
            if let Some(e) = parse_enemy(&line) {
                combat.push(e);
            }
        }
        if class.contains("choice") {
            for (la, _) in elements(inner, "a") {
                if let Some(target) = sect_target(&attr(&la, "href")) {
                    choices.push(Choice { text: line.clone(), target });
                    break;
                }
            }
        }
        text.push(line);
    }
    for (_, inner) in elements(body, "figure") {
        let src = figure_src(inner);
        if !src.is_empty() {
            text.push(format!("[illustration: {src}]"));
        }
    }
    let random = text.iter().any(|p| p.contains("Random Number Table"));
    Section { number, text, choices, combat, random }
}

/// `Kraan: COMBAT SKILL 16   ENDURANCE 24` → an [`Enemy`].
fn parse_enemy(line: &str) -> Option<Enemy> {
    let upper = line.to_ascii_uppercase();
    let cs_at = upper.find("COMBAT SKILL")?;
    let en_at = upper.find("ENDURANCE")?;
    let name = line[..cs_at].trim().trim_end_matches([':', '\u{2013}', '-']).trim();
    let cs = first_number(&line[cs_at + "COMBAT SKILL".len()..])?;
    let en = first_number(&line[en_at + "ENDURANCE".len()..])?;
    Some(Enemy { enemy: if name.is_empty() { "Enemy".into() } else { name.to_string() }, combat_skill: cs, endurance: en })
}

fn first_number(s: &str) -> Option<i32> {
    let digits: String = s.trim_start_matches(|c: char| !c.is_ascii_digit()).chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Frontmatter page → plain paragraphs (illustrations listed by file name).
pub fn parse_page(html: &str, key: &str, label: &str) -> Page {
    let body = maintext(html);
    let mut text: Vec<String> = Vec::new();
    for (_, inner) in elements(body, "p") {
        let line = text_of(inner);
        if !line.is_empty() {
            text.push(line);
        }
    }
    for (_, inner) in elements(body, "figure") {
        let src = figure_src(inner);
        if !src.is_empty() {
            text.push(format!("[illustration: {src}]"));
        }
    }
    if text.is_empty() {
        let all = text_of(body);
        if !all.is_empty() {
            text.push(all);
        }
    }
    Page { key: key.to_string(), title: label.to_string(), text }
}

/// The `<h4>` headings of `discplnz.htm` are the Kai Disciplines to pick from.
pub fn parse_disciplines(html: &str) -> Vec<String> {
    elements(maintext(html), "h4")
        .into_iter()
        .map(|(_, inner)| text_of(inner))
        .filter(|s| !s.is_empty())
        .collect()
}

// --------------------------------------------------------- custom format ---

/// Parses the plain-text gamebook format documented in the README:
///
/// ```text
/// # My Book
/// [1]
/// Text paragraphs.
/// !combat Giak 12 14
/// -> 2 If you fight
/// ```
/// Also returns any format problems found (line-numbered where possible), so
/// the caller can tell the user what's wrong instead of silently dropping it.
pub fn parse_custom(input: &str, key: &str) -> (Book, Vec<String>) {
    let mut book = Book { key: key.to_string(), title: key.to_string(), ..Default::default() };
    let mut cur: Option<Section> = None;
    let mut para = String::new();
    let mut problems: Vec<String> = Vec::new();

    fn flush_para(para: &mut String, cur: &mut Option<Section>) {
        let p = para.trim().to_string();
        para.clear();
        if !p.is_empty() {
            if let Some(s) = cur.as_mut() {
                s.text.push(p);
            }
        }
    }

    for (i, raw) in input.lines().enumerate() {
        let lineno = i + 1;
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("# ") {
            book.title = rest.trim().to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            if let Ok(n) = rest.trim().parse::<u32>() {
                flush_para(&mut para, &mut cur);
                if let Some(s) = cur.take() {
                    book.sections.push(s);
                }
                cur = Some(Section { number: n, ..Default::default() });
                continue;
            }
            problems.push(format!("line {lineno}: section header '{line}' must be [N]"));
            continue;
        }
        if let Some(rest) = line.strip_prefix("->") {
            flush_para(&mut para, &mut cur);
            let rest = rest.trim();
            let (num, label) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            match num.parse::<u32>() {
                Ok(target) => {
                    if let Some(s) = cur.as_mut() {
                        let text = if label.trim().is_empty() { format!("Turn to {target}.") } else { label.trim().to_string() };
                        s.choices.push(Choice { text: text.clone(), target });
                        s.text.push(text);
                    }
                }
                Err(_) => problems.push(format!("line {lineno}: choice target '{num}' is not a number")),
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("!combat") {
            flush_para(&mut para, &mut cur);
            let parts: Vec<&str> = rest.split_whitespace().collect();
            let parsed = if parts.len() >= 3 {
                match (parts[parts.len() - 2].parse::<i32>(), parts[parts.len() - 1].parse::<i32>()) {
                    (Ok(cs), Ok(en)) => Some((parts[..parts.len() - 2].join(" "), cs, en)),
                    _ => None,
                }
            } else {
                None
            };
            match parsed {
                Some((name, cs, en)) => {
                    if let Some(s) = cur.as_mut() {
                        s.text.push(format!("{name}: COMBAT SKILL {cs}   ENDURANCE {en}"));
                        s.combat.push(Enemy { enemy: name, combat_skill: cs, endurance: en });
                    }
                }
                None => problems.push(format!("line {lineno}: !combat needs <name> <combat skill> <endurance>")),
            }
            continue;
        }
        if line.is_empty() {
            flush_para(&mut para, &mut cur);
        } else {
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(line);
        }
    }
    flush_para(&mut para, &mut cur);
    if let Some(s) = cur.take() {
        book.sections.push(s);
    }
    for s in &mut book.sections {
        s.random = s.text.iter().any(|p| p.contains("Random Number Table"));
    }
    let numbers: std::collections::HashSet<u32> = book.sections.iter().map(|s| s.number).collect();
    for s in &book.sections {
        for c in &s.choices {
            if !numbers.contains(&c.target) {
                problems.push(format!("section {}: choice \u{2192} {} points at a missing section", s.number, c.target));
            }
        }
    }
    (book, problems)
}

// ------------------------------------------------------------------ rng ----

/// xorshift64*, seeded from the clock. A gamebook die does not need more, and
/// this keeps the dependency list where it is.
// ponytail: not cryptographic and does not need to be; swap for `rand` only if
// someone ever wants a reproducible seeded replay.
#[derive(Debug)]
pub struct Rng(u64);

impl Default for Rng {
    fn default() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545_F491_4F6C_DD1D);
        Self(nanos | 1)
    }
}

impl Rng {
    #[cfg(test)]
    pub fn seeded(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A pick from the Random Number Table: 0-9.
    pub fn digit(&mut self) -> u8 {
        (self.next_u64() % 10) as u8
    }
}

// -------------------------------------------------------------- combat -----

#[derive(Debug)]
pub struct Combat {
    pub enemy: Enemy,
    pub enemy_endurance: i32,
    pub mindblast: bool,
    pub evadable: bool,
    pub log: Vec<String>,
    pub over: Option<String>,
}

impl Combat {
    pub fn new(enemy: Enemy, sheet: &Sheet, evadable: bool) -> Self {
        Self {
            enemy_endurance: enemy.endurance,
            enemy,
            mindblast: sheet.has("Mindblast"),
            evadable,
            log: Vec::new(),
            over: None,
        }
    }

    pub fn ratio(&self, sheet: &Sheet) -> i32 {
        sheet.combat_skill + if self.mindblast { 2 } else { 0 } - self.enemy.combat_skill
    }

    /// One round with the given Random Number Table pick. Returns the log line.
    pub fn round(&mut self, sheet: &mut Sheet, random: u8) -> String {
        if self.over.is_some() {
            return String::new();
        }
        let ratio = self.ratio(sheet);
        let (e_loss, lw_loss) = crt_lookup(ratio, random);
        if e_loss == KILL {
            self.enemy_endurance = 0;
        } else {
            self.enemy_endurance -= e_loss as i32;
        }
        if lw_loss == KILL {
            sheet.endurance = 0;
        } else {
            sheet.endurance -= lw_loss as i32;
        }
        self.enemy_endurance = self.enemy_endurance.max(0);
        sheet.endurance = sheet.endurance.max(0);
        let fmt = |v: u8| if v == KILL { "K".to_string() } else { v.to_string() };
        let line = format!(
            "pick {random} \u{b7} you \u{2212}{} (END {}) \u{b7} enemy \u{2212}{} (END {})",
            fmt(lw_loss),
            sheet.endurance,
            fmt(e_loss),
            self.enemy_endurance
        );
        self.log.push(line.clone());
        if self.log.len() > 30 {
            self.log.remove(0);
        }
        if sheet.endurance <= 0 {
            self.over = Some("You are dead.".to_string());
        } else if self.enemy_endurance <= 0 {
            self.over = Some(format!("{} is slain.", self.enemy.enemy));
        }
        line
    }
}

// ------------------------------------------------------------ the module ---

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct QuestCfg {
    pub dir: String,
    /// Extra Lone Wolf books, `"Title|code|sections"`.
    pub books: Vec<String>,
}

impl Default for QuestCfg {
    fn default() -> Self {
        Self { dir: "vault/quests".to_string(), books: Vec::new() }
    }
}

/// One row of the library.
#[derive(Clone, Debug)]
pub struct LibEntry {
    pub title: String,
    pub key: String,
    /// `Some(sections)` for a Lone Wolf book, `None` for a custom `.txt`.
    pub sections: Option<u32>,
    pub downloaded: bool,
    pub last: Option<u32>,
    /// Custom-format diagnostics found on last scan (always 0 for a Lone Wolf entry).
    pub problems: usize,
}

impl LibEntry {
    fn is_lw(&self) -> bool {
        self.sections.is_some()
    }
}

enum QEvent {
    Progress { done: usize, total: usize },
    Done(Box<Book>),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Library,
    Play,
    /// A frontmatter page (`?` rules, `m` map).
    Page,
}

/// A modal on top of the play view; at most one is open at a time.
enum Modal {
    /// "HOW THIS WORKS", shown once at the start of a new game.
    Intro,
    Picker { chosen: Vec<usize>, sel: usize },
    Prompt { label: &'static str, text: String },
    Combat(Combat),
}

struct Dice {
    started: Instant,
    value: u8,
}

/// What the current section is waiting for before the choices unlock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pending {
    Combat,
    Roll,
}

/// The digits a choice sentence claims for itself: `0–4` and `1 to 3` expand to
/// a range, `0, 1 or 2` to a list, a lone `7` to itself. Everything from
/// `turn to` on is ignored, so the target number is never mistaken for a pick.
/// `None` when the sentence names no single-digit number at all.
pub fn choice_numbers(text: &str) -> Option<Vec<u8>> {
    let lower = text.to_ascii_lowercase();
    let head = match lower.find("turn to") {
        Some(i) => &lower[..i],
        None => &lower[..],
    };
    let chars: Vec<char> = head.chars().collect();
    let mut nums: Vec<u8> = Vec::new();
    let mut range_from: Option<u8> = None;
    let mut i = 0usize;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i - start > 1 {
            // 12, 350: a section number, not a Random Number Table pick.
            range_from = None;
            continue;
        }
        let d = chars[start] as u8 - b'0';
        match range_from.take() {
            Some(a) => {
                for v in a.min(d)..=a.max(d) {
                    if !nums.contains(&v) {
                        nums.push(v);
                    }
                }
            }
            None => {
                if !nums.contains(&d) {
                    nums.push(d);
                }
            }
        }
        // A dash or the word "to" right after the digit opens a range.
        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if matches!(chars.get(j), Some('-' | '\u{2013}' | '\u{2014}')) {
            range_from = Some(d);
            i = j + 1;
        } else if chars.get(j) == Some(&'t') && chars.get(j + 1) == Some(&'o') && chars.get(j + 2).is_some_and(|c| c.is_whitespace()) {
            range_from = Some(d);
            i = j + 2;
        }
    }
    (!nums.is_empty()).then_some(nums)
}

/// Which choice a Random Number Table pick decides, if any of them says so.
pub fn roll_target(section: &Section, roll: u8) -> Option<usize> {
    section
        .choices
        .iter()
        .position(|c| choice_numbers(&c.text).is_some_and(|n| n.contains(&roll)))
}

pub struct Quest {
    cfg: QuestCfg,
    dir: PathBuf,
    lib: Vec<LibEntry>,
    lib_sel: usize,
    view: View,
    book: Option<Book>,
    game: Option<Game>,
    scroll: u16,
    choice_sel: usize,
    sheet_sel: usize,
    /// `a` flips the Action Chart against its default: shown by itself from
    /// 100 columns, hidden below that. Editing (tab, +/-, i, x) never needs it.
    sheet_toggle: bool,
    page_key: &'static str,
    modal: Option<Modal>,
    /// What this section visit still owes: a fight or a number. Until it is
    /// `None` the choices are dimmed and the digit keys do nothing.
    pending: Option<Pending>,
    /// The last Random Number Table pick made in this section.
    last_roll: Option<u8>,
    /// `x` was pressed once; a second one removes the selected item.
    x_confirm: bool,
    /// The "you keep the chart" reminder has not been shown yet this session.
    reminder_pending: bool,
    /// ... and this section visit is the one showing it.
    show_reminder: bool,
    dice: Option<Dice>,
    rng: Rng,
    downloading: Option<(String, usize, usize)>,
    licence_shown: bool,
    body_h: Cell<u16>,
    rx: Option<Receiver<QEvent>>,
    tx: Option<Sender<QEvent>>,
}

impl Default for Quest {
    fn default() -> Self {
        Self::new()
    }
}

impl Quest {
    pub fn new() -> Self {
        Self {
            cfg: QuestCfg::default(),
            dir: PathBuf::new(),
            lib: Vec::new(),
            lib_sel: 0,
            view: View::Library,
            book: None,
            game: None,
            scroll: 0,
            choice_sel: 0,
            sheet_sel: 0,
            sheet_toggle: false,
            page_key: "gamerulz",
            modal: None,
            pending: None,
            last_roll: None,
            x_confirm: false,
            reminder_pending: true,
            show_reminder: false,
            dice: None,
            rng: Rng::default(),
            downloading: None,
            licence_shown: false,
            body_h: Cell::new(10),
            rx: None,
            tx: None,
        }
    }

    /// `code` must already be a validated book code (see [`known_books`]):
    /// a single plain path component, never `..` or absolute. Defence in
    /// depth against a config entry that slipped past validation somehow.
    fn book_dir(&self, code: &str) -> PathBuf {
        let base = self.dir.join("lw");
        let dir = base.join(code);
        assert!(
            matches!(Path::new(code).components().collect::<Vec<_>>().as_slice(), [std::path::Component::Normal(_)])
                && dir.starts_with(&base),
            "quest: book code escapes the quests dir: {code:?}"
        );
        dir
    }
    fn save_path(&self) -> PathBuf {
        self.dir.join("save.json")
    }

    /// Rebuilds the library list: the known Lone Wolf books plus every
    /// `*.txt` in the quest directory.
    fn scan(&mut self, ctx: &Ctx) {
        let saves = self.load_saves(ctx);
        let mut lib: Vec<LibEntry> = Vec::new();
        for (title, code, sections) in known_books(&self.cfg.books) {
            let downloaded = self.book_dir(&code).join("book.json").is_file();
            let last = saves.get(&code).map(|g| g.section);
            lib.push(LibEntry { title, key: code, sections: Some(sections), downloaded, last, problems: 0 });
        }
        if let Ok(rd) = fs::read_dir(&self.dir) {
            let mut custom: Vec<LibEntry> = rd
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("txt")))
                .filter_map(|e| {
                    let key = e.path().file_stem()?.to_string_lossy().into_owned();
                    let (title, problems) = match fs::read_to_string(e.path()) {
                        Ok(content) => {
                            let (book, problems) = parse_custom(&content, &key);
                            (book.title, problems.len())
                        }
                        Err(_) => (key.clone(), 0),
                    };
                    let last = saves.get(&key).map(|g| g.section);
                    Some(LibEntry { title, key, sections: None, downloaded: true, last, problems })
                })
                .collect();
            custom.sort_by(|a, b| a.title.cmp(&b.title));
            lib.extend(custom);
        }
        self.lib_sel = self.lib_sel.min(lib.len().saturating_sub(1));
        self.lib = lib;
    }

    /// Missing `save.json` is fine (a fresh install). A file that exists but
    /// won't parse is quarantined to `save.json.bad` so it doesn't get
    /// silently wiped on the next save, and the user is told.
    fn load_saves(&self, ctx: &Ctx) -> BTreeMap<String, Game> {
        let Ok(s) = fs::read_to_string(self.save_path()) else {
            return BTreeMap::new();
        };
        match serde_json::from_str::<SaveFile>(&s) {
            Ok(sf) => sf.games,
            Err(_) => {
                let mut bad = self.save_path().into_os_string();
                bad.push(".bad");
                let _ = fs::rename(self.save_path(), PathBuf::from(bad));
                let _ = ctx.notify.send(Notice::Footer("quest: save.json was unreadable \u{2014} moved to save.json.bad".into()));
                BTreeMap::new()
            }
        }
    }

    fn save_game(&self, ctx: &Ctx) {
        let Some(game) = &self.game else { return };
        let mut all = self.load_saves(ctx);
        all.insert(game.key.clone(), game.clone());
        let json = match serde_json::to_string_pretty(&SaveFile { games: all }) {
            Ok(j) => j,
            Err(e) => {
                let _ = ctx.notify.send(Notice::Footer(format!("quest: save: {e}")));
                return;
            }
        };
        let _ = fs::create_dir_all(&self.dir);
        if let Err(e) = write_atomic(&self.save_path(), &json) {
            let _ = ctx.notify.send(Notice::Footer(format!("quest: save: {e}")));
        }
    }

    fn open_selected(&mut self, ctx: &Ctx) {
        let Some(entry) = self.lib.get(self.lib_sel).cloned() else { return };
        if entry.is_lw() && !entry.downloaded {
            let _ = ctx.notify.send(Notice::Footer(format!("quest: {} is not downloaded \u{2014} press d", entry.title)));
            return;
        }
        let book = if entry.is_lw() {
            fs::read_to_string(self.book_dir(&entry.key).join("book.json"))
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<Book>(&s).map_err(|e| e.to_string()))
                .map(|b| (b, Vec::new()))
        } else {
            fs::read_to_string(self.dir.join(format!("{}.txt", entry.key)))
                .map_err(|e| e.to_string())
                .map(|s| parse_custom(&s, &entry.key))
        };
        match book {
            Ok((book, problems)) => {
                if book.sections.is_empty() {
                    let _ = ctx.notify.send(Notice::Footer("quest: the book has no sections".into()));
                    return;
                }
                if !problems.is_empty() {
                    let shown = problems.iter().take(2).cloned().collect::<Vec<_>>().join(" \u{b7} ");
                    let _ = ctx.notify.send(Notice::Footer(format!("quest: {} format problem(s) \u{2014} {shown}", problems.len())));
                }
                let saved = self.load_saves(ctx).get(&book.key).cloned();
                let first = book.sections.first().map(|s| s.number).unwrap_or(1);
                self.game = Some(saved.unwrap_or(Game {
                    key: book.key.clone(),
                    title: book.title.clone(),
                    section: first,
                    ..Default::default()
                }));
                self.book = Some(book);
                self.view = View::Play;
                self.modal = None;
                self.enter_section();
            }
            Err(e) => {
                let _ = ctx.notify.send(Notice::Footer(format!("quest: {e}")));
            }
        }
    }

    fn download_selected(&mut self, ctx: &Ctx) {
        let Some(entry) = self.lib.get(self.lib_sel).cloned() else { return };
        let Some(sections) = entry.sections else {
            let _ = ctx.notify.send(Notice::Footer("quest: custom books are already local".into()));
            return;
        };
        if self.downloading.is_some() {
            let _ = ctx.notify.send(Notice::Footer("quest: a download is already running".into()));
            return;
        }
        if !self.licence_shown {
            self.licence_shown = true;
            let _ = ctx.notify.send(Notice::Footer(LICENSE_NOTICE.to_string()));
        }
        self.downloading = Some((entry.title.clone(), 0, sections as usize + FRONT.len()));
        let dir = self.book_dir(&entry.key);
        if let Some(tx) = self.tx.clone() {
            spawn_download(&ctx.rt, entry.title, entry.key, sections, dir, tx);
        }
    }

    fn section(&self) -> Option<&Section> {
        let (b, g) = (self.book.as_ref()?, self.game.as_ref()?);
        b.section(g.section)
    }

    fn goto(&mut self, target: u32, ctx: &Ctx) {
        if let Some(g) = self.game.as_mut() {
            g.goto(target);
            g.log_line(format!("\u{2192} {target}"));
        }
        self.modal = None;
        self.enter_section();
        self.save_game(ctx);
    }

    /// Arriving at a section: reset the view and work out what the section
    /// wants from the player before the choices become live.
    // ponytail: a section that asks for both a fight and a number only tracks
    // the fight; the guidance line still names the second step afterwards.
    fn enter_section(&mut self) {
        self.scroll = 0;
        self.choice_sel = 0;
        self.last_roll = None;
        self.show_reminder = std::mem::take(&mut self.reminder_pending);
        self.pending = match self.section() {
            Some(s) if !s.combat.is_empty() => Some(Pending::Combat),
            Some(s) if s.random => Some(Pending::Roll),
            _ => None,
        };
    }

    /// `1`-`9` (and Enter on the highlighted row) take a choice.
    fn take_choice(&mut self, idx: usize, ctx: &Ctx) -> bool {
        if let Some(p) = self.pending {
            let _ = ctx.notify.send(Notice::Footer(match p {
                Pending::Combat => "quest: fight first \u{2014} press c".into(),
                Pending::Roll => "quest: pick a number first \u{2014} press r".to_string(),
            }));
            return false;
        }
        let Some(target) = self.section().and_then(|s| s.choices.get(idx)).map(|c| c.target) else {
            return false;
        };
        self.goto(target, ctx);
        true
    }

    /// The highlighted "what now" line under the section text.
    fn guidance(&self) -> String {
        let (Some(game), Some(s)) = (&self.game, self.section()) else {
            return String::new();
        };
        if game.sheet.endurance <= 0 {
            return "\u{25b6} Endurance 0 \u{2014} you died: n for a new game, b to go back".into();
        }
        match self.pending {
            Some(Pending::Combat) => {
                let e = &s.combat[0];
                format!("\u{25b6} Fight: press c to open combat ({} \u{b7} CS {} \u{b7} END {})", e.enemy, e.combat_skill, e.endurance)
            }
            Some(Pending::Roll) => "\u{25b6} Pick a number: press r".into(),
            None if s.choices.is_empty() => "\u{25b6} The End \u{2014} n for a new game, b to go back".into(),
            None if !s.combat.is_empty() => "\u{25b6} Combat won \u{2014} choose below".into(),
            None => match self.last_roll {
                Some(n) => format!("\u{25b6} You picked {n} \u{2014} now choose below"),
                None if s.choices.len() == 1 => "\u{25b6} Choose 1 below".into(),
                None => format!("\u{25b6} Choose 1\u{2013}{} below", s.choices.len().min(9)),
            },
        }
    }

    /// What a landed die means here, for the dice modal's caption.
    fn roll_meaning(&self, value: u8) -> String {
        match self.section().and_then(|s| roll_target(s, value).map(|i| s.choices[i].target)) {
            Some(t) => format!("you picked {value} \u{2192} turn to {t}"),
            None => format!("you picked {value}"),
        }
    }

    fn new_game(&mut self, ctx: &Ctx) {
        let Some(book) = &self.book else { return };
        let cs = 10 + self.rng.digit() as i32;
        let en = 20 + self.rng.digit() as i32;
        let first = book.sections.first().map(|s| s.number).unwrap_or(1);
        let mut game = Game {
            key: book.key.clone(),
            title: book.title.clone(),
            section: first,
            sheet: Sheet { combat_skill: cs, endurance: en, endurance_max: en, ..Default::default() },
            ..Default::default()
        };
        game.log_line(format!("new game \u{b7} COMBAT SKILL {cs} \u{b7} ENDURANCE {en}"));
        self.game = Some(game);
        self.enter_section();
        // The house rules first, the disciplines after (see `key_intro`).
        self.modal = Some(Modal::Intro);
        self.save_game(ctx);
    }

    fn roll(&mut self) {
        let value = self.rng.digit();
        self.dice = Some(Dice { started: Instant::now(), value });
    }

    fn open_combat(&mut self, ctx: &Ctx) {
        let Some(section) = self.section() else { return };
        let Some(enemy) = section.combat.first().cloned() else {
            let _ = ctx.notify.send(Notice::Footer("quest: no combat in this section".into()));
            return;
        };
        let evadable = section.evadable();
        let Some(game) = &self.game else { return };
        self.modal = Some(Modal::Combat(Combat::new(enemy, &game.sheet, evadable)));
    }

    /// Flat list of editable sheet fields, in `Tab` order.
    fn sheet_fields(sheet: &Sheet) -> Vec<Field> {
        let mut f = vec![Field::Cs, Field::End, Field::Gold, Field::Meals];
        f.extend((0..sheet.weapons.len().max(2)).map(Field::Weapon));
        f.extend((0..sheet.backpack.len().max(8)).map(Field::Pack));
        f.extend((0..sheet.special.len() + 1).map(Field::Special));
        f
    }

    fn adjust(&mut self, delta: i32) {
        let Some(game) = self.game.as_mut() else { return };
        let fields = Self::sheet_fields(&game.sheet);
        match fields.get(self.sheet_sel) {
            Some(Field::Cs) => game.sheet.combat_skill = (game.sheet.combat_skill + delta).max(0),
            Some(Field::End) => {
                game.sheet.endurance = (game.sheet.endurance + delta).clamp(0, game.sheet.endurance_max.max(0));
            }
            Some(Field::Gold) => game.sheet.gold = (game.sheet.gold + delta).max(0),
            Some(Field::Meals) => game.sheet.meals = (game.sheet.meals + delta).max(0),
            _ => {}
        }
    }

    fn set_item(&mut self, text: String) {
        let Some(game) = self.game.as_mut() else { return };
        let fields = Self::sheet_fields(&game.sheet);
        match fields.get(self.sheet_sel) {
            Some(&Field::Weapon(i)) => {
                while game.sheet.weapons.len() <= i {
                    game.sheet.weapons.push(String::new());
                }
                game.sheet.weapons[i] = text;
            }
            Some(&Field::Pack(i)) => {
                while game.sheet.backpack.len() <= i {
                    game.sheet.backpack.push(String::new());
                }
                game.sheet.backpack[i] = text;
            }
            Some(&Field::Special(i)) => {
                if i < game.sheet.special.len() {
                    game.sheet.special[i] = text;
                } else {
                    game.sheet.special.push(text);
                }
            }
            _ => {}
        }
    }

    fn clear_item(&mut self) {
        let Some(game) = self.game.as_mut() else { return };
        let fields = Self::sheet_fields(&game.sheet);
        match fields.get(self.sheet_sel) {
            Some(&Field::Weapon(i)) => {
                if let Some(w) = game.sheet.weapons.get_mut(i) {
                    w.clear();
                }
            }
            Some(&Field::Pack(i)) => {
                if let Some(b) = game.sheet.backpack.get_mut(i) {
                    b.clear();
                }
            }
            Some(&Field::Special(i)) => {
                if i < game.sheet.special.len() {
                    game.sheet.special.remove(i);
                    self.sheet_sel = self.sheet_sel.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Cs,
    End,
    Gold,
    Meals,
    Weapon(usize),
    Pack(usize),
    Special(usize),
}

/// A book code becomes a path component (see [`Quest::book_dir`]), so it must
/// not be able to smuggle in `..` or an absolute path.
fn valid_book_code(s: &str) -> bool {
    (2..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Printable and short enough to sit in a library row / footer notice.
fn valid_book_title(s: &str) -> bool {
    !s.is_empty() && s.chars().count() <= 60 && s.chars().all(|c| !c.is_control())
}

/// The built-in book list plus any `Title|code|sections` entries from config.
/// Entries with a bad code/title, an unparsable section count or a duplicate
/// code are silently skipped here; [`Quest::start`] reports how many.
fn known_books(extra: &[String]) -> Vec<(String, String, u32)> {
    let mut out: Vec<(String, String, u32)> =
        BOOKS.iter().map(|(t, c, n)| (t.to_string(), c.to_string(), *n)).collect();
    for line in extra {
        let parts: Vec<&str> = line.split('|').map(str::trim).collect();
        if parts.len() == 3 {
            if let Ok(n) = parts[2].parse::<u32>() {
                if valid_book_title(parts[0]) && valid_book_code(parts[1]) && !out.iter().any(|(_, c, _)| c == parts[1]) {
                    out.push((parts[0].to_string(), parts[1].to_string(), n));
                }
            }
        }
    }
    out
}

/// `<file>.tmp` + rename, so a reader never sees a half-written save.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

// ------------------------------------------------------------- download ----

/// Downloads every page of a Lone Wolf book into `dir` (skipping files already
/// there, so an interrupted download resumes), then parses `book.json`.
fn spawn_download(
    rt: &tokio::runtime::Handle,
    title: String,
    code: String,
    sections: u32,
    dir: PathBuf,
    tx: Sender<QEvent>,
) {
    rt.spawn(async move {
        if let Err(e) = fs::create_dir_all(&dir) {
            let _ = tx.send(QEvent::Failed(format!("{}: {e}", dir.display())));
            return;
        }
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(PAGE_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(QEvent::Failed(e.to_string()));
                return;
            }
        };

        let mut names: Vec<String> = FRONT.iter().map(|(k, _)| format!("{k}.htm")).collect();
        names.extend((1..=sections).map(|n| format!("sect{n}.htm")));
        let total = names.len();
        let mut done = 0usize;
        let mut failed = 0usize;

        for batch in names.chunks(CONCURRENCY) {
            let mut handles = Vec::with_capacity(batch.len());
            for name in batch {
                let path = dir.join(name);
                if path.is_file() {
                    continue;
                }
                let url = format!("{BASE_URL}/{code}/{name}");
                let client = client.clone();
                handles.push(tokio::spawn(async move { fetch_to(&client, &url, &path).await }));
            }
            for h in handles {
                if !matches!(h.await, Ok(Ok(()))) {
                    failed += 1;
                }
            }
            done += batch.len();
            if tx.send(QEvent::Progress { done: done.min(total), total }).is_err() {
                return;
            }
        }

        match build_book(&dir, &title, &code, sections) {
            Ok(book) => {
                match serde_json::to_string(&book) {
                    Ok(json) => {
                        if let Err(e) = write_atomic(&dir.join("book.json"), &json) {
                            let _ = tx.send(QEvent::Failed(e.to_string()));
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(QEvent::Failed(e.to_string()));
                        return;
                    }
                }
                if failed > 0 {
                    let _ = tx.send(QEvent::Failed(format!("{failed} page(s) failed")));
                }
                let _ = tx.send(QEvent::Done(Box::new(book)));
            }
            Err(e) => {
                let _ = tx.send(QEvent::Failed(e));
            }
        }
    });
}

async fn fetch_to(client: &reqwest::Client, url: &str, path: &Path) -> Result<(), String> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        buf.extend_from_slice(&chunk);
        if buf.len() >= MAX_PAGE_BYTES {
            buf.truncate(MAX_PAGE_BYTES);
            break;
        }
    }
    fs::write(path, &buf).map_err(|e| e.to_string())
}

/// Parses the downloaded `.htm` files in `dir` into a [`Book`].
fn build_book(dir: &Path, title: &str, code: &str, sections: u32) -> Result<Book, String> {
    let mut book = Book { title: title.to_string(), key: code.to_string(), ..Default::default() };
    for (key, label) in FRONT {
        if let Ok(html) = fs::read_to_string(dir.join(format!("{key}.htm"))) {
            if key == "discplnz" {
                book.disciplines = parse_disciplines(&html);
            }
            book.pages.push(parse_page(&html, key, label));
        }
    }
    for n in 1..=sections {
        if let Ok(html) = fs::read_to_string(dir.join(format!("sect{n}.htm"))) {
            book.sections.push(parse_section(&html, n));
        }
    }
    if book.sections.is_empty() {
        return Err("no sections downloaded".to_string());
    }
    Ok(book)
}

// ---------------------------------------------------------------- module ---

impl Module for Quest {
    fn id(&self) -> &'static str {
        "quest"
    }
    fn title(&self) -> &'static str {
        "QUEST"
    }
    fn describe(&self) -> &'static str {
        "Gamebooks on a holotape: Lone Wolf from Project Aon, or your own"
    }

    fn manual(&self) -> &'static str {
        "\
QUEST is a paper gamebook on a holotape: the book tells you
what happens, you keep the Action Chart yourself.

  ↑/↓ enter   library: pick a book and open it
  d / r       download a Lone Wolf book / rescan the folder
  1-9         take a choice (here the digits belong to the
              tab, not to the shell) - ↑/↓ + enter also works
  b / n / l   step back / new game / reload the save
  r / c       roll a number / open the combat panel
  m / ? / esc map / rules / back to the library

The ▶ line under the text says what to do right now. While a
fight or a roll is owed, the choices stay dim and do nothing.

Action Chart: tab walks the fields, +/- changes a number,
i adds an item, x removes one (x twice); a hides or shows it.
Nothing is written for you: the book says it, you record it.

The books come from Project Aon, downloaded to your own
machine for personal use - thank Joe Dever."
    }

    fn help(&self) -> &'static str {
        match (&self.modal, self.view) {
            (Some(Modal::Intro), _) => "enter to continue",
            (Some(Modal::Picker { .. }), _) => "\u{2191}/\u{2193} move   space pick 5   enter confirm   esc cancel   q quit",
            (Some(Modal::Prompt { .. }), _) => "type the item   enter save   esc cancel",
            (Some(Modal::Combat(_)), _) => "enter/r fight a round   e evade   esc close   q quit",
            (None, View::Library) => "\u{2191}/\u{2193} select   enter open   d download   r rescan   1-9 tabs   q quit",
            (None, View::Page) => "\u{2191}/\u{2193} scroll   esc back   1-9 tabs   q quit",
            (None, View::Play) => {
                "1-9 choose (tab keys are the module's here)   \u{2191}/\u{2193}+enter   b back   r dice   c combat   n new   a chart   tab/+/- stat   i item   x x remove   l load   m map   ? rules   esc library"
            }
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<QuestCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.cfg = cfg;
        let accepted_extra = known_books(&self.cfg.books).len().saturating_sub(BOOKS.len());
        let skipped = self.cfg.books.len().saturating_sub(accepted_extra);
        if skipped > 0 {
            let _ = ctx.notify.send(Notice::Footer(format!(
                "quest: {skipped} config book entr{} skipped (bad code/title or duplicate)",
                if skipped == 1 { "y" } else { "ies" }
            )));
        }
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        let p = Path::new(&self.cfg.dir);
        self.dir = if p.is_absolute() { p.to_path_buf() } else { exe_dir.join(p) };
        let (tx, rx) = mpsc::channel();
        self.tx = Some(tx);
        self.rx = Some(rx);
        self.scan(ctx);
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = 0;
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                n += 1;
                match ev {
                    QEvent::Progress { done, total } => {
                        if let Some(d) = self.downloading.as_mut() {
                            d.1 = done;
                            d.2 = total;
                        }
                    }
                    QEvent::Done(book) => {
                        self.downloading = None;
                        let key = book.key.clone();
                        if let Some(e) = self.lib.iter_mut().find(|e| e.key == key) {
                            e.downloaded = true;
                        }
                        let _ = ctx.notify.send(Notice::Footer(format!(
                            "quest: downloaded {}: {} sections",
                            book.title,
                            book.sections.len()
                        )));
                    }
                    QEvent::Failed(msg) => {
                        self.downloading = None;
                        if let Some(g) = self.game.as_mut() {
                            g.log_line(format!("download: {msg}"));
                        }
                        let _ = ctx.notify.send(Notice::Footer(format!("quest: download failed: {msg} \u{2014} press d to retry")));
                    }
                }
            }
        }
        n
    }

    fn tick(&mut self, _ctx: &Ctx) {
        if let Some(d) = self.dice.as_mut() {
            if d.started.elapsed() < DICE_SPIN {
                // Spin: a fresh face every frame until the 0.6 s is up.
                d.value = (d.value + 3) % 10;
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        // Any key after the die lands dismisses it. While it's still
        // spinning, only swallow keys that would act on the game (they'd
        // otherwise queue up and fire the moment it lands); q and the
        // shell's tab keys still get through.
        if let Some(d) = &self.dice {
            if d.started.elapsed() >= DICE_SPIN {
                let value = d.value;
                self.dice = None;
                if let Some(g) = self.game.as_mut() {
                    g.log_line(format!("random number: {value}"));
                }
                if let Some(Modal::Combat(_)) = &self.modal {
                    if let (Some(Modal::Combat(c)), Some(g)) = (self.modal.as_mut(), self.game.as_mut()) {
                        c.round(&mut g.sheet, value);
                    }
                    self.save_game(ctx);
                    return true;
                }
                self.last_roll = Some(value);
                if self.pending == Some(Pending::Roll) {
                    self.pending = None;
                }
                // A choice that names this number is pre-selected; the player
                // still presses it.
                if let Some(i) = self.section().and_then(|s| roll_target(s, value)) {
                    self.choice_sel = i;
                }
                return true;
            }
            return matches!(key.code, KeyCode::Enter | KeyCode::Char('r') | KeyCode::Char('c'))
                || matches!(key.code, KeyCode::Char(c) if c.is_ascii_digit());
        }
        match self.modal.take() {
            Some(Modal::Intro) => {
                // Any key moves on; the disciplines (if any) come next.
                if self.book.as_ref().is_some_and(|b| !b.disciplines.is_empty()) {
                    self.modal = Some(Modal::Picker { chosen: Vec::new(), sel: 0 });
                }
                return true;
            }
            Some(Modal::Picker { chosen, sel }) => return self.key_picker(key, chosen, sel, ctx),
            Some(Modal::Prompt { label, text }) => return self.key_prompt(key, label, text, ctx),
            Some(Modal::Combat(c)) => return self.key_combat(key, c, ctx),
            None => {}
        }
        match self.view {
            View::Library => self.key_library(key, ctx),
            View::Play => self.key_play(key, ctx),
            View::Page => self.key_page(key),
        }
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        match self.view {
            View::Library => self.draw_library(f, area, t),
            View::Page => self.draw_page(f, area, t),
            View::Play => self.draw_play(f, area, t),
        }
        if let Some(d) = &self.dice {
            let caption = if d.started.elapsed() >= DICE_SPIN {
                self.roll_meaning(d.value)
            } else {
                "rolling\u{2026}".to_string()
            };
            draw_dice(f, area, t, d.value, &caption);
        }
    }

    fn overview(&self, width: u16, height: u16, t: Theme) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(" QUEST", t.title))];
        let body = match &self.game {
            Some(g) => format!(
                " {} \u{b7} section {} \u{b7} END {}/{}",
                g.title, g.section, g.sheet.endurance, g.sheet.endurance_max
            ),
            None => " no adventure in progress".to_string(),
        };
        lines.push(Line::from(Span::styled(clip(&body, width as usize), t.value)));
        lines.truncate(height.max(1) as usize);
        lines
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(10)
    }

    fn wants_fast_frames(&self, active: bool) -> bool {
        active && self.dice.as_ref().is_some_and(|d| d.started.elapsed() < DICE_SPIN)
    }

    fn status(&self) -> String {
        let downloaded = self.lib.iter().filter(|e| e.downloaded).count();
        match &self.game {
            Some(g) => format!(
                "quest {} books ({} local), playing {} section {} end {}/{}",
                self.lib.len(),
                downloaded,
                g.title,
                g.section,
                g.sheet.endurance,
                g.sheet.endurance_max
            ),
            None => format!("quest {} books ({} local), idle", self.lib.len(), downloaded),
        }
    }
}

// ------------------------------------------------------------------ keys ---

impl Quest {
    fn key_library(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Up => {
                self.lib_sel = self.lib_sel.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.lib_sel = (self.lib_sel + 1).min(self.lib.len().saturating_sub(1));
                true
            }
            KeyCode::Enter => {
                self.open_selected(ctx);
                true
            }
            KeyCode::Char('d') => {
                self.download_selected(ctx);
                true
            }
            KeyCode::Char('r') => {
                self.scan(ctx);
                true
            }
            _ => false,
        }
    }

    fn key_page(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                true
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(self.body_h.get().max(1));
                true
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(self.body_h.get().max(1));
                true
            }
            KeyCode::Esc | KeyCode::Backspace => {
                self.view = View::Play;
                self.scroll = 0;
                true
            }
            _ => false,
        }
    }

    fn key_play(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        let choices = self.section().map(|s| s.choices.len()).unwrap_or(0);
        // Any key that is not a second `x` cancels the pending removal.
        let confirm_x = std::mem::take(&mut self.x_confirm);
        match key.code {
            // Digits are the shell's tab keys everywhere else; the play view
            // consumes them so `3` picks the third choice.
            KeyCode::Char(c @ '1'..='9') => {
                let idx = c as usize - '1' as usize;
                self.take_choice(idx, ctx);
                true
            }
            KeyCode::Up => {
                self.choice_sel = self.choice_sel.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.choice_sel = (self.choice_sel + 1).min(choices.saturating_sub(1));
                true
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(self.body_h.get().max(1));
                true
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(self.body_h.get().max(1));
                true
            }
            KeyCode::Enter => {
                self.take_choice(self.choice_sel, ctx);
                true
            }
            KeyCode::Char('b') => {
                if self.game.as_mut().is_some_and(Game::back) {
                    self.enter_section();
                    self.save_game(ctx);
                }
                true
            }
            KeyCode::Char('n') => {
                self.new_game(ctx);
                true
            }
            KeyCode::Char('r') => {
                self.roll();
                true
            }
            KeyCode::Char('c') => {
                self.open_combat(ctx);
                true
            }
            KeyCode::Char('a') => {
                self.sheet_toggle = !self.sheet_toggle;
                true
            }
            KeyCode::Tab => {
                let len = self.game.as_ref().map(|g| Self::sheet_fields(&g.sheet).len()).unwrap_or(0);
                if len > 0 {
                    self.sheet_sel = (self.sheet_sel + 1) % len;
                }
                true
            }
            KeyCode::Char('+') => {
                self.adjust(1);
                self.save_game(ctx);
                true
            }
            KeyCode::Char('-') => {
                self.adjust(-1);
                self.save_game(ctx);
                true
            }
            KeyCode::Char('i') => {
                self.modal = Some(Modal::Prompt { label: "ITEM", text: String::new() });
                true
            }
            KeyCode::Char('x') => {
                if confirm_x {
                    self.clear_item();
                    self.save_game(ctx);
                } else {
                    self.x_confirm = true;
                    let _ = ctx.notify.send(Notice::Footer("quest: press x again to remove the selected item".into()));
                }
                true
            }
            KeyCode::Char('l') => {
                if let Some(book) = &self.book {
                    if let Some(g) = self.load_saves(ctx).get(&book.key) {
                        self.game = Some(g.clone());
                        self.enter_section();
                    }
                }
                true
            }
            KeyCode::Char('m') => {
                self.page_key = "map";
                self.view = View::Page;
                self.scroll = 0;
                true
            }
            KeyCode::Char('?') => {
                self.page_key = "gamerulz";
                self.view = View::Page;
                self.scroll = 0;
                true
            }
            KeyCode::Esc | KeyCode::Backspace => {
                // esc always leaves the book; `a` is the chart's own switch.
                {
                    self.view = View::Library;
                    self.scan(ctx);
                }
                true
            }
            _ => false,
        }
    }

    fn key_picker(&mut self, key: KeyEvent, mut chosen: Vec<usize>, mut sel: usize, ctx: &Ctx) -> bool {
        let len = self.book.as_ref().map(|b| b.disciplines.len()).unwrap_or(0);
        match key.code {
            KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Down => sel = (sel + 1).min(len.saturating_sub(1)),
            KeyCode::Char(' ') => {
                if let Some(pos) = chosen.iter().position(|&i| i == sel) {
                    chosen.remove(pos);
                } else if chosen.len() < 5 {
                    chosen.push(sel);
                }
            }
            KeyCode::Enter => {
                if chosen.len() == 5 {
                    if let (Some(book), Some(game)) = (self.book.as_ref(), self.game.as_mut()) {
                        game.sheet.disciplines =
                            chosen.iter().filter_map(|&i| book.disciplines.get(i).cloned()).collect();
                        game.log_line(format!("disciplines: {}", game.sheet.disciplines.join(", ")));
                    }
                    self.save_game(ctx);
                    return true; // modal already taken → closed
                }
                let _ = ctx.notify.send(Notice::Footer(format!("quest: pick 5 disciplines ({} so far)", chosen.len())));
            }
            KeyCode::Esc => return true,
            _ => {}
        }
        self.modal = Some(Modal::Picker { chosen, sel });
        true
    }

    fn key_prompt(&mut self, key: KeyEvent, label: &'static str, mut text: String, ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Char(c) => text.push(c),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Enter => {
                let t = text.trim().to_string();
                if !t.is_empty() {
                    self.set_item(t);
                    self.save_game(ctx);
                }
                return true;
            }
            KeyCode::Esc => return true,
            _ => {}
        }
        self.modal = Some(Modal::Prompt { label, text });
        true
    }

    fn key_combat(&mut self, key: KeyEvent, mut c: Combat, ctx: &Ctx) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => return true,
            KeyCode::Enter | KeyCode::Char('r') => {
                if c.over.is_none() {
                    let n = self.rng.digit();
                    if let Some(g) = self.game.as_mut() {
                        let line = c.round(&mut g.sheet, n);
                        g.log_line(line);
                    }
                    self.save_game(ctx);
                }
            }
            KeyCode::Char('e') => {
                if c.evadable && c.over.is_none() {
                    c.over = Some("You evade the fight.".to_string());
                } else {
                    let _ = ctx.notify.send(Notice::Footer("quest: this section does not allow evading".into()));
                }
            }
            _ => {}
        }
        // Slain, evaded or dead: the section's choices are live again.
        if c.over.is_some() {
            self.pending = None;
        }
        self.modal = Some(Modal::Combat(c));
        true
    }
}

// ------------------------------------------------------------------ draw ---

/// `[####------]`: how much of `max` is left. A 10-cell bar, empty when the
/// maximum is nonsense (a book could name an enemy with 0 ENDURANCE).
fn bar(cur: i32, max: i32) -> String {
    let filled = if max > 0 { (cur.clamp(0, max) * 10 / max) as usize } else { 0 };
    format!("[{}{}]", "\u{2588}".repeat(filled), "\u{b7}".repeat(10 - filled))
}

fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        s.chars().take(w.saturating_sub(1)).collect::<String>() + "\u{2026}"
    }
}

impl Quest {
    fn draw_library(&self, f: &mut Frame, area: Rect, t: Theme) {
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(2)]).split(area);
        let head = match &self.downloading {
            Some((title, done, total)) => format!(" downloading {done}/{total} \u{b7} {title}"),
            None => " LIBRARY".to_string(),
        };
        f.render_widget(Paragraph::new(Line::from(Span::styled(clip(&head, area.width as usize), t.title))), rows[0]);

        let w = rows[1].width as usize;
        let items: Vec<ListItem> = if self.lib.is_empty() {
            vec![ListItem::new("no books \u{2014} press d on a Lone Wolf row to download one").style(t.frame)]
        } else {
            self.lib
                .iter()
                .map(|e| {
                    let state = if !e.is_lw() {
                        "custom".to_string()
                    } else if e.downloaded {
                        "downloaded".to_string()
                    } else {
                        "not downloaded".to_string()
                    };
                    let sections = e.sections.map(|n| format!("{n} sections")).unwrap_or_else(|| "text".into());
                    let last = e.last.map(|n| format!(" \u{b7} at {n}")).unwrap_or_default();
                    let problems = if e.problems > 0 { format!(" \u{b7} {} format problems", e.problems) } else { String::new() };
                    ListItem::new(clip(&format!(" {} \u{b7} {state} \u{b7} {sections}{last}{problems}", e.title), w))
                })
                .collect()
        };
        let mut state = ListState::default();
        if !self.lib.is_empty() {
            state.select(Some(self.lib_sel.min(self.lib.len() - 1)));
        }
        f.render_stateful_widget(List::new(items).highlight_style(t.tab_active), rows[1], &mut state);

        let notice = vec![
            Line::from(Span::styled(
                clip("Lone Wolf \u{a9} Joe Dever \u{b7} Internet Edition by Project Aon \u{b7} downloaded for personal use only", area.width as usize),
                t.frame,
            )),
            Line::from(Span::styled(clip("https://www.projectaon.org/en/Main/License", area.width as usize), t.frame)),
        ];
        f.render_widget(Paragraph::new(notice), rows[2]);
    }

    fn draw_page(&self, f: &mut Frame, area: Rect, t: Theme) {
        let page = self.book.as_ref().and_then(|b| b.page(self.page_key));
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
        let title = page.map(|p| p.title.clone()).unwrap_or_else(|| "not available".to_string());
        f.render_widget(Paragraph::new(Line::from(Span::styled(clip(&title, area.width as usize), t.title))), rows[0]);
        self.body_h.set(rows[1].height);
        let text = page.map(|p| p.text.join("\n\n")).unwrap_or_else(|| {
            "This page was not downloaded with the book.".to_string()
        });
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), rows[1]);
    }

    fn draw_play(&self, f: &mut Frame, area: Rect, t: Theme) {
        let show_sheet = ((area.width >= 100) != self.sheet_toggle) && area.width >= 40;
        let cols = if show_sheet {
            Layout::horizontal([Constraint::Min(0), Constraint::Length(30)]).split(area)
        } else {
            Layout::horizontal([Constraint::Min(0)]).split(area)
        };
        self.draw_story(f, cols[0], t);
        if show_sheet && cols.len() > 1 {
            self.draw_sheet(f, cols[1], t);
        }
        if let Some(modal) = &self.modal {
            match modal {
                Modal::Intro => {
                    let mut lines: Vec<String> = INTRO.iter().map(|s| (*s).to_string()).collect();
                    lines.push(String::new());
                    lines.push("enter to continue".into());
                    draw_panel(f, area, t, "HOW THIS WORKS", &lines);
                }
                Modal::Picker { chosen, sel } => self.draw_picker(f, area, t, chosen, *sel),
                Modal::Prompt { label, text } => draw_prompt(f, area, t, label, text),
                Modal::Combat(c) => self.draw_combat(f, area, t, c),
            }
        }
    }

    fn draw_story(&self, f: &mut Frame, area: Rect, t: Theme) {
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).split(area);
        let header = match (&self.game, self.section()) {
            (Some(g), Some(_)) => format!(" SECTION {}", g.section),
            (Some(g), None) => format!(" SECTION {} (missing)", g.section),
            _ => " no book loaded".to_string(),
        };
        f.render_widget(Paragraph::new(Line::from(Span::styled(clip(&header, area.width as usize), t.title))), rows[0]);

        let mut guide: Vec<String> = Vec::new();
        if self.show_reminder {
            guide.push(CHART_REMINDER.to_string());
        }
        let g = self.guidance();
        if !g.is_empty() {
            guide.push(g);
        }
        let guide_h = if rows[1].height >= 3 { guide.len().min(2) as u16 } else { 0 };
        let body = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(guide_h),
            Constraint::Length(self.choice_rows(rows[1].height)),
        ])
        .split(rows[1]);
        self.body_h.set(body[0].height);
        let text = match self.section() {
            Some(s) => s.text.join("\n\n"),
            None => "Press n for a new game, or esc for the library.".to_string(),
        };
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), body[0]);
        if guide_h > 0 {
            let w = body[1].width as usize;
            let lines: Vec<Line<'static>> =
                guide.iter().take(guide_h as usize).map(|g| Line::from(Span::styled(clip(&format!(" {g}"), w), t.warn))).collect();
            f.render_widget(Paragraph::new(lines), body[1]);
        }

        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(s) = self.section() {
            let w = body[2].width as usize;
            for (i, c) in s.choices.iter().enumerate().take(9) {
                // Locked choices stay readable but obviously inactive.
                let style = match (self.pending.is_some(), i == self.choice_sel) {
                    (true, _) => t.frame,
                    (false, true) => t.tab_active,
                    (false, false) => t.title,
                };
                lines.push(Line::from(Span::styled(clip(&format!(" {}) {}", i + 1, c.text), w), style)));
            }
        }
        f.render_widget(Paragraph::new(lines), body[2]);

        let tail = match (&self.game, self.section()) {
            (Some(g), Some(s)) => {
                let mut bits = Vec::new();
                if !s.combat.is_empty() {
                    bits.push("c combat".to_string());
                }
                if s.random {
                    bits.push("r random number".to_string());
                }
                if let Some(last) = g.log.last() {
                    bits.push(clip(last, 60));
                }
                bits.join(" \u{b7} ")
            }
            _ => String::new(),
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(clip(&tail, area.width as usize), t.frame))),
            rows[2],
        );
    }

    /// How many rows the choice block gets: one per choice, never more than
    /// half the body.
    fn choice_rows(&self, height: u16) -> u16 {
        let n = self.section().map(|s| s.choices.len().min(9)).unwrap_or(0) as u16;
        n.min(height / 2)
    }

    fn draw_sheet(&self, f: &mut Frame, area: Rect, t: Theme) {
        let Some(game) = &self.game else {
            f.render_widget(Paragraph::new(Line::from(Span::styled(" ACTION CHART", t.title))), area);
            return;
        };
        let s = &game.sheet;
        let fields = Self::sheet_fields(s);
        let w = area.width as usize;
        let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(" ACTION CHART", t.title))];
        // `\u{25b6}` marks the field Tab is on; that is the only field a key
        // can change, so that is the only row that carries its hint.
        let row = |lines: &mut Vec<Line<'static>>, idx: Option<usize>, text: String, hint: &str| {
            let selected = idx.is_some_and(|i| i == self.sheet_sel);
            let (mark, style) = if selected { ("\u{25b6}", t.tab_active) } else { (" ", t.value) };
            let hint = if selected { format!(" \u{b7} {hint}") } else { String::new() };
            lines.push(Line::from(Span::styled(clip(&format!("{mark}{text}{hint}"), w), style)));
        };
        let find = |f: Field| fields.iter().position(|x| *x == f);
        let head = |lines: &mut Vec<Line<'static>>, text: String| {
            lines.push(Line::from(Span::styled(clip(&text, w), t.frame)));
        };
        head(&mut lines, " STATS".into());
        row(&mut lines, find(Field::Cs), format!("COMBAT SKILL {}", s.combat_skill), "+/- to change");
        row(&mut lines, find(Field::End), format!("ENDURANCE {}/{}", s.endurance, s.endurance_max), "+/- to change");
        row(&mut lines, find(Field::Gold), format!("GOLD {}", s.gold), "+/- to change");
        row(&mut lines, find(Field::Meals), format!("MEALS {}", s.meals), "+/- to change");
        head(&mut lines, " WEAPONS \u{b7} i add \u{b7} x remove".into());
        for i in 0..s.weapons.len().max(2) {
            let v = s.weapons.get(i).filter(|x| !x.is_empty()).cloned().unwrap_or_else(|| "\u{2014}".into());
            row(&mut lines, find(Field::Weapon(i)), format!(" {v}"), "i add \u{b7} x remove");
        }
        let used = s.backpack.iter().filter(|x| !x.is_empty()).count();
        head(&mut lines, format!(" BACKPACK {used}/{} \u{b7} i add \u{b7} x remove", s.backpack.len().max(8)));
        for i in 0..s.backpack.len().max(8) {
            let v = s.backpack.get(i).filter(|x| !x.is_empty()).cloned().unwrap_or_else(|| "\u{2014}".into());
            row(&mut lines, find(Field::Pack(i)), format!(" {v}"), "i add \u{b7} x remove");
        }
        head(&mut lines, " SPECIAL ITEMS \u{b7} i add \u{b7} x remove".into());
        for i in 0..s.special.len() + 1 {
            let v = s.special.get(i).cloned().unwrap_or_else(|| "\u{2014}".into());
            row(&mut lines, find(Field::Special(i)), format!(" {v}"), "i add \u{b7} x remove");
        }
        head(&mut lines, " KAI DISCIPLINES".into());
        if s.disciplines.is_empty() {
            lines.push(Line::from(Span::styled("  none picked \u{2014} n for a new game", t.frame)));
        }
        for d in &s.disciplines {
            lines.push(Line::from(Span::styled(clip(&format!("  {d}"), w), t.value)));
        }
        for h in SHEET_HINT.lines() {
            lines.push(Line::from(Span::styled(clip(h, w), t.warn)));
        }
        lines.truncate(area.height.max(1) as usize);
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_picker(&self, f: &mut Frame, area: Rect, t: Theme, chosen: &[usize], sel: usize) {
        let list: Vec<String> = self
            .book
            .as_ref()
            .map(|b| b.disciplines.clone())
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let mark = if chosen.contains(&i) { "[x]" } else { "[ ]" };
                let cursor = if i == sel { ">" } else { " " };
                format!("{cursor}{mark} {d}")
            })
            .collect();
        let title = format!("PICK 5 KAI DISCIPLINES ({}/5)", chosen.len());
        draw_panel(f, area, t, &title, &list);
    }

    fn draw_combat(&self, f: &mut Frame, area: Rect, t: Theme, c: &Combat) {
        let sheet = self.game.as_ref().map(|g| &g.sheet);
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!(
            "{:<14.14} CS {:>2} END {} {}/{}",
            c.enemy.enemy,
            c.enemy.combat_skill,
            bar(c.enemy_endurance, c.enemy.endurance),
            c.enemy_endurance,
            c.enemy.endurance
        ));
        if let Some(s) = sheet {
            lines.push(format!(
                "{:<14.14} CS {:>2} END {} {}/{}",
                "you",
                s.combat_skill,
                bar(s.endurance, s.endurance_max),
                s.endurance,
                s.endurance_max
            ));
            lines.push(format!(
                "COMBAT RATIO {:+} \u{b7} you {}{} vs CS {}",
                c.ratio(s),
                s.combat_skill,
                if c.mindblast { " +2 (Mindblast)" } else { "" },
                c.enemy.combat_skill
            ));
        }
        lines.push(String::new());
        if c.log.is_empty() {
            lines.push("no rounds fought yet".into());
        }
        lines.extend(c.log.iter().cloned());
        lines.push(String::new());
        match &c.over {
            Some(over) => {
                lines.push(format!("\u{2588}\u{2588} {} \u{2588}\u{2588}", over.to_ascii_uppercase()));
                lines.push("esc closes \u{b7} the choices below are live again".into());
            }
            None => lines.push(format!(
                "enter fight a round \u{b7} e evade ({}) \u{b7} esc close",
                if c.evadable { "allowed here" } else { "not here" }
            )),
        }
        draw_panel(f, area, t, "COMBAT", &lines);
    }
}

/// A centred panel with a title and a list of lines; degrades to nothing on a
/// degenerate area.
fn draw_panel(f: &mut Frame, area: Rect, t: Theme, title: &str, lines: &[String]) {
    if area.width < 8 || area.height < 3 {
        return;
    }
    let w = area.width.saturating_sub(4).max(4).min(62);
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let block = ratatui::widgets::Block::bordered().border_style(t.frame).title(Span::styled(format!(" {title} "), t.title));
    let inner = block.inner(rect);
    let mut out: Vec<Line<'static>> = Vec::new();
    for l in lines.iter().take(inner.height as usize) {
        out.push(Line::from(Span::styled(clip(l, inner.width as usize), t.value)));
    }
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(out), inner);
}

fn draw_prompt(f: &mut Frame, area: Rect, t: Theme, label: &str, text: &str) {
    // NOTES' `TITLE>` prompt, with the module's own label.
    draw_panel(f, area, t, label, &[format!("{label}> {text}\u{2588}"), String::new(), "enter save \u{b7} esc cancel".into()])
}

/// The Random Number Table pick as a big block digit.
fn draw_dice(f: &mut Frame, area: Rect, t: Theme, value: u8, caption: &str) {
    use crate::ui::bigfont;
    let text = (value % 10).to_string();
    let cols = bigfont::text_cols(&text);
    let Some(scale) = bigfont::fit_scale(area.width, area.height, cols) else {
        let line = Line::from(Span::styled(clip(&format!(" {DICE_TITLE}: {caption}"), area.width as usize), t.title));
        f.render_widget(Paragraph::new(line), Rect { height: 1.min(area.height), ..area });
        return;
    };
    let cw = (cols * scale as usize) as u16 + 1;
    let ch = bigfont::ROWS as u16 * scale + 1;
    let x0 = area.x as i32 + (area.width as i32 - cw as i32).max(0) / 2;
    let y0 = area.y as i32 + (area.height as i32 - ch as i32).max(0) / 2;
    let clear = Rect {
        x: x0.max(area.x as i32) as u16,
        y: y0.max(area.y as i32) as u16,
        width: cw.min(area.width),
        height: ch.min(area.height),
    };
    f.render_widget(ratatui::widgets::Clear, clear);
    let buf = f.buffer_mut();
    bigfont::blit(buf, area, x0 + 1, y0 + 1, &text, scale, t.frame, true);
    bigfont::blit(buf, area, x0, y0, &text, scale, t.title, false);
    // A digit alone says nothing: name the table above it and the meaning below.
    let mut band = |y: i32, s: &str, style: ratatui::style::Style| {
        if y < area.y as i32 || y >= (area.y + area.height) as i32 {
            return;
        }
        let text = clip(s, area.width as usize);
        let w = text.chars().count() as u16;
        let rect = Rect { x: area.x + area.width.saturating_sub(w) / 2, y: y as u16, width: w, height: 1 };
        f.render_widget(ratatui::widgets::Clear, rect);
        f.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), rect);
    };
    band(y0 - 1, DICE_TITLE, t.frame);
    band(y0 + ch as i32, caption, t.warn);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const SECTION_FIXTURE: &str = r#"<html><body><div class="container"><article>
    <div class="numbered"><div class="maintext table-responsive">
      <h3>17</h3>
      <p>You raise your weapon to strike at the beast.</p>
      <p>Deduct 1 point from your <span class="smallcaps">COMBAT SKILL</span> and fight the Kraan.</p>
      <p class="combat">Kraan: <span class="smallcaps">COMBAT&nbsp;SKILL</span>&nbsp;16 &nbsp;&nbsp;<span class="smallcaps">ENDURANCE</span>&nbsp;24</p>
      <p>Pick a number from the <a href="random.htm">Random Number Table</a>.</p>
      <p class="choice">If you pick 0, <a href="sect53.htm">turn to 53</a>.</p>
      <p class="choice">If you pick 1&ndash;2, <a href="sect274.htm">turn to 274</a>.</p>
    </div><p id="page-navigation"/></div></article>
    <footer><div id="license"><p>Text &copy; 1984 Joe Dever.</p>
    <p>Distribution of this Internet Edition is restricted under the terms of the <a href="license.htm">Project Aon License</a>.</p>
    </div></footer></div></body></html>"#;

    const EXAMPLE_CUSTOM: &str = include_str!("../../vault/quests/vault-13.txt");

    #[test]
    fn parses_a_real_section_page() {
        let s = parse_section(SECTION_FIXTURE, 0);
        assert_eq!(s.number, 17);
        assert!(s.text[0].starts_with("You raise your weapon"));
        assert_eq!(s.choices.len(), 2);
        assert_eq!(s.choices[0].target, 53);
        assert_eq!(s.choices[1].target, 274);
        assert!(s.choices[1].text.contains("1\u{2013}2"), "&ndash; decoded: {}", s.choices[1].text);
        assert_eq!(
            s.combat,
            vec![Enemy { enemy: "Kraan".into(), combat_skill: 16, endurance: 24 }]
        );
        assert!(s.random, "the Random Number Table prompt is detected");
        // Footer/licence boilerplate must not leak into the section text.
        assert!(!s.text.iter().any(|p| p.contains("Project Aon License")), "{:?}", s.text);
        assert!(!s.text.iter().any(|p| p.contains("1984 Joe Dever")));
    }

    #[test]
    fn unknown_markup_still_yields_text() {
        let s = parse_section("<html><body><p>bare page, no maintext</p></body></html>", 42);
        assert_eq!(s.number, 42, "falls back to the requested number");
        assert!(s.text.iter().any(|p| p.contains("bare page")));
        assert!(s.choices.is_empty());
    }

    #[test]
    fn parses_disciplines_and_pages() {
        let html = r#"<div class="maintext"><h3>Kai Disciplines</h3><p>intro</p>
          <h4><a id="camflage">Camouflage</a></h4><p>a</p>
          <h4><a id="hunting">Hunting</a></h4><p>b</p>
          <figure><img src="weapons.png"/></figure></div>"#;
        assert_eq!(parse_disciplines(html), vec!["Camouflage", "Hunting"]);
        let p = parse_page(html, "discplnz", "Kai Disciplines");
        assert_eq!(p.key, "discplnz");
        assert!(p.text.iter().any(|l| l == "[illustration: weapons.png]"));
    }

    #[test]
    fn evade_detection_reads_the_text() {
        let mut s = Section { text: vec!["You may evade the fight.".into()], ..Default::default() };
        assert!(s.evadable());
        s.text = vec!["No way out.".into()];
        assert!(!s.evadable());
    }

    #[test]
    fn custom_example_round_trips() {
        let (b, problems) = parse_custom(EXAMPLE_CUSTOM, "vault-13");
        assert_eq!(b.key, "vault-13");
        assert_eq!(b.title, "Vault 13: The Water Chip Requisition");
        assert!(b.sections.len() >= 36, "{} sections", b.sections.len());
        assert!(problems.is_empty(), "a well-formed book reports nothing: {problems:?}");
        let endings = b.sections.iter().filter(|s| s.choices.is_empty()).count();
        assert_eq!(endings, 3, "one good, one bad, one absurd");
        assert!(b.sections.iter().filter(|s| !s.combat.is_empty()).count() >= 3);
        assert!(b.sections.iter().filter(|s| s.random).count() >= 3, "random-number branches");
        // Every enemy is beatable by a starting character.
        for e in b.sections.iter().flat_map(|s| s.combat.iter()) {
            assert!((8..=16).contains(&e.combat_skill) && (10..=24).contains(&e.endurance), "{e:?}");
        }
        // Every random branch names its picks in a form the parser understands.
        for s in b.sections.iter().filter(|s| s.random) {
            let covered: Vec<u8> = s.choices.iter().filter_map(|c| choice_numbers(&c.text)).flatten().collect();
            for n in 0..=9u8 {
                assert!(covered.contains(&n), "section {}: pick {n} decides nothing", s.number);
            }
        }
        // Section numbers are unique and every choice target exists.
        let nums: Vec<u32> = b.sections.iter().map(|s| s.number).collect();
        for s in &b.sections {
            for c in &s.choices {
                assert!(nums.contains(&c.target), "section {} -> missing {}", s.number, c.target);
            }
        }
        let combat: Vec<&Enemy> = b.sections.iter().flat_map(|s| s.combat.iter()).collect();
        assert!(!combat.is_empty(), "the example has at least one fight");
        assert!(combat[0].combat_skill > 0 && combat[0].endurance > 0);
        assert!(b.section(1).is_some());
    }

    #[test]
    fn custom_parser_handles_paragraphs_and_odd_lines() {
        let (b, problems) = parse_custom("# T\n[1]\nline one\nline two\n\nsecond para\n-> 2 Go on\n!combat Giak 12 14\n[2]\nend\n", "k");
        assert_eq!(b.title, "T");
        assert_eq!(b.sections.len(), 2);
        assert!(problems.is_empty(), "{problems:?}");
        let s1 = b.section(1).unwrap();
        assert_eq!(s1.text[0], "line one line two");
        assert_eq!(s1.text[1], "second para");
        assert_eq!(s1.choices, vec![Choice { text: "Go on".into(), target: 2 }]);
        assert_eq!(s1.combat, vec![Enemy { enemy: "Giak".into(), combat_skill: 12, endurance: 14 }]);
        // A bare "-> 5" with no label still works (though it targets a
        // section that does not exist in this tiny fixture).
        let (b2, problems2) = parse_custom("[1]\n-> 5\n", "k");
        assert_eq!(b2.section(1).unwrap().choices[0].target, 5);
        assert_eq!(problems2.len(), 1);
    }

    #[test]
    fn custom_parser_reports_format_problems_with_line_numbers() {
        let bad = "# Bad\n[one]\ntext\n-> abc go\n!combat OnlyOneArg\n[2]\n-> 99\n";
        let (b, problems) = parse_custom(bad, "bad");
        // Section "one" never parsed, so only section 2 exists.
        assert_eq!(b.sections.len(), 1);
        assert!(problems.iter().any(|p| p == "line 2: section header '[one]' must be [N]"), "{problems:?}");
        assert!(problems.iter().any(|p| p == "line 4: choice target 'abc' is not a number"), "{problems:?}");
        assert!(problems.iter().any(|p| p == "line 5: !combat needs <name> <combat skill> <endurance>"), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("section 2: choice \u{2192} 99 points at a missing section")), "{problems:?}");
        assert_eq!(problems.len(), 4, "{problems:?}");
    }

    #[test]
    fn crt_spot_checks_against_the_printed_table() {
        // Column 0 (ratio 0) appears on both halves of the printed table and
        // must agree; these are read off crtneg.png / crtpos.png.
        assert_eq!(crt_lookup(0, 5), (7, 2));
        assert_eq!(crt_lookup(0, 0), (12, 0));
        assert_eq!(crt_lookup(0, 1), (3, 5));
        // Extremes: the worst ratio kills Lone Wolf on a 1, the best kills the enemy on a 0.
        assert_eq!(crt_lookup(-12, 1), (0, KILL));
        assert_eq!(crt_lookup(11, 0), (KILL, 0));
        assert_eq!(crt_lookup(-11, 9), (5, 3));
        assert_eq!(crt_lookup(9, 6), (14, 1));
        // Out-of-range picks are clamped, never a panic.
        assert_eq!(crt_lookup(0, 200), crt_lookup(0, 9));
    }

    #[test]
    fn crt_columns_cover_every_ratio() {
        assert_eq!(crt_col(-100), 0);
        assert_eq!(crt_col(-11), 0);
        assert_eq!(crt_col(-10), 1);
        assert_eq!(crt_col(0), 6);
        assert_eq!(crt_col(10), 11);
        assert_eq!(crt_col(11), 12);
        assert_eq!(crt_col(999), 12);
        for r in -30..30 {
            assert!(crt_col(r) < 13);
        }
    }

    #[test]
    fn combat_round_math_and_end_conditions() {
        let mut sheet = Sheet { combat_skill: 15, endurance: 25, endurance_max: 25, ..Default::default() };
        let enemy = Enemy { enemy: "Kraan".into(), combat_skill: 16, endurance: 24 };
        let mut c = Combat::new(enemy, &sheet, false);
        assert_eq!(c.ratio(&sheet), -1);
        c.round(&mut sheet, 5); // ratio -1 -> column 5, random 5 -> (6,3)
        assert_eq!(c.enemy_endurance, 24 - 6);
        assert_eq!(sheet.endurance, 25 - 3);
        assert!(c.over.is_none());
        // Mindblast shifts the ratio by +2.
        sheet.disciplines = vec!["Mindblast".into()];
        let c2 = Combat::new(Enemy { enemy: "x".into(), combat_skill: 16, endurance: 5 }, &sheet, false);
        assert_eq!(c2.ratio(&sheet), 1);
    }

    #[test]
    fn combat_kill_results_end_the_fight() {
        let mut sheet = Sheet { combat_skill: 30, endurance: 25, endurance_max: 25, ..Default::default() };
        let mut c = Combat::new(Enemy { enemy: "Giak".into(), combat_skill: 5, endurance: 40 }, &sheet, false);
        c.round(&mut sheet, 0); // ratio +25 -> last column, random 0 -> (K, 0)
        assert_eq!(c.enemy_endurance, 0);
        assert_eq!(c.over.as_deref(), Some("Giak is slain."));
        // A finished fight ignores further rounds.
        let before = sheet.endurance;
        c.round(&mut sheet, 9);
        assert_eq!(sheet.endurance, before);

        let mut sheet2 = Sheet { combat_skill: 1, endurance: 25, endurance_max: 25, ..Default::default() };
        let mut c2 = Combat::new(Enemy { enemy: "Vordak".into(), combat_skill: 30, endurance: 40 }, &sheet2, false);
        c2.round(&mut sheet2, 1); // ratio -29 -> column 0, random 1 -> (0, K)
        assert_eq!(sheet2.endurance, 0);
        assert_eq!(c2.over.as_deref(), Some("You are dead."));
    }

    #[test]
    fn history_back_is_bounded_and_reversible() {
        let mut g = Game { section: 1, ..Default::default() };
        g.goto(141);
        g.goto(85);
        assert_eq!(g.section, 85);
        assert!(g.back());
        assert_eq!(g.section, 141);
        assert!(g.back());
        assert_eq!(g.section, 1);
        assert!(!g.back(), "no history left");
        for i in 0..MAX_HISTORY as u32 + 20 {
            g.goto(i);
        }
        assert_eq!(g.history.len(), MAX_HISTORY);
    }

    #[test]
    fn save_round_trips_through_json() {
        let mut g = Game { key: "01fftd".into(), title: "Flight from the Dark".into(), section: 141, ..Default::default() };
        g.goto(85);
        g.sheet.gold = 12;
        g.sheet.backpack[0] = "Rope".into();
        g.log_line("hello".into());
        let mut games = BTreeMap::new();
        games.insert(g.key.clone(), g.clone());
        let json = serde_json::to_string(&SaveFile { games }).unwrap();
        let back: SaveFile = serde_json::from_str(&json).unwrap();
        let r = back.games.get("01fftd").unwrap();
        assert_eq!(r.section, 85);
        assert_eq!(r.history, vec![141]);
        assert_eq!(r.sheet.gold, 12);
        assert_eq!(r.sheet.backpack[0], "Rope");
        assert_eq!(r.log, vec!["hello".to_string()]);
    }

    #[test]
    fn corrupt_save_file_is_quarantined_not_wiped() {
        let (ctx, rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = Quest::new();
        q.dir = std::env::temp_dir().join(format!("pipboy-quest-test-corrupt-{}", std::process::id()));
        fs::create_dir_all(&q.dir).unwrap();
        fs::write(q.save_path(), "not valid json").unwrap();

        let saves = q.load_saves(&ctx);
        assert!(saves.is_empty(), "starts empty rather than losing the file silently");
        let bad = q.dir.join("save.json.bad");
        assert!(bad.is_file(), "the unreadable file was quarantined");
        let n = rx.try_recv().expect("a footer notice was sent");
        assert!(matches!(n, Notice::Footer(ref m) if m.contains("save.json.bad")), "{n:?}");

        // A later save must not clobber the quarantined copy.
        q.game = Some(Game { key: "k".into(), title: "K".into(), ..Default::default() });
        q.save_game(&ctx);
        assert!(bad.is_file(), "save.json.bad survives a subsequent save");
        assert!(q.save_path().is_file());

        let _ = fs::remove_dir_all(&q.dir);
    }

    fn play_quest() -> Quest {
        let mut q = Quest::new();
        let (book, _) = parse_custom(EXAMPLE_CUSTOM, "vault-13");
        q.game = Some(Game {
            key: book.key.clone(),
            title: book.title.clone(),
            section: 1,
            sheet: Sheet { combat_skill: 14, endurance: 22, endurance_max: 22, ..Default::default() },
            ..Default::default()
        });
        q.book = Some(book);
        q.view = View::Play;
        q.dir = std::env::temp_dir().join("pipboy-quest-test-never-written");
        q.enter_section();
        q
    }

    /// Moves the test game to a section without going through the play view.
    fn at(q: &mut Quest, section: u32) {
        q.game.as_mut().unwrap().section = section;
        q.enter_section();
    }

    fn screen(q: &Quest, w: u16, h: u16) -> String {
        let t = Theme::new(crate::config::ThemeKind::Color);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| q.draw(f, f.area(), t)).unwrap();
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .chunks(w as usize)
            .map(|r| r.concat())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn choice_ranges_are_read_out_of_the_sentence() {
        assert_eq!(choice_numbers("If it is 0\u{2013}4, turn to 10"), Some(vec![0, 1, 2, 3, 4]));
        assert_eq!(choice_numbers("If it is 5-9, turn to 19"), Some(vec![5, 6, 7, 8, 9]));
        assert_eq!(choice_numbers("If it is 0, 1 or 2, turn to 33"), Some(vec![0, 1, 2]));
        assert_eq!(choice_numbers("If you pick 1 to 3"), Some(vec![1, 2, 3]));
        assert_eq!(choice_numbers("If you pick 7, turn to 88"), Some(vec![7]));
        // The target number is never mistaken for a pick.
        assert_eq!(choice_numbers("Turn to 4"), None);
        assert_eq!(choice_numbers("If you have the Water Chip"), None);
        assert_eq!(choice_numbers("Take 13-C to the requisition desk"), None);
    }

    #[test]
    fn a_roll_preselects_the_choice_it_decides() {
        let s = Section {
            number: 8,
            random: true,
            choices: vec![
                Choice { text: "If it is 0\u{2013}4, turn to 10".into(), target: 10 },
                Choice { text: "If it is 5-9, turn to 11".into(), target: 11 },
            ],
            ..Default::default()
        };
        assert_eq!(roll_target(&s, 3), Some(0));
        assert_eq!(roll_target(&s, 5), Some(1));
        assert_eq!(roll_target(&Section::default(), 5), None);
    }

    #[test]
    fn combat_and_dice_sections_lock_the_choices() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = play_quest();
        at(&mut q, 7); // the rad roach
        assert_eq!(q.pending, Some(Pending::Combat));
        assert!(q.guidance().starts_with("\u{25b6} Fight: press c"), "{}", q.guidance());
        assert!(q.on_key(KeyEvent::from(KeyCode::Char('1')), &ctx), "the digit is still eaten");
        assert_eq!(q.game.as_ref().unwrap().section, 7, "but it does not move");

        // Fighting it out unlocks them again.
        q.open_combat(&ctx);
        for _ in 0..40 {
            if q.pending.is_none() {
                break;
            }
            q.on_key(KeyEvent::from(KeyCode::Enter), &ctx);
        }
        assert_eq!(q.pending, None, "the fight ended one way or the other");
        let expected = if q.game.as_ref().unwrap().sheet.endurance <= 0 { "Endurance 0" } else { "Combat won" };
        assert!(q.guidance().contains(expected), "{}", q.guidance());

        // A dice section wants r, and the landed die unlocks it.
        let mut q = play_quest();
        at(&mut q, 8);
        assert_eq!(q.pending, Some(Pending::Roll));
        assert_eq!(q.guidance(), "\u{25b6} Pick a number: press r");
        assert!(!q.take_choice(0, &ctx));
        assert_eq!(q.game.as_ref().unwrap().section, 8);
        q.dice = Some(Dice { started: Instant::now() - DICE_SPIN, value: 7 });
        q.on_key(KeyEvent::from(KeyCode::Enter), &ctx);
        assert_eq!(q.pending, None);
        assert_eq!(q.last_roll, Some(7));
        assert_eq!(q.choice_sel, 1, "7 falls in the 5-9 branch");
        assert_eq!(q.guidance(), "\u{25b6} You picked 7 \u{2014} now choose below");
        assert!(q.roll_meaning(7).contains("\u{2192} turn to"), "{}", q.roll_meaning(7));
    }

    #[test]
    fn guidance_covers_the_plain_states() {
        let mut q = play_quest();
        assert_eq!(q.guidance(), "\u{25b6} Choose 1\u{2013}2 below");
        at(&mut q, 4); // one choice only
        assert_eq!(q.guidance(), "\u{25b6} Choose 1 below");
        at(&mut q, 38); // an ending
        assert_eq!(q.guidance(), "\u{25b6} The End \u{2014} n for a new game, b to go back");
        q.game.as_mut().unwrap().sheet.endurance = 0;
        assert!(q.guidance().starts_with("\u{25b6} Endurance 0 \u{2014} you died"), "{}", q.guidance());
    }

    #[test]
    fn the_chart_reminder_shows_once_a_session() {
        let mut q = play_quest();
        assert!(q.show_reminder, "the first section of the session says who keeps the chart");
        assert!(screen(&q, 120, 40).contains("you keep the chart yourself"));
        at(&mut q, 4);
        assert!(!q.show_reminder, "and only then");
    }

    #[test]
    fn a_new_game_explains_the_house_rules_first() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = play_quest();
        q.on_key(KeyEvent::from(KeyCode::Char('n')), &ctx);
        assert!(matches!(q.modal, Some(Modal::Intro)));
        assert!(screen(&q, 120, 40).contains("HOW THIS WORKS"));
        q.on_key(KeyEvent::from(KeyCode::Enter), &ctx);
        // The custom book has no Kai Disciplines, so nothing follows it.
        assert!(q.modal.is_none());
    }

    #[test]
    fn removing_an_item_takes_two_presses_of_x() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = play_quest();
        q.sheet_sel = 4; // the first weapon slot
        q.modal = Some(Modal::Prompt { label: "ITEM", text: "Crowbar".into() });
        q.on_key(KeyEvent::from(KeyCode::Enter), &ctx);
        assert_eq!(q.game.as_ref().unwrap().sheet.weapons[0], "Crowbar");

        q.on_key(KeyEvent::from(KeyCode::Char('x')), &ctx);
        assert_eq!(q.game.as_ref().unwrap().sheet.weapons[0], "Crowbar", "one x only arms it");
        q.on_key(KeyEvent::from(KeyCode::Char('a')), &ctx); // any other key cancels
        q.on_key(KeyEvent::from(KeyCode::Char('x')), &ctx);
        assert_eq!(q.game.as_ref().unwrap().sheet.weapons[0], "Crowbar");
        q.on_key(KeyEvent::from(KeyCode::Char('x')), &ctx);
        assert_eq!(q.game.as_ref().unwrap().sheet.weapons[0], "", "x x removes it");
    }

    #[test]
    fn the_chart_panel_names_its_sections_and_its_keys() {
        let q = play_quest();
        // 120 columns: the chart is on by itself; `a` would hide it.
        let s = screen(&q, 120, 40);
        for want in ["ACTION CHART", "STATS", "WEAPONS", "BACKPACK 0/8", "SPECIAL ITEMS", "KAI DISCIPLINES", "tab next", "a hide"] {
            assert!(s.contains(want), "missing {want:?} in:\n{s}");
        }
        assert!(s.contains("\u{25b6}COMBAT SKILL"), "the selected field is marked:\n{s}");
    }

    #[test]
    fn the_combat_panel_spells_out_the_fight() {
        let mut q = play_quest();
        at(&mut q, 7);
        q.game.as_mut().unwrap().sheet.disciplines = vec!["Mindblast".into()];
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        q.open_combat(&ctx);
        let s = screen(&q, 120, 40);
        for want in ["COMBAT", "Rad Roach", "COMBAT RATIO", "+2 (Mindblast)", "enter fight a round", "e evade (allowed here)", "esc close"] {
            assert!(s.contains(want), "missing {want:?} in:\n{s}");
        }
    }

    #[test]
    fn the_dice_modal_says_what_the_number_means() {
        let mut q = play_quest();
        at(&mut q, 8);
        q.dice = Some(Dice { started: Instant::now() - DICE_SPIN, value: 7 });
        let s = screen(&q, 120, 40);
        assert!(s.contains("Random Number Table"), "{s}");
        assert!(s.contains("you picked 7 \u{2192} turn to 11"), "{s}");
    }

    #[test]
    fn digit_keys_are_consumed_only_in_the_play_view() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = play_quest();
        let target = q.section().unwrap().choices[0].target;
        assert!(q.on_key(KeyEvent::from(KeyCode::Char('1')), &ctx), "play view eats digits");
        assert_eq!(q.game.as_ref().unwrap().section, target);

        // Library and page views leave digits to the shell's tab switching.
        let mut lib = play_quest();
        lib.view = View::Library;
        assert!(!lib.on_key(KeyEvent::from(KeyCode::Char('1')), &ctx));
        let mut page = play_quest();
        page.view = View::Page;
        assert!(!page.on_key(KeyEvent::from(KeyCode::Char('1')), &ctx));
    }

    #[test]
    fn out_of_range_digit_is_still_consumed_but_does_not_move() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = play_quest();
        assert!(q.on_key(KeyEvent::from(KeyCode::Char('9')), &ctx));
        assert_eq!(q.game.as_ref().unwrap().section, 1, "no 9th choice, no move");
    }

    #[test]
    fn esc_returns_to_the_library() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = play_quest();
        assert!(q.on_key(KeyEvent::from(KeyCode::Esc), &ctx));
        assert!(q.view == View::Library);
    }

    #[test]
    fn dice_spin_only_swallows_game_acting_keys() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = play_quest();
        q.dice = Some(Dice { started: Instant::now(), value: 3 });
        assert!(!q.on_key(KeyEvent::from(KeyCode::Char('q')), &ctx), "q passes through while spinning");
        assert!(q.dice.is_some(), "still spinning, untouched by the pass-through key");
        assert!(q.on_key(KeyEvent::from(KeyCode::Char('5')), &ctx), "digits act on the game, so swallowed");
        assert!(q.on_key(KeyEvent::from(KeyCode::Enter), &ctx));
        assert!(q.on_key(KeyEvent::from(KeyCode::Char('r')), &ctx));
        assert!(q.on_key(KeyEvent::from(KeyCode::Char('c')), &ctx));
    }

    #[test]
    fn known_books_accepts_config_entries_and_ignores_junk() {
        let base = known_books(&[]).len();
        let more = known_books(&[
            "The Kingdoms of Terror|06tkot|350".to_string(),
            "broken line".to_string(),
            "Bad|07x|not-a-number".to_string(),
            "Flight from the Dark|01fftd|350".to_string(), // duplicate code ignored
        ]);
        assert_eq!(more.len(), base + 1);
        assert!(more.iter().any(|(_, c, n)| c == "06tkot" && *n == 350));
    }

    #[test]
    fn known_books_rejects_path_traversal_codes_and_bad_titles() {
        let base = known_books(&[]).len();
        let more = known_books(&[
            "Escape|../../../Temp/x|10".to_string(),
            "Absolute|/etc/passwd|10".to_string(),
            "Upper|ABCXYZ|10".to_string(),
            format!("{}|abcxyz|10", "x".repeat(61)),
        ]);
        assert_eq!(more.len(), base, "every entry above is rejected: {more:?}");
    }

    #[test]
    #[should_panic(expected = "escapes the quests dir")]
    fn book_dir_rejects_path_traversal() {
        let mut q = Quest::new();
        q.dir = std::env::temp_dir().join("pipboy-quest-test-book-dir");
        let _ = q.book_dir("../../../Temp/x");
    }

    #[test]
    fn download_failure_produces_a_footer_notice_even_with_no_game() {
        let (ctx, rx) = crate::shell::test_ctx(toml::Table::new());
        let mut q = Quest::new();
        let (tx, qrx) = mpsc::channel();
        q.tx = Some(tx.clone());
        q.rx = Some(qrx);
        assert!(q.game.is_none());
        tx.send(QEvent::Failed("network error".into())).unwrap();
        q.poll(&ctx);
        let n = rx.try_recv().expect("a footer notice was sent");
        assert!(matches!(n, Notice::Footer(ref m) if m.contains("download failed") && m.contains("network error")), "{n:?}");
    }

    #[test]
    fn rng_digits_stay_in_range() {
        let mut r = Rng::seeded(12345);
        let mut seen = [false; 10];
        for _ in 0..2000 {
            let d = r.digit();
            assert!(d < 10);
            seen[d as usize] = true;
        }
        assert!(seen.iter().all(|&s| s), "every face turns up");
    }

    #[test]
    fn sheet_fields_and_editing() {
        let mut q = play_quest();
        let fields = Quest::sheet_fields(&q.game.as_ref().unwrap().sheet);
        assert_eq!(fields[0], Field::Cs);
        assert_eq!(fields.len(), 4 + 2 + 8 + 1);
        q.sheet_sel = 2; // gold
        q.adjust(5);
        assert_eq!(q.game.as_ref().unwrap().sheet.gold, 5);
        q.adjust(-50);
        assert_eq!(q.game.as_ref().unwrap().sheet.gold, 0, "never negative");
        q.sheet_sel = 1; // endurance, clamped to max
        q.adjust(100);
        let s = &q.game.as_ref().unwrap().sheet;
        assert_eq!(s.endurance, s.endurance_max);
        q.sheet_sel = 4; // first weapon
        q.set_item("Axe".into());
        assert_eq!(q.game.as_ref().unwrap().sheet.weapons[0], "Axe");
        q.clear_item();
        assert_eq!(q.game.as_ref().unwrap().sheet.weapons[0], "");
    }

    #[test]
    fn overview_and_status_have_both_shapes() {
        let t = Theme::new(crate::config::ThemeKind::Color);
        let idle = Quest::new();
        assert_eq!(idle.overview(40, 2, t).len(), 2);
        assert!(idle.status().contains("idle"));
        let q = play_quest();
        let ov = q.overview(60, 2, t);
        assert!(format!("{:?}", ov).contains("section 1"));
        assert!(q.status().contains("section 1"));
    }

    #[test]
    fn help_differs_per_view() {
        let mut q = play_quest();
        assert!(q.help().contains("1-9 choose"));
        q.view = View::Library;
        assert!(q.help().contains("d download"));
        q.view = View::Play;
        q.modal = Some(Modal::Combat(Combat::new(
            Enemy { enemy: "x".into(), combat_skill: 1, endurance: 1 },
            &Sheet::default(),
            false,
        )));
        assert!(q.help().contains("evade"));
    }

    #[test]
    fn draws_every_state_at_every_size() {
        let t = Theme::new(crate::config::ThemeKind::Color);
        for (w, h) in [(1u16, 1u16), (40, 12), (120, 40)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            // library (empty and populated), play, sheet, combat, picker, prompt, dice, page
            let mut lib = Quest::new();
            term.draw(|f| lib.draw(f, f.area(), t)).unwrap();
            lib.lib = vec![
                LibEntry { title: "Flight from the Dark".into(), key: "01fftd".into(), sections: Some(350), downloaded: false, last: None, problems: 0 },
                LibEntry { title: "Vault 13".into(), key: "vault-13".into(), sections: None, downloaded: true, last: Some(4), problems: 2 },
            ];
            lib.downloading = Some(("Flight from the Dark".into(), 142, 350));
            term.draw(|f| lib.draw(f, f.area(), t)).unwrap();

            let mut q = play_quest();
            term.draw(|f| q.draw(f, f.area(), t)).unwrap();
            q.sheet_toggle = true;
            term.draw(|f| q.draw(f, f.area(), t)).unwrap();
            q.modal = Some(Modal::Combat(Combat::new(
                Enemy { enemy: "Rad Roach".into(), combat_skill: 9, endurance: 12 },
                &q.game.as_ref().unwrap().sheet,
                true,
            )));
            term.draw(|f| q.draw(f, f.area(), t)).unwrap();
            q.modal = Some(Modal::Picker { chosen: vec![0], sel: 1 });
            term.draw(|f| q.draw(f, f.area(), t)).unwrap();
            q.modal = Some(Modal::Prompt { label: "ITEM", text: "Rope".into() });
            term.draw(|f| q.draw(f, f.area(), t)).unwrap();
            q.modal = None;
            q.dice = Some(Dice { started: Instant::now(), value: 7 });
            term.draw(|f| q.draw(f, f.area(), t)).unwrap();
            q.dice = None;
            q.view = View::Page;
            term.draw(|f| q.draw(f, f.area(), t)).unwrap();

            // A game whose section number is missing from the book.
            let mut broken = play_quest();
            broken.game.as_mut().unwrap().section = 9999;
            term.draw(|f| broken.draw(f, f.area(), t)).unwrap();
        }
    }
}
