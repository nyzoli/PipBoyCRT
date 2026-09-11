//! WASTELAND module: the devices on your local network.
//!
//! The source is the Windows IPv4 neighbour (ARP) table, read with
//! `GetIpNetTable2` on a background thread; an optional ICMP sweep of the local
//! /24 (the [`Pinger`] NET already owns) makes silent hosts show up there in
//! the first place. No connection is made outside the subnet for the sweep or
//! the neighbour table — reverse-DNS lookups are the exception, going out to
//! whatever resolver Windows is configured to use. The only thing written to
//! disk is `wasteland.json` next to the executable (MAC → name, first/last
//! seen, last local IP) so a device that is asleep is still listed.
//!
//! Everything `unsafe` lives in the [`ffi`] module at the bottom: every return
//! code is checked, the table the API allocates is released by a `Drop` guard
//! (`FreeMibTable`), and nothing here panics on a hostile or empty table.

use crate::module::{Ctx, Module, Notice, Slot};
use crate::net::icmp::Pinger;
use crate::net::parse_ipconfig;
use crate::style::Theme;
use crate::ui::widgets::truncate;
use chrono::{DateTime, Local, TimeZone};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Shortest accepted `interval`: a sweep plus reverse DNS is not free.
const MIN_INTERVAL: u64 = 20;
/// The sweep runs on the first scan and every Nth one after that.
const SWEEP_EVERY: u64 = 5;
/// Per-host ICMP timeout during a sweep.
const SWEEP_TIMEOUT_MS: u32 = 300;
/// Pings in flight during a sweep.
const SWEEP_PARALLEL: usize = 32;
/// Timeout of the single ping behind the `p` key.
const PING_TIMEOUT_MS: u32 = 1000;
/// Largest /24 host range a sweep will touch.
const MAX_HOSTS: usize = 254;
/// Reverse-DNS lookups started per scan, and how long each one may take.
const MAX_LOOKUPS: usize = 64;
const DNS_TIMEOUT: Duration = Duration::from_secs(1);
/// Lookup threads sharing the [`MAX_LOOKUPS`] work queue, so a scan with many
/// cold addresses waits at most `MAX_LOOKUPS / DNS_PARALLEL * DNS_TIMEOUT`.
const DNS_PARALLEL: usize = 8;
/// How long a resolved (or unresolved) name is trusted before asking again.
const DNS_TTL: Duration = Duration::from_secs(3600);
/// A device that has not answered for longer than this drops off the list.
const KEEP_OFFLINE_SECS: i64 = 7 * 24 * 3600;
/// Longest accepted host name; a reverse-DNS answer is untrusted text.
const MAX_NAME: usize = 40;
/// Plausibility cap on the neighbour count the API reports.
const MAX_ROWS: usize = 4096;

/// Column widths of the wide layout.
const IP_W: usize = 15;
const NAME_W: usize = 18;
const VEND_W: usize = 11;
const MAC_W: usize = 17;
const SEEN_W: usize = 5;
/// Below this width only IP, NAME and SEEN are shown.
const NARROW: u16 = 80;

// ---- config ------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WastelandCfg {
    /// Rescan period in seconds, clamped to at least [`MIN_INTERVAL`].
    pub interval: u64,
    /// Ping every address of the subnet so silent hosts enter the ARP table.
    /// Visible to anything watching the network — set to `false` to stay quiet.
    pub sweep: bool,
    /// `"192.168.100.0/24"` overrides the subnet derived from the gateway.
    pub subnet: String,
}

impl Default for WastelandCfg {
    fn default() -> Self {
        Self { interval: 60, sweep: true, subnet: String::new() }
    }
}

// ---- pure helpers (subnet, OUI, formatting) ----------------------------------

/// `"192.168.100.0/24"` → network address + prefix bits. Only sane prefixes
/// (8–30) are accepted; anything else is a config typo.
fn parse_cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, bits) = s.trim().split_once('/')?;
    let addr: Ipv4Addr = addr.trim().parse().ok()?;
    let bits: u8 = bits.trim().parse().ok()?;
    if !(8..=30).contains(&bits) {
        return None;
    }
    Some((network(addr, bits), bits))
}

/// The network address of `ip` under a `bits`-long prefix.
fn network(ip: Ipv4Addr, bits: u8) -> Ipv4Addr {
    let mask: u32 = if bits == 0 { 0 } else { u32::MAX << (32 - bits.min(32)) };
    Ipv4Addr::from(u32::from(ip) & mask)
}

fn in_subnet(ip: Ipv4Addr, net: Ipv4Addr, bits: u8) -> bool {
    network(ip, bits) == net
}

/// `192.168.1.0/24` for the title line.
fn subnet_label(net: Ipv4Addr, bits: u8) -> String {
    format!("{net}/{bits}")
}

/// Every host address of the subnet, capped at [`MAX_HOSTS`] (a /24 worth).
fn host_addrs(net: Ipv4Addr, bits: u8) -> Vec<Ipv4Addr> {
    let span = 1u64 << (32 - bits.min(32)) as u32;
    let hosts = span.saturating_sub(2).min(MAX_HOSTS as u64) as u32;
    let base = u32::from(net);
    (1..=hosts).map(|i| Ipv4Addr::from(base + i)).collect()
}

/// The broadcast address of `net` under a `bits`-long prefix.
fn broadcast(net: Ipv4Addr, bits: u8) -> Ipv4Addr {
    let mask: u32 = if bits == 0 { 0 } else { u32::MAX << (32 - bits.min(32)) };
    Ipv4Addr::from(u32::from(net) | !mask)
}

/// Is this neighbour worth showing? Multicast, the subnet's own network and
/// broadcast address, APIPA and entries without a real MAC are noise, and so
/// is anything off the subnet. A host address merely ending in `.0` or `.255`
/// is not noise on anything narrower than a /24 — only the network's own
/// network/broadcast address is dropped.
fn keep_entry(ip: Ipv4Addr, mac: &[u8], net: Ipv4Addr, bits: u8) -> bool {
    if mac.len() != 6 || mac.iter().all(|&b| b == 0) || mac.iter().all(|&b| b == 0xff) {
        return false;
    }
    let o = ip.octets();
    if o[0] >= 224 || ip.is_broadcast() {
        return false;
    }
    if o[0] == 169 && o[1] == 254 {
        return false;
    }
    if !in_subnet(ip, net, bits) {
        return false;
    }
    ip != net && ip != broadcast(net, bits)
}

/// `aa-bb-cc-dd-ee-ff`, the spelling Windows itself uses.
fn mac_string(mac: &[u8]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join("-")
}

/// A best-effort vendor table: the prefixes seen on a typical home network.
/// ponytail: 40 hand-picked OUIs instead of the 30k-entry IEEE registry —
/// swap in the full list only if "?" starts showing up more often than names.
const OUI: [(&str, &str); 43] = [
    ("000393", "Apple"),
    ("ACBC32", "Apple"),
    ("F01898", "Apple"),
    ("001632", "Samsung"),
    ("781FDB", "Samsung"),
    ("B827EB", "Raspberry Pi"),
    ("DCA632", "Raspberry Pi"),
    ("E45F01", "Raspberry Pi"),
    ("50C7BF", "TP-Link"),
    ("A42BB0", "TP-Link"),
    ("24A43C", "Ubiquiti"),
    ("788A20", "Ubiquiti"),
    ("FCECDA", "Ubiquiti"),
    ("001B21", "Intel"),
    ("3C970E", "Intel"),
    ("8C1645", "Intel"),
    ("240AC4", "Espressif"),
    ("30AEA4", "Espressif"),
    ("84F3EB", "Espressif"),
    ("A020A6", "Espressif"),
    ("44650D", "Amazon"),
    ("FC65DE", "Amazon"),
    ("3C5AB4", "Google"),
    ("F4F5E8", "Google"),
    ("000E58", "Sonos"),
    ("5CAAFD", "Sonos"),
    ("001788", "Philips Hue"),
    ("640980", "Xiaomi"),
    ("8CBEBE", "Xiaomi"),
    ("00E0FC", "Huawei"),
    ("001422", "Dell"),
    ("F8BC12", "Dell"),
    ("001B78", "HP"),
    ("3CD92B", "HP"),
    ("6C5F1C", "Lenovo"),
    ("00155D", "Microsoft"),
    ("001132", "Synology"),
    ("00089B", "QNAP"),
    ("00146C", "Netgear"),
    ("2C56DC", "ASUS"),
    ("3810D5", "AVM FRITZ!Box"),
    ("001A2F", "Cisco"),
    ("0009BF", "Nintendo"),
];

