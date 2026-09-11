mod audio;
mod clock;
mod config;
mod module;
mod modules;
mod net;
mod open;
mod radio;
mod sfx;
mod shell;
mod stat;
mod style;
mod ui;
mod weather;
mod wifi;

use audio::Audio;
use config::RawConfig;
use module::{Blackboard, Ctx, Module, ModuleConfig, Notice};
use shell::Shell;
use std::sync::mpsc;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let (cfg, notice) = RawConfig::load(&RawConfig::path());
    let rt = tokio::runtime::Runtime::new()?;

    // `pipboy --probe [secs]`: nincs TUI, csak a modulok indulnak és a status() sorok látszanak.
    // A hangeszközt ilyenkor nem foglaljuk el (egy futó példány mellett is elindul).
    let probe: Option<u64> = (std::env::args().nth(1).as_deref() == Some("--probe"))
        .then(|| std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(12));
    let audio = Arc::new(if probe.is_some() { Audio::silent() } else { Audio::start() });

    let (notify, notices) = mpsc::channel::<Notice>();
    let ctx = Ctx {
        rt: rt.handle().clone(),
        config: Arc::new(ModuleConfig(cfg.modules.0)),
        audio,
        board: Blackboard::new(),
        notify,
    };
    let registry: Vec<Box<dyn Module>> = vec![
        Box::new(modules::stat::Stat::new()),
        Box::new(modules::weather::Weather::new()),
        Box::new(modules::radio::Radio::new()),
        Box::new(modules::music::Music::new()),
        Box::new(modules::net::Net::new()),
        Box::new(modules::wifi::Wifi::new()),
        Box::new(modules::wasteland::Wasteland::new()),
        Box::new(modules::clock::Clock::new()),
        Box::new(modules::news::News::new()),
        Box::new(modules::mail::Mail::new()),
        Box::new(modules::notes::Notes::new()),
        Box::new(modules::syslog::Syslog::new()),
        Box::new(modules::art::Art::new()),
        Box::new(modules::globe::Globe::new()),
        Box::new(modules::term::Term::new()),
    ];
    let mut shell = Shell::new(registry, style::Theme::new(cfg.theme), notice, ctx, notices);

    if let Some(secs) = probe {
        println!("probe {secs}s — no TUI, no audio device (the radio reports \"no audio device\")");
        shell.probe(secs);
        drop(rt);
        return Ok(());
    }

    // `panic = "abort"` (release profile) makes every panic fatal, on any
    // thread, so the terminal must be restored unconditionally here.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        hook(info);
    }));
    let mut terminal = ratatui::init();
    let result = shell.run(&mut terminal);
    ratatui::restore();
    drop(rt);
    result
}
