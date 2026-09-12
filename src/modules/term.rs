//! TERM module: an embedded terminal tab that runs `claude` (or any command)
//! in a Windows pseudo console (ConPTY) and renders its screen through the
//! `vt100` terminal emulator.
//!
//! The process is **not** started automatically: the first `Enter` on the tab
//! spawns it. While the tab is *captured* every key belongs to the child
//! process (including `q`, the digits and `Ctrl+C`); only the configured
//! `release_key` (`f12` by default: one key, the same on every keyboard layout) comes back to the Pip-Boy.

use crate::module::{Ctx, Module, Notice, Slot};
use crate::style::Theme;
use crate::ui::widgets::screen_line;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How long a lone Esc waits for a following character before it is sent.
const ESC_HOLD: Duration = Duration::from_millis(40);
const HELP_VIEW: &str = "enter attach   pgup/pgdn scroll   r restart   1-9 tabs   q quit";
/// Fallback while `start()` has not run yet (and the default spelling).
const HELP_CAPTURED: &str = "TERM \u{b7} captured \u{b7} f12 to release \u{b7} shift+pgup/pgdn scroll";
/// Output newer than this keeps the shell at 20 fps.
const BUSY: Duration = Duration::from_millis(500);

#[derive(serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct TermCfg {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub release_key: String,
    pub scrollback: usize,
}

impl Default for TermCfg {
    fn default() -> Self {
        Self {
            command: "pwsh".to_string(),
            args: Vec::new(),
            cwd: "vault".to_string(),
            release_key: "f12".to_string(),
            scrollback: 1000,
        }
    }
}

// ---- command resolution ----------------------------------------------------

/// Resolve `command` into the argv to spawn, `args` already appended.
///
/// An absolute path is used as is; anything else is looked up in `dirs` (the
/// `PATH`). A command that already names its own extension (e.g.
/// `pwsh.exe`) is tried bare in each directory first — appending the usual
/// extensions blindly would look for `pwsh.exe.exe` — and only falls back to
/// the extension search below in case just a shim variant is on PATH. A bare
/// command is tried with `.exe`, `.cmd` and `.bat`, in that order per
/// directory — the same order `cmd.exe` itself uses. `None` = not found.
///
/// A real `.exe` gets `args` as ordinary argv entries — no shell involved,
/// so no interpolation risk. A `.cmd`/`.bat` shim (npm installs
/// `claude.cmd`) is not directly executable, so it is run through
/// `cmd.exe /s /c`; cmd.exe re-parses that argument with its own
/// metacharacter rules (`&`, `|`, `<`, `>`, …) rather than treating it as
/// plain argv, so the shim path and every arg are folded into one
/// `cmd_quote`d string instead of passed as separate CreateProcess argv
/// entries — see `cmd_quote`.
pub fn resolve_argv(command: &str, args: &[String], dirs: &[PathBuf], exists: &dyn Fn(&Path) -> bool) -> Option<Vec<String>> {
    const EXTS: [&str; 3] = ["exe", "cmd", "bat"];
    let has_ext = Path::new(command).extension().is_some();
    let found = if Path::new(command).is_absolute() {
        let p = PathBuf::from(command);
        exists(&p).then_some(p)
    } else if has_ext {
        dirs.iter()
            .map(|d| d.join(command))
            .find(|p| exists(p))
            .or_else(|| dirs.iter().flat_map(|d| EXTS.iter().map(move |e| d.join(format!("{command}.{e}")))).find(|p| exists(p)))
    } else {
        dirs.iter().flat_map(|d| EXTS.iter().map(move |e| d.join(format!("{command}.{e}")))).find(|p| exists(p))
    }?;
    let ext = found.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let program = found.to_string_lossy().into_owned();
    Some(match ext.as_str() {
        "cmd" | "bat" => {
            let mut inner = cmd_quote(&program);
            for a in args {
                inner.push(' ');
                inner.push_str(&cmd_quote(a));
            }
            vec!["cmd.exe".to_string(), "/s".to_string(), "/c".to_string(), format!("\"{inner}\"")]
        }
        _ => {
            let mut v = vec![program];
            v.extend(args.iter().cloned());
            v
        }
    })
}

/// Quote one token for cmd.exe's own `/C` parsing (used only for the
/// `.cmd`/`.bat` shim path — a real `.exe` never goes through cmd.exe).
/// Wraps in double quotes, so spaces stay part of one argument, and
/// caret-escapes the metacharacters cmd.exe still honors inside a quoted
/// string so they stay literal instead of chaining/redirecting commands.
fn cmd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '&' | '|' | '<' | '>' | '^') {
            out.push('^');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default()
}

// ---- keys ------------------------------------------------------------------

/// Parse a key spec like `"ctrl+]"`, `"f12"`, `"alt+shift+x"`.
/// A literal `+` is spelled `"plus"` (`"ctrl++"` would split into empty parts).
pub fn parse_key_spec(s: &str) -> Option<(KeyModifiers, KeyCode)> {
    let mut mods = KeyModifiers::NONE;
    let mut code = None;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "esc" | "escape" => code = Some(KeyCode::Esc),
            "tab" => code = Some(KeyCode::Tab),
            "enter" => code = Some(KeyCode::Enter),
            "space" => code = Some(KeyCode::Char(' ')),
            "plus" => code = Some(KeyCode::Char('+')),
            f if f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
                code = Some(KeyCode::F(f[1..].parse().ok()?));
            }
            other => {
                let mut chars = other.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => code = Some(KeyCode::Char(c.to_ascii_lowercase())),
                    _ => return None,
                }
            }
        }
    }
    code.map(|c| (mods, c))
}