/// Vendor for a MAC, matched on its first three bytes, case- and
/// separator-insensitive (`AA:BB:CC`, `aa-bb-cc`, `aabbcc` all work).
fn oui(mac: &str) -> Option<&'static str> {
    let hex: String = mac.chars().filter(|c| c.is_ascii_hexdigit()).take(6).collect();
    if hex.len() < 6 {
        return None;
    }
    let hex = hex.to_ascii_uppercase();
    OUI.iter().find(|(p, _)| *p == hex).map(|(_, v)| *v)
}

/// `now` / `12 min` / `3 h` / `2 d` for the SEEN column.
fn seen_ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => "now".to_string(),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86_400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86_400),
    }
}

fn stamp(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => "?".to_string(),
    }
}

/// Reverse-DNS answers and renames are untrusted text: keep printable
/// characters only, drop anything that could repaint the terminal, cap the
/// length.
fn sanitize_name(s: &str) -> String {
    let clean: String = s
        .chars()
        .filter(|&c| {
            let code = c as u32;
            code >= 0x20
                && code != 0x7f
                && !(0x80..=0x9f).contains(&code)
                && !matches!(code, 0x200b..=0x200d | 0x202d | 0x202e | 0x2066..=0x2069 | 0xfeff)
        })
        .collect();
    truncate(clean.trim(), MAX_NAME)
}

/// Truncates to `n` characters, then pads to exactly `n` with spaces.
fn pad(s: &str, n: usize) -> String {
    let t = truncate(s, n);
    let fill = n.saturating_sub(t.chars().count());
    format!("{t}{}", " ".repeat(fill))
}

// ---- memory (wasteland.json) -------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
struct MemEntry {
    /// Reverse-DNS answer or the user's rename; empty = unknown.
    name: String,
    first_seen: i64,
    last_seen: i64,
    /// Last local IPv4 the MAC had, so a sleeping device is still listable.
    ip: String,
}

type Memory = BTreeMap<String, MemEntry>;

/// A MAC the memory has never held is a device that has never been here.
fn is_new_mac(mem: &Memory, mac: &str) -> bool {
    !mem.contains_key(mac)
}

/// A MAC seen for the first time this cycle is flagged & announced as new —
/// unless this is the baseline scan of an empty memory, when every device
/// would otherwise be noise, not signal.
fn note_if_new(mem: &Memory, session_new: &mut HashSet<String>, first_run: bool, mac: &str) -> bool {
    if first_run || !is_new_mac(mem, mac) {
        return false;
    }
    session_new.insert(mac.to_string());
    true
}

/// A corrupt file is not fatal: the caller starts fresh and says so.
fn parse_memory(s: &str) -> Option<Memory> {
    if s.trim().is_empty() {
        return Some(Memory::new());
    }
    serde_json::from_str(s).ok()
}

fn render_memory(m: &Memory) -> String {
    serde_json::to_string_pretty(m).unwrap_or_else(|_| "{}".to_string())
}

/// Write via `<file>.tmp` + rename, so a crash mid-write cannot leave a
/// half-written memory behind.
fn save_memory(path: &Path, m: &Memory) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, render_memory(m))?;
    // On Windows `rename` already replaces an existing destination file, so
    // there is no separate remove step to race a concurrent reader against.
    std::fs::rename(&tmp, path)
}

// ---- the shared snapshot -----------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub ip: Ipv4Addr,
    pub mac: String,
    pub name: Option<String>,
    pub vendor: Option<&'static str>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub online: bool,
    /// First seen in this session (never in `wasteland.json` before).
    pub is_new: bool,
}

impl Device {
    fn label(&self) -> String {
        match (&self.name, self.vendor) {
            (Some(n), _) if !n.is_empty() => n.clone(),
            (_, Some(v)) => v.to_string(),
            _ => "?".to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WastelandSnapshot {
    pub devices: Vec<Device>,
    pub gateway: Option<String>,
}

/// One finished scan, on its way to the UI thread.
#[derive(Clone, Debug, Default)]
struct Scan {
    devices: Vec<Device>,
    gateway: Option<String>,
    /// This machine's own address on the subnet.
    me: Option<String>,
    subnet: String,
    at: Option<DateTime<Local>>,
    /// `"192.168.100.42 (Espressif)"` for every MAC seen for the first time.
    fresh: Vec<String>,
    /// Set only on the scan that started from an empty memory: the device
    /// count for the one-line "first scan" footer, instead of flagging NEW.
    baseline: Option<usize>,
}

/// Online first, then numerically by address (`.9` before `.10`).
fn sort_devices(devices: &mut [Device]) {
    devices.sort_by_key(|d| (std::cmp::Reverse(d.online), d.ip));
}

// ---- background worker -------------------------------------------------------

enum WlEvent {
    Scan(Box<Scan>),
    Ping { ip: Ipv4Addr, rtt: Option<u32> },
    Error(String),
    Note(String),
}

enum WlCmd {
    Refresh,
    Sweep(bool),
    Rename { mac: String, name: String },
    Ping(Ipv4Addr),
}

struct Worker {
    cfg: WastelandCfg,
    path: PathBuf,
    mem: Memory,
    sweep: bool,
    cycle: u64,
    /// The memory file was missing or unreadable at startup: the first scan
    /// is a baseline, not a flood of NEW devices.
    first_run: bool,
    /// MACs first seen in this session.
    session_new: HashSet<String>,
    /// IP → (name, when looked up); `None` = asked and got nothing.
    dns: HashMap<Ipv4Addr, (Option<String>, Instant)>,
}

impl Worker {
    fn new(cfg: WastelandCfg, path: PathBuf, tx: &Sender<WlEvent>) -> Self {
        let sweep = cfg.sweep;
        let (mem, first_run) = match std::fs::read_to_string(&path) {
            Ok(s) => match parse_memory(&s) {
                Some(m) => (m, false),
                None => {
                    let _ = tx.send(WlEvent::Note(format!(
                        "wasteland: {} unreadable — starting a fresh memory",
                        path.file_name().and_then(|s| s.to_str()).unwrap_or("wasteland.json")
                    )));
                    (Memory::new(), true)
                }
            },
            Err(_) => (Memory::new(), true),
        };
        Self {
            cfg,
            path,
            mem,
            sweep,
            cycle: 0,
            first_run,
            session_new: HashSet::new(),
            dns: HashMap::new(),
        }
    }

    /// The subnet to work on: the config wins, otherwise the gateway's /24.
    fn subnet(&self, gateway: Option<&str>) -> Option<(Ipv4Addr, u8)> {
        if let Some(s) = parse_cidr(&self.cfg.subnet) {
            return Some(s);
        }
        let gw: Ipv4Addr = gateway?.parse().ok()?;
        Some((network(gw, 24), 24))
    }

    /// A cached name, only if it is still within [`DNS_TTL`] — never blocks.
    fn cached(&self, ip: Ipv4Addr) -> Option<String> {
        let (name, at) = self.dns.get(&ip)?;
        (at.elapsed() < DNS_TTL).then(|| name.clone())?
    }

    /// Builds the device list from the live neighbour table plus memory,
    /// using only already-known names (memory, OUI, cached DNS) — no lookup
    /// blocks this pass, so it is cheap enough to send to the UI right away.
    /// Returns the devices, the fresh-device labels, and every address that
    /// still needs a reverse-DNS lookup.
    fn build(&mut self, net: Ipv4Addr, bits: u8) -> (Vec<Device>, Vec<String>, Vec<Ipv4Addr>) {
        let now = Local::now().timestamp();
        let mut devices = Vec::new();
        let mut fresh = Vec::new();
        let mut pending = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for nb in ffi::neighbours() {
            if !keep_entry(nb.ip, &nb.mac, net, bits) || !nb.online {
                continue;
            }
            let mac = mac_string(&nb.mac);
            if !seen.insert(mac.clone()) {
                continue; // same MAC on two interfaces: one row is enough
            }
            let vendor = oui(&mac);
            if note_if_new(&self.mem, &mut self.session_new, self.first_run, &mac) {
                fresh.push(format!("{} ({})", nb.ip, vendor.unwrap_or("unknown vendor")));
            }
            let mut entry = self.mem.get(&mac).cloned().unwrap_or(MemEntry {
                name: String::new(),
                first_seen: now,
                last_seen: now,
                ip: nb.ip.to_string(),
            });
            // A name already on file (a lookup from before, or the user's own
            // rename) wins; otherwise a fresh cached answer, if one is still
            // within its TTL; a cold address is queued for the caller.
            if entry.name.is_empty() {
                match self.cached(nb.ip) {
                    Some(name) => entry.name = name,
                    None => pending.push(nb.ip),
                }
            }
            entry.last_seen = now;
            entry.ip = nb.ip.to_string();
            devices.push(Device {
                ip: nb.ip,
                mac: mac.clone(),
                name: (!entry.name.is_empty()).then(|| entry.name.clone()),
                vendor,
                first_seen: entry.first_seen,
                last_seen: now,
                online: true,
                is_new: self.session_new.contains(&mac),
            });
            self.mem.insert(mac, entry);
        }

        // Known but silent: still listed, dimmed, until they go stale.
        self.mem.retain(|_, e| now - e.last_seen <= KEEP_OFFLINE_SECS);
        for (mac, e) in &self.mem {
            if seen.contains(mac) {
                continue;
            }
            let Ok(ip) = e.ip.parse::<Ipv4Addr>() else { continue };
            if !in_subnet(ip, net, bits) {
                continue;
            }
            devices.push(Device {
                ip,
                mac: mac.clone(),
                name: (!e.name.is_empty()).then(|| e.name.clone()),
                vendor: oui(mac),
                first_seen: e.first_seen,
                last_seen: e.last_seen,
                online: false,
                is_new: false,
            });
        }
        sort_devices(&mut devices);
        (devices, fresh, pending)
    }

    /// One scan, in two passes: the neighbour table plus already-known names
    /// go to the UI immediately via `tx`; only then are the cold addresses
    /// resolved (in parallel) and a second, fully-named `Scan` returned.
    fn scan(&mut self, tx: &Sender<WlEvent>) -> Scan {
        let iface = run_ipconfig().and_then(|o| parse_ipconfig(&o));
        let gateway = iface.as_ref().map(|i| i.gateway.clone());
        let Some((net, bits)) = self.subnet(gateway.as_deref()) else {
            return Scan { subnet: "no subnet".to_string(), at: Some(Local::now()), ..Scan::default() };
        };
        if self.sweep && self.cycle % SWEEP_EVERY == 0 {
            sweep(net, bits);
        }
        self.cycle = self.cycle.wrapping_add(1);

        let (mut devices, fresh, mut pending) = self.build(net, bits);
        let me = iface.map(|i| i.ip);
        let subnet = subnet_label(net, bits);
        let _ = tx.send(WlEvent::Scan(Box::new(Scan {
            devices: devices.clone(),
            gateway: gateway.clone(),
            me: me.clone(),
            subnet: subnet.clone(),
            at: Some(Local::now()),
            fresh: Vec::new(), // announced once, on the final scan below
            baseline: None,
        })));

        pending.truncate(MAX_LOOKUPS);
        let resolved = resolve_many(&pending);
        for (ip, name) in &resolved {
            self.dns.insert(*ip, (name.clone(), Instant::now()));
        }
        let names: HashMap<Ipv4Addr, String> =
            resolved.into_iter().filter_map(|(ip, n)| n.map(|n| (ip, n))).collect();
        merge_names(&mut devices, &names);
        for d in &devices {
            if let (Some(name), Some(e)) = (&d.name, self.mem.get_mut(&d.mac)) {
                if e.name.is_empty() {
                    e.name = name.clone();
                }
            }
        }

        let baseline = self.first_run.then(|| devices.len());
        self.first_run = false;
        let _ = save_memory(&self.path, &self.mem);
        Scan { devices, gateway, me, subnet, at: Some(Local::now()), fresh, baseline }
    }
}

/// Fills in the name of any device still missing one, from freshly resolved
/// reverse-DNS answers. Pure — no I/O — so it is easy to test on its own.
fn merge_names(devices: &mut [Device], names: &HashMap<Ipv4Addr, String>) {
    for d in devices.iter_mut() {
        if d.name.is_none() {
            if let Some(n) = names.get(&d.ip) {
                d.name = Some(n.clone());
            }
        }
    }
}

/// Resolves each address on up to [`DNS_PARALLEL`] threads sharing one work
/// queue, instead of one after another — a scan with many cold addresses
/// would otherwise block for up to `MAX_LOOKUPS * DNS_TIMEOUT`.
fn resolve_many(ips: &[Ipv4Addr]) -> Vec<(Ipv4Addr, Option<String>)> {
    if ips.is_empty() {
        return Vec::new();
    }
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..DNS_PARALLEL.min(ips.len()) {
            let next = &next;
            let tx = tx.clone();
            s.spawn(move || loop {
                let Some(&ip) = ips.get(next.fetch_add(1, Ordering::Relaxed)) else { return };
                if tx.send((ip, reverse_dns(ip))).is_err() {
                    return;
                }
            });
        }
    });
    drop(tx);
    rx.try_iter().collect()
}

fn run_ipconfig() -> Option<String> {
    let out = Command::new("ipconfig").output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Reverse DNS with a hard per-host ceiling: `getnameinfo` has no timeout of
/// its own, so it runs on a throwaway thread we simply stop waiting for.
fn reverse_dns(ip: Ipv4Addr) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ffi::host_name(ip));
    });
    rx.recv_timeout(DNS_TIMEOUT).ok().flatten().map(|n| sanitize_name(&n)).filter(|n| !n.is_empty())
}

