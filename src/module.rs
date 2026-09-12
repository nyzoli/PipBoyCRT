//! Module contract: everything a tab/feature needs to plug into the shell.
//!
//! A module owns its state, its background sources and its drawing. The shell
//! (`main.rs`/`shell.rs`) owns the frame, the tab strip, the footer, the
//! OVERVIEW composition and key routing. Modules never reference each other
//! directly: shared data goes through the [`Blackboard`], shared services
//! (audio, runtime, config) come from [`Ctx`], and requests towards the shell
//! go through [`Notice`].
//!
//! Adding a module = one file implementing [`Module`] + one line in the
//! registry in `main.rs`. See `docs/adding-a-module.md`.

use crate::style::Theme;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde::de::DeserializeOwned;
use std::any::Any;
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};

/// Where a module's compact block goes on the OVERVIEW tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Not shown on OVERVIEW.
    None,
    /// Left column, ordered by the number (ascending).
    Left(u8),
    /// Right column, ordered by the number (ascending).
    Right(u8),
}

/// A request from a module towards the shell.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// One-line message in the footer until the next key press.
    Footer(String),
    /// Make the tab of the module with this id active.
    Activate(&'static str),
    /// Blink the header title with `Some(text)` (warn style); `None` restores it.
    Alert(Option<&'static str>),
}

/// Typed key/value store shared by all modules (clone in, clone out).
///
/// Producers `publish` a snapshot under a stable key (by convention the
/// module id); consumers `get` a clone. Values must be `Clone + Send + Sync`.
#[derive(Clone, Default)]
pub struct Blackboard(Arc<RwLock<HashMap<&'static str, Box<dyn Any + Send + Sync>>>>);

impl Blackboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish<T: Clone + Send + Sync + 'static>(&self, key: &'static str, value: T) {
        let mut map = self.0.write().unwrap_or_else(|e| e.into_inner());
        map.insert(key, Box::new(value));
    }

    pub fn get<T: Clone + Send + Sync + 'static>(&self, key: &str) -> Option<T> {
        let map = self.0.read().unwrap_or_else(|e| e.into_inner());
        map.get(key).and_then(|b| b.downcast_ref::<T>()).cloned()
    }
}

/// Blackboard key of the [`AudioFocus`] token.
pub const AUDIO_FOCUS: &str = "audio.focus";

/// Who may be audible right now. RADIO and MUSIC share one mixer but must not
/// play at the same time: whichever one starts publishes itself here with a
/// fresh [`next_focus_seq`]; the other sees a newer `seq` in its `poll` and
/// pauses itself. Neither module has to know the other exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFocus {
    /// Module id of the owner (`"radio"`, `"music"`).
    pub owner: &'static str,
    pub seq: u64,
}

/// Monotonic ticket for [`AudioFocus::seq`]. Starts at 1, so a module's
/// initial `0` is older than any published focus.
pub fn next_focus_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// The raw `config.toml` table; each module reads its own section by id.
#[derive(Clone, Debug, Default)]
pub struct ModuleConfig(pub toml::Table);

impl ModuleConfig {
    /// Deserialize the `[<key>]` section; a missing section yields `T::default()`,
    /// an invalid one yields an error message for the footer (and the default).
    pub fn section<T: DeserializeOwned + Default>(&self, key: &str) -> (T, Option<String>) {
        match self.0.get(key) {
            None => (T::default(), None),
            Some(v) => match v.clone().try_into::<T>() {
                Ok(t) => (t, None),
                Err(e) => {
                    let first = e.to_string().lines().next().unwrap_or("").to_string();
                    (T::default(), Some(format!("config [{key}]: {first}")))
                }
            },
        }
    }
}

/// Services every module receives from the shell.
#[derive(Clone)]
pub struct Ctx {
    /// Tokio runtime handle for async work (HTTP, timers). Background threads
    /// may use `handle.block_on`.
    pub rt: tokio::runtime::Handle,
    pub config: Arc<ModuleConfig>,
    /// Shared audio output (mixer + alarm player). See `audio.rs`.
    pub audio: Arc<crate::audio::Audio>,
    pub board: Blackboard,
    /// Requests towards the shell (footer notice, tab activation, header alert).
    pub notify: Sender<Notice>,
}