/// Does `key` match a parsed key spec? Extra modifier bits (the SHIFT
/// crossterm reports for an uppercase character, for instance) are tolerated.
pub fn matches_spec(key: KeyEvent, spec: (KeyModifiers, KeyCode)) -> bool {
    let (mods, code) = spec;
    let same_code = match (key.code, code) {
        (KeyCode::Char(a), KeyCode::Char(b)) => a.to_ascii_lowercase() == b.to_ascii_lowercase(),
        (a, b) => a == b,
    };
    same_code && key.modifiers.contains(mods)
}

/// `\x1b[A` without modifiers, `\x1b[1;5A` with them. In DECCKM (application
/// cursor key) mode — vim, less, … — an unmodified press uses the SS3 form
/// (`\x1bOA`) instead; a modifier still needs the CSI parameter, which SS3
/// has no room for, so modified presses stay on the `\x1b[1;<param>` form
/// regardless of DECCKM.
fn arrow(letter: char, param: u8, app_cursor: bool) -> Vec<u8> {
    if app_cursor && param == 1 {
        format!("\x1bO{letter}").into_bytes()
    } else if param == 1 {
        format!("\x1b[{letter}").into_bytes()
    } else {
        format!("\x1b[1;{param}{letter}").into_bytes()
    }
}

/// `\x1b[3~` without modifiers, `\x1b[3;5~` with them.
fn tilde(n: u8, param: u8) -> Vec<u8> {
    if param == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{param}~").into_bytes()
    }
}

/// Translate a key press into the bytes an xterm-compatible program expects.
/// `app_cursor` is the child's DECCKM state (`vt100::Screen::application_cursor`);
/// it changes arrows/Home/End to the SS3 form. `None` = nothing to send (a
/// key the pty has no encoding for).
pub fn key_bytes(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    let param = 1 + u8::from(key.modifiers.contains(KeyModifiers::SHIFT)) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let bytes = match key.code {
        // AltGr arrives as Ctrl+Alt on Windows: `\\` `[` `]` `{` `}` `@` on a
        // Hungarian/German layout. That is plain text, not Ctrl+key.
        KeyCode::Char(c) if ctrl && alt => c.to_string().into_bytes(),
        // Alt + a symbol: the same AltGr keys when the terminal reports only
        // ALT. Meta bindings live on letters, digits and `.` (Alt+. = yank
        // last arg), so those keep the ESC prefix.
        KeyCode::Char(c) if alt && !ctrl && !(c.is_ascii_alphanumeric() || c == '.') => c.to_string().into_bytes(),
        KeyCode::Char(c) => {
            let mut v = Vec::new();
            if alt {
                v.push(0x1b);
            }
            let upper = c.to_ascii_uppercase() as u32;
            match c {
                // C0: `@`..`_` (so Ctrl+C → 0x03, Ctrl+] → 0x1d) plus the two
                // conventional extras.
                _ if ctrl && c.is_ascii() && (0x40..=0x5f).contains(&upper) => v.push((upper as u8) & 0x1f),
                ' ' if ctrl => v.push(0),
                '?' if ctrl => v.push(0x7f),
                _ => v.extend_from_slice(c.to_string().as_bytes()),
            }
            v
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![if ctrl { 0x08 } else { 0x7f }],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => arrow('A', param, app_cursor),
        KeyCode::Down => arrow('B', param, app_cursor),
        KeyCode::Right => arrow('C', param, app_cursor),
        KeyCode::Left => arrow('D', param, app_cursor),
        KeyCode::Home => arrow('H', param, app_cursor),
        KeyCode::End => arrow('F', param, app_cursor),
        KeyCode::Insert => tilde(2, param),
        KeyCode::Delete => tilde(3, param),
        KeyCode::PageUp => tilde(5, param),
        KeyCode::PageDown => tilde(6, param),
        KeyCode::F(n) => match n {
            1..=4 if param == 1 => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
            1..=4 => format!("\x1b[1;{param}{}", (b'P' + n - 1) as char).into_bytes(),
            5 => tilde(15, param),
            6..=10 => tilde(11 + n, param),
            11 | 12 => tilde(12 + n, param),
            _ => return None,
        },
        _ => return None,
    };
    Some(bytes)
}

/// What a key does while the tab is **not** capturing. Split out so the state
/// machine is testable without spawning a process.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Capture the keyboard (and start the process if it is not running).
    Attach,
    /// Kill and restart the process.
    Restart,
    /// Scroll the scrollback: `+1` page back, `-1` page forward.
    Scroll(i8),
    /// Not ours — let the shell have it (tab switching, `q`, …).
    Pass,
}

pub fn view_action(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter | KeyCode::Char('i') => Action::Attach,
        KeyCode::Char('r') => Action::Restart,
        KeyCode::PageUp => Action::Scroll(1),
        KeyCode::PageDown => Action::Scroll(-1),
        _ => Action::Pass,
    }
}


