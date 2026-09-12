//! A generikus héj: keret, fülsor, fejléc, lábléc, OVERVIEW-kompozíció,
//! billentyű-útvonal és a poll/tick ciklus.
//!
//! A héj nem tud semmit a modulok belsejéről: a [`Module`] szerződésen át
//! rajzoltat, kérdez és kézbesít. A modulok kérései a [`Notice`] csatornán
//! érkeznek vissza (lábléc-üzenet, fülváltás, fejléc-riasztás).
use crate::config::RawConfig;
use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use serde::Deserialize;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Tabs, Wrap};
use ratatui::Frame;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const FRAME_FAST: Duration = Duration::from_millis(50);
const FRAME_SLOW: Duration = Duration::from_millis(250);
const OVERVIEW_TITLE: &str = "OVERVIEW";
const OVERVIEW_HELP: &str = "1-9 tabs   space play/pause   +/- volume   m mute   q quit";
const SETUP_TITLE: &str = "SETUP";
const SETUP_HELP: &str = "↑/↓ select   space toggle   a all   1-9 tabs   q quit";
const SETUP_HINT: &str = "disabled modules use no CPU or network";
/// A lábléc végére fűzött súgó-emlékeztető, ha kifér.
const HELP_HINT: &str = "   h help";
/// A kézikönyv-ablak alsó sora.
const MANUAL_HINT: &str = "h / esc / any key closes";
const OVERVIEW_MANUAL: &str = "\
OVERVIEW is the wall panel: the important part of every tab at
once. Compact stats, the big clock and quest timer, weather,
the radio with its VU, news, mail, notes, Wi-Fi, syslog and
the globe - whatever you left switched on in SETUP.

The blocks arrange themselves: two columns above 100 columns
wide, one below. Nothing here is interactive; for that, go to
the tab itself.

  1-9   jump to a tab     ←/→ Tab   next or previous tab
  0     the SETUP tab
  space play or pause the radio, from anywhere
  +/-   volume            m  mute
  h     this manual, on any tab
  q     quit (esc never quits; tabs use it to step back)";
const SETUP_MANUAL: &str = "\
SETUP is the switchboard: every module with one line about
what it does and a box saying whether it is switched on.

  ↑/↓   pick a module
  space or enter   switch it on or off
  a     switch every module on
  1-9   jump to a tab     0  come back here

A disabled module has no tab, uses no CPU and opens no network
connection - its background threads are never started at all.
Switching one back on starts it there and then, and a module
that already ran keeps its threads and its global keys.

Every change is written to config.toml right away, so the
Pip-Boy comes back tomorrow exactly as you left it.";
/// A riasztás villogásának képkocka-sebessége (a v2 időzítő-animációjával azonos).
const ALERT_FPS: u128 = 10;

/// A héj saját `[shell]` szekciója.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ShellCfg {
    /// Kikapcsolt modulok azonosítói (`["mail", "term"]`).
    pub disabled: Vec<String>,
}

pub struct Shell {
    modules: Vec<Box<dyn Module>>,
    /// Modulonként: látszik-e a füle, pollozzuk-e. A registry teljes marad,
    /// hogy a SETUP a kikapcsoltakat is fel tudja sorolni.
    enabled: Vec<bool>,
    /// Modulonként: futott-e már a `start`. Az újra-bekapcsolás nem indít
    /// másodszor — a szálai már élnek, csak megint pollozzuk őket.
    started: Vec<bool>,
    /// A SETUP listájának kurzora.
    setup_sel: usize,
    /// Ide írjuk vissza a `[shell] disabled` sort.
    config_path: PathBuf,
    ctx: Ctx,
    theme: Theme,
    notices: Receiver<Notice>,
    /// 0 = OVERVIEW, `1..=modules.len()` a modulok fülei.
    tab: usize,
    notice: Option<String>,
    /// Villogó fejléc-cím a riasztás kezdetétől.
    alert: Option<(&'static str, Instant)>,
    /// Nyitott kézikönyv-ablak a görgetési eltolásával.
    manual: Option<u16>,
}

impl Shell {
    /// A `notices` a `ctx.notify` párja: a modulok kérései ezen érkeznek.
    pub fn new(
        modules: Vec<Box<dyn Module>>,
        theme: Theme,
        notice: Option<String>,
        ctx: Ctx,
        notices: Receiver<Notice>,
    ) -> Self {
        let (cfg, err) = ctx.config.section::<ShellCfg>("shell");
        let enabled: Vec<bool> = modules.iter().map(|m| !cfg.disabled.iter().any(|d| d == m.id())).collect();
        let started = vec![false; modules.len()];
        Self {
            enabled,
            started,
            setup_sel: 0,
            config_path: RawConfig::path(),
            modules,
            ctx,
            theme,
            notices,
            tab: 0,
            notice: notice.or(err),
            alert: None,
            manual: None,
        }
    }

    /// A be nem kapcsolt modulok szálai el sem indulnak.
    fn start_enabled(&mut self) {
        for i in 0..self.modules.len() {
            if self.enabled[i] && !self.started[i] {
                self.modules[i].start(&self.ctx);
                self.started[i] = true;
            }
        }
    }

    /// `0` = OVERVIEW, `1..=modules.len()` a modulok, az utolsó a SETUP.
    fn setup_tab(&self) -> usize {
        self.modules.len() + 1
    }

    /// A fülsorban látszó fülek logikai indexei, balról jobbra.
    fn visible_tabs(&self) -> Vec<usize> {
        std::iter::once(0)
            .chain(self.enabled.iter().enumerate().filter(|(_, e)| **e).map(|(i, _)| i + 1))
            .chain(std::iter::once(self.setup_tab()))
            .collect()
    }

    fn tab_title(&self, tab: usize) -> &'static str {
        match tab {
            0 => OVERVIEW_TITLE,
            t if t == self.setup_tab() => SETUP_TITLE,
            t => self.modules[t - 1].title(),
        }
    }

    /// Be/ki egy modul: indítás első bekapcsoláskor, aktív fül esetén vissza
    /// OVERVIEW-ra, majd mentés (a futás állapota hibás írásnál is változik).
    fn toggle(&mut self, i: usize) {
        self.enabled[i] = !self.enabled[i];
        if self.enabled[i] {
            self.start_enabled();
        } else if self.tab == i + 1 {
            self.tab = 0;
        }
        let word = if self.enabled[i] { "enabled" } else { "disabled" };
        let msg = format!("saved: {} {word}", self.modules[i].id());
        self.notice = Some(match self.save_disabled() {
            Ok(()) => msg,
            Err(e) => format!("config not saved: {e}"),
        });
    }