/// Pings every host of the subnet so silent devices enter the ARP table.
/// [`SWEEP_PARALLEL`] workers share one index; each owns its own ICMP handle.
fn sweep(net: Ipv4Addr, bits: u8) {
    let hosts = host_addrs(net, bits);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..SWEEP_PARALLEL.min(hosts.len()) {
            let next = &next;
            let hosts = &hosts;
            s.spawn(move || {
                let Some(p) = Pinger::new() else { return };
                loop {
                    let Some(&ip) = hosts.get(next.fetch_add(1, Ordering::Relaxed)) else { return };
                    // The reply is irrelevant: the point is the ARP entry it leaves behind.
                    let _ = p.ping(ip, SWEEP_TIMEOUT_MS);
                }
            });
        }
    });
}

fn run(cfg: WastelandCfg, path: PathBuf, tx: Sender<WlEvent>, crx: Receiver<WlCmd>) {
    let interval = Duration::from_secs(cfg.interval.max(MIN_INTERVAL));
    let mut w = Worker::new(cfg, path, &tx);
    let mut next = Instant::now();
    loop {
        if Instant::now() >= next {
            let scan = w.scan(&tx);
            if scan.devices.is_empty() && scan.subnet == "no subnet" {
                let _ = tx.send(WlEvent::Error("no gateway found — set [wasteland] subnet".into()));
            }
            if let Some(n) = scan.baseline {
                let _ =
                    tx.send(WlEvent::Note(format!("wasteland: first scan — {n} devices recorded as known")));
            }
            if tx.send(WlEvent::Scan(Box::new(scan))).is_err() {
                return;
            }
            next = Instant::now() + interval;
        }
        let wait = next.saturating_duration_since(Instant::now());
        match crx.recv_timeout(wait) {
            Ok(WlCmd::Refresh) => next = Instant::now(),
            Ok(WlCmd::Sweep(on)) => w.sweep = on,
            Ok(WlCmd::Rename { mac, name }) => {
                if let Some(e) = w.mem.get_mut(&mac) {
                    e.name = name;
                }
                let _ = save_memory(&w.path, &w.mem);
                next = Instant::now();
            }
            Ok(WlCmd::Ping(ip)) => {
                let rtt = Pinger::new().and_then(|p| p.ping(ip, PING_TIMEOUT_MS));
                if tx.send(WlEvent::Ping { ip, rtt }).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---- the module --------------------------------------------------------------

/// What the list keys mean right now. `Detail` and `Rename` bind to a MAC,
/// not a list index — a background scan can re-sort the list (online devices
/// move to the top, a stale one drops off) out from under an open view, and
/// an index would then act on whatever now sits at that position instead.
#[derive(Clone, Debug, PartialEq)]
enum Mode {
    List,
    /// Detail view of the device with this MAC.
    Detail { mac: String },
    /// `NAME> ` prompt over the device with this MAC.
    Rename { mac: String, input: String },
}

pub struct Wasteland {
    cfg: WastelandCfg,
    devices: Vec<Device>,
    gateway: Option<String>,
    me: Option<String>,
    subnet: String,
    scanned: Option<DateTime<Local>>,
    loading: bool,
    err: Option<String>,
    sel: usize,
    mode: Mode,
    /// Sweep state for this session (`s` toggles it).
    sweep: bool,
    /// Result of the last `p`: address + what to show.
    ping: Option<(Ipv4Addr, String)>,
    /// A new device arrived and the user has not looked at the tab since.
    new_unseen: Cell<bool>,
    body_height: Cell<u16>,
    rx: Option<Receiver<WlEvent>>,
    tx: Option<Sender<WlCmd>>,
}

impl Wasteland {
    pub fn new() -> Self {
        let cfg = WastelandCfg::default();
        Self {
            sweep: cfg.sweep,
            cfg,
            devices: Vec::new(),
            gateway: None,
            me: None,
            subnet: String::new(),
            scanned: None,
            loading: false,
            err: None,
            sel: 0,
            mode: Mode::List,
            ping: None,
            new_unseen: Cell::new(false),
            body_height: Cell::new(10),
            rx: None,
            tx: None,
        }
    }

    fn online(&self) -> usize {
        self.devices.iter().filter(|d| d.online).count()
    }

    fn sleeping(&self) -> usize {
        self.devices.len() - self.online()
    }

    fn selected(&self) -> Option<&Device> {
        self.devices.get(self.sel)
    }

    fn find_mac(&self, mac: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.mac == mac)
    }

    /// The device a key like `n`/`p` should act on right now: the one the
    /// current `Detail`/`Rename` mode is bound to, or otherwise whatever the
    /// list has selected.
    fn acted_on(&self) -> Option<&Device> {
        match &self.mode {
            Mode::Detail { mac } | Mode::Rename { mac, .. } => self.find_mac(mac),
            Mode::List => self.selected(),
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.devices.is_empty() {
            self.sel = 0;
            return;
        }
        self.sel = (self.sel as i32 + delta).clamp(0, self.devices.len() as i32 - 1) as usize;
    }

    fn send(&self, cmd: WlCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// Style and trailing tag of one row.
    fn row_style(&self, d: &Device, t: Theme) -> (ratatui::style::Style, &'static str) {
        let ip = d.ip.to_string();
        if !d.online {
            (t.frame, "")
        } else if d.is_new {
            (t.warn, "NEW")
        } else if self.me.as_deref() == Some(ip.as_str()) {
            (t.value, "you")
        } else if self.gateway.as_deref() == Some(ip.as_str()) {
            (t.title, "⌂ gateway")
        } else {
            (t.text, "")
        }
    }

    fn title_line(&self, width: u16, t: Theme) -> Line<'static> {
        let subnet = if self.subnet.is_empty() { "…".to_string() } else { self.subnet.clone() };
        let mut head =
            format!("WASTELAND · {subnet} · {} online · {} sleeping", self.online(), self.sleeping());
        if let Some(at) = self.scanned {
            head.push_str(&format!(" · scan {}", at.format("%H:%M")));
        }
        if !self.sweep {
            head.push_str(" · sweep off");
        }
        let mut spans = vec![Span::styled(truncate(&head, width as usize), t.title)];
        if let Some(e) = &self.err {
            let rest = (width as usize).saturating_sub(head.chars().count());
            if rest > 3 {
                spans.push(Span::styled(truncate(&format!(" · {e}"), rest), t.warn));
            }
        }
        Line::from(spans)
    }

    fn row_line(&self, d: &Device, width: u16, t: Theme) -> Line<'static> {
        let (style, tag) = self.row_style(d, t);
        let now = Local::now().timestamp();
        let seen = seen_ago(now - d.last_seen);
        if width < NARROW {
            let name_w = (width as usize).saturating_sub(IP_W + 2 + SEEN_W).max(3);
            return Line::from(Span::styled(
                format!("{} {} {}", pad(&d.ip.to_string(), IP_W), pad(&d.label(), name_w), pad(&seen, SEEN_W)),
                style,
            ));
        }
        let name = match &d.name {
            Some(n) => n.clone(),
            None => "?".to_string(),
        };
        let mut s = format!(
            "{} {} {} {} {}",
            pad(&d.ip.to_string(), IP_W),
            pad(&name, NAME_W),
            pad(d.vendor.unwrap_or("?"), VEND_W),
            pad(&d.mac, MAC_W),
            pad(&seen, SEEN_W)
        );
        if !tag.is_empty() {
            s.push(' ');
            s.push_str(tag);
        }
        Line::from(Span::styled(truncate(&s, width as usize), style))
    }

    fn column_header(&self, width: u16, t: Theme) -> Line<'static> {
        let s = if width < NARROW {
            let name_w = (width as usize).saturating_sub(IP_W + 2 + SEEN_W).max(3);
            format!("{} {} {}", pad("IP", IP_W), pad("NAME", name_w), pad("SEEN", SEEN_W))
        } else {
            format!(
                "{} {} {} {} {}",
                pad("IP", IP_W),
                pad("NAME", NAME_W),
                pad("VENDOR", VEND_W),
                pad("MAC", MAC_W),
                pad("SEEN", SEEN_W)
            )
        };
        Line::from(Span::styled(truncate(&s, width as usize), t.title.add_modifier(Modifier::UNDERLINED)))
    }

    fn draw_list(&self, f: &mut Frame, area: Rect, t: Theme) {
        let prompt_h = u16::from(matches!(self.mode, Mode::Rename { .. }));
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(prompt_h),
        ])
        .split(area);
        f.render_widget(Paragraph::new(self.title_line(rows[0].width, t)), rows[0]);
        f.render_widget(Paragraph::new(self.column_header(rows[1].width, t)), rows[1]);
        if self.devices.is_empty() {
            let (msg, style) = if self.loading {
                ("LOADING\u{2026}".to_string(), t.title)
            } else {
                (
                    match &self.err {
                        Some(e) => format!("n/a: {e}"),
                        None => "no devices found".to_string(),
                    },
                    t.frame,
                )
            };
            if rows[2].height > 0 {
                let line =
                    Rect { x: rows[2].x, y: rows[2].y + rows[2].height / 2, width: rows[2].width, height: 1 };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(truncate(&msg, rows[2].width as usize), style)))
                        .alignment(Alignment::Center),
                    line,
                );
            }
        } else {
            let items: Vec<ListItem> =
                self.devices.iter().map(|d| ListItem::new(self.row_line(d, rows[2].width, t))).collect();
            let mut state = ListState::default();
            state.select(Some(self.sel.min(self.devices.len() - 1)));
            f.render_stateful_widget(List::new(items).highlight_style(t.tab_active), rows[2], &mut state);
        }
        if let (Mode::Rename { input, .. }, true) = (&self.mode, rows[3].height > 0) {
            let line = Line::from(vec![Span::styled("NAME> ", t.title), Span::raw(input.clone())]);
            f.render_widget(Paragraph::new(line), rows[3]);
            let x = (rows[3].x + 6 + input.chars().count() as u16)
                .min(rows[3].x + rows[3].width.saturating_sub(1));
            f.set_cursor_position((x, rows[3].y));
        }
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect, t: Theme, d: &Device) {
        let now = Local::now().timestamp();
        let ping = match &self.ping {
            Some((ip, s)) if *ip == d.ip => s.clone(),
            _ => "press p".to_string(),
        };
        let (_, tag) = self.row_style(d, t);
        let mut lines = vec![
            Line::from(Span::styled(truncate(&d.ip.to_string(), area.width as usize), t.title)),
            Line::from(Span::styled(format!("name    {}", d.label()), t.value)),
            Line::from(format!("vendor  {}", d.vendor.unwrap_or("?"))),
            Line::from(format!("mac     {}", d.mac)),
            Line::from(format!("state   {}", if d.online { "online" } else { "sleeping" })),
            Line::from(format!("first   {}", stamp(d.first_seen))),
            Line::from(format!("last    {} ({} ago)", stamp(d.last_seen), seen_ago(now - d.last_seen))),
            Line::from(format!("ping    {ping}")),
        ];
        if !tag.is_empty() {
            lines.push(Line::from(Span::styled(format!("        {tag}"), t.warn)));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    /// The rename prompt's whole state machine, key by key.
    /// `Some(true)` = commit, `Some(false)` = cancel, `None` = keep typing.
    fn rename_key(input: &mut String, code: KeyCode) -> Option<bool> {
        match code {
            KeyCode::Esc => Some(false),
            KeyCode::Enter => Some(true),
            KeyCode::Backspace => {
                input.pop();
                None
            }
            KeyCode::Char(c) => {
                if input.chars().count() < MAX_NAME {
                    input.push(c);
                }
                None
            }
            _ => None,
        }
    }
}

impl Module for Wasteland {
    fn id(&self) -> &'static str {
        "wasteland"
    }
    fn title(&self) -> &'static str {
        "WASTELAND"
    }
    fn describe(&self) -> &'static str {
        "Devices on your local network: who is home, who is new"
    }
    fn help(&self) -> &'static str {
        match self.mode {
            Mode::List => "↑/↓ select   enter details   n rename   r rescan   s sweep   1-9 tabs   q quit",
            Mode::Detail { .. } => "p ping   n rename   esc back   q quit",
            Mode::Rename { .. } => "type a name · enter save · esc cancel",
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        self.loading = true;
        let (cfg, notice) = ctx.config.section::<WastelandCfg>(self.id());
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        self.cfg = cfg;
        self.cfg.interval = self.cfg.interval.max(MIN_INTERVAL);
        self.sweep = self.cfg.sweep;
        let exe_dir =
            std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        let path = exe_dir.join("wasteland.json");
        let (tx_ev, rx_ev) = mpsc::channel();
        let (tx_cmd, rx_cmd) = mpsc::channel();
        self.rx = Some(rx_ev);
        self.tx = Some(tx_cmd);
        let cfg = self.cfg.clone();
        std::thread::spawn(move || run(cfg, path, tx_ev, rx_cmd));
    }

    fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = 0;
        let Some(rx) = self.rx.take() else { return 0 };
        while let Ok(ev) = rx.try_recv() {
            n += 1;
            match ev {
                WlEvent::Scan(s) => {
                    self.loading = false;
                    if !s.fresh.is_empty() {
                        self.new_unseen.set(true);
                        for label in &s.fresh {
                            let _ = ctx.notify.send(Notice::Footer(format!("wasteland: new device {label}")));
                        }
                    }
                    if s.subnet != "no subnet" {
                        self.err = None;
                    }
                    self.devices = s.devices;
                    self.gateway = s.gateway;
                    self.me = s.me;
                    self.subnet = s.subnet;
                    self.scanned = s.at;
                    self.sel = self.sel.min(self.devices.len().saturating_sub(1));
                    // A device the open Detail/Rename view is bound to can vanish
                    // (aged out, or memory reset) between scans — drop back to
                    // the list rather than act on a MAC that is no longer here.
                    if let Mode::Detail { mac } | Mode::Rename { mac, .. } = &self.mode {
                        if self.find_mac(mac).is_none() {
                            self.mode = Mode::List;
                            let _ = ctx
                                .notify
                                .send(Notice::Footer("wasteland: device no longer listed".into()));
                        }
                    }
                }
                WlEvent::Ping { ip, rtt } => {
                    let text = match rtt {
                        Some(ms) => format!("{ms} ms"),
                        None => "timeout".to_string(),
                    };
                    let _ = ctx.notify.send(Notice::Footer(format!("wasteland: ping {ip} → {text}")));
                    self.ping = Some((ip, text));
                }
                WlEvent::Error(e) => {
                    self.loading = false;
                    self.err = Some(e);
                }
                WlEvent::Note(msg) => {
                    let _ = ctx.notify.send(Notice::Footer(msg));
                }
            }
        }
        self.rx = Some(rx);
        if n > 0 {
            ctx.board.publish(
                self.id(),
                WastelandSnapshot { devices: self.devices.clone(), gateway: self.gateway.clone() },
            );
        }
        n
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        // Ctrl+C must still quit while the prompt owns every other key.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if let Mode::Rename { mac, input } = &mut self.mode {
            let commit = Self::rename_key(input, key.code);
            let mac = mac.clone();
            let input = input.clone();
            match commit {
                Some(true) => {
                    let name = sanitize_name(&input);
                    // Bound to the MAC, not `self.sel`: a background scan may
                    // have re-sorted the list while the prompt was open.
                    if let Some(d) = self.devices.iter_mut().find(|d| d.mac == mac) {
                        d.name = (!name.is_empty()).then(|| name.clone());
                        self.send(WlCmd::Rename { mac, name });
                    } else {
                        let _ = ctx
                            .notify
                            .send(Notice::Footer("wasteland: device no longer listed — rename cancelled".into()));
                    }
                    self.mode = Mode::List;
                }
                Some(false) => self.mode = Mode::List,
                None => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-(self.body_height.get().max(1) as i32)),
            KeyCode::PageDown => self.move_sel(self.body_height.get().max(1) as i32),
            KeyCode::Enter => {
                if let Some(d) = self.selected() {
                    self.mode = Mode::Detail { mac: d.mac.clone() };
                }
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if matches!(self.mode, Mode::Detail { .. }) {
                    self.mode = Mode::List;
                } else {
                    return false;
                }
            }
            KeyCode::Char('n') => {
                if let Some(d) = self.acted_on() {
                    self.mode = Mode::Rename { mac: d.mac.clone(), input: d.name.clone().unwrap_or_default() };
                }
            }
            KeyCode::Char('p') => match self.acted_on() {
                Some(d) => {
                    let ip = d.ip;
                    self.ping = Some((ip, "\u{2026}".to_string()));
                    self.send(WlCmd::Ping(ip));
                }
                None => return false,
            },
            KeyCode::Char('r') => {
                self.loading = self.devices.is_empty();
                self.send(WlCmd::Refresh);
            }
            KeyCode::Char('s') => {
                self.sweep = !self.sweep;
                self.send(WlCmd::Sweep(self.sweep));
                let state = if self.sweep { "on" } else { "off" };
                let _ = ctx.notify.send(Notice::Footer(format!("wasteland: ping sweep {state}")));
            }
            _ => return false,
        }
        true
    }

    fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        // Looking at the tab is what acknowledges a new device.
        self.new_unseen.set(false);
        self.body_height.set(area.height.saturating_sub(2));
        match &self.mode {
            Mode::Detail { mac } => match self.find_mac(mac) {
                Some(d) => self.draw_detail(f, area, t, d),
                None => self.draw_list(f, area, t),
            },
            _ => self.draw_list(f, area, t),
        }
    }

    fn header(&self, width: u16, t: Theme) -> Vec<Span<'static>> {
        if !self.new_unseen.get() || !self.devices.iter().any(|d| d.is_new) {
            return vec![];
        }
        let s = "☢ new".to_string();
        if s.chars().count() > width as usize {
            return vec![];
        }
        vec![Span::styled(s, t.warn)]
    }

    fn overview(&self, width: u16, height: u16, t: Theme) -> Vec<Line<'static>> {
        let w = (width as usize).saturating_sub(1);
        let mut lines = vec![
            Line::from(Span::styled(" WASTELAND", t.title)),
            Line::from(format!(" {} online · {} sleeping", self.online(), self.sleeping())),
        ];
        // The newest arrival is the interesting line; the gateway otherwise.
        if let Some(d) = self.devices.iter().filter(|d| d.is_new).max_by_key(|d| d.first_seen) {
            lines.push(Line::from(Span::styled(
                truncate(&format!(" {} {} (NEW)", d.ip, d.label()), w),
                t.warn,
            )));
        } else if let Some(gw) = &self.gateway {
            lines.push(Line::from(truncate(&format!(" gateway {gw}"), w)));
        }
        lines.truncate(height.max(1) as usize);
        lines
    }

    fn overview_slot(&self) -> Slot {
        Slot::Left(5)
    }

    fn status(&self) -> String {
        format!(
            "wasteland {} online, {} known, gateway={}",
            self.online(),
            self.devices.len(),
            self.gateway.as_deref().unwrap_or("n/a")
        )
    }
}

