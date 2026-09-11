use crate::modules::radio::Radio;
use crate::radio::RadioStatus;
use crate::style::Theme;
use crate::ui::widgets::{spectrum, vu_bar};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

fn host(url: &str) -> &str {
    url.trim_start_matches("https://").trim_start_matches("http://").split('/').next().unwrap_or(url)
}

/// The now-playing title line, with a trailing `★` while `r.favorited` is set
/// (cleared by the module as soon as the title changes; task R2).
fn title_line(title: &str, favorited: bool, t: Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("   {title}"), t.nowplaying)];
    if favorited {
        spans.push(Span::styled(" \u{2605}", t.title));
    }
    Line::from(spans)
}

pub fn draw(f: &mut Frame, m: &Radio, area: Rect, t: Theme) {
    if area.width < 100 {
        return draw_narrow(f, m, area, t);
    }
    draw_wide(f, m, area, t);
}

fn draw_narrow(f: &mut Frame, m: &Radio, area: Rect, t: Theme) {
    let r = &m.state;
    let [now, list, log] = Layout::vertical([Constraint::Length(6), Constraint::Min(3), Constraint::Length(4)]).areas(area);

    // ---- most szól ----
    let name = if m.current_name().is_empty() { "–" } else { m.current_name() };
    let status = match &r.status {
        RadioStatus::Stopped => Span::styled("■ STOPPED", t.frame),
        RadioStatus::Connecting => Span::styled("… CONNECTING", t.nowplaying),
        RadioStatus::Playing => Span::styled("▶ NOW PLAYING", t.nowplaying),
        RadioStatus::Paused => Span::styled("‖ PAUSED", t.warn),
        RadioStatus::Error(e) => Span::styled(format!("✕ ERROR: {e}"), t.danger),
    };
    let el = r.since.map(|s| s.elapsed().as_secs()).unwrap_or(0);
    let vol_bar = format!("{}{}", "▮".repeat((r.volume / 10) as usize), "▯".repeat(10 - (r.volume / 10) as usize));
    let vol = if r.muted { "MUTE".to_string() } else { format!("{}%", r.volume) };
    let vu_w = (now.width.saturating_sub(28) as usize).min(40);
    let lines = vec![
        Line::from(vec![Span::raw(" "), status]),
        Line::from(vec![
            Span::styled(format!("   {name}"), t.value),
            Span::styled(format!("   {}   {:02}:{:02}:{:02}", r.format, el / 3600, el / 60 % 60, el % 60), t.frame),
        ]),
        title_line(&r.title, r.favorited, t),
        Line::raw(""),
        {
            let mut line = vu_bar((r.level * 300.0).min(100.0) as u8, vu_w, t);
            line.spans.insert(0, Span::raw("   "));
            line.spans.push(Span::styled("   VOL ", t.frame));
            line.spans.push(Span::styled(vol_bar, t.graph));
            line.spans.push(Span::styled(format!(" {vol}"), t.value));
            line
        },
    ];
    f.render_widget(Paragraph::new(lines), now);

    // ---- állomáslista ----
    let items: Vec<ListItem> = m
        .stations
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mark = if r.current == Some(i) { "▶" } else { " " };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {mark} "), t.nowplaying),
                Span::raw(format!("{:<26}", s.name)),
                Span::styled(host(&s.url).to_string(), t.frame),
            ]))
        })
        .collect();
    let header = Line::from(vec![
        Span::styled(" STATIONS ", t.title),
        Span::styled(format!("config.toml · {} stations", m.stations.len()), t.frame),
    ]);
    let [lh, lb] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(list);
    f.render_widget(Paragraph::new(header), lh);
    let mut state = ListState::default().with_selected(Some(r.selected));
    f.render_stateful_widget(List::new(items).highlight_style(t.tab_active), lb, &mut state);

    // ---- napló ----
    let mut lg = vec![Line::from(Span::styled(" LOG", t.title))];
    lg.extend(r.log.iter().map(|s| Line::from(Span::styled(format!("   {s}"), t.frame))));
    f.render_widget(Paragraph::new(lg), log);
}

fn draw_wide(f: &mut Frame, m: &Radio, area: Rect, t: Theme) {
    let r = &m.state;
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    // ---- bal: állomáslista + napló ----
    let [lh, lb, log] = Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(4)]).areas(left);
    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" STATIONS ", t.title),
        Span::styled(format!("config.toml · {} stations", m.stations.len()), t.frame),
    ])), lh);
    let items: Vec<ListItem> = m.stations.iter().enumerate().map(|(i, s)| {
        let mark = if r.current == Some(i) { "▶" } else { " " };
        ListItem::new(Line::from(vec![
            Span::styled(format!(" {mark} "), t.nowplaying),
            Span::raw(format!("{:<26}", s.name)),
            Span::styled(host(&s.url).to_string(), t.frame),
        ]))
    }).collect();
    let mut state = ListState::default().with_selected(Some(r.selected));
    f.render_stateful_widget(List::new(items).highlight_style(t.tab_active), lb, &mut state);
    let mut lg = vec![Line::from(Span::styled(" LOG", t.title))];
    lg.extend(r.log.iter().map(|s| Line::from(Span::styled(format!("   {s}"), t.frame))));
    f.render_widget(Paragraph::new(lg), log);

    // ---- jobb: most szól + nagy VU + hangerő ----
    let [now, vu, vol] = Layout::vertical([Constraint::Length(5), Constraint::Min(4), Constraint::Length(2)]).areas(right);
    let name = if m.current_name().is_empty() { "–" } else { m.current_name() };
    let status = match &r.status {
        RadioStatus::Stopped => Span::styled("■ STOPPED", t.frame),
        RadioStatus::Connecting => Span::styled("… CONNECTING", t.nowplaying),
        RadioStatus::Playing => Span::styled("▶ NOW PLAYING", t.nowplaying),
        RadioStatus::Paused => Span::styled("‖ PAUSED", t.warn),
        RadioStatus::Error(e) => Span::styled(format!("✕ ERROR: {e}"), t.danger),
    };
    let el = r.since.map(|s| s.elapsed().as_secs()).unwrap_or(0);
    f.render_widget(Paragraph::new(vec![
        Line::from(vec![Span::raw(" "), status]),
        Line::from(Span::styled(format!("   {name}"), t.value)),
        Line::from(Span::styled(format!("   {}   {:02}:{:02}:{:02}", r.format, el / 3600, el / 60 % 60, el % 60), t.frame)),
        title_line(&r.title, r.favorited, t),
    ]), now);

    let [sh, sb] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(vu);
    f.render_widget(Paragraph::new(Line::from(Span::styled(" SPECTRUM ", t.title))), sh);
    f.render_widget(Paragraph::new(spectrum(&r.spectrum, sb, t)), sb);

    let vol_bar = format!("{}{}", "▮".repeat((r.volume / 10) as usize), "▯".repeat(10 - (r.volume / 10) as usize));
    let vol_txt = if r.muted { "MUTE".to_string() } else { format!("{}%", r.volume) };
    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled("   VOL ", t.frame), Span::styled(vol_bar, t.graph), Span::styled(format!(" {vol_txt}"), t.value),
    ])), vol);
}
