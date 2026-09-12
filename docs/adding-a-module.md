# Adding a module

A module is one tab of the Pip-Boy: it owns its state, its background work and
its drawing. The shell owns the frame, the tab strip, the footer, the OVERVIEW
composition and key routing. **Adding a module = one file implementing
`Module` + one line in the registry in `src/main.rs`.**

The contract lives in [`src/module.rs`](../src/module.rs); the shared audio
service in [`src/audio.rs`](../src/audio.rs). Read those two files first, they
are short and documented.

## Rules

- **Never block the UI thread.** Network, disk and slow system calls go on a
  `std::thread` or a tokio task (`ctx.rt.spawn`); results come back through
  your own `std::sync::mpsc` channel, drained in `poll()`.
- **Never reference another module.** Shared data goes through
  `ctx.board` (`publish` a `Clone + Send + Sync` snapshot under your `id()`,
  `get` someone else's), shared services through `ctx` (`audio`, `rt`,
  `config`), requests to the shell through `ctx.notify`.
- **Read the blackboard in `poll`, cache what `draw` needs.** `draw`,
  `header` and `overview` take `&self` and get no `Ctx`, so a consumer caches
  the values it needs in `poll`. `get` clones, so gate the clone on a cheap
  change key: the producer publishes one next to its snapshot
  (`board.publish("weather.at", w.fetched_at)`) and the consumer compares it
  before copying. CLOCK is the example: it caches sunrise/sunset from WEATHER
  and only re-reads when `weather.at` differs from the cached one.
- **Modules live on the UI thread only** (`Module` is not `Send`), so
  `RefCell` state is fine — that is how a `&self` `draw` can drive a stateful
  widget (`ListState`, an editor). Background work still goes to its own
  thread and comes back through a channel.
- **Degrade, don't die.** A failing source shows `n/a` / an error line in your
  tab and retries; it never panics. In release builds (`panic = "abort"`) a
  panic on any thread ends the app after the terminal is restored — never
  panic on bad data.
- **Only `Theme` styles, only the 16 ANSI colors**, English UI strings, no
  panics on tiny areas or empty data (`saturating_sub`, `.get()`, `.min()`).
- **Config**: read your `[<id>]` section with
  `ctx.config.section::<MyCfg>(self.id())`; derive `Default` and
  `#[serde(default)]` so a missing section works.
- **Tests** where there is branching logic (parsers, state machines, layout
  math). Drawing is checked by hand in the terminal.

## Lifecycle

```
describe()          one English line (at most 70 chars) for the SETUP list
manual()            the tab's manual, shown by the shell's `h` overlay:
                    plain lines, at most 70 chars each — what the tab is
                    and what every one of its keys does
start(ctx)          once, before the first frame (spawn sources here);
                    a module disabled in SETUP is never started
poll(ctx) -> n      every frame: drain your channel, publish snapshots
tick(ctx)           every frame (4–20 Hz): timers, animations
on_key(key, ctx)    only while your tab is active; return true if consumed
on_global_key(..)   any tab, for keys not consumed by the active module
draw(f, area, t)    your tab body
header(width, t)    spans for the header's right side; `width` is the budget
overview(w, h, t)   compact block for OVERVIEW; overview_slot() places it
wants_fast_frames   true → the shell renders at 20 fps instead of 4
status()            one line for `pipboy.exe --probe N`
```

### The `header(width)` budget

The header shares one line with the tab strip, so space is scarce. The shell
asks the modules in **registry order** and passes each one the number of
columns still **remaining**, then renders them in that same order with two
spaces between non-empty groups — so put fixed-size indicators (STAT's
`BAT 42%⇡`) early in the registry: they reserve their budget before a
downstream module sees a shrunk remainder. Return `vec![]` when your content
does not fit, or shrink it: RADIO drops the elapsed time first, then
ellipsizes the station name, and returns nothing under 6 columns. A group
wider than its budget is dropped, so ignoring `width` means not being shown
at all.