// ---- the IPv4 neighbour table, straight from iphlpapi ------------------------

/// One row of the neighbour table, already in safe types.
struct Neighbour {
    ip: Ipv4Addr,
    mac: Vec<u8>,
    /// Reachable / Stale / Permanent — the states that mean "answered lately".
    online: bool,
}

mod ffi {
    use super::{Neighbour, MAX_ROWS};
    use std::ffi::c_void;
    use std::net::Ipv4Addr;
    use std::ptr::null_mut;
    use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIpNetTable2, MIB_IPNET_TABLE2};
    use windows_sys::Win32::Networking::WinSock::{
        getnameinfo, NlnsPermanent, NlnsReachable, NlnsStale, WSAStartup, AF_INET, NI_NAMEREQD, SOCKADDR,
        SOCKADDR_IN, WSADATA,
    };

    /// `ERROR_SUCCESS`.
    const OK: u32 = 0;

    /// A table iphlpapi allocated for us; released in `Drop`.
    struct Table(*mut MIB_IPNET_TABLE2);

    impl Drop for Table {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { FreeMibTable(self.0.cast::<c_void>()) };
            }
        }
    }

    /// The IPv4 neighbour table. An unavailable table is an empty list, not an
    /// error: the tab simply shows nothing that scan.
    pub fn neighbours() -> Vec<Neighbour> {
        let mut table: *mut MIB_IPNET_TABLE2 = null_mut();
        if unsafe { GetIpNetTable2(AF_INET, &mut table) } != OK || table.is_null() {
            return Vec::new();
        }
        let _guard = Table(table);
        let mut out = Vec::new();
        unsafe {
            let n = ((*table).NumEntries as usize).min(MAX_ROWS);
            for row in std::slice::from_raw_parts((*table).Table.as_ptr(), n) {
                if row.Address.si_family != AF_INET {
                    continue;
                }
                let ip = Ipv4Addr::from(row.Address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes());
                let len = (row.PhysicalAddressLength as usize).min(row.PhysicalAddress.len());
                let state = row.State;
                out.push(Neighbour {
                    ip,
                    mac: row.PhysicalAddress[..len].to_vec(),
                    online: state == NlnsReachable || state == NlnsStale || state == NlnsPermanent,
                });
            }
        }
        out
    }

    /// WinSock must be started before `getnameinfo`; once per process is enough
    /// and there is nothing to clean up before exit.
    fn winsock() -> bool {
        use std::sync::OnceLock;
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| {
            let mut data: WSADATA = unsafe { std::mem::zeroed() };
            unsafe { WSAStartup(0x0202, &mut data) == 0 }
        })
    }

    /// Reverse DNS for one address. `NI_NAMEREQD` means "a name or nothing",
    /// so the IP is never echoed back as its own host name.
    pub fn host_name(ip: Ipv4Addr) -> Option<String> {
        if !winsock() {
            return None;
        }
        let mut sa: SOCKADDR_IN = unsafe { std::mem::zeroed() };
        sa.sin_family = AF_INET;
        // Network byte order, the same way `Pinger` hands an address over.
        sa.sin_addr.S_un.S_addr = u32::from_ne_bytes(ip.octets());
        let mut host = [0u8; 256];
        let code = unsafe {
            getnameinfo(
                std::ptr::from_ref(&sa).cast::<SOCKADDR>(),
                std::mem::size_of::<SOCKADDR_IN>() as i32,
                host.as_mut_ptr(),
                host.len() as u32,
                std::ptr::null_mut(),
                0,
                NI_NAMEREQD as i32,
            )
        };
        if code != 0 {
            return None;
        }
        let end = host.iter().position(|&b| b == 0).unwrap_or(host.len());
        let name = String::from_utf8_lossy(&host[..end]).into_owned();
        (!name.is_empty()).then_some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const NET: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 0);
    const MAC: [u8; 6] = [0xb8, 0x27, 0xeb, 0x01, 0x02, 0x03];

    fn dev(ip: [u8; 4], online: bool) -> Device {
        dev_mac(ip, online, &MAC)
    }

    fn dev_mac(ip: [u8; 4], online: bool, mac: &[u8; 6]) -> Device {
        let now = Local::now().timestamp();
        Device {
            ip: Ipv4Addr::from(ip),
            mac: mac_string(mac),
            name: Some("pi-hole".into()),
            vendor: Some("Raspberry Pi"),
            first_seen: now - 86_400,
            last_seen: if online { now } else { now - 7200 },
            online,
            is_new: false,
        }
    }

    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn screen(term: &Terminal<TestBackend>, w: u16, h: u16) -> String {
        (0..h)
            .map(|y| {
                (0..w).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>() + "\n"
            })
            .collect()
    }

    #[test]
    fn subnet_filter_drops_noise_and_foreign_addresses() {
        let keep = |ip: [u8; 4], mac: &[u8]| keep_entry(Ipv4Addr::from(ip), mac, NET, 24);
        assert!(keep([192, 168, 1, 42], &MAC));
        assert!(!keep([224, 0, 0, 251], &MAC), "multicast");
        assert!(!keep([239, 255, 255, 250], &MAC), "multicast");
        assert!(!keep([192, 168, 1, 255], &MAC), "subnet broadcast");
        assert!(!keep([255, 255, 255, 255], &MAC), "broadcast");
        assert!(!keep([192, 168, 1, 9], &[0xff; 6]), "broadcast MAC");
        assert!(!keep([192, 168, 1, 9], &[0; 6]), "zero MAC");
        assert!(!keep([192, 168, 1, 9], &MAC[..4]), "short MAC");
        assert!(!keep([169, 254, 3, 4], &MAC), "link-local");
        assert!(!keep([10, 0, 0, 5], &MAC), "other subnet");
    }

    #[test]
    fn keep_entry_only_drops_the_subnets_own_network_and_broadcast() {
        // On a /16, "x.x.2.0" and "x.x.2.255" are ordinary host addresses —
        // only the /16's own network (10.1.0.0) and broadcast (10.1.255.255)
        // are noise.
        let net16 = Ipv4Addr::new(10, 1, 0, 0);
        let keep16 = |ip: [u8; 4]| keep_entry(Ipv4Addr::from(ip), &MAC, net16, 16);
        assert!(keep16([10, 1, 2, 0]), "not the /16 network address");
        assert!(keep16([10, 1, 2, 255]), "not the /16 broadcast address");
        assert!(!keep16([10, 1, 0, 0]), "the /16 network address itself");
        assert!(!keep16([10, 1, 255, 255]), "the /16 broadcast address itself");
    }

    #[test]
    fn cidr_parsing_and_membership() {
        assert_eq!(parse_cidr("192.168.100.0/24"), Some((Ipv4Addr::new(192, 168, 100, 0), 24)));
        assert_eq!(parse_cidr(" 10.1.2.77/16 "), Some((Ipv4Addr::new(10, 1, 0, 0), 16)), "host bits cleared");
        assert_eq!(parse_cidr(""), None);
        assert_eq!(parse_cidr("192.168.1.0"), None);
        assert_eq!(parse_cidr("192.168.1.0/33"), None);
        assert_eq!(parse_cidr("192.168.1.0/0"), None);
        assert_eq!(network(Ipv4Addr::new(192, 168, 1, 42), 24), NET);
        assert!(in_subnet(Ipv4Addr::new(192, 168, 1, 1), NET, 24));
        assert!(!in_subnet(Ipv4Addr::new(192, 168, 2, 1), NET, 24));
        assert_eq!(subnet_label(NET, 24), "192.168.1.0/24");
        let hosts = host_addrs(NET, 24);
        assert_eq!(hosts.len(), MAX_HOSTS);
        assert_eq!((hosts[0], hosts[253]), (Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 254)));
        assert_eq!(host_addrs(Ipv4Addr::new(10, 0, 0, 0), 16).len(), MAX_HOSTS, "capped at a /24 worth");
    }

    #[test]
    fn oui_lookup_is_case_and_separator_insensitive() {
        assert_eq!(oui("b8-27-eb-11-22-33"), Some("Raspberry Pi"));
        assert_eq!(oui("B8:27:EB:11:22:33"), Some("Raspberry Pi"));
        assert_eq!(oui("b827eb112233"), Some("Raspberry Pi"));
        assert_eq!(oui("24-0a-c4-aa-bb-cc"), Some("Espressif"));
        assert_eq!(oui("de-ad-be-ef-00-01"), None);
        assert_eq!(oui("b8-27"), None, "too short to have an OUI");
        assert_eq!(mac_string(&MAC), "b8-27-eb-01-02-03");
    }

    #[test]
    fn seen_ago_steps_through_the_units() {
        assert_eq!(seen_ago(-5), "now");
        assert_eq!(seen_ago(0), "now");
        assert_eq!(seen_ago(59), "now");
        assert_eq!(seen_ago(120), "2 min");
        assert_eq!(seen_ago(3 * 3600), "3 h");
        assert_eq!(seen_ago(2 * 86_400), "2 d");
    }

    #[test]
    fn names_are_sanitized_and_capped() {
        assert_eq!(sanitize_name("  pi-hole.lan \n"), "pi-hole.lan");
        assert_eq!(sanitize_name("ev\u{1b}[2Jil\u{7}"), "ev[2Jil", "control bytes dropped");
        assert_eq!(sanitize_name(&"x".repeat(80)).chars().count(), MAX_NAME);
        assert_eq!(sanitize_name(""), "");
    }

    #[test]
    fn memory_round_trips_and_survives_a_corrupt_file() {
        let mut m = Memory::new();
        m.insert(
            "b8-27-eb-01-02-03".into(),
            MemEntry { name: "pi".into(), first_seen: 100, last_seen: 200, ip: "192.168.1.9".into() },
        );
        let back = parse_memory(&render_memory(&m)).expect("round trip");
        assert_eq!(back, m);
        assert_eq!(parse_memory(""), Some(Memory::new()), "an empty file is a fresh memory");
        assert_eq!(parse_memory("{not json"), None, "a corrupt file starts fresh, never panics");
        assert_eq!(parse_memory("[1,2,3]"), None);
        // Missing keys keep their defaults instead of failing the whole file.
        let partial = parse_memory(r#"{"aa-bb-cc-dd-ee-ff":{"name":"tv"}}"#).unwrap();
        assert_eq!(partial["aa-bb-cc-dd-ee-ff"].last_seen, 0);
    }

    #[test]
    fn memory_saves_atomically_and_reloads() {
        let dir = std::env::temp_dir().join(format!("pipboy-wasteland-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wasteland.json");
        let mut m = Memory::new();
        m.insert("aa-bb-cc-00-11-22".into(), MemEntry { name: "nas".into(), ..MemEntry::default() });
        save_memory(&path, &m).unwrap();
        assert!(!path.with_extension("json.tmp").exists(), "the temp file is renamed away");
        assert_eq!(parse_memory(&std::fs::read_to_string(&path).unwrap()), Some(m.clone()));
        m.insert("aa-bb-cc-00-11-33".into(), MemEntry::default());
        save_memory(&path, &m).unwrap();
        assert_eq!(parse_memory(&std::fs::read_to_string(&path).unwrap()).unwrap().len(), 2, "overwrites");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_devices_are_the_macs_not_in_memory() {
        let mut mem = Memory::new();
        assert!(is_new_mac(&mem, "b8-27-eb-01-02-03"), "an empty memory makes everything new");
        mem.insert("b8-27-eb-01-02-03".into(), MemEntry::default());
        assert!(!is_new_mac(&mem, "b8-27-eb-01-02-03"), "known MAC is not new any more");
        assert!(is_new_mac(&mem, "24-0a-c4-aa-bb-cc"), "unknown MAC is new");
    }

    #[test]
    fn baseline_scan_flags_nothing_new_later_scans_do() {
        let mut mem = Memory::new();
        let mut session_new = HashSet::new();
        assert!(
            !note_if_new(&mem, &mut session_new, true, "b8-27-eb-01-02-03"),
            "an empty-memory baseline scan flags nothing"
        );
        assert!(session_new.is_empty());
        assert!(
            note_if_new(&mem, &mut session_new, false, "b8-27-eb-01-02-03"),
            "same MAC, a later scan: genuinely new"
        );
        assert!(session_new.contains("b8-27-eb-01-02-03"));
        mem.insert("b8-27-eb-01-02-03".into(), MemEntry::default());
        assert!(
            !note_if_new(&mem, &mut session_new, false, "b8-27-eb-01-02-03"),
            "now on file, no longer new"
        );
    }

    #[test]
    fn merge_names_only_fills_devices_still_missing_a_name() {
        let mut devices = vec![dev([192, 168, 1, 9], true), dev([192, 168, 1, 10], true)];
        devices[1].name = None;
        let mut names = HashMap::new();
        names.insert(Ipv4Addr::new(192, 168, 1, 9), "should-not-overwrite".to_string());
        names.insert(Ipv4Addr::new(192, 168, 1, 10), "nas".to_string());
        names.insert(Ipv4Addr::new(192, 168, 1, 99), "unrelated".to_string());
        merge_names(&mut devices, &names);
        assert_eq!(devices[0].name.as_deref(), Some("pi-hole"), "already-named device is untouched");
        assert_eq!(devices[1].name.as_deref(), Some("nas"), "unnamed device gets the resolved name");
    }

    #[test]
    fn sort_puts_online_first_then_numeric_ip_order() {
        let mut v = vec![
            dev([192, 168, 1, 10], true),
            dev([192, 168, 1, 9], false),
            dev([192, 168, 1, 9], true),
            dev([192, 168, 1, 2], true),
        ];
        sort_devices(&mut v);
        let order: Vec<String> = v.iter().map(|d| format!("{}{}", d.ip, if d.online { "+" } else { "-" })).collect();
        assert_eq!(order, ["192.168.1.2+", "192.168.1.9+", "192.168.1.10+", "192.168.1.9-"]);
    }

    #[test]
    fn rename_prompt_state_machine() {
        let mut input = String::new();
        assert_eq!(Wasteland::rename_key(&mut input, KeyCode::Char('n')), None);
        assert_eq!(Wasteland::rename_key(&mut input, KeyCode::Char('a')), None);
        assert_eq!(Wasteland::rename_key(&mut input, KeyCode::Char('s')), None);
        assert_eq!(input, "nas");
        assert_eq!(Wasteland::rename_key(&mut input, KeyCode::Backspace), None);
        assert_eq!(input, "na");
        assert_eq!(Wasteland::rename_key(&mut input, KeyCode::Up), None, "arrows are ignored, not typed");
        assert_eq!(Wasteland::rename_key(&mut input, KeyCode::Esc), Some(false));
        assert_eq!(Wasteland::rename_key(&mut input, KeyCode::Enter), Some(true));
        let mut long = "x".repeat(MAX_NAME);
        Wasteland::rename_key(&mut long, KeyCode::Char('y'));
        assert_eq!(long.chars().count(), MAX_NAME, "capped");
    }

    #[test]
    fn rename_mode_round_trip_through_on_key() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        let mut m = Wasteland::new();
        m.devices = vec![dev([192, 168, 1, 9], true)];
        assert!(m.on_key(KeyEvent::from(KeyCode::Char('n')), &ctx));
        assert_eq!(m.mode, Mode::Rename { mac: mac_string(&MAC), input: "pi-hole".into() });
        assert!(m.help().contains("esc cancel"));
        for _ in 0..7 {
            m.on_key(KeyEvent::from(KeyCode::Backspace), &ctx);
        }
        for c in "nas".chars() {
            m.on_key(KeyEvent::from(KeyCode::Char(c)), &ctx);
        }
        assert!(m.on_key(KeyEvent::from(KeyCode::Enter), &ctx));
        assert_eq!(m.mode, Mode::List);
        assert_eq!(m.devices[0].name.as_deref(), Some("nas"));
        // Esc cancels without touching the name.
        m.on_key(KeyEvent::from(KeyCode::Char('n')), &ctx);
        m.on_key(KeyEvent::from(KeyCode::Char('x')), &ctx);
        m.on_key(KeyEvent::from(KeyCode::Esc), &ctx);
        assert_eq!(m.mode, Mode::List);
        assert_eq!(m.devices[0].name.as_deref(), Some("nas"));
        // Ctrl+C is never typed into the prompt.
        m.on_key(KeyEvent::from(KeyCode::Char('n')), &ctx);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!m.on_key(ctrl_c, &ctx));
    }

    #[test]
    fn rename_commits_by_mac_even_after_the_list_is_re_sorted() {
        let (ctx, _rx) = crate::shell::test_ctx(toml::Table::new());
        const MAC_B: [u8; 6] = [0x24, 0x0a, 0xc4, 0xaa, 0xbb, 0xcc];
        let mut m = Wasteland::new();
        m.devices = vec![dev_mac([192, 168, 1, 9], true, &MAC), dev_mac([192, 168, 1, 20], true, &MAC_B)];
        m.sel = 0;
        assert!(m.on_key(KeyEvent::from(KeyCode::Char('n')), &ctx));
        assert_eq!(m.mode, Mode::Rename { mac: mac_string(&MAC), input: "pi-hole".into() });

        // A background scan re-sorts the list while the prompt is open: the
        // device the prompt is bound to moves to index 1.
        m.devices.swap(0, 1);
        assert_eq!(m.sel, 0, "sel still points at whatever is now at index 0");

        for _ in 0..7 {
            m.on_key(KeyEvent::from(KeyCode::Backspace), &ctx);
        }
        for c in "nas".chars() {
            m.on_key(KeyEvent::from(KeyCode::Char(c)), &ctx);
        }
        assert!(m.on_key(KeyEvent::from(KeyCode::Enter), &ctx));
        assert_eq!(m.mode, Mode::List);

        let renamed = m.devices.iter().find(|d| d.mac == mac_string(&MAC)).unwrap();
        let other = m.devices.iter().find(|d| d.mac == mac_string(&MAC_B)).unwrap();
        assert_eq!(renamed.name.as_deref(), Some("nas"), "the ORIGINAL device (by MAC) is renamed");
        assert_eq!(other.name.as_deref(), Some("pi-hole"), "the device now sitting at index 0 is untouched");
    }

    #[test]
    fn selection_detail_and_sweep_toggle() {
        let (ctx, rx) = crate::shell::test_ctx(toml::Table::new());
        let mut m = Wasteland::new();
        m.move_sel(1);
        assert_eq!(m.sel, 0, "nothing to select yet");
        assert!(!m.on_key(KeyEvent::from(KeyCode::Char('p')), &ctx), "no device to ping");
        m.devices = vec![dev([192, 168, 1, 9], true), dev([192, 168, 1, 10], false)];
        m.move_sel(10);
        assert_eq!(m.sel, 1);
        m.move_sel(-10);
        assert_eq!(m.sel, 0);
        assert!(m.on_key(KeyEvent::from(KeyCode::Enter), &ctx));
        assert_eq!(m.mode, Mode::Detail { mac: mac_string(&MAC) });
        assert!(m.help().contains("p ping"));
        assert!(m.on_key(KeyEvent::from(KeyCode::Esc), &ctx));
        assert_eq!(m.mode, Mode::List);
        assert!(!m.on_key(KeyEvent::from(KeyCode::Esc), &ctx), "esc in the list is the shell's");
        assert!(m.on_key(KeyEvent::from(KeyCode::Char('s')), &ctx));
        assert!(!m.sweep);
        assert_eq!(rx.try_recv(), Ok(Notice::Footer("wasteland: ping sweep off".into())));
    }

    #[test]
    fn poll_publishes_a_snapshot_and_announces_new_devices() {
        let (ctx, rx) = crate::shell::test_ctx(toml::Table::new());
        let (tx, rrx) = mpsc::channel();
        let mut m = Wasteland::new();
        m.rx = Some(rrx);
        m.loading = true;
        let mut new = dev([192, 168, 1, 42], true);
        new.is_new = true;
        tx.send(WlEvent::Scan(Box::new(Scan {
            devices: vec![dev([192, 168, 1, 9], true), new],
            gateway: Some("192.168.1.1".into()),
            me: Some("192.168.1.9".into()),
            subnet: "192.168.1.0/24".into(),
            at: Some(Local::now()),
            fresh: vec!["192.168.1.42 (Raspberry Pi)".into()],
            baseline: None,
        })))
        .unwrap();
        assert_eq!(m.poll(&ctx), 1);
        assert!(!m.loading);
        assert_eq!(m.devices.len(), 2);
        assert!(m.new_unseen.get());
        assert_eq!(
            rx.try_recv(),
            Ok(Notice::Footer("wasteland: new device 192.168.1.42 (Raspberry Pi)".into()))
        );
        let snap: WastelandSnapshot = ctx.board.get("wasteland").expect("published");
        assert_eq!(snap.devices.len(), 2);
        assert_eq!(snap.gateway.as_deref(), Some("192.168.1.1"));
        assert_eq!(m.status(), "wasteland 2 online, 2 known, gateway=192.168.1.1");

        // The header badge shows until the tab is drawn, then stops.
        let t = Theme::new(ThemeKind::Color);
        assert_eq!(m.header(20, t).len(), 1);
        assert!(m.header(2, t).is_empty(), "dropped when it does not fit");
        let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        assert!(m.header(20, t).is_empty(), "acknowledged by looking at the tab");

        tx.send(WlEvent::Ping { ip: Ipv4Addr::new(192, 168, 1, 9), rtt: Some(7) }).unwrap();
        tx.send(WlEvent::Error("no gateway found".into())).unwrap();
        tx.send(WlEvent::Note("wasteland.json unreadable".into())).unwrap();
        assert_eq!(m.poll(&ctx), 3);
        assert_eq!(m.ping.as_ref().map(|p| p.1.clone()), Some("7 ms".to_string()));
        assert_eq!(m.err.as_deref(), Some("no gateway found"));

        // A "no subnet" scan must not clear the error it caused.
        tx.send(WlEvent::Scan(Box::new(Scan {
            subnet: "no subnet".into(),
            at: Some(Local::now()),
            ..Scan::default()
        })))
        .unwrap();
        m.poll(&ctx);
        assert_eq!(m.err.as_deref(), Some("no gateway found"), "still failing, error stays");

        // A scan on a real subnet that simply finds nothing clears a stale
        // error — success is not the same as "devices were found".
        tx.send(WlEvent::Scan(Box::new(Scan {
            subnet: "192.168.1.0/24".into(),
            at: Some(Local::now()),
            ..Scan::default()
        })))
        .unwrap();
        m.poll(&ctx);
        assert_eq!(m.err, None, "an empty but successful scan clears a stale error");
    }

    #[test]
    fn config_defaults_and_interval_floor() {
        let cfg = WastelandCfg::default();
        assert_eq!((cfg.interval, cfg.sweep, cfg.subnet.as_str()), (60, true, ""));
        let table: toml::Table =
            toml::from_str("[wasteland]\ninterval = 5\nsweep = false\nsubnet = \"192.168.100.0/24\"\n").unwrap();
        let (parsed, notice) = crate::module::ModuleConfig(table).section::<WastelandCfg>("wasteland");
        assert!(notice.is_none());
        assert!(!parsed.sweep);
        assert_eq!(parsed.interval.max(MIN_INTERVAL), MIN_INTERVAL);
        assert_eq!(parse_cidr(&parsed.subnet), Some((Ipv4Addr::new(192, 168, 100, 0), 24)));
    }

    #[test]
    fn overview_and_title_read_like_the_spec() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = Wasteland::new();
        m.subnet = "192.168.100.0/24".into();
        m.gateway = Some("192.168.100.1".into());
        m.devices = vec![dev([192, 168, 100, 9], true), dev([192, 168, 100, 11], false)];
        m.scanned = Local.with_ymd_and_hms(2026, 9, 11, 15, 24, 0).single();
        let title = plain(&m.title_line(100, t));
        assert!(title.starts_with("WASTELAND · 192.168.100.0/24 · 1 online · 1 sleeping · scan 15:24"), "{title}");
        let ov = m.overview(40, 3, t);
        assert_eq!(plain(&ov[0]).trim(), "WASTELAND");
        assert_eq!(plain(&ov[1]).trim(), "1 online · 1 sleeping");
        assert!(plain(&ov[2]).contains("gateway 192.168.100.1"));
        let mut fresh = dev([192, 168, 100, 42], true);
        fresh.is_new = true;
        fresh.name = None;
        m.devices.push(fresh);
        assert!(plain(&m.overview(40, 3, t)[2]).contains("192.168.100.42 Raspberry Pi (NEW)"));
        assert_eq!(m.overview(40, 1, t).len(), 1);
        assert_eq!(m.overview_slot(), Slot::Left(5));
    }

    #[test]
    fn list_shows_loading_then_rows_and_narrows_below_80() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = Wasteland::new();
        m.loading = true;
        let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        assert!(screen(&term, 100, 10).contains("LOADING"));

        m.loading = false;
        m.subnet = "192.168.1.0/24".into();
        m.gateway = Some("192.168.1.1".into());
        m.me = Some("192.168.1.9".into());
        let mut gw = dev([192, 168, 1, 1], true);
        gw.name = None;
        m.devices = vec![gw, dev([192, 168, 1, 9], true), dev([192, 168, 1, 77], false)];
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let s = screen(&term, 100, 10);
        assert!(s.contains("IP") && s.contains("VENDOR") && s.contains("MAC") && s.contains("SEEN"), "{s}");
        assert!(s.contains("⌂ gateway"), "{s}");
        assert!(s.contains("you"), "{s}");
        assert!(s.contains("b8-27-eb-01-02-03"), "{s}");
        assert!(s.contains("2 h"), "the sleeping device's SEEN column: {s}");

        let mut narrow = Terminal::new(TestBackend::new(50, 10)).unwrap();
        narrow.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let s = screen(&narrow, 50, 10);
        assert!(s.contains("NAME") && s.contains("SEEN"), "{s}");
        assert!(!s.contains("VENDOR"), "narrow drops VENDOR and MAC: {s}");
    }

    #[test]
    fn draw_does_not_panic_in_any_state_at_tiny_sizes() {
        let t = Theme::new(ThemeKind::Color);
        let states = || {
            let empty = Wasteland::new();
            let mut loading = Wasteland::new();
            loading.loading = true;
            let mut errored = Wasteland::new();
            errored.err = Some("no gateway found — set [wasteland] subnet".into());
            let mut list = Wasteland::new();
            list.devices = vec![dev([192, 168, 1, 9], true), dev([192, 168, 1, 10], false)];
            list.subnet = "192.168.1.0/24".into();
            list.scanned = Some(Local::now());
            list.sel = 1;
            let mut detail = Wasteland::new();
            detail.devices = vec![dev([192, 168, 1, 9], true)];
            detail.mode = Mode::Detail { mac: mac_string(&MAC) };
            detail.ping = Some((Ipv4Addr::new(192, 168, 1, 9), "7 ms".into()));
            let mut detail_vanished = Wasteland::new();
            detail_vanished.mode = Mode::Detail { mac: mac_string(&MAC) };
            let mut prompt = Wasteland::new();
            prompt.devices = vec![dev([192, 168, 1, 9], true)];
            prompt.mode = Mode::Rename { mac: mac_string(&MAC), input: "Árvíztűrő".into() };
            vec![empty, loading, errored, list, detail, detail_vanished, prompt]
        };
        for (w, h) in [(40u16, 12u16), (1, 1), (120, 40)] {
            for m in states() {
                let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                term.draw(|f| m.draw(f, f.area(), t)).unwrap();
            }
        }
    }
}
