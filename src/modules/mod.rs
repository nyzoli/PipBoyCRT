//! A beépített modulok. Új modul = egy fájl itt + egy sor a `main.rs` registryben.
//! Lásd `docs/adding-a-module.md`.
pub mod art;
pub mod clock;
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
// Registered by the coordinator; the allow goes away with the registry line.
#[allow(dead_code)]
pub mod wasteland;
