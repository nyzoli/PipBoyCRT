//! WASTELAND: two views of the same network, swapped with `v`.
//!
//! [`local`] (LOCAL NET) lists the devices around you, [`conn`] (CONN) the
//! connections your own machine has open. They share nothing but the tab: each
//! keeps its own source thread, its own cadence and its own config section
//! (`[wasteland]`, `[comms]`). This wrapper is the only thing the shell sees —
//! it forwards every call to whichever view is on screen.

pub mod conn;
pub mod local;

use crate::module::{Ctx, Module, Slot};
use crate::style::Theme;
use conn::ConnView;
use local::LocalNet;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Local,
    Conn,
}

pub struct Wasteland {
    view: View,
    local: LocalNet,
    conn: ConnView,
    /// `[comms] enabled`, known once `start` has run. `false` pins the tab to
    /// LOCAL NET: `v` then has nowhere to go.
    conn_enabled: bool,
}

impl Wasteland {
    pub fn new() -> Self {
        Self { view: View::Local, local: LocalNet::new(), conn: ConnView::new(), conn_enabled: true }
    }
}

impl Module for Wasteland {
    fn id(&self) -> &'static str {
        local::ID
    }
    fn title(&self) -> &'static str {
        "WASTELAND"
    }
    fn describe(&self) -> &'static str {
        "Devices on your local network and who your machine talks to"
    }
    fn manual(&self) -> &'static str {
        "\
WASTELAND shows who else is out there. Two views; v swaps
them (a rename or filter prompt keeps the key, of course).

LOCAL NET - every device on your network, with its name,
vendor, MAC and when it was last seen. A newcomer is announced.
  ↑/↓   pick a device       enter  details
  n     rename it (the name sticks to the MAC, not the IP)
  s     ping sweep on and off for this session
  r     rescan now          p      ping it once, in details

CONN - the connections your machine has open, by process.
  ↑/↓   pick a row          enter  details
  s     sort by process, remote or traffic
  f     filter              l      show or hide loopback
  r     re-read the table now

Know your neighbours. First rule of the wasteland."
    }
    fn help(&self) -> &'static str {
        match self.view {
            View::Local => self.local.help(),
            View::Conn => self.conn.help(),
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        self.local.start(ctx);
        // A no-op when `[comms] enabled = false` — no thread, nothing read.
        self.conn.start(ctx);
        self.conn_enabled = self.conn.enabled;
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let conn = if self.conn_enabled { self.conn.poll(ctx) } else { 0 };
        self.local.poll(ctx) + conn
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        // The active view gets first refusal, so a `v` typed into its rename or
        // filter prompt stays a letter instead of swapping the view underneath.
        let consumed = match self.view {
            View::Local => self.local.on_key(key, ctx),
            View::Conn => self.conn.on_key(key, ctx),
        };
        if consumed {
            return true;
        }
        if self.conn_enabled && key.code == KeyCode::Char('v') {
            self.view = match self.view {
                View::Local => View::Conn,
                View::Conn => View::Local,
            };
            return true;
        }
        false
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        match self.view {
            View::Local => self.local.draw(f, area, t),
            View::Conn => self.conn.draw(f, area, t),
        }
    }

    fn header(&self, width: u16, t: Theme) -> Vec<Span<'static>> {
        self.local.header(width, t)
    }

    fn overview(&self, width: u16, height: u16, t: Theme) -> Vec<Line<'static>> {
        let mut lines = self.local.overview(width, height, t);
        if self.conn_enabled && lines.len() < height as usize {
            lines.push(self.conn.summary_line(width));
        }
        lines
    }

    fn overview_slot(&self) -> Slot {
        self.local.overview_slot()
    }

    fn status(&self) -> String {
        if !self.conn_enabled {
            return self.local.status();
        }
        format!("{} | {}", self.local.status(), self.conn.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    #[test]
    fn v_swaps_the_view_but_a_prompt_keeps_it() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut m = Wasteland::new();
        assert_eq!(m.view, View::Local);
        assert!(m.help().starts_with("v conn"));

        assert!(m.on_key(key(KeyCode::Char('v')), &ctx));
        assert_eq!(m.view, View::Conn);
        assert!(m.help().starts_with("v local net"));
        assert!(m.on_key(key(KeyCode::Char('v')), &ctx));
        assert_eq!(m.view, View::Local);

        // LOCAL NET's rename prompt owns every letter, `v` included.
        m.local.open_rename_for_test();
        assert!(m.on_key(key(KeyCode::Char('v')), &ctx));
        assert_eq!(m.view, View::Local, "the v went into the name, not into the view");
        assert!(m.on_key(key(KeyCode::Esc), &ctx), "esc closes the prompt");

        // And so does CONN's filter prompt.
        m.view = View::Conn;
        assert!(m.on_key(key(KeyCode::Char('f')), &ctx));
        assert!(m.on_key(key(KeyCode::Char('v')), &ctx));
        assert_eq!(m.view, View::Conn, "the v went into the filter");
        assert!(m.on_key(key(KeyCode::Esc), &ctx));
        assert!(m.on_key(key(KeyCode::Char('v')), &ctx));
        assert_eq!(m.view, View::Local);

        // Switched off in config, the tab is LOCAL NET and nothing else.
        m.conn_enabled = false;
        assert!(!m.on_key(key(KeyCode::Char('v')), &ctx), "unconsumed — the shell may have a use for it");
        assert_eq!(m.view, View::Local);
    }

    #[test]
    fn overview_holds_both_views_when_there_is_room() {
        let t = Theme::new(ThemeKind::Color);
        let m = Wasteland::new();
        let plain = |l: &Line| l.spans.iter().map(|s| s.content.to_string()).collect::<String>();

        let tall: Vec<String> = m.overview(40, 5, t).iter().map(plain).collect();
        assert_eq!(tall[0].trim(), "WASTELAND");
        assert!(tall.iter().any(|l| l.contains("online")), "LOCAL NET's own lines: {tall:?}");
        assert!(tall.last().unwrap().contains("connections"), "CONN's summary line: {tall:?}");

        let short: Vec<String> = m.overview(40, 2, t).iter().map(plain).collect();
        assert_eq!(short.len(), 2);
        assert!(!short.iter().any(|l| l.contains("connections")), "no room for CONN: {short:?}");
        assert_eq!(m.overview_slot(), Slot::Left(5));

        // `status` is one line per view, unless CONN is switched off.
        let mut off = Wasteland::new();
        off.conn_enabled = false;
        assert!(!off.status().contains(" | "));
        assert!(m.status().contains(" | "));
    }
}