/// A tab in the Pip-Boy. Implementors are registered in `main.rs`.
///
/// Lifecycle on the UI thread: `start` once → every frame `poll` → `tick`
/// (4–20 Hz) → `draw` (if the module's tab is active, or its `overview`
/// block when OVERVIEW is active). Keys go first to the active module's
/// `on_key`; unconsumed keys are then offered to every module's
/// `on_global_key`; what is still unconsumed is handled by the shell
/// (tab switching, quit).
///
/// Only the methods without a default body are mandatory. Modules live on the
/// UI thread only, so they need not be `Send`: `RefCell` state is fine.
pub trait Module {
    /// Stable identifier: config section name, blackboard key, `Notice::Activate` target.
    fn id(&self) -> &'static str;
    /// Tab label, uppercase (`"STAT"`).
    fn title(&self) -> &'static str;
    /// Footer help while this tab is active.
    fn help(&self) -> &'static str;
    /// One English line (at most 70 chars) for the SETUP list.
    fn describe(&self) -> &'static str {
        ""
    }
    /// The tab's manual, shown by the shell's `h` overlay: what the tab is and
    /// what every key does. Plain lines, no markdown, at most 70 chars each.
    fn manual(&self) -> &'static str {
        ""
    }

    /// Start background sources. Called once on the UI thread before the first frame.
    fn start(&mut self, _ctx: &Ctx) {}
    /// Drain the module's own channels; return the number of events handled.
    /// Publish snapshots to `ctx.board` here. Must not block.
    fn poll(&mut self, _ctx: &Ctx) -> usize {
        0
    }
    /// Time-based housekeeping (timers, animations). 4–20 Hz; must not block.
    fn tick(&mut self, _ctx: &Ctx) {}
    /// A key while this module's tab is active. Return `true` if consumed.
    fn on_key(&mut self, _key: KeyEvent, _ctx: &Ctx) -> bool {
        false
    }
    /// A key not consumed by the active module (any tab). Return `true` if consumed.
    fn on_global_key(&mut self, _key: KeyEvent, _ctx: &Ctx) -> bool {
        false
    }

    /// Draw the tab body. Must not panic on tiny areas or empty data.
    fn draw(&self, f: &mut Frame, area: Rect, t: Theme);
    /// Spans for the header's right side (e.g. now playing, battery).
    ///
    /// `width` is the *remaining* budget in columns: the shell asks the modules
    /// in registry order, so register fixed-size indicators (STAT's `BAT`)
    /// before flexible ones (RADIO's now-playing text), which shrink to what
    /// is left. Return nothing when it does not fit; anything longer than
    /// `width` is cut from the right by the shell.
    fn header(&self, _width: u16, _t: Theme) -> Vec<Span<'static>> {
        vec![]
    }
    /// Compact block for OVERVIEW, sized for `width` columns and at most `height` rows.
    fn overview(&self, _width: u16, _height: u16, _t: Theme) -> Vec<Line<'static>> {
        vec![]
    }
    /// Placement of the OVERVIEW block.
    fn overview_slot(&self) -> Slot {
        Slot::None
    }
    /// `true` when the module shows moving content that deserves 20 fps
    /// (`active` = this tab or OVERVIEW is visible).
    fn wants_fast_frames(&self, _active: bool) -> bool {
        false
    }
    /// One-line diagnostic for `--probe`.
    fn status(&self) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blackboard_roundtrip_and_type_safety() {
        let b = Blackboard::new();
        b.publish("weather", 42u32);
        assert_eq!(b.get::<u32>("weather"), Some(42));
        assert_eq!(b.get::<String>("weather"), None, "wrong type is None, not a panic");
        assert_eq!(b.get::<u32>("missing"), None);
        b.publish("weather", 7u32);
        assert_eq!(b.get::<u32>("weather"), Some(7));
    }

    #[test]
    fn focus_seq_is_monotonic() {
        let a = next_focus_seq();
        let b = next_focus_seq();
        assert!(a >= 1 && b > a, "{a} < {b}, both above the initial 0");
    }

    #[derive(serde::Deserialize, Default, Debug, PartialEq)]
    #[serde(default)]
    struct DemoCfg {
        feed: String,
        limit: u32,
    }

    #[test]
    fn config_section_default_and_error() {
        let table: toml::Table = toml::from_str("[news]\nfeed = \"hn\"\nlimit = 30\n[bad]\nlimit = \"x\"\n").unwrap();
        let cfg = ModuleConfig(table);
        let (n, notice) = cfg.section::<DemoCfg>("news");
        assert_eq!(n, DemoCfg { feed: "hn".into(), limit: 30 });
        assert!(notice.is_none());
        let (d, notice) = cfg.section::<DemoCfg>("nope");
        assert_eq!(d, DemoCfg::default());
        assert!(notice.is_none());
        let (b, notice) = cfg.section::<DemoCfg>("bad");
        assert_eq!(b, DemoCfg::default());
        assert!(notice.unwrap().starts_with("config [bad]"));
    }
}
