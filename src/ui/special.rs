//! S.P.E.C.I.A.L. character sheet — the STAT tab's second view (`s`).
//!
//! The figure is an original design: no traced mascot, only the block and
//! box-drawing characters the 16-colour CRT font actually has.
use crate::modules::stat::Stat;
use crate::stat::{Special, SpecialInput, ATTRS};
use crate::style::Theme;
use crate::ui::widgets::gauge;
use chrono::{Datelike, Local, Timelike};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use std::time::Instant;

/// The vault dweller: smiling, thumb up, in a jumpsuit. Row 2 carries the eyes.
const FIGURE: [&str; 14] = [
    "         ▄▄▄▄▄▄▄▄",
    "       ▄█▀▀▀▀▀▀▀▀█▄",
    "      ▐█  ■    ■  █▌",
    "      ▐█    ▄▄    █▌",
    "       █▄ ▀▄▄▄▄▀ ▄█",
    "        ▀█▄▄▄▄▄▄█▀",
    "    ▄     ▐████▌",
    "   ▐█▌ ┌──┴┴┴┴┴┴──┐",
    "  ▄███ │ ▄▄▄▄▄▄▄▄ │",
    "  ████▌│   ▌77▐   │",
    "  ▀███▀│ ▀▀▀▀▀▀▀▀ │",
    "       └─┐      ┌─┘",
    "          ██  ██",
    "        ▄██▄▄██▄",
];

/// The same dweller squeezed into eight rows for 80-column windows.
const FIGURE_SMALL: [&str; 8] = [
    "      ▄▄▄▄▄▄▄▄",
    "    ▄█▀▀▀▀▀▀▀▀█▄",
    "   ▐█  ■    ■  █▌",
    "    █▄ ▀▄▄▄▄▀ ▄█",
    " ▄     ▐████▌",
    "▐█▌ ┌──┴┴┴┴┴┴──┐",
    "███ │  ▌ 77 ▐  │",
    "▀█▀ └─┐      ┌─┘",
];

/// Which row of either figure holds the eyes.
const EYE_ROW: usize = 2;

/// A 500 ms blink every two seconds, off the process clock — wide enough to
/// land at least one frame at the shell's 4 fps without asking for fast frames.
fn blinking() -> bool {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    (START.get_or_init(Instant::now).elapsed().as_millis() + 1000) % 2000 < 500
}

fn figure_block(big: bool, blink: bool, sp: &Special, t: Theme) -> Vec<Line<'static>> {
    let rows: &[&str] = if big { &FIGURE } else { &FIGURE_SMALL };
    let head = if big { 6 } else { 4 };
    let mut out: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let text = if i == EYE_ROW && blink { row.replace('■', "─") } else { (*row).to_string() };
            Line::from(Span::styled(text, if i < head { t.title } else { t.value }))
        })
        .collect();
    out.push(Line::default());
    let stats = format!("LVL {} · HP {}", sp.level, sp.hp);
    if big {
        out.push(Line::from(Span::styled(format!(" VAULT DWELLER · {stats}"), t.value)));
    } else {
        out.push(Line::from(Span::styled(" VAULT DWELLER", t.value)));
        out.push(Line::from(Span::styled(format!(" {stats}"), t.value)));
    }
    out
}

fn attr_lines(sp: &Special, t: Theme) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(" S.P.E.C.I.A.L.", t.title)), Line::default()];
    for (i, (letter, name)) in ATTRS.iter().enumerate() {
        let (val, bar, style) = match sp.attrs[i] {
            Some(v) => (format!("{v:>2}"), gauge(v as f32 / 10.0, 10), t.value),
            None => (" ?".to_string(), "░".repeat(10), t.frame),
        };
        out.push(Line::from(vec![
            Span::styled(format!(" {letter}  "), t.title),
            Span::styled(format!("{name:<13}"), t.text),
            Span::styled(format!("{val}  "), style),
            Span::styled(bar, style),
        ]));
    }
    out
}

fn perk_lines(sp: &Special, t: Theme) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(" PERKS", t.title)), Line::default()];
    if sp.perks.is_empty() {
        out.push(Line::from(Span::styled(" none earned right now", t.frame)));
        return out;
    }
    for p in &sp.perks {
        out.push(Line::from(Span::styled(format!(" {}", p.name), t.warn)));
        out.push(Line::from(Span::styled(format!("   {}", p.flavor), t.frame)));
    }
    out
}

