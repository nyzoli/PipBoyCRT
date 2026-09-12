//! Shared audio output for all modules: one output device, one mixer.
//!
//! The device sink lives on its own thread; modules get a cloneable
//! [`rodio::mixer::Mixer`] to attach their own `Player`s (radio), and a
//! command API for the built-in alarm player (timers). Volume/mute are
//! applied to the alarm player here; modules with their own `Player` apply
//! the same values themselves (the radio module owns the user-facing volume).

use rodio::mixer::Mixer;
use rodio::{DeviceSinkBuilder, Player};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

enum AudioCmd {
    Alarm,
    Geiger,
    Stop,
    Volume(f32),
}

pub struct Audio {
    mixer: Option<Mixer>,
    tx: Mutex<Option<Sender<AudioCmd>>>,
}

impl Audio {
    /// Open the default output device on a dedicated thread. If there is no
    /// device — or it does not answer within 3 s — `mixer()` is `None` and every
    /// command is a no-op; the app runs on.
    pub fn start() -> Self {
        let (ready_tx, ready_rx) = mpsc::channel::<Option<Mixer>>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
        thread::spawn(move || {
            let sink = match DeviceSinkBuilder::open_default_sink() {
                Ok(s) => s,
                Err(_) => {
                    let _ = ready_tx.send(None);
                    return;
                }
            };
            let mut sink = sink;
            sink.log_on_drop(false);
            let _ = ready_tx.send(Some(sink.mixer().clone()));
            let alarm = Player::connect_new(sink.mixer());
            alarm.set_volume(0.7);
            // The sink must stay alive as long as anything plays: this thread holds it.
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    AudioCmd::Alarm => {
                        alarm.clear();
                        for _ in 0..5 {
                            for s in crate::sfx::alarm_sequence() {
                                alarm.append(s);
                            }
                        }
                        alarm.play();
                    }
                    AudioCmd::Geiger => {
                        alarm.clear();
                        for s in crate::sfx::geiger_burst() {
                            alarm.append(s);
                        }
                        alarm.play();
                    }
                    AudioCmd::Stop => alarm.clear(),
                    AudioCmd::Volume(v) => alarm.set_volume(v),
                }
            }
        });
        // Ha a WASAPI beragad, 3 s után úgy viselkedünk, mintha nem lenne eszköz:
        // az induló képernyő sosem vár a hangkártyára.
        let mixer = ready_rx.recv_timeout(Duration::from_secs(3)).unwrap_or(None);
        let tx = if mixer.is_some() { Some(cmd_tx) } else { None };
        Self { mixer, tx: Mutex::new(tx) }
    }

    /// A test double with no device: `mixer()` is `None`, commands are ignored.
    pub fn silent() -> Self {
        Self { mixer: None, tx: Mutex::new(None) }
    }

    /// Mixer to attach a module's own `Player` to (`Player::connect_new(&mixer)`).
    pub fn mixer(&self) -> Option<Mixer> {
        self.mixer.clone()
    }

    fn send(&self, cmd: AudioCmd) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(cmd);
        }
    }

    /// Play the timer alarm (five repetitions of the synthesized sequence).
    pub fn alarm(&self) {
        self.send(AudioCmd::Alarm);
    }

    /// Play a short Geiger crackle burst (DOSIMETER over-exposure).
    pub fn geiger(&self) {
        self.send(AudioCmd::Geiger);
    }

    /// Stop the alarm.
    pub fn stop_alarm(&self) {
        self.send(AudioCmd::Stop);
    }

    /// Effective volume for the alarm player: `0.0` when muted, else 0.0–1.0.
    pub fn set_volume(&self, v: f32) {
        self.send(AudioCmd::Volume(v.clamp(0.0, 1.0)));
    }
}