/// ConPTY asks the terminal where the cursor is (`ESC[6n`, DSR) right after
/// start-up and holds *all* output until a Cursor Position Report comes back.
/// A real terminal answers on its own; here we are the terminal, so we must.
/// ponytail: a query split across two read chunks is missed; ConPTY sends it
/// as one 4-byte write in practice.
fn dsr_requests(chunk: &[u8]) -> usize {
    chunk.windows(4).filter(|w| *w == b"[6n").count()
}

/// `ESC[row;colR`, 1-based, for the given 0-based screen cursor.
fn cursor_report((row, col): (u16, u16)) -> Vec<u8> {
    format!("[{};{}R", row + 1, col + 1).into_bytes()
}

// ---- the module ------------------------------------------------------------

struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

pub struct Term {
    cfg: TermCfg,
    /// Resolved argv, `None` when the command was not found.
    argv: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    release: Option<(KeyModifiers, KeyCode)>,
    help_captured: &'static str,
    // ponytail: the shell can switch the visible tab out from under us via
    // `Notice::Activate` (CLOCK's alarm does this) and this module has no
    // way to find out — `captured` stays true and the `⌨ TERM` header
    // marker keeps showing until the user comes back and releases with
    // `release_key`. Deliberate: guessing "we were displaced" from timing
    // (`wants_fast_frames`/last-key-age) is nondeterministic; a real fix
    // needs the shell to tell a module it lost focus.
    captured: bool,
    pty: Option<Pty>,
    parser: RefCell<vt100::Parser>,
    rx: Option<Receiver<Vec<u8>>>,
    /// Last pty size handed to ConPTY and the parser (rows, cols).
    size: Cell<(u16, u16)>,
    exited: Option<u32>,
    last_output: Option<Instant>,
    /// An Esc held back for `ESC_HOLD`: a terminal that sends AltGr symbols as
    /// `ESC` + char (Alt-sends-Escape) delivers two key events, and the ESC
    /// must not reach the child in that case.
    pending_esc: Option<Instant>,
    #[cfg(test)]
    sent: Vec<u8>,
    scroll: usize,
}

impl Term {
    pub fn new() -> Self {
        Self {
            cfg: TermCfg::default(),
            argv: None,
            cwd: None,
            release: parse_key_spec("f12"),
            help_captured: HELP_CAPTURED,
            captured: false,
            pty: None,
            parser: RefCell::new(vt100::Parser::new(24, 80, 0)),
            rx: None,
            size: Cell::new((0, 0)),
            exited: None,
            last_output: None,
            pending_esc: None,
            #[cfg(test)]
            sent: Vec::new(),
            scroll: 0,
        }
    }

    fn spawn(&mut self) -> anyhow::Result<()> {
        let argv = self.argv.clone().ok_or_else(|| anyhow::anyhow!("command not found: {}", self.cfg.command))?;
        let (rows, cols) = match self.size.get() {
            (r, c) if r < 2 || c < 2 => (24, 80),
            s => s,
        };
        let pair = portable_pty::native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.env("TERM", "xterm-256color");
        if let Some(cwd) = &self.cwd {
            cmd.cwd(cwd);
        }
        let child = pair.slave.spawn_command(cmd)?;
        // Our own slave handle must go, otherwise the reader never sees EOF.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        });

        *self.parser.borrow_mut() = vt100::Parser::new(rows, cols, self.cfg.scrollback);
        self.size.set((rows, cols));
        self.scroll = 0;
        self.exited = None;
        self.last_output = Some(Instant::now());
        self.pty = Some(Pty { master: pair.master, writer, child });
        Ok(())
    }

    fn kill(&mut self) {
        if let Some(pty) = &mut self.pty {
            // ponytail: kills the ConPTY client. With the `cmd.exe /c` shim
            // that is cmd.exe; dropping the master closes the console, which
            // hangs up the grandchild too. Walk the process tree only if a
            // stray `node` ever survives.
            let _ = pty.child.kill();
        }
        self.pty = None;
        self.rx = None;
        self.captured = false;
    }

    /// `Enter`/`i`: capture the keyboard, starting the process if needed.
    fn attach(&mut self, ctx: &Ctx) {
        if self.pty.is_none() {
            if let Err(e) = self.spawn() {
                let _ = ctx.notify.send(Notice::Footer(format!("term: {e}")));
                return;
            }
        }
        self.captured = true;
    }

    fn restart(&mut self, ctx: &Ctx) {
        self.kill();
        self.exited = None;
        if let Err(e) = self.spawn() {
            let _ = ctx.notify.send(Notice::Footer(format!("term: {e}")));
        }
    }

    /// Write to the child (no-op without a process).
    fn send(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        self.sent.extend_from_slice(bytes);
        if let Some(pty) = self.pty.as_mut() {
            let _ = pty.writer.write_all(bytes);
            let _ = pty.writer.flush();
        }
    }

    /// An Esc that was held back and not followed by a character goes out.
    fn flush_pending_esc(&mut self) {
        if self.pending_esc.is_some_and(|at| at.elapsed() >= ESC_HOLD) {
            self.pending_esc = None;
            self.send(&[0x1b]);
        }
    }

