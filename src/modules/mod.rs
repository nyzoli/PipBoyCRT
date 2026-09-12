//! A beépített modulok. Új modul = egy fájl itt + egy sor a `main.rs` registryben.
//! Lásd `docs/adding-a-module.md`.
pub mod art;
pub mod clock;
pub mod dosimeter;
pub mod globe;
pub mod mail;
pub mod music;
pub mod net;
pub mod news;
pub mod notes;
/// Registered by the coordinator (one line in the `main.rs` registry, after ART);
/// until then nothing constructs it, so the whole module reads as dead code.
#[allow(dead_code)]
pub mod quest;
pub mod radio;
pub mod stat;
pub mod syslog;
pub mod term;
pub mod weather;
pub mod wifi;
pub mod wasteland;