### Audio focus

Only one module plays sound at a time. When your module starts playing,
publish `AudioFocus { owner: self.id(), seq: next_focus_seq() }` under the
`AUDIO_FOCUS` blackboard key (both in `src/module.rs`), and in `poll` read it
back: a newer `seq` from another owner means you pause yourself and send one
`Notice::Footer`. RADIO and MUSIC are the two examples.

Keys reserved by the shell: `←` `→` `Tab` `Shift+Tab` `1`–`9` (tabs), `q`
`Ctrl+C` (quit), `h` (the `manual()` overlay). `Esc` is free: use it for
"back"/"cancel" inside your tab. A module that owns the whole keyboard (TERM
while attached, the NOTES editor) simply consumes `h` too and the overlay
stays shut.
Global keys owned by existing modules: `Space` `+` `-` `m` (radio). Pick
tab-local keys freely; document them in `help()`.

## Minimal example

```rust
// src/modules/hello.rs
use crate::module::{Ctx, Module, Slot};
use crate::style::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct HelloCfg { greeting: String }

pub struct Hello { cfg: HelloCfg, rx: Option<Receiver<String>>, last: String, presses: u32 }

impl Hello {
    pub fn new() -> Self { Self { cfg: HelloCfg::default(), rx: None, last: String::new(), presses: 0 } }
}

impl Module for Hello {
    fn id(&self) -> &'static str { "hello" }
    fn title(&self) -> &'static str { "HELLO" }
    fn help(&self) -> &'static str { "g say hello   1-9 tabs   q quit" }
    fn describe(&self) -> &'static str { "Says hello, and counts how often you asked" }
    fn manual(&self) -> &'static str { "\
HELLO greets you and keeps score of how often you asked.

  g     say hello once more
  1-9   jump to a tab

The greeting itself comes from [hello] greeting in
config.toml, so it can be as formal as your vault requires." }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<HelloCfg>(self.id());
        self.cfg = cfg;
        if let Some(n) = notice { let _ = ctx.notify.send(crate::module::Notice::Footer(n)); }
        let (tx, rx): (Sender<String>, Receiver<String>) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if tx.send(chrono::Local::now().format("%H:%M:%S").to_string()).is_err() { return; }
            }
        });
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = 0;
        if let Some(rx) = &self.rx {
            while let Ok(s) = rx.try_recv() { self.last = s; n += 1; }
        }
        if n > 0 { ctx.board.publish(self.id(), self.last.clone()); }
        n
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> bool {
        if key.code == KeyCode::Char('g') { self.presses += 1; return true; }
        false
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        let greeting = if self.cfg.greeting.is_empty() { "Hello, Vault Dweller" } else { &self.cfg.greeting };
        let lines = vec![
            Line::from(Span::styled(format!(" {greeting}"), t.title)),
            Line::from(format!("   last tick {} · h pressed {}×", self.last, self.presses)),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }

    fn overview(&self, _w: u16, _h: u16, t: Theme) -> Vec<Line<'static>> {
        vec![Line::from(vec![Span::styled(" HELLO ", t.title), Span::raw(self.last.clone())])]
    }
    fn overview_slot(&self) -> Slot { Slot::Right(9) }
    fn status(&self) -> String { format!("hello last={}", self.last) }
}
```

Register it in `src/main.rs`:

```rust
let modules: Vec<Box<dyn Module>> = vec![
    Box::new(modules::stat::Stat::new()),
    // ...
    Box::new(modules::hello::Hello::new()),
];
```

Tab order and the `1`–`9` keys follow the registry order; OVERVIEW is always
first and is built by the shell from every module's `overview()` block.

## Checklist before a pull request

- `cargo test` green, `cargo build --release` without warnings.
- The tab is readable at 80×24 and fills 120×40 sensibly.
- Sources fail gracefully (unplug the network, remove the config section).
- `pipboy.exe --probe 10` shows your `status()` line.
- README: one line in the tab list, keys in the table, config keys under Config.