/// The perks squeezed onto one wrapped line for the 80-column layout.
fn perk_summary(sp: &Special, t: Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(" PERKS ", t.title)];
    if sp.perks.is_empty() {
        spans.push(Span::styled("none earned right now", t.frame));
    } else {
        spans.push(Span::styled(sp.perks.iter().map(|p| p.name).collect::<Vec<_>>().join(" · "), t.warn));
    }
    Line::from(spans)
}

pub fn draw(f: &mut Frame, m: &Stat, area: Rect, t: Theme) {
    let now = Local::now();
    let sp = Special::new(&SpecialInput {
        stat: m.snap.as_ref(),
        wifi: m.wifi_seen,
        weather: m.weather_code,
        favorites: m.favorites,
        hour: now.hour(),
        ymd: now.year() * 10_000 + now.month() as i32 * 100 + now.day() as i32,
    });

    // Under 80 columns the figure has no room: attributes only.
    if area.width < 80 {
        f.render_widget(Paragraph::new(attr_lines(&sp, t)), area);
        return;
    }
    if area.width < 100 {
        let [body, perks] = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(area);
        let [fig, at] = Layout::horizontal([Constraint::Length(20), Constraint::Min(0)]).areas(body);
        f.render_widget(Paragraph::new(figure_block(false, blinking(), &sp, t)), fig);
        f.render_widget(Paragraph::new(attr_lines(&sp, t)), at);
        f.render_widget(Paragraph::new(perk_summary(&sp, t)).wrap(Wrap { trim: true }), perks);
        return;
    }
    let [fig, at, pk] =
        Layout::horizontal([Constraint::Length(32), Constraint::Length(33), Constraint::Min(0)]).areas(area);
    f.render_widget(Paragraph::new(figure_block(true, blinking(), &sp, t)), fig);
    f.render_widget(Paragraph::new(attr_lines(&sp, t)), at);
    f.render_widget(Paragraph::new(perk_lines(&sp, t)).wrap(Wrap { trim: false }), pk);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use crate::stat::{BatteryInfo, StatSnapshot};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn text_at(m: &Stat, w: u16, h: u16) -> String {
        let t = Theme::new(ThemeKind::Color);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, m, f.area(), t)).unwrap();
        let buf = term.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn draws_at_every_size_without_panicking() {
        let empty = Stat::new();
        for (w, h) in [(1, 1), (40, 12), (79, 24), (80, 24), (100, 30), (120, 40)] {
            let out = text_at(&empty, w, h);
            if w >= 40 {
                assert!(out.contains("STRENGTH"), "{w}x{h} shows the attributes");
                assert!(out.contains('?'), "{w}x{h} shows ? for the missing data");
            }
        }

        let mut m = Stat::new();
        m.snap = Some(StatSnapshot {
            cpu_cores: vec![10.0; 8],
            cpu_mhz: 3600,
            mem_total: 32 << 30,
            uptime_s: 9 * 86_400,
            procs: 300,
            battery: Some(BatteryInfo { pct: 91, charging: true }),
            ..Default::default()
        });
        m.wifi_seen = Some((7, true));
        m.weather_code = Some(0);
        m.favorites = Some(12);
        let out = text_at(&m, 120, 40);
        assert!(out.contains("STRENGTH") && out.contains("VAULT DWELLER"));
        assert!(out.contains("Ghoulish") && out.contains("Cap Collector"), "perks are listed");
        assert!(!out.contains('?'), "with every source present nothing is unknown");
        assert!(text_at(&m, 80, 24).contains("Ghoulish"), "80 columns keeps the perk names");
    }

    #[test]
    fn the_figure_blinks_and_keeps_its_shape() {
        let sp = Special::default();
        let t = Theme::new(ThemeKind::Color);
        let open = figure_block(true, false, &sp, t);
        let shut = figure_block(true, true, &sp, t);
        assert_eq!(open.len(), shut.len());
        assert_ne!(open[EYE_ROW], shut[EYE_ROW], "the eyes close");
        assert_eq!(open[0], shut[0], "the rest of the figure is unchanged");
        assert!(FIGURE.iter().chain(FIGURE_SMALL.iter()).all(|r| r.chars().count() <= 26));
    }
}
