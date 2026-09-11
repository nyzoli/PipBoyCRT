//! A beépített modulok. Új modul = egy fájl itt + egy sor a `main.rs` registryben.
//! Lásd `docs/adding-a-module.md`.
pub mod art;
pub mod clock;
// The registry line in main.rs lands with the coordinator's wiring.
#[allow(dead_code)]
pub mod dosimeter;
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
