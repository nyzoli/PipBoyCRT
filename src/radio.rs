//! RADIO forrás: stream-lejátszás, ICY-metaadat, VU + spektrum egy saját szálon.
//!
//! A lejátszó a héj közös mixerére (`Audio::mixer()`) csatlakozik; saját
//! hangeszközt nem nyit. A riasztás nem itt szól, hanem az `audio.rs`-ben.
use crate::modules::radio::Station;
use crate::ui::vu::{Vu, BANDS, VU_BLOCK};
use icy_metadata::{IcyHeaders, IcyMetadataReader, RequestIcyMetadata};
use rodio::mixer::Mixer;
use rodio::{Decoder, Player, Source};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use stream_download::http::reqwest::Client;
use stream_download::http::HttpStream;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};
use tokio::runtime::Handle;

const MAX_RECONNECT: u32 = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum RadioStatus {
    Stopped,
    Connecting,
    Playing,
    Paused,
    Error(String),
}

#[derive(Clone, Debug)]
pub enum RadioEvent {
    Connected { name: Option<String>, bitrate: Option<u32>, content_type: String },
    Title(String),
    Level(f32),
    Spectrum([u8; BANDS]),
    Log(String),
    Error(String),
}

/// A modul parancsai a lejátszó szálnak; a modulon kívülre nem szivárog.
#[derive(Clone, Debug, PartialEq)]
pub enum RadioCmd {
    Tune(Station),
    Play,
    Pause,
    Volume(u8),
    Mute(bool),
}

/// "audio/mpeg" → "mp3", "audio/aac" / "audio/aacp" → "aac", egyéb → a perjel utáni rész vagy "?".
pub fn codec_label(content_type: &str) -> &str {
    let sub = content_type.split(';').next().unwrap_or("").trim().rsplit('/').next().unwrap_or("");
    match sub {
        "mpeg" | "mp3" => "mp3",
        "aac" | "aacp" => "aac",
        "" => "?",
        other => other,
    }
}

#[derive(Debug, PartialEq)]
pub enum Reconnect { Healthy, Retry { attempt: u32, wait_s: u64 }, GiveUp }

/// A Timeout-ág döntése: egészséges streamnél semmi; különben a következő próba vagy feladás.
pub fn reconnect_decision(attempts: u32, playing: bool) -> Reconnect {
    if playing { return Reconnect::Healthy; }
    if attempts >= MAX_RECONNECT { return Reconnect::GiveUp; }
    let attempt = attempts + 1;
    Reconnect::Retry { attempt, wait_s: 2u64.pow(attempt) }
}

/// Elindítja a lejátszó szálat a héj mixerére. `mixer == None` → nincs hangeszköz.
pub fn spawn(handle: Handle, mixer: Option<Mixer>, tx: Sender<RadioEvent>) -> Sender<RadioCmd> {
    let (ctx, crx) = mpsc::channel();
    std::thread::spawn(move || run(handle, mixer, tx, crx));
    ctx
}

/// Egy parancs alkalmazása; `Tune` és a csatorna lezárása kívül marad.
fn apply(cmd: RadioCmd, player: &Player, volume: &mut f32, muted: &mut bool, paused: &mut bool) {
    match cmd {
        RadioCmd::Play => { *paused = false; player.play(); }
        RadioCmd::Pause => { *paused = true; player.pause(); }
        RadioCmd::Volume(v) => { *volume = v as f32 / 100.0; if !*muted { player.set_volume(*volume); } }
        RadioCmd::Mute(m) => { *muted = m; player.set_volume(if m { 0.0 } else { *volume }); }
        RadioCmd::Tune(_) => {}
    }
}