    fn save_disabled(&self) -> std::io::Result<()> {
        let ids: Vec<&str> =
            self.modules.iter().zip(&self.enabled).filter(|(_, on)| !**on).map(|(m, _)| m.id()).collect();
        RawConfig::save_disabled(&self.config_path, &ids)
    }

    pub fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
        self.start_enabled();
        self.drain_startup_notices();
        loop {
            terminal.draw(|f| self.draw(f))?;
            let timeout = if self.wants_fast_frames() { FRAME_FAST } else { FRAME_SLOW };
            if event::poll(timeout)? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press && self.on_key(k) {
                        return Ok(());
                    }
                }
            }
            self.poll_modules();
            self.drain_notices();
        }
    }

    /// Egy kör `poll`/`tick` a bekapcsolt modulokon.
    fn poll_modules(&mut self) {
        for (i, m) in self.modules.iter_mut().enumerate() {
            if self.enabled[i] {
                m.poll(&self.ctx);
                m.tick(&self.ctx);
            }
        }
    }

    /// TUI nélküli diagnosztika: `start`, majd `secs` másodpercig `poll`/`tick`,
    /// másodpercenként egy `status()` sor modulonként.
    pub fn probe(&mut self, secs: u64) {
        self.start_enabled();
        let mut events = vec![0usize; self.modules.len()];
        let t0 = Instant::now();
        let mut next = t0 + Duration::from_secs(1);
        while t0.elapsed() < Duration::from_secs(secs) {
            for (i, m) in self.modules.iter_mut().enumerate() {
                if !self.enabled[i] {
                    continue;
                }
                events[i] += m.poll(&self.ctx);
                m.tick(&self.ctx);
            }
            self.drain_notices();
            if Instant::now() >= next {
                for (i, m) in self.modules.iter().enumerate() {
                    println!("{:>5.1}s {:<8} ev={:<5} {}", t0.elapsed().as_secs_f32(), m.id(), events[i], m.status());
                }
                next += Duration::from_secs(1);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // ---- billentyűk ----

    /// Aktív modul → minden modul `on_global_key` → héj. `true` = kilépés.
    fn on_key(&mut self, key: KeyEvent) -> bool {
        // A nyitott kézikönyv mindent elnyel: egy tévedésből leütött `q` sem
        // léphet ki alóla, csak a görgetés megy át.
        if let Some(scroll) = self.manual {
            self.manual = match key.code {
                KeyCode::Up => Some(scroll.saturating_sub(1)),
                KeyCode::Down => Some(scroll.saturating_add(1).min(self.manual_max_scroll())),
                _ => None,
            };
            return false;
        }
        self.notice = None;
        if self.tab == self.setup_tab() {
            if self.setup_key(key) {
                return false;
            }
        } else if self.tab > 0 && self.modules[self.tab - 1].on_key(key, &self.ctx) {
            return false;
        }
        // A "started stays started" modellel konzisztens: egy elindított, majd
        // kikapcsolt modul szálai továbbra is futnak, ezért a globális
        // kulcsait (rádió lejátszás/szünet, hangerő) is válaszolnia kell.
        for (i, m) in self.modules.iter_mut().enumerate() {
            if self.started[i] && m.on_global_key(key, &self.ctx) {
                return false;
            }
        }
        self.shell_key(key)
    }

    /// A SETUP fül billentyűi. `true` = elfogyasztottuk.
    fn setup_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => self.setup_sel = self.setup_sel.saturating_sub(1),
            KeyCode::Down => self.setup_sel = (self.setup_sel + 1).min(self.modules.len().saturating_sub(1)),
            KeyCode::Char(' ') | KeyCode::Enter => {
                if self.setup_sel < self.modules.len() {
                    self.toggle(self.setup_sel);
                }
            }
            KeyCode::Char('a') => {
                self.enabled.iter_mut().for_each(|e| *e = true);
                self.start_enabled();
                self.notice = Some(match self.save_disabled() {
                    Ok(()) => "saved: all modules enabled".into(),
                    Err(e) => format!("config not saved: {e}"),
                });
            }
            _ => return false,
        }
        true
    }

    fn shell_key(&mut self, key: KeyEvent) -> bool {
        let visible = self.visible_tabs();
        match key.code {
            KeyCode::Char('h') => self.manual = Some(0),
            KeyCode::Char('q') => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Left | KeyCode::BackTab => self.step(&visible, -1),
            KeyCode::Right | KeyCode::Tab => self.step(&visible, 1),
            KeyCode::Char('0') => self.tab = self.setup_tab(),
            KeyCode::Char(c @ '1'..='9') => {
                if let Some(&tab) = visible.get(c as usize - '1' as usize) {
                    self.tab = tab;
                }
            }
            _ => {}
        }
        false
    }

    /// Szomszédos látható fül (a kikapcsoltakat átugorva), körbe.
    fn step(&mut self, visible: &[usize], d: isize) {
        let n = visible.len() as isize;
        let cur = visible.iter().position(|&t| t == self.tab).unwrap_or(0) as isize;
        self.tab = visible[(cur + d).rem_euclid(n) as usize];
    }

    fn drain_notices(&mut self) {
        self.take_notices(false);
    }

    /// Az első képkocka előtt: az induláskor felgyűlt üzenetek közül az **első**
    /// nyer (és marad az első billentyűig), a config fájl szintű üzenete pedig
    /// mindet veri — az a `Shell::new`-ban már be van állítva.
    fn drain_startup_notices(&mut self) {
        self.take_notices(true);
    }

    fn take_notices(&mut self, first_wins: bool) {
        while let Ok(n) = self.notices.try_recv() {
            match n {
                Notice::Footer(s) => {
                    if !(first_wins && self.notice.is_some()) {
                        self.notice = Some(s);
                    }
                }
                Notice::Activate(id) => {
                    if let Some(i) = self.modules.iter().position(|m| m.id() == id) {
                        if self.enabled[i] {
                            self.tab = i + 1;
                        }
                    }
                }
                Notice::Alert(text) => self.alert = text.map(|t| (t, Instant::now())),
            }
        }
    }

    /// 20 kép/s csak akkor, ha valamelyik modul mozgó tartalmat mutat.
    fn wants_fast_frames(&self) -> bool {
        self.modules
            .iter()
            .enumerate()
            .any(|(i, m)| self.enabled[i] && m.wants_fast_frames(self.tab == 0 || self.tab == i + 1))
    }

    // ---- rajzolás ----

    fn titles(&self) -> Vec<&'static str> {
        self.visible_tabs().into_iter().map(|t| self.tab_title(t)).collect()
    }

    fn draw(&self, f: &mut Frame) {
        let t = self.theme;
        let full = f.area();
        // A CRT-lencse jobb szélén torzul a kép: egy oszlop szabadon marad.
        let area = Rect { width: full.width.saturating_sub(1), ..full };
        let clock = chrono::Local::now().format("%Y-%m-%d  %H:%M:%S").to_string();
        let title = match self.alert {
            Some((text, since)) if blink(since) => Span::styled(text, t.warn),
            _ => Span::styled(" PIP-BOY 3000 ", t.title),
        };
        let outer = Block::bordered()
            .border_type(BorderType::Double)
            .border_style(t.frame)
            .title(Line::from(title))
            .title_top(Line::from(Span::styled(format!(" {clock} "), t.value)).right_aligned());
        let inner = outer.inner(area);
        f.render_widget(outer, area);

        // A minimum-méret az egész ablakra vonatkozik, nem a bezel-margó utánira.
        if full.width < 40 || full.height < 12 {
            let msg = format!("Window too small: min 40×12, now {}×{}", full.width, full.height);
            f.render_widget(Paragraph::new(msg).style(t.warn), inner);
            return;
        }

        let [tabs_row, rule1, body, rule2, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        self.draw_tabs(f, tabs_row);
        let rule = Paragraph::new("═".repeat(inner.width as usize)).style(t.frame);
        f.render_widget(rule.clone(), rule1);
        f.render_widget(rule, rule2);
        if self.tab == 0 {
            self.draw_overview(f, body);
        } else if self.tab == self.setup_tab() {
            self.draw_setup(f, body);
        } else {
            self.modules[self.tab - 1].draw(f, body, t);
        }
        self.draw_footer(f, footer);
        if let Some(scroll) = self.manual {
            self.draw_manual(f, inner, scroll);
        }
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let titles = self.titles();
        let tabs_w: usize = titles.iter().map(|s| s.len() + 2).sum::<usize>() + titles.len().saturating_sub(1);
        let right_w = area.width.saturating_sub(tabs_w as u16 + 1);
        let left_w = area.width - right_w;
        let [left, right] = Layout::horizontal([Constraint::Length(left_w), Constraint::Length(right_w)]).areas(area);

        self.draw_header(f, right, right_w as usize);
        self.draw_tab_strip(f, left, &titles, tabs_w);
    }

    fn draw_tab_strip(&self, f: &mut Frame, area: Rect, titles: &[&str], tabs_w: usize) {
        let t = self.theme;
        let active = self.visible_tabs().iter().position(|&x| x == self.tab).unwrap_or(0);
        if tabs_w <= area.width as usize {
            let tabs = Tabs::new(titles.iter().map(|s| format!(" {s} ")))
                .select(active)
                .style(t.text)
                .highlight_style(t.tab_active)
                .divider(" ")
                .padding("", "");
            f.render_widget(tabs, area);
            return;
        }

        let avail = area.width as usize;
        let (mut start, mut end) = visible_tab_window(titles, active, avail);
        let reserve = (start > 0) as usize + (end < titles.len()) as usize;
        if reserve > 0 {
            let (s2, e2) = visible_tab_window(titles, active, avail.saturating_sub(reserve));
            start = s2;
            end = e2;
        }
        let need_left = start > 0;
        let need_right = end < titles.len();
        let [lm, mid, rm] = Layout::horizontal([
            Constraint::Length(need_left as u16),
            Constraint::Length(area.width.saturating_sub(need_left as u16 + need_right as u16)),
            Constraint::Length(need_right as u16),
        ])
        .areas(area);

        if need_left {
            f.render_widget(Paragraph::new("‹").style(t.frame), lm);
        }
        if need_right {
            f.render_widget(Paragraph::new("›").style(t.frame), rm);
        }

        let visible = &titles[start..end];
        let mut tabs = Tabs::new(visible.iter().map(|s| format!(" {s} ")))
            .style(t.text)
            .highlight_style(t.tab_active)
            .divider(" ")
            .padding("", "");
        if active >= start && active < end {
            tabs = tabs.select(active - start);
        }
        f.render_widget(tabs, mid);
    }

    /// A modulok `header()` spanjei a fülsor jobb szegmensében.
    ///
    /// Kérdezés és rajzolás is registry-sorrendben (a fix méretű elemek —
    /// pl. STAT `BAT` jelzője — a registryben korán foglalnak); a csoportok
    /// közt két szóköz. A kész sor sosem lóg túl a szegmensen: a felesleg
    /// jobbról elmarad.
    fn draw_header(&self, f: &mut Frame, area: Rect, right_w: usize) {
        if right_w < 8 {
            return;
        }
        // Egy szóköz a jobb szélen marad, hogy a keret ne érjen a szöveghez.
        let budget = right_w - 1;
        let mut groups: Vec<Vec<Span>> = Vec::new();
        let mut used = 0usize;
        for (i, m) in self.modules.iter().enumerate() {
            if !self.enabled[i] {
                continue;
            }
            let left = budget.saturating_sub(used + if groups.is_empty() { 0 } else { 2 });
            let group = m.header(left as u16, self.theme);
            let w = span_width(&group);
            if group.is_empty() || w > left {
                continue;
            }
            used += w + if groups.is_empty() { 0 } else { 2 };
            groups.push(group);
        }
        if groups.is_empty() {
            return;
        }
        let mut spans: Vec<Span> = Vec::new();
        for group in groups {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.extend(group);
        }
        spans.push(Span::raw(" "));
        f.render_widget(Paragraph::new(Line::from(spans)).right_aligned(), area);
    }

    /// A `Slot::Left`/`Right` blokkok két oszlopban (100 alatt egyben), `n` szerint rendezve.
    fn draw_overview(&self, f: &mut Frame, area: Rect) {
        let single = area.width < 100;
        let (left, right) = if single {
            (area, area)
        } else {
            let [l, r] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
            (l, r)
        };
        let mut lefts: Vec<(u8, Vec<Line>)> = Vec::new();
        let mut rights: Vec<(u8, Vec<Line>)> = Vec::new();
        for (i, m) in self.modules.iter().enumerate() {
            if !self.enabled[i] {
                continue;
            }
            match m.overview_slot() {
                Slot::None => {}
                Slot::Left(n) => lefts.push((n, m.overview(left.width, area.height, self.theme))),
                Slot::Right(n) => rights.push((n, m.overview(right.width, area.height, self.theme))),
            }
        }
        lefts.sort_by_key(|(n, _)| *n);
        rights.sort_by_key(|(n, _)| *n);
        let lefts: Vec<Vec<Line>> = lefts.into_iter().map(|(_, b)| b).collect();
        let rights: Vec<Vec<Line>> = rights.into_iter().map(|(_, b)| b).collect();
        if single {
            render_blocks(f, lefts.into_iter().chain(rights).collect(), area);
        } else {
            render_blocks(f, lefts, left);
            render_blocks(f, rights, right);
        }
    }

    /// A modulok listája jelölőnégyzettel és egysoros leírással.
    fn draw_setup(&self, f: &mut Frame, area: Rect) {
        let t = self.theme;
        let on = self.enabled.iter().filter(|e| **e).count();
        let head = format!("SETUP · {on} of {} modules enabled · config.toml", self.modules.len());
        let mut lines = vec![Line::from(Span::styled(head, t.title)), Line::from("")];
        for (i, m) in self.modules.iter().enumerate() {
            let style = if self.enabled[i] { t.title } else { t.frame };
            let mark = if self.enabled[i] { "[x]" } else { "[ ]" };
            let cursor = if i == self.setup_sel { "›" } else { " " };
            lines.push(Line::from(vec![
                Span::styled(format!("{cursor} {mark} {:<8}", m.title()), style),
                Span::styled(format!("  {}", m.describe()), t.text),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(SETUP_HINT, t.frame)));
        // A kiválasztott sor mindig látszik: fölötte a cím és az üres sor.
        let off = (self.setup_sel + 3).saturating_sub(area.height as usize);
        f.render_widget(Paragraph::new(lines).scroll((off as u16, 0)), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let help = match self.tab {
            0 => OVERVIEW_HELP,
            t if t == self.setup_tab() => SETUP_HELP,
            t => self.modules[t - 1].help(),
        };
        let text = match &self.notice {
            Some(n) => format!(" {n}"),
            None => {
                // A `h help` emlékeztető a modulok `help()` sorai helyett itt
                // készül — egy helyen, és csak ha tényleg kifér.
                let mut s = format!(" {help}");
                if s.chars().count() + HELP_HINT.len() <= area.width as usize {
                    s.push_str(HELP_HINT);
                }
                s
            }
        };
        f.render_widget(Paragraph::new(text).style(self.theme.frame), area);
    }

    /// Az aktív fül kézikönyve; OVERVIEW-é és SETUP-é a héjé.
    fn manual_text(&self) -> &'static str {
        match self.tab {
            0 => OVERVIEW_MANUAL,
            t if t == self.setup_tab() => SETUP_MANUAL,
            t => self.modules[t - 1].manual(),
        }
    }

    /// ponytail: a forrássorok száma a felső korlát, nem a tördelt soroké — a
    /// doboz legfeljebb 74 oszlop, a sorok legfeljebb 70 karakter, így alig
    /// tördel. Ha kellene, a `draw_manual` tördelt sorszáma jöhet ide egy `Cell`-ben.
    fn manual_max_scroll(&self) -> u16 {
        self.manual_text().lines().count().saturating_sub(1) as u16
    }

    /// Középre zárt kézikönyv-ablak az aktuális fül fölött.
    fn draw_manual(&self, f: &mut Frame, area: Rect, scroll: u16) {
        let t = self.theme;
        let text = self.manual_text();
        let lines = text.lines().count() as u16;
        let w = area.width.saturating_sub(6).min(74);
        let h = area.height.saturating_sub(4).min(lines + 4);
        let [row] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
        let [popup] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(row);
        f.render_widget(Clear, popup);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(t.frame)
            .title(Line::from(Span::styled(format!(" MANUAL · {} ", self.tab_title(self.tab)), t.title)));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let [body, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        f.render_widget(
            Paragraph::new(text).style(t.text).wrap(Wrap { trim: false }).scroll((scroll, 0)),
            body,
        );
        f.render_widget(Paragraph::new(MANUAL_HINT).style(t.frame).centered(), hint);
    }
}

/// Blokkok egymás alatt, `Flex::SpaceAround` térközzel (a v2 OVERVIEW elrendezése).
fn render_blocks(f: &mut Frame, blocks: Vec<Vec<Line>>, area: Rect) {
    if blocks.is_empty() {
        return;
    }
    let cons: Vec<Constraint> = blocks.iter().map(|b| Constraint::Length(b.len() as u16)).collect();
    let rects = Layout::vertical(cons).flex(Flex::SpaceAround).split(area);
    for (b, r) in blocks.into_iter().zip(rects.iter()) {
        f.render_widget(Paragraph::new(b), *r);
    }
}

/// Egy fejléc-csoport szélessége oszlopokban.
fn span_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// A riasztás-cím villogása: 10 kép/s, 3 kockánként vált (≈1,7 Hz).
fn blink(since: Instant) -> bool {
    let frame = since.elapsed().as_millis() * ALERT_FPS / 1000;
    (frame / 3) % 2 == 0
}

/// Visible window `[start, end)` of tabs that fits `width` columns, keeping `active` in view.
/// Each tab costs `title.len() + 2 + 1` (" TITLE " label + divider). Prefers showing `active`
/// as the second visible tab so there's context to its left, but clamps to the tab list bounds.
fn visible_tab_window(titles: &[&str], active: usize, width: usize) -> (usize, usize) {
    let n = titles.len();
    if n == 0 {
        return (0, 0);
    }
    let cost = |i: usize| titles[i].len() + 2 + 1;
    let total: usize = (0..n).map(cost).sum();
    if total <= width {
        return (0, n);
    }
    let fit_end = |start: usize| -> usize {
        let mut w = 0usize;
        let mut end = start;
        while end < n {
            let c = cost(end);
            if end > start && w + c > width {
                break;
            }
            w += c;
            end += 1;
        }
        end
    };
    let mut start = active.saturating_sub(1);
    let mut end = fit_end(start);
    while active >= end && start + 1 < n {
        start += 1;
        end = fit_end(start);
    }
    (start, end)
}

/// Közös teszt-`Ctx` a modulok tesztjeihez is: néma hang, üres config, üres tábla.
#[cfg(test)]
pub fn test_ctx(table: toml::Table) -> (Ctx, Receiver<Notice>) {
    use crate::module::{Blackboard, ModuleConfig};
    use std::sync::Arc;
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    let rt = RT.get_or_init(|| tokio::runtime::Builder::new_current_thread().build().unwrap());
    let (notify, rx) = std::sync::mpsc::channel();
    let ctx = Ctx {
        rt: rt.handle().clone(),
        config: Arc::new(ModuleConfig(table)),
        audio: Arc::new(crate::audio::Audio::silent()),
        board: Blackboard::new(),
        notify,
    };
    (ctx, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use std::sync::{Arc, Mutex};

    const TITLES: [&str; 6] = ["OVERVIEW", "STAT", "WEATHER", "RADIO", "NET", "CLOCK"];

    #[test]
    fn all_tabs_fit_when_wide_enough() {
        assert_eq!(visible_tab_window(&TITLES, 0, 200), (0, 6));
    }

    #[test]
    fn narrow_window_keeps_last_active_visible() {
        // enough width for roughly 3 tabs; active is the last tab (index 5 of 6)
        let (start, end) = visible_tab_window(&TITLES, 5, 20);
        assert_eq!(end, 6);
        assert!(start <= 5 && 5 < end);
    }

    #[test]
    fn active_zero_starts_at_zero() {
        let (start, _end) = visible_tab_window(&TITLES, 0, 20);
        assert_eq!(start, 0);
    }

    // ---- fake modul a héj-tesztekhez ----

    #[derive(Default)]
    struct Log {
        local: Vec<KeyCode>,
        global: Vec<KeyCode>,
        starts: Vec<&'static str>,
        polls: Vec<&'static str>,
    }

    struct Fake {
        id: &'static str,
        title: &'static str,
        /// Ezt a kulcsot az aktív fülön elfogyasztja.
        eats: Option<KeyCode>,
        /// Ezt globálisan fogyasztja el.
        eats_global: Option<KeyCode>,
        log: Arc<Mutex<Log>>,
    }

    impl Fake {
        fn new(id: &'static str, title: &'static str, log: Arc<Mutex<Log>>) -> Self {
            Self { id, title, eats: None, eats_global: None, log }
        }
    }

    impl Module for Fake {
        fn id(&self) -> &'static str {
            self.id
        }
        fn title(&self) -> &'static str {
            self.title
        }
        fn help(&self) -> &'static str {
            "fake"
        }
        fn describe(&self) -> &'static str {
            "a fake module for the shell tests"
        }
        fn manual(&self) -> &'static str {
            "FAKE MANUAL\nsecond line of the fake manual"
        }
        fn start(&mut self, _ctx: &Ctx) {
            self.log.lock().unwrap().starts.push(self.id);
        }
        fn poll(&mut self, _ctx: &Ctx) -> usize {
            self.log.lock().unwrap().polls.push(self.id);
            0
        }
        fn on_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> bool {
            self.log.lock().unwrap().local.push(key.code);
            Some(key.code) == self.eats
        }
        fn on_global_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> bool {
            self.log.lock().unwrap().global.push(key.code);
            Some(key.code) == self.eats_global
        }
        fn draw(&self, _f: &mut Frame, _area: Rect, _t: Theme) {}
    }

    fn shell(mods: Vec<Box<dyn Module>>) -> (Shell, std::sync::mpsc::Sender<Notice>) {
        shell_cfg(mods, toml::Table::new())
    }

    /// A teszt-héj sosem a valódi `config.toml`-t írja: minden hívás saját fájlt kap.
    fn shell_cfg(mods: Vec<Box<dyn Module>>, table: toml::Table) -> (Shell, std::sync::mpsc::Sender<Notice>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let (ctx, rx) = test_ctx(table);
        let notify = ctx.notify.clone();
        let mut s = Shell::new(mods, Theme::new(ThemeKind::Color), None, ctx, rx);
        let dir = std::env::temp_dir().join("pipboy-test-shell");
        std::fs::create_dir_all(&dir).unwrap();
        s.config_path = dir.join(format!("config-{}.toml", N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&s.config_path);
        (s, notify)
    }

    fn two_fakes(log: &Arc<Mutex<Log>>) -> Vec<Box<dyn Module>> {
        vec![Box::new(Fake::new("a", "A", log.clone())), Box::new(Fake::new("b", "B", log.clone()))]
    }

    fn disabled(ids: &str) -> toml::Table {
        format!("[shell]\ndisabled = [{ids}]\n").parse().unwrap()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tabs_wrap_both_ways_and_jump_by_number() {
        let log = Arc::new(Mutex::new(Log::default()));
        let mods: Vec<Box<dyn Module>> = vec![
            Box::new(Fake::new("a", "A", log.clone())),
            Box::new(Fake::new("b", "B", log.clone())),
        ];
        let (mut s, _n) = shell(mods);
        assert_eq!(s.tab, 0, "OVERVIEW indul");
        s.on_key(key(KeyCode::Left));
        assert_eq!(s.tab, 3, "körbe balra az utolsó fülre: SETUP");
        s.on_key(key(KeyCode::Right));
        assert_eq!(s.tab, 0);
        s.on_key(key(KeyCode::Char('2')));
        assert_eq!(s.tab, 1);
        s.on_key(key(KeyCode::Char('1')));
        assert_eq!(s.tab, 0);
        s.on_key(key(KeyCode::Char('9')));
        assert_eq!(s.tab, 0, "nem létező fül: marad");
    }

    #[test]
    fn quit_keys() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell(vec![Box::new(Fake::new("a", "A", log))]);
        assert!(s.on_key(key(KeyCode::Char('q'))));
        assert!(!s.on_key(key(KeyCode::Esc)), "Esc is free for modules, never quits");
        assert!(s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn activate_notice_switches_tab_and_footer_shows_notice() {
        let log = Arc::new(Mutex::new(Log::default()));
        let mods: Vec<Box<dyn Module>> = vec![
            Box::new(Fake::new("a", "A", log.clone())),
            Box::new(Fake::new("b", "B", log.clone())),
        ];
        let (mut s, notify) = shell(mods);
        notify.send(Notice::Activate("b")).unwrap();
        notify.send(Notice::Footer("hello".into())).unwrap();
        notify.send(Notice::Alert(Some(" BOOM "))).unwrap();
        s.drain_notices();
        assert_eq!(s.tab, 2);
        assert_eq!(s.notice.as_deref(), Some("hello"));
        assert!(s.alert.is_some());
        notify.send(Notice::Activate("nincs")).unwrap();
        notify.send(Notice::Alert(None)).unwrap();
        s.drain_notices();
        assert_eq!(s.tab, 2, "ismeretlen id nem vált fület");
        assert!(s.alert.is_none());
    }

    #[test]
    fn consumed_key_never_reaches_the_shell() {
        let log = Arc::new(Mutex::new(Log::default()));
        let mut a = Fake::new("a", "A", log.clone());
        a.eats = Some(KeyCode::Right);
        let (mut s, _n) = shell(vec![Box::new(a), Box::new(Fake::new("b", "B", log.clone()))]);
        s.tab = 1;
        s.on_key(key(KeyCode::Right));
        assert_eq!(s.tab, 1, "az aktív modul elfogyasztotta a → gombot");
        assert!(log.lock().unwrap().global.is_empty(), "on_global_key sem látta");
        s.on_key(key(KeyCode::Left));
        assert_eq!(s.tab, 0, "a nem fogyasztott gomb a héjhoz jut");
    }

    #[test]
    fn unconsumed_key_reaches_every_on_global_key() {
        let log = Arc::new(Mutex::new(Log::default()));
        let mut b = Fake::new("b", "B", log.clone());
        b.eats_global = Some(KeyCode::Char('m'));
        let (mut s, _n) = shell(vec![Box::new(Fake::new("a", "A", log.clone())), Box::new(b)]);
        s.start_enabled();
        s.tab = 1;
        assert!(!s.on_key(key(KeyCode::Char('m'))));
        let l = log.lock().unwrap();
        assert_eq!(l.local, vec![KeyCode::Char('m')], "előbb az aktív modul");
        assert_eq!(l.global, vec![KeyCode::Char('m'), KeyCode::Char('m')], "aztán minden modul globálisan");
    }

    #[test]
    fn started_then_disabled_module_still_answers_global_keys_never_started_does_not() {
        let log = Arc::new(Mutex::new(Log::default()));
        let mods: Vec<Box<dyn Module>> = vec![
            Box::new(Fake::new("a", "A", log.clone())),
            Box::new(Fake::new("b", "B", log.clone())),
        ];
        let (mut s, _n) = shell_cfg(mods, disabled("\"b\""));
        s.start_enabled();
        assert_eq!(log.lock().unwrap().starts, vec!["a"], "csak \"a\" indul, \"b\" kikapcsolva sosem");
        s.toggle(0); // "a" kikapcsolása azután, hogy már elindult
        assert!(s.started[0], "started stays started");
        log.lock().unwrap().global.clear();
        s.on_key(key(KeyCode::Char('x')));
        assert_eq!(
            log.lock().unwrap().global.len(),
            1,
            "csak az elindított \"a\" kapja a globális kulcsot, a sosem indított \"b\" nem"
        );
    }

    /// Füstteszt: a valódi registry minden füle kirajzolható üres adattal is,
    /// a legkisebb és a nagy ablakban egyaránt (nincs panic, nincs alulcsordulás).
    #[test]
    fn draws_every_tab_at_common_sizes_without_panic() {
        let (ctx, rx) = test_ctx(toml::Table::new());
        let registry: Vec<Box<dyn Module>> = vec![
            Box::new(crate::modules::stat::Stat::new()),
            Box::new(crate::modules::weather::Weather::new()),
            Box::new(crate::modules::radio::Radio::new()),
            Box::new(crate::modules::net::Net::new()),
            Box::new(crate::modules::clock::Clock::new()),
            Box::new(crate::modules::news::News::new()),
            Box::new(crate::modules::notes::Notes::new()),
        ];
        let n = registry.len();
        let mut s = Shell::new(registry, Theme::new(ThemeKind::Color), None, ctx, rx);
        s.alert = Some((" ▶ TIMER DONE ◀ ", Instant::now()));
        for (w, h) in [(39u16, 11u16), (40, 12), (80, 24), (100, 30), (120, 40)] {
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            for tab in 0..=n + 1 {
                s.tab = tab;
                term.draw(|f| s.draw(f)).unwrap();
            }
        }
    }

    #[test]
    fn startup_notices_first_wins_and_the_config_notice_beats_them() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, notify) = shell(vec![Box::new(Fake::new("a", "A", log))]);
        notify.send(Notice::Footer("first".into())).unwrap();
        notify.send(Notice::Footer("second".into())).unwrap();
        s.drain_startup_notices();
        assert_eq!(s.notice.as_deref(), Some("first"), "induláskor az első nyer");
        notify.send(Notice::Footer("later".into())).unwrap();
        s.drain_notices();
        assert_eq!(s.notice.as_deref(), Some("later"), "futás közben az utolsó");

        let (ctx, rx) = test_ctx(toml::Table::new());
        let notify = ctx.notify.clone();
        let mut s = Shell::new(vec![], Theme::new(ThemeKind::Color), Some("config".into()), ctx, rx);
        notify.send(Notice::Footer("module".into())).unwrap();
        s.drain_startup_notices();
        assert_eq!(s.notice.as_deref(), Some("config"), "a fájl szintű üzenet elsőbbséget élvez");
    }

    /// Fejléc-költségvetés: registry-sorrendű kérdezés és rajzolás egyaránt, és a
    /// szerződését figyelmen kívül hagyó modul kimarad (a sor sosem lóg túl).
    #[test]
    fn header_budget_is_registry_order_and_greedy_modules_are_dropped() {
        struct Hdr(&'static str, String, Arc<Mutex<Vec<(&'static str, u16)>>>);
        impl Module for Hdr {
            fn id(&self) -> &'static str {
                self.0
            }
            fn title(&self) -> &'static str {
                self.0
            }
            fn help(&self) -> &'static str {
                ""
            }
            fn header(&self, width: u16, _t: Theme) -> Vec<Span<'static>> {
                self.2.lock().unwrap().push((self.0, width));
                vec![Span::raw(self.1.clone())]
            }
            fn draw(&self, _f: &mut Frame, _area: Rect, _t: Theme) {}
        }
        let asked = Arc::new(Mutex::new(Vec::new()));
        // "bat" first in the registry, as STAT is in main.rs: it reserves its
        // budget before the greedy module downstream sees a shrunk remainder.
        let mods: Vec<Box<dyn Module>> = vec![
            Box::new(Hdr("bat", "BAT 42%".into(), asked.clone())),
            Box::new(Hdr("greedy", "G".repeat(200), asked.clone())),
        ];
        let (ctx, rx) = test_ctx(toml::Table::new());
        let s = Shell::new(mods, Theme::new(ThemeKind::Color), None, ctx, rx);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();

        let asked = asked.lock().unwrap();
        assert_eq!(asked[0].0, "bat", "registry-sorrend: a korai, fix méretű elem foglal előbb");
        assert_eq!(asked[1].0, "greedy");
        assert!(asked[1].1 < asked[0].1, "a maradék költségvetés fogy: {asked:?}");

        let row: String = (0..100u16).map(|x| term.backend().buffer()[(x, 1)].symbol()).collect();
        assert!(row.contains("BAT 42%"), "a szerződést tartó modul látszik: {row:?}");
        assert!(!row.contains("GG"), "a költségvetést túllépő modul kimarad: {row:?}");
    }

    /// F1 valós modulokkal és a valós fülsor-szélességgel (7 fül + OVERVIEW + SETUP):
    /// STAT elébb foglal (fix méretű `BAT`), RADIO csak a maradékot kapja.
    /// 88×24-nél a maradék 11 oszlop: BAT látszik, a rádió kimarad; 128-nál
    /// mindkettő látszik. Fordított sorrendnél a rádió enné meg a 11 oszlopot.
    #[test]
    fn header_shows_stat_battery_and_radio_in_real_registry_order() {
        use crate::modules::radio::{Radio, RadioCfg, Station};
        use crate::modules::stat::Stat;
        use crate::radio::RadioStatus;
        use crate::stat::{BatteryInfo, StatSnapshot};

        let (ctx, rx) = test_ctx(toml::Table::new());
        let mut stat = Stat::new();
        stat.snap = Some(StatSnapshot { battery: Some(BatteryInfo { pct: 42, charging: false }), ..Default::default() });

        let mut radio = Radio::new();
        radio.stations = RadioCfg::default().station;
        radio.stations[0] = Station { name: "Radio Paradise".into(), url: "https://stream.radioparadise.com/mp3-128".into() };
        radio.state.current = Some(0);
        radio.state.status = RadioStatus::Playing;
        radio.state.since = Some(Instant::now());

        let log = Arc::new(Mutex::new(Log::default()));
        let filler = |id: &'static str, title: &'static str| -> Box<dyn Module> { Box::new(Fake::new(id, title, log.clone())) };
        let registry: Vec<Box<dyn Module>> = vec![
            Box::new(stat),
            filler("weather", "WEATHER"),
            Box::new(radio),
            filler("net", "NET"),
            filler("clock", "CLOCK"),
            filler("news", "NEWS"),
            filler("notes", "NOTES"),
        ];
        let s = Shell::new(registry, Theme::new(ThemeKind::Color), None, ctx, rx);
        let header_row = |w: u16| -> String {
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 24)).unwrap();
            term.draw(|f| s.draw(f)).unwrap();
            (0..w).map(|x| term.backend().buffer()[(x, 1)].symbol()).collect()
        };

        let narrow = header_row(88);
        assert!(narrow.contains("BAT 42%"), "88 cols: STAT reserves first: {narrow:?}");
        assert!(!narrow.contains('♪'), "88 cols: RADIO gets the 4-column remainder and yields: {narrow:?}");

        let wide = header_row(128);
        assert!(wide.contains("BAT 42%"), "128 cols: STAT visible: {wide:?}");
        assert!(wide.contains('♪'), "128 cols: RADIO visible: {wide:?}");
    }

    #[test]
    fn fast_frames_asks_every_module_with_the_active_flag() {
        struct Fast;
        impl Module for Fast {
            fn id(&self) -> &'static str {
                "fast"
            }
            fn title(&self) -> &'static str {
                "FAST"
            }
            fn help(&self) -> &'static str {
                ""
            }
            fn wants_fast_frames(&self, active: bool) -> bool {
                active
            }
            fn draw(&self, _f: &mut Frame, _area: Rect, _t: Theme) {}
        }
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell(vec![Box::new(Fake::new("a", "A", log)), Box::new(Fast)]);
        s.tab = 1;
        assert!(!s.wants_fast_frames(), "a FAST füle nem aktív");
        s.tab = 2;
        assert!(s.wants_fast_frames());
        s.tab = 0;
        assert!(s.wants_fast_frames(), "OVERVIEW minden modulnak aktív");
    }
    #[test]
    fn disabled_module_is_not_started_polled_and_has_no_tab() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell_cfg(two_fakes(&log), disabled("\"b\""));
        s.start_enabled();
        s.poll_modules();
        let l = log.lock().unwrap();
        assert_eq!(l.starts, vec!["a"], "a kikapcsolt modul szálai el sem indulnak");
        assert_eq!(l.polls, vec!["a"]);
        drop(l);
        assert_eq!(s.titles(), vec!["OVERVIEW", "A", "SETUP"], "nincs füle a kikapcsoltnak");
    }

    #[test]
    fn number_keys_and_arrows_skip_disabled_modules() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, notify) = shell_cfg(two_fakes(&log), disabled("\"a\""));
        s.on_key(key(KeyCode::Char('2')));
        assert_eq!(s.tab, 2, "a 2-es a B fül: az A kimarad");
        s.on_key(key(KeyCode::Char('3')));
        assert_eq!(s.tab, 3, "a 3-as a SETUP");
        s.on_key(key(KeyCode::Right));
        assert_eq!(s.tab, 0, "körbe OVERVIEW-ra");
        s.on_key(key(KeyCode::Right));
        assert_eq!(s.tab, 2, "a nyíl átugorja a kikapcsolt A-t");
        s.on_key(key(KeyCode::Char('0')));
        assert_eq!(s.tab, 3, "0 = SETUP");
        notify.send(Notice::Activate("a")).unwrap();
        s.drain_notices();
        assert_eq!(s.tab, 3, "kikapcsolt modul Activate-je nem vált fület");
    }

    #[test]
    fn setup_toggle_enables_disables_and_starts_once() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell_cfg(two_fakes(&log), disabled("\"b\""));
        s.start_enabled();
        s.tab = s.setup_tab();

        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(s.enabled, vec![true, true]);
        assert_eq!(s.notice.as_deref(), Some("saved: b enabled"));
        assert_eq!(log.lock().unwrap().starts, vec!["a", "b"], "bekapcsoláskor indul");

        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(s.enabled, vec![true, true]);
        assert_eq!(log.lock().unwrap().starts, vec!["a", "b"], "újra-bekapcsolás nem indít másodszor");

        s.on_key(key(KeyCode::Up));
        s.on_key(key(KeyCode::Enter));
        assert_eq!(s.enabled, vec![false, true], "Enter ugyanaz, mint a szóköz");
        let text = std::fs::read_to_string(&s.config_path).unwrap();
        assert!(text.contains("disabled = [\"a\"]"), "a mentés a fájlba is kiíródik: {text:?}");

        s.on_key(key(KeyCode::Char('a')));
        assert_eq!(s.enabled, vec![true, true], "a = mindet be");
        assert_eq!(s.notice.as_deref(), Some("saved: all modules enabled"));
    }

    #[test]
    fn disabling_the_active_tab_falls_back_to_overview() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell(two_fakes(&log));
        s.tab = 1;
        s.toggle(0);
        assert!(!s.enabled[0]);
        assert_eq!(s.tab, 0, "a kikapcsolt aktív fülről OVERVIEW-ra ugrunk");
    }

    #[test]
    fn setup_lists_every_module_with_its_description() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell_cfg(two_fakes(&log), disabled("\"b\""));
        s.tab = s.setup_tab();
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let screen: String = (0..24u16)
            .map(|y| (0..100u16).map(|x| term.backend().buffer()[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("SETUP · 1 of 2 modules enabled · config.toml"), "{screen}");
        assert!(screen.contains("[x] A"), "a bekapcsolt pipa: {screen}");
        assert!(screen.contains("[ ] B"), "a kikapcsolt üres: {screen}");
        assert!(screen.contains("a fake module for the shell tests"), "describe() látszik: {screen}");
        assert!(screen.contains(SETUP_HINT), "{screen}");
        assert!(screen.contains("space toggle"), "a SETUP súghatója: {screen}");
    }

    // ---- kézikönyv-ablak ----

    fn screen_of(s: &Shell, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        (0..h)
            .map(|y| (0..w).map(|x| term.backend().buffer()[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn h_opens_the_manual_overlay_and_h_closes_it() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell(two_fakes(&log));
        s.tab = 1;
        assert!(!screen_of(&s, 100, 24).contains("FAKE MANUAL"), "csukva nem látszik");

        s.on_key(key(KeyCode::Char('h')));
        assert_eq!(s.manual, Some(0));
        let open = screen_of(&s, 100, 24);
        assert!(open.contains("FAKE MANUAL"), "{open}");
        assert!(open.contains("second line of the fake manual"), "{open}");
        assert!(open.contains("MANUAL · A"), "a fül neve a címben: {open}");
        assert!(open.contains(MANUAL_HINT), "{open}");

        s.on_key(key(KeyCode::Char('h')));
        assert_eq!(s.manual, None);
        assert!(!screen_of(&s, 100, 24).contains("FAKE MANUAL"));
    }

    #[test]
    fn an_open_manual_swallows_every_key_including_q() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell(two_fakes(&log));
        s.start_enabled();
        s.tab = 1;
        s.on_key(key(KeyCode::Char('h')));
        log.lock().unwrap().local.clear();
        log.lock().unwrap().global.clear();

        assert!(!s.on_key(key(KeyCode::Char('q'))), "a q nem lép ki, csak becsukja");
        assert_eq!(s.manual, None);
        assert_eq!(s.tab, 1, "és nem is vált fület");
        let l = log.lock().unwrap();
        assert!(l.local.is_empty() && l.global.is_empty(), "a modulok nem látják a kulcsot");
    }

    #[test]
    fn a_module_that_consumes_h_keeps_the_overlay_closed() {
        let log = Arc::new(Mutex::new(Log::default()));
        let mut a = Fake::new("a", "A", log.clone());
        a.eats = Some(KeyCode::Char('h'));
        let (mut s, _n) = shell(vec![Box::new(a), Box::new(Fake::new("b", "B", log.clone()))]);
        s.tab = 1;
        s.on_key(key(KeyCode::Char('h')));
        assert_eq!(s.manual, None, "a TERM/NOTES-modell: az aktív modul elfogyasztotta");
        s.tab = 2;
        s.on_key(key(KeyCode::Char('h')));
        assert_eq!(s.manual, Some(0), "a szomszéd fülön viszont nyílik");
    }

    #[test]
    fn manual_overlay_draws_at_extreme_sizes() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell(two_fakes(&log));
        for tab in [0usize, 1, 3] {
            s.tab = tab;
            s.manual = Some(0);
            for (w, h) in [(1u16, 1u16), (40, 12), (39, 11), (80, 24), (200, 60)] {
                screen_of(&s, w, h);
            }
        }
    }

    #[test]
    fn manual_scroll_clamps_at_both_ends() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell(two_fakes(&log));
        s.tab = 1; // a Fake kézikönyve két sor
        s.on_key(key(KeyCode::Char('h')));
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.manual, Some(0), "0 alá nem megy");
        for _ in 0..10 {
            s.on_key(key(KeyCode::Down));
        }
        assert_eq!(s.manual, Some(1), "a sorok száma a korlát");
        assert!(screen_of(&s, 100, 24).contains("second line"), "görgetve is látszik valami");
    }

    #[test]
    fn the_footer_offers_the_manual_when_it_fits() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut s, _n) = shell(two_fakes(&log));
        s.tab = 1;
        assert!(screen_of(&s, 100, 24).contains("h help"), "széles ablakban kifér");
        let narrow = screen_of(&s, 40, 12);
        assert!(narrow.contains("fake"), "a help() maga megmarad: {narrow}");
    }

    /// Minden valódi modulnak van kézikönyve, és minden sora belefér a dobozba.
    #[test]
    fn every_module_has_a_manual_within_the_line_budget() {
        let registry: Vec<Box<dyn Module>> = vec![
            Box::new(crate::modules::stat::Stat::new()),
            Box::new(crate::modules::weather::Weather::new()),
            Box::new(crate::modules::radio::Radio::new()),
            Box::new(crate::modules::music::Music::new()),
            Box::new(crate::modules::net::Net::new()),
            Box::new(crate::modules::wifi::Wifi::new()),
            Box::new(crate::modules::wasteland::Wasteland::new()),
            Box::new(crate::modules::clock::Clock::new()),
            Box::new(crate::modules::dosimeter::Dosimeter::new()),
            Box::new(crate::modules::news::News::new()),
            Box::new(crate::modules::mail::Mail::new()),
            Box::new(crate::modules::notes::Notes::new()),
            Box::new(crate::modules::syslog::Syslog::new()),
            Box::new(crate::modules::art::Art::new()),
            Box::new(crate::modules::globe::Globe::new()),
            Box::new(crate::modules::term::Term::new()),
        ];
        let check = |title: &str, text: &str| {
            assert!(!text.trim().is_empty(), "{title}: üres kézikönyv");
            for line in text.lines() {
                let n = line.chars().count();
                assert!(n <= 70, "{title}: {n} karakteres sor: {line:?}");
            }
        };
        for m in &registry {
            check(m.title(), m.manual());
        }
        check(OVERVIEW_TITLE, OVERVIEW_MANUAL);
        check(SETUP_TITLE, SETUP_MANUAL);
    }
}
