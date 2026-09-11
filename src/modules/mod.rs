//! A beépített modulok. Új modul = egy fájl itt + egy sor a `main.rs` registryben.
//! Lásd `docs/adding-a-module.md`.
pub mod art;
pub mod clock;
// A coordinator registers COMMS in `main.rs`; until then nothing references it.
#[allow(dead_code)]
pub mod comms;
pub mod globe;
pub mod mail;
pub mod music;
pub mod net;
pub mod news;
pub mod notes;
pub mod radio;
pub mod stat;
pub mod syslog;
pub mod term;
pub mod weather;
pub mod wifi;
pub mod wasteland;