    fn scroll_by(&mut self, pages: i8) {
        let page = usize::from(self.size.get().0).max(1);
        self.scroll = match pages {
            p if p > 0 => (self.scroll + page).min(self.cfg.scrollback),
            _ => self.scroll.saturating_sub(page),
        };
        self.parser.borrow_mut().screen_mut().set_scrollback(self.scroll);
    }

    fn state(&self) -> &'static str {
        if self.argv.is_none() {
            "not found"
        } else if self.pty.is_some() {
            "running"
        } else if self.exited.is_some() {
            "exited"
        } else {
            "idle"
        }
    }

    /// Feed up to `DRAIN_CAP` buffered chunks to the parser. A hard cap keeps
    /// a spewing child from starving the UI thread inside one `poll`; the
    /// rest simply waits for the next frame's drain.
    fn drain(&mut self) -> usize {
        const DRAIN_CAP: usize = 64;
        let mut n = 0;
        let mut dsr = 0;
        if let Some(rx) = &self.rx {
            while n < DRAIN_CAP {
                let Ok(chunk) = rx.try_recv() else { break };
                dsr += dsr_requests(&chunk);
                self.parser.borrow_mut().process(&chunk);
                n += 1;
            }
        }
        if dsr > 0 {
            // ConPTY holds every byte of output until this report arrives.
            let reply = cursor_report(self.parser.borrow().screen().cursor_position());
            if let Some(pty) = self.pty.as_mut() {
                for _ in 0..dsr {
                    let _ = pty.writer.write_all(&reply);
                }
                let _ = pty.writer.flush();
            }
        }
        n
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // No orphan `claude` outlives the app.
        self.kill();
    }
}

impl Module for Term {
    fn id(&self) -> &'static str {
        "term"
    }
    fn title(&self) -> &'static str {
        "TERM"
    }
    fn describe(&self) -> &'static str {
        "An embedded terminal (pwsh; run claude in it)"
    }
    fn manual(&self) -> &'static str {
        "\
TERM is a real terminal inside the Pip-Boy - pwsh by default,
whatever you put in [term] otherwise. Good for a quick
command, or for running the claude CLI with the Vault-Tec
persona shipped in the vault folder.

  enter or i   attach the keyboard to it (the first attach
               starts the process)
  F12          release it back to the Pip-Boy; the key is
               [term] release_key
  PgUp/PgDn    scroll the scrollback while detached, and
               shift+PgUp / shift+PgDn while attached
  r            restart the process