fn run(handle: Handle, mixer: Option<Mixer>, tx: Sender<RadioEvent>, crx: Receiver<RadioCmd>) {
    let Some(mixer) = mixer else {
        let _ = tx.send(RadioEvent::Error("no audio device".into()));
        return;
    };
    let player = Player::connect_new(&mixer);
    let mut volume = 0.7f32;
    let mut muted = false;
    let mut paused = false;
    let mut current: Option<Station> = None;
    let mut attempts = 0u32;
    player.set_volume(volume);

    loop {
        match crx.recv_timeout(Duration::from_millis(500)) {
            Ok(RadioCmd::Tune(st)) => {
                paused = false;
                attempts = 0;
                current = connect(&handle, &player, &st, &tx, paused).then_some(st);
            }
            Ok(cmd) => apply(cmd, &player, &mut volume, &mut muted, &mut paused),
            Err(RecvTimeoutError::Timeout) => {
                // Élő stream sosem fogy el: ha a sor üres, a kapcsolat szakadt.
                let Some(st) = current.clone() else { continue };
                let (attempt, wait) = match reconnect_decision(attempts, !player.empty()) {
                    Reconnect::Healthy => { attempts = 0; continue; }
                    Reconnect::GiveUp => {
                        let _ = tx.send(RadioEvent::Error("stream lost".into()));
                        current = None;
                        continue;
                    }
                    Reconnect::Retry { attempt, wait_s } => (attempt, wait_s),
                };
                attempts = attempt;
                let _ = tx.send(RadioEvent::Log(format!(
                    "stream lost, retry in {wait} s ({attempts}/{MAX_RECONNECT})"
                )));
                // Várakozás a következő próbáig, de egy közben érkező Tune azonnal átveszi az irányítást.
                let deadline = std::time::Instant::now() + Duration::from_secs(wait);
                let mut retuned = false;
                while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero()) {
                    match crx.recv_timeout(left) {
                        Ok(RadioCmd::Tune(st)) => {
                            paused = false;
                            attempts = 0;
                            current = connect(&handle, &player, &st, &tx, paused).then_some(st);
                            retuned = true;
                            break;
                        }
                        Ok(cmd) => apply(cmd, &player, &mut volume, &mut muted, &mut paused),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                if !retuned {
                    connect(&handle, &player, &st, &tx, paused);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Felépíti a láncot és elindítja; hibát `RadioEvent::Error`-ként küld. `true`, ha szól.
/// `paused`: ha a lejátszó szüneteltetve volt (rendszerint reconnect közben Pause érkezett),
/// a csatlakozás nem indítja el automatikusan a lejátszást.
fn connect(handle: &Handle, player: &Player, st: &Station, tx: &Sender<RadioEvent>, paused: bool) -> bool {
    match open(handle, st, tx.clone()) {
        Ok((src, ev)) => {
            player.clear(); // clear() szüneteltet is, ezért utána play(), ha nincs szüneteltetve
            player.append(src);
            if !paused {
                player.play();
            }
            let _ = tx.send(ev);
            true
        }
        Err(e) => {
            let _ = tx.send(RadioEvent::Error(e));
            false
        }
    }
}

fn open(handle: &Handle, st: &Station, tx: Sender<RadioEvent>) -> Result<(Box<dyn Source + Send>, RadioEvent), String> {
    let url = st.url.parse().map_err(|e| format!("bad URL: {e}"))?;
    let client = Client::builder()
        .request_icy_metadata()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let stream = handle.block_on(HttpStream::new(client, url)).map_err(|e| e.to_string())?;
    let icy = IcyHeaders::parse_from_headers(stream.headers());
    let ev = RadioEvent::Connected {
        name: icy.name().map(String::from),
        bitrate: icy.bitrate(),
        content_type: stream.header("content-type").unwrap_or("").to_string(),
    };
    let metaint = icy.metadata_interval();
    let reader = handle
        .block_on(StreamDownload::from_stream(
            stream,
            TempStorageProvider::new(),
            Settings::default().prefetch_bytes(64 * 1024),
        ))
        .map_err(|e| e.to_string())?;
    let tx2 = tx.clone();
    let reader = IcyMetadataReader::new(reader, metaint, move |m| {
        if let Ok(m) = m {
            if let Some(t) = m.stream_title() {
                let _ = tx2.send(RadioEvent::Title(t.to_string()));
            }
        }
    });
    let dec = Decoder::new(reader).map_err(|e| format!("decoder: {e}"))?;
    Ok((Box::new(Vu::new(dec, tx, VU_BLOCK, RadioEvent::Level, RadioEvent::Spectrum)), ev))
}

// --- Favorites: RADIO's `*` saves into a note in notes.md instead of its own
// file (task F). Reuses `modules::notes`' parse/render — it already carries
// the file's exact formatting rules — instead of re-implementing them here.
use crate::modules::notes::{parse_notes, render_notes, Note};

/// Title of the notes.md note that collects saved RADIO tracks.
pub const FAVORITES_NOTE: &str = "Favorite tracks";

/// Append `entry` as the last body line of the `# Favorite tracks` note,
/// creating the note at the end of the file if it doesn't exist yet. Pure:
/// returns the new file content, built through the same parse/render round
/// trip `notes.md` itself uses, so the result matches `render_notes`'
/// formatting (one blank line between notes).
pub fn add_favorite(notes_md: &str, entry: &str) -> String {
    let mut notes = parse_notes(notes_md);
    match notes.iter_mut().find(|n| n.title == FAVORITES_NOTE) {
        Some(n) => n.body.push(entry.to_string()),
        None => notes.push(Note { title: FAVORITES_NOTE.to_string(), body: vec![entry.to_string()] }),
    }
    render_notes(&notes)
}

/// The non-empty body lines of the `# Favorite tracks` note (empty if absent).
pub fn favorites(notes_md: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_section = false;
    for line in notes_md.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            in_section = rest == FAVORITES_NOTE;
            continue;
        }
        if in_section && !line.trim().is_empty() {
            out.push(line);
        }
    }
    out
}

/// The most recently saved entry, if any.
pub fn last_favorite(notes_md: &str) -> Option<&str> {
    favorites(notes_md).into_iter().next_back()
}

/// Number of saved favourite tracks — used by STAT's S.P.E.C.I.A.L. Charisma.
pub fn count_favorites(notes_md: &str) -> usize {
    favorites(notes_md).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_favorite_creates_the_note_when_absent() {
        let out = add_favorite("", "- 2026-09-10 21:05 \u{b7} FIP \u{b7} A \u{b7} B");
        let notes = parse_notes(&out);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, FAVORITES_NOTE);
        assert_eq!(notes[0].body, vec!["- 2026-09-10 21:05 \u{b7} FIP \u{b7} A \u{b7} B".to_string()]);
    }

    #[test]
    fn add_favorite_creates_the_note_after_existing_ones_with_blank_separation() {
        let out = add_favorite("# Shopping\nmilk\n", "- entry1");
        assert_eq!(out, "# Shopping\nmilk\n\n# Favorite tracks\n- entry1\n");
        let notes = parse_notes(&out);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].title, "Shopping");
        assert_eq!(notes[1].title, FAVORITES_NOTE);
    }

    #[test]
    fn add_favorite_appends_to_existing_note_and_leaves_others_untouched() {
        let start = "# Favorite tracks\n- entry1\n\n# Shopping\nmilk\n";
        let out = add_favorite(start, "- entry2");
        let notes = parse_notes(&out);
        assert_eq!(notes[0].title, FAVORITES_NOTE);
        assert_eq!(notes[0].body, vec!["- entry1".to_string(), "- entry2".to_string()]);
        assert_eq!(notes[1].title, "Shopping");
        assert_eq!(notes[1].body, vec!["milk".to_string()]);
    }

    #[test]
    fn favorites_and_last_favorite() {
        assert_eq!(favorites(""), Vec::<&str>::new());
        assert_eq!(last_favorite(""), None);
        let md = "# Favorite tracks\n- entry1\n- entry2\n\n# Shopping\nmilk\n";
        assert_eq!(favorites(md), vec!["- entry1", "- entry2"]);
        assert_eq!(last_favorite(md), Some("- entry2"));
        assert_eq!(count_favorites(md), 2);
    }

    #[test]
    fn codec_labels() {
        assert_eq!(codec_label("audio/mpeg"), "mp3");
        assert_eq!(codec_label("audio/aacp"), "aac");
        assert_eq!(codec_label("audio/ogg; charset=utf-8"), "ogg");
        assert_eq!(codec_label(""), "?");
    }

    #[test]
    fn reconnect_policy() {
        assert_eq!(reconnect_decision(0, true), Reconnect::Healthy);
        assert_eq!(reconnect_decision(2, true), Reconnect::Healthy);
        assert_eq!(reconnect_decision(0, false), Reconnect::Retry { attempt: 1, wait_s: 2 });
        assert_eq!(reconnect_decision(1, false), Reconnect::Retry { attempt: 2, wait_s: 4 });
        assert_eq!(reconnect_decision(2, false), Reconnect::Retry { attempt: 3, wait_s: 8 });
        assert_eq!(reconnect_decision(3, false), Reconnect::GiveUp);
    }
}
