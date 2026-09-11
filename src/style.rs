use crate::config::ThemeKind;
use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub title: Style,
    pub tab_active: Style,
    pub value: Style,
    pub graph: Style,
    pub frame: Style,
    pub warn: Style,
    pub danger: Style,
    pub nowplaying: Style,
    pub text: Style,
    pub vu_low: Style,
    pub vu_mid: Style,
    pub vu_high: Style,
}

impl Theme {
    pub fn new(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Color => Self::color(),
            ThemeKind::Mono => Self::mono(),
        }
    }

    fn color() -> Self {
        Self {
            title: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            tab_active: Style::new().fg(Color::Black).bg(Color::Green),
            value: Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            graph: Style::new().fg(Color::Green),
            frame: Style::new().fg(Color::DarkGray),
            warn: Style::new().fg(Color::Yellow),
            danger: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            nowplaying: Style::new().fg(Color::Magenta),
            text: Style::new(),
            vu_low: Style::new().fg(Color::Green),
            vu_mid: Style::new().fg(Color::Yellow),
            vu_high: Style::new().fg(Color::Red),
        }
    }

    fn mono() -> Self {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        Self {
            title: bold,
            tab_active: Style::new().add_modifier(Modifier::REVERSED),
            value: bold,
            graph: Style::new(),
            frame: Style::new().add_modifier(Modifier::DIM),
            warn: bold,
            danger: Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            nowplaying: bold,
            text: Style::new(),
            vu_low: Style::new().add_modifier(Modifier::DIM),
            vu_mid: Style::new(),
            vu_high: Style::new().add_modifier(Modifier::BOLD),
        }
    }

    /// Terhelés-százalék stílusa: 80 felett warn, 90 felett danger.
    pub fn load(&self, pct: f32) -> Style {
        if pct >= 90.0 {
            self.danger
        } else if pct >= 80.0 {
            self.warn
        } else {
            self.value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_thresholds() {
        let t = Theme::new(ThemeKind::Color);
        assert_eq!(t.load(79.9), t.value);
        assert_eq!(t.load(80.0), t.warn);
        assert_eq!(t.load(90.0), t.danger);
    }
}