While attached every key belongs to the child - q, h and
ctrl+c included. That is exactly why the release key is one
key that means the same thing on every keyboard layout."
    }
    fn help(&self) -> &'static str {
        if self.captured {
            self.help_captured
        } else {
            HELP_VIEW
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<TermCfg>(self.id());
        self.cfg = cfg;
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        let configured = match Path::new(&self.cfg.cwd).is_absolute() {
            true => PathBuf::from(&self.cfg.cwd),
            false => exe_dir.join(&self.cfg.cwd),
        };
        // Configured directory, else the exe directory, else let the child
        // inherit ours.
        self.cwd = [configured, exe_dir].into_iter().find(|p| p.is_dir());
        self.argv = resolve_argv(&self.cfg.command, &self.cfg.args, &path_dirs(), &|p| p.is_file());
        let configured_release = parse_key_spec(&self.cfg.release_key);
        if configured_release.is_none() {
            let _ = ctx.notify.send(Notice::Footer(format!(
                "config [term] release_key: cannot parse {:?}, using f12",
                self.cfg.release_key
            )));
        }
        // A garbage `release_key` must never leave the tab unquittable: fall
        // back to the default rather than `None` (which would swallow every
        // key forever once captured).
        self.release = configured_release.or_else(|| parse_key_spec("f12"));
        // ponytail: `help()` returns `&'static str`, so the configured key
        // name is baked into one leaked string — once per process, never
        // freed on purpose.
        self.help_captured = Box::leak(format!("TERM \u{b7} captured \u{b7} {} to release \u{b7} shift+pgup/pgdn scroll", self.cfg.release_key).into_boxed_str());
        *self.parser.borrow_mut() = vt100::Parser::new(24, 80, self.cfg.scrollback);
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        self.flush_pending_esc();
        let n = self.drain();
        if n > 0 {
            self.last_output = Some(Instant::now());
            // Fresh output always jumps back to the live screen.
            if self.scroll != 0 {
                self.scroll = 0;
                self.parser.borrow_mut().screen_mut().set_scrollback(0);
            }
        }
        let exit = self.pty.as_mut().and_then(|p| p.child.try_wait().ok().flatten());
        if let Some(status) = exit {
            // The child may have written its final bytes right before
            // exiting, after the drain above already ran dry — one more
            // pass so the screen shown alongside "process exited" isn't
            // missing its last line.
            self.drain();
            self.exited = Some(status.exit_code());
            self.pty = None;
            self.captured = false;
            let _ = ctx.notify.send(Notice::Footer(format!("term: {} exited (code {})", self.cfg.command, status.exit_code())));
        }
        n
    }

    fn captures_keyboard(&self) -> bool {
        self.captured
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        if self.captured {
            if self.release.is_some_and(|spec| matches_spec(key, spec)) {
                self.captured = false;
                return true;
            }
            // Shift+PgUp/PgDn scroll the scrollback even while attached, like
            // any real terminal; plain PgUp/PgDn still belong to the child.
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                match key.code {
                    KeyCode::PageUp => { self.scroll_by(1); return true; }
                    KeyCode::PageDown => { self.scroll_by(-1); return true; }
                    _ => {}
                }
            }
            // A lone Esc is held back briefly (see `pending_esc`).
            if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                if self.pending_esc.is_some() {
                    self.send(&[0x1b]);
                }
                self.pending_esc = Some(Instant::now());
                return true;
            }
            if let Some(at) = self.pending_esc.take() {
                let altgr_artifact = at.elapsed() < ESC_HOLD
                    && matches!(key.code, KeyCode::Char(_))
                    && !key.modifiers.contains(KeyModifiers::CONTROL);
                if !altgr_artifact {
                    self.send(&[0x1b]);
                }
            }
            let app_cursor = self.parser.borrow().screen().application_cursor();
            if let Some(bytes) = key_bytes(key, app_cursor) {
                self.send(&bytes);
            }
            // Everything else is the child's, `q` and Ctrl+C included.
            return true;
        }
        match view_action(key) {
            Action::Attach => {
                self.attach(ctx);
                true
            }
            Action::Restart => {
                self.restart(ctx);
                true
            }
            Action::Scroll(pages) => {
                self.scroll_by(pages);
                true
            }
            Action::Pass => false,
        }
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.argv.is_none() {
            let msg = format!(" command not found: {} \u{2014} set [term] command in config.toml", self.cfg.command);
            f.render_widget(Paragraph::new(msg).style(t.danger), area);
            return;
        }

        // vt100 cannot take a 1-row/1-col grid (its wrap logic underflows), so
        // a sliver of a pane keeps the previous size and just clips.
        let (rows, cols) = (area.height.max(2), area.width.max(2));
        if self.size.get() != (rows, cols) {
            self.size.set((rows, cols));
            if let Some(pty) = &self.pty {
                let _ = pty.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
            }
            self.parser.borrow_mut().screen_mut().set_size(rows, cols);
        }

        if self.pty.is_none() && self.exited.is_none() {
            let lines = vec![
                Line::from(Span::styled(" TERM", t.title)),
                Line::from(""),
                Line::from(format!("  {} \u{2014} press enter to start", self.cfg.command)),
                Line::from(Span::styled(format!("  every key then goes to the process; {} releases", self.cfg.release_key), t.frame)),
            ];
            f.render_widget(Paragraph::new(lines), area);
            return;
        }

        let parser = self.parser.borrow();
        let screen = parser.screen();
        let lines: Vec<Line<'static>> = (0..rows).map(|r| screen_line(screen, r, cols, t)).collect();
        f.render_widget(Paragraph::new(lines), area);

        if let Some(code) = self.exited {
            // The last screen stays; only the bottom row carries the notice.
            let msg = format!(" process exited (code {code}) \u{2014} enter to restart");
            let y = area.y + area.height - 1;
            f.render_widget(Paragraph::new(msg).style(t.warn), Rect { y, height: 1, ..area });
        } else if self.captured && !screen.hide_cursor() {
            let (crow, ccol) = screen.cursor_position();
            if crow < rows && ccol < cols {
                f.set_cursor_position((area.x + ccol, area.y + crow));
            }
        }
    }

    fn header(&self, width: u16, t: Theme) -> Vec<Span<'static>> {
        if !self.captured || width < 6 {
            return vec![];
        }
        vec![Span::styled("\u{2328} TERM", t.warn)]
    }

    fn overview(&self, _width: u16, _height: u16, t: Theme) -> Vec<Line<'static>> {
        vec![Line::from(Span::styled(" TERM", t.title)), Line::from(format!("  {} {}", self.cfg.command, self.state()))]
    }

    fn overview_slot(&self) -> Slot {
        Slot::Right(5)
    }

    fn wants_fast_frames(&self, active: bool) -> bool {
        active && self.pty.is_some() && (self.captured || self.last_output.is_some_and(|t| t.elapsed() < BUSY))
    }

    fn status(&self) -> String {
        format!("term {} running={} captured={}", self.cfg.command, self.pty.is_some(), self.captured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use crate::ui::widgets::map_color;
    use ratatui::style::Color;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn bytes(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        key_bytes(KeyEvent::new(code, mods), false).expect("encodable key")
    }

    #[test]
    fn keys_become_terminal_bytes() {
        assert_eq!(bytes(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(bytes(KeyCode::Down, KeyModifiers::NONE), b"\x1b[B");
        assert_eq!(bytes(KeyCode::Right, KeyModifiers::NONE), b"\x1b[C");
        assert_eq!(bytes(KeyCode::Left, KeyModifiers::NONE), b"\x1b[D");
        assert_eq!(bytes(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![3]);
        assert_eq!(bytes(KeyCode::Char(']'), KeyModifiers::CONTROL), vec![0x1d]);
        assert_eq!(bytes(KeyCode::Backspace, KeyModifiers::NONE), vec![0x7f]);
        assert_eq!(bytes(KeyCode::Tab, KeyModifiers::NONE), b"\t");
        assert_eq!(bytes(KeyCode::Esc, KeyModifiers::NONE), vec![0x1b]);
        assert_eq!(bytes(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(bytes(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(bytes(KeyCode::F(12), KeyModifiers::NONE), b"\x1b[24~");
        assert_eq!(bytes(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        // Non-ASCII goes out as UTF-8, not as a lone byte.
        assert_eq!(bytes(KeyCode::Char('é'), KeyModifiers::NONE), "é".as_bytes());
        // Alt prefixes ESC; Ctrl+arrow uses the xterm modifier parameter.
        assert_eq!(bytes(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::CONTROL), b"\x1b[1;5A");
        assert!(key_bytes(key(KeyCode::Null), false).is_none());
    }

    #[test]
    fn decckm_switches_arrows_and_home_end_to_ss3() {
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true).unwrap(), b"\x1bOA");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), true).unwrap(), b"\x1bOB");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), true).unwrap(), b"\x1bOC");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), true).unwrap(), b"\x1bOD");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), true).unwrap(), b"\x1bOH");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), true).unwrap(), b"\x1bOF");
        // Normal (non-DECCKM) mode is unaffected — same bytes as before.
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), false).unwrap(), b"\x1b[A");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), false).unwrap(), b"\x1b[H");
        // A modifier needs the CSI parameter, which SS3 has no room for, so
        // it stays on the CSI form even under DECCKM.
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), true).unwrap(), b"\x1b[1;5A");
    }

    /// AltGr = Ctrl+Alt on Windows; `\\` on a Hungarian keyboard must stay `\\`,
    /// not become 0x1C (seen live as `^\\` in PowerShell, 2026-09-10).
    #[test]
    fn altgr_characters_are_plain_text_not_control_codes() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Char('\\'), altgr), false).unwrap(), b"\\");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Char(']'), altgr), false).unwrap(), b"]");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Char('@'), altgr), false).unwrap(), b"@");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), false).unwrap(), vec![3u8], "real Ctrl+C is still a control code");
    }

    #[test]
    fn alt_symbol_is_plain_text_but_alt_letter_keeps_the_meta_prefix() {
        assert_eq!(bytes(KeyCode::Char('\\'), KeyModifiers::ALT), b"\\");
        assert_eq!(bytes(KeyCode::Char(']'), KeyModifiers::ALT), b"]");
        assert_eq!(bytes(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(bytes(KeyCode::Char('.'), KeyModifiers::ALT), b"\x1b.");
    }

    /// A terminal that sends AltGr symbols as ESC + char: the ESC is held back
    /// and dropped when the character follows at once; a lone Esc still goes
    /// out after the hold.
    #[test]
    fn esc_followed_by_a_char_is_the_char_alone_but_a_lone_esc_is_sent() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut term = Term::new();
        term.captured = true;
        assert!(term.on_key(key(KeyCode::Esc), &ctx));
        assert!(term.on_key(key(KeyCode::Char(']')), &ctx));
        assert_eq!(term.sent, b"]", "ESC swallowed, only the symbol reaches the child");

        term.sent.clear();
        assert!(term.on_key(key(KeyCode::Esc), &ctx));
        term.pending_esc = Some(Instant::now() - ESC_HOLD * 2);
        term.poll(&ctx);
        assert_eq!(term.sent, b"\x1b", "a lone Esc is delivered after the hold");
        assert!(term.pending_esc.is_none());

        term.sent.clear();
        assert!(term.on_key(key(KeyCode::Esc), &ctx));
        term.pending_esc = Some(Instant::now() - ESC_HOLD * 2);
        assert!(term.on_key(key(KeyCode::Char('j')), &ctx));
        assert_eq!(term.sent, b"\x1bj", "Esc then a slow keypress: both go out in order");
    }

    #[test]
    fn shift_pageup_scrolls_while_captured_and_output_resets_it() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut term = Term::new();
        term.captured = true;
        assert!(term.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT), &ctx));
        assert!(term.scroll > 0, "Shift+PgUp scrolls back while attached");
        assert!(term.captured, "and stays attached");
        assert!(term.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT), &ctx));
        assert_eq!(term.scroll, 0);
    }

    #[test]
    fn release_key_specs_parse_and_match() {
        let spec = parse_key_spec("ctrl+]").unwrap();
        assert_eq!(spec, (KeyModifiers::CONTROL, KeyCode::Char(']')));
        assert!(matches_spec(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL), spec));
        assert!(!matches_spec(key(KeyCode::Char(']')), spec), "the bare key is the child's");
        assert!(!matches_spec(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), spec));

        assert_eq!(parse_key_spec("f12").unwrap(), (KeyModifiers::NONE, KeyCode::F(12)));
        assert_eq!(parse_key_spec("Esc").unwrap(), (KeyModifiers::NONE, KeyCode::Esc));
        assert_eq!(parse_key_spec("alt+shift+X").unwrap(), (KeyModifiers::ALT | KeyModifiers::SHIFT, KeyCode::Char('x')));
        assert!(parse_key_spec("ctrl").is_none(), "modifier only");
        assert!(parse_key_spec("ctrl+nope").is_none());
    }

    #[test]
    fn unparsable_release_key_falls_back_to_ctrl_bracket_not_none() {
        let mut term_section = toml::Table::new();
        term_section.insert("release_key".to_string(), toml::Value::String("not a real spec!!".to_string()));
        let mut table = toml::Table::new();
        table.insert("term".to_string(), toml::Value::Table(term_section));
        let (ctx, notices) = crate::shell::test_ctx(table);

        let mut term = Term::new();
        term.start(&ctx);
        assert_eq!(term.release, parse_key_spec("f12"), "garbage config falls back to the default, not None");
        match notices.try_recv() {
            Ok(Notice::Footer(msg)) => assert!(msg.contains("release_key"), "footer notice: {msg}"),
            other => panic!("expected a footer notice about the bad release_key, got {other:?}"),
        }

        // The app must stay quittable: F12 still releases even though the
        // configured spec was garbage (previously `release` became `None`
        // and every key, forever, went to the child).
        term.captured = true;
        assert!(term.on_key(key(KeyCode::F(12)), &ctx));
        assert!(!term.captured, "f12 still releases");
    }

    #[test]
    fn command_resolution_handles_shims_absolute_paths_and_misses() {
        let dirs = vec![PathBuf::from("C:/one"), PathBuf::from("C:/npm")];
        let shim = dirs[1].join("claude.cmd");
        let exe = dirs[0].join("claude.exe");
        // Only the npm `.cmd` shim exists → run it through cmd.exe, the
        // whole thing quoted as one `/S`-wrapped command string.
        let only_cmd = |p: &Path| p == shim;
        assert_eq!(
            resolve_argv("claude", &[], &dirs, &only_cmd).unwrap(),
            vec!["cmd.exe".to_string(), "/s".to_string(), "/c".to_string(), format!("\"\"{}\"\"", shim.to_string_lossy())]
        );
        // A real `.exe` earlier on the PATH wins and is spawned directly.
        let both = |p: &Path| p == exe || p == shim;
        assert_eq!(resolve_argv("claude", &[], &dirs, &both).unwrap(), vec![exe.to_string_lossy().into_owned()]);
        // An absolute path is used as is, without searching the PATH; extra
        // args are plain argv entries for a real .exe — no shell involved.
        let abs = |p: &Path| p == Path::new("D:/tools/claude.exe");
        assert_eq!(
            resolve_argv("D:/tools/claude.exe", &["--version".to_string()], &[], &abs).unwrap(),
            vec!["D:/tools/claude.exe".to_string(), "--version".to_string()]
        );
        assert!(resolve_argv("claude", &[], &dirs, &|_| false).is_none());
        assert!(resolve_argv("D:/nope/claude.exe", &[], &dirs, &|_| false).is_none());
    }

    #[test]
    fn command_with_its_own_extension_is_not_searched_as_double_extension() {
        let dirs = vec![PathBuf::from("C:/tools")];
        let pwsh = dirs[0].join("pwsh.exe");
        // Must resolve the literal file, not go looking for `pwsh.exe.exe`.
        assert_eq!(resolve_argv("pwsh.exe", &[], &dirs, &|p| p == pwsh).unwrap(), vec![pwsh.to_string_lossy().into_owned()]);
        // If only a PATHEXT-suffixed shim is on PATH, the fallback still finds it.
        let shim = dirs[0].join("pwsh.exe.cmd");
        assert_eq!(
            resolve_argv("pwsh.exe", &[], &dirs, &|p| p == shim).unwrap(),
            vec!["cmd.exe".to_string(), "/s".to_string(), "/c".to_string(), format!("\"\"{}\"\"", shim.to_string_lossy())]
        );
    }

    #[test]
    fn cmd_shim_args_are_quoted_for_cmd_exe_not_the_shell() {
        let dirs = vec![PathBuf::from("C:/npm")];
        let shim = dirs[0].join("claude.cmd");
        let exists = |p: &Path| p == shim;

        // A space-containing arg must stay one argument, not split in two.
        let args = vec!["--prompt".to_string(), "hello world".to_string()];
        let argv = resolve_argv("claude", &args, &dirs, &exists).unwrap();
        assert_eq!(&argv[..3], &["cmd.exe", "/s", "/c"]);
        let inner = &argv[3];
        assert!(inner.starts_with('"') && inner.ends_with('"'), "wrapped for /S: {inner}");
        assert!(inner.contains("\"hello world\""), "the space stays inside one quoted token: {inner}");

        // A cmd.exe metacharacter must not chain a second command.
        let danger = vec!["a&b".to_string()];
        let argv = resolve_argv("claude", &danger, &dirs, &exists).unwrap();
        assert!(argv[3].contains("a^&b"), "& is caret-escaped: {}", argv[3]);
    }

    #[test]
    fn colors_fold_onto_the_16_ansi_slots() {
        assert_eq!(map_color(vt100::Color::Idx(9)), Some(Color::Indexed(9)));
        assert_eq!(map_color(vt100::Color::Idx(0)), Some(Color::Indexed(0)));
        assert_eq!(map_color(vt100::Color::Default), None);
        assert!(matches!(map_color(vt100::Color::Rgb(255, 0, 0)), Some(Color::Indexed(9 | 1))));
        assert!(matches!(map_color(vt100::Color::Rgb(0, 0, 0)), Some(Color::Indexed(0))));
        assert!(matches!(map_color(vt100::Color::Rgb(255, 255, 255)), Some(Color::Indexed(15 | 7))));
        // 196 is the cube's pure red, 231 its white, 232 the darkest grey.
        assert!(matches!(map_color(vt100::Color::Idx(196)), Some(Color::Indexed(9 | 1))));
        assert!(matches!(map_color(vt100::Color::Idx(231)), Some(Color::Indexed(15 | 7))));
        assert!(matches!(map_color(vt100::Color::Idx(232)), Some(Color::Indexed(0 | 8))));
    }

    #[test]
    fn rows_merge_equal_style_runs() {
        let t = Theme::new(ThemeKind::Color);
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"ab\x1b[31mcd");
        let line = screen_line(parser.screen(), 0, 10, t);
        // "ab" (default) + "cd" (red) + the untouched rest of the row.
        assert_eq!(line.spans.len(), 3, "one run per style: {:?}", line.spans);
        assert_eq!(line.spans[0].content.as_ref(), "ab");
        assert_eq!(line.spans[1].content.as_ref(), "cd");
        assert_eq!(line.spans[1].style.fg, Some(Color::Indexed(1)));
        assert_eq!(line.spans[2].content.as_ref(), "      ");
        assert_eq!(line.spans.iter().map(|s| s.content.chars().count()).sum::<usize>(), 10, "the row is exactly `cols` wide");
    }

    #[test]
    fn capture_state_machine() {
        let (ctx, _notices) = crate::shell::test_ctx(toml::Table::new());
        // Not captured: only our own keys are consumed, the shell keeps the rest.
        assert_eq!(view_action(key(KeyCode::Enter)), Action::Attach);
        assert_eq!(view_action(key(KeyCode::Char('i'))), Action::Attach);
        assert_eq!(view_action(key(KeyCode::Char('r'))), Action::Restart);
        assert_eq!(view_action(key(KeyCode::PageUp)), Action::Scroll(1));
        assert_eq!(view_action(key(KeyCode::PageDown)), Action::Scroll(-1));
        assert_eq!(view_action(key(KeyCode::Char('q'))), Action::Pass, "q must still quit");
        assert_eq!(view_action(key(KeyCode::Char('3'))), Action::Pass, "digits still switch tabs");

        let mut term = Term::new();
        // No command resolved: Enter reports it and does not capture.
        assert!(term.on_key(key(KeyCode::Enter), &ctx));
        assert!(!term.captured);
        assert_eq!(term.state(), "not found");

        // Captured: every key is eaten, only the release key gets out.
        term.captured = true;
        assert!(term.on_key(key(KeyCode::Char('q')), &ctx));
        assert!(term.captured, "q belongs to the process");
        assert!(term.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), &ctx));
        assert!(term.captured, "Ctrl+C belongs to the process");
        assert!(term.on_key(key(KeyCode::Char('1')), &ctx));
        assert!(term.captured);
        assert!(term.on_key(key(KeyCode::F(12)), &ctx));
        assert!(!term.captured, "f12 releases");
        assert!(term.help().starts_with("enter attach"));
        term.captured = true;
        assert!(term.help().contains("captured"));
    }

    #[test]
    fn draw_and_summaries_survive_tiny_areas_without_a_process() {
        let t = Theme::new(ThemeKind::Color);
        let mut term = Term::new();
        term.argv = Some(vec!["claude".to_string()]);

        for (w, h) in [(40u16, 12u16), (1, 1)] {
            let mut tui = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            tui.draw(|f| term.draw(f, f.area(), t)).unwrap();
        }
        // Not-found tab and an exited process at the same sizes.
        let missing = Term::new();
        let mut exited = Term::new();
        exited.argv = Some(vec!["claude".to_string()]);
        exited.exited = Some(1);
        for (w, h) in [(40u16, 12u16), (1, 1)] {
            let mut tui = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            tui.draw(|f| missing.draw(f, f.area(), t)).unwrap();
            let mut tui = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            tui.draw(|f| exited.draw(f, f.area(), t)).unwrap();
        }

        assert_eq!(term.status(), "term pwsh running=false captured=false");
        assert_eq!(term.overview(30, 5, t).len(), 2);
        assert_eq!(term.overview_slot(), Slot::Right(5));
        assert!(term.header(20, t).is_empty(), "no marker while released");
        term.captured = true;
        assert_eq!(term.header(20, t).len(), 1);
        assert!(term.header(4, t).is_empty(), "dropped when it does not fit");
        assert!(!term.wants_fast_frames(true), "nothing running, stay at 4 fps");
    }

    /// Without this answer ConPTY never releases a single byte of output —
    /// the tab stays black even for `cmd /c echo hi` (seen live, 2026-09-10).
    #[test]
    fn conpty_cursor_query_is_answered_with_a_cursor_position_report() {
        assert_eq!(dsr_requests(b"[6n[?9001h[?1004h"), 1);
        assert_eq!(dsr_requests(b"plain text [6m"), 0);
        assert_eq!(cursor_report((0, 0)), b"[1;1R");
        assert_eq!(cursor_report((3, 10)), b"[4;11R");
    }

    #[test]
    fn drain_caps_chunks_per_call_so_a_spewing_child_cannot_stall_the_ui() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let mut term = Term::new();
        term.rx = Some(rx);
        for _ in 0..100 {
            tx.send(b"x".to_vec()).unwrap();
        }
        assert_eq!(term.drain(), 64, "capped even though 100 chunks were queued");
        assert_eq!(term.drain(), 36, "the rest drains on a later call");
        assert_eq!(term.drain(), 0);
    }
}
