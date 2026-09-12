//! WASTELAND · CONN view: who your machine is talking to.
//!
//! The source is the Windows connection table — `GetExtendedTcpTable` for IPv4
//! and IPv6 (owner PID included) plus `GetExtendedUdpTable` for bound UDP
//! sockets — read on a background thread every `[comms] interval` seconds.
//! Process names come from `sysinfo`, per-connection byte counters from TCP
//! ESTATS (`Set/GetPerTcpConnectionEStats`), which only answers to an elevated
//! process: without elevation the traffic columns stay `—` and the title says
//! so. Remote addresses are turned into names with reverse DNS, the only thing
//! here that leaves the machine — through whatever resolver Windows uses.
//!
//! Everything `unsafe` lives in the [`ffi`] module at the bottom: every return
//! code is checked, the tables are plain byte buffers we own, and nothing here
//! panics on an empty or hostile table.

use crate::module::{ConnSnapshot, Ctx, Notice, RemoteConn, CONNECTIONS, CONNECTIONS_AT};
use crate::net::reverse_dns;
use crate::style::Theme;
use crate::ui::widgets::{bytes, truncate};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde::Deserialize;
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Config section key of this view; it keeps COMMS's own `[comms]` table.
pub const ID: &str = "comms";
/// Trailing hint on the title line: `v` swaps in the tab's other view.
const V_HINT: &str = " · v: LOCAL NET";
/// Shortest accepted `interval`: a table read plus reverse DNS is not free.
const MIN_INTERVAL: u64 = 1;
/// How long a pid → name answer is trusted before sysinfo is asked again.
const NAME_TTL: Duration = Duration::from_secs(60);
/// How long a resolved (or unresolved) reverse-DNS answer is trusted.
const DNS_TTL: Duration = Duration::from_secs(3600);
/// Standing threads resolving names off the scan thread.
const DNS_POOL: usize = 4;
/// Longest the resolver's work queue is allowed to back up; past this an
/// enqueue is just dropped for the cycle (tried again once the address is
/// still cold next time).
const DNS_QUEUE: usize = 64;
/// Plausibility cap on the row count a table reports.
const MAX_ROWS: usize = 8192;
/// Longest accepted host name; a reverse-DNS answer is untrusted text.
const MAX_NAME: usize = 40;
/// `ERROR_ACCESS_DENIED` — what ESTATS answers without elevation.
const ACCESS_DENIED: u32 = 5;

/// Column widths of the wide layout; NAME takes whatever is left over.
const PROC_W: usize = 12;
const REMOTE_W: usize = 15;
const PORT_W: usize = 9;
const STATE_W: usize = 11;
const AGE_W: usize = 4;
const RATE_W: usize = 8;
/// Below this width only PROC, REMOTE and PORT are shown.
const NARROW: u16 = 80;

// ---- config ------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CommsCfg {
    /// `false` leaves the CONN view out of WASTELAND entirely: no thread, no
    /// connection tables, and `v` has nowhere to switch to.
    pub enabled: bool,
    /// Rescan period in seconds, clamped to at least [`MIN_INTERVAL`].
    pub interval: u64,
    /// Show connections to 127.0.0.1 / ::1 as well (`l` toggles it for a session).
    pub show_loopback: bool,
}

impl Default for CommsCfg {
    fn default() -> Self {
        Self { enabled: true, interval: 3, show_loopback: false }
    }
}

// ---- data --------------------------------------------------------------------

/// Only TCP has rows today: the UDP table is read for the bound-socket count
/// and nothing else, so a UDP variant here would be dead weight until a row
/// actually carries one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Proto {
    Tcp,
}

/// What identifies a connection across cycles. The detail view binds to this,
/// not to a list index: a rescan re-sorts the list out from under an open view.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    proto: Proto,
    local: SocketAddr,
    remote: SocketAddr,
}

#[derive(Clone, Debug)]
pub struct Conn {
    proto: Proto,
    local: SocketAddr,
    remote: SocketAddr,
    state: &'static str,
    pid: u32,
    process: String,
    /// Full image path, when sysinfo knows it.
    exe: Option<String>,
    /// Reverse-DNS name of the remote address.
    name: Option<String>,
    first_seen: Instant,
    bytes_in: Option<u64>,
    bytes_out: Option<u64>,
    rate_in: Option<u64>,
    rate_out: Option<u64>,
}

impl Conn {
    fn key(&self) -> Key {
        Key { proto: self.proto, local: self.local, remote: self.remote }
    }
}

/// Whether the byte counters are readable at all on this run.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Estats {
    #[default]
    Unknown,
    Ok,
    Denied,
}

impl Estats {
    fn label(self) -> &'static str {
        match self {
            Estats::Unknown => "unknown",
            Estats::Ok => "ok",
            Estats::Denied => "denied",
        }
    }
}

/// One cycle's worth of the connection table.
#[derive(Debug)]
pub struct Snap {
    conns: Vec<Conn>,
    /// TCP sockets in LISTEN plus bound UDP sockets — collapsed into a count.
    listening: usize,
    estats: Estats,
}

#[derive(Debug)]
pub enum CommsEvent {
    Snap(Box<Snap>),
    /// Footer-worthy note, sent at most once per cause.
    Note(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CommsCmd {
    Refresh,
    Loopback(bool),
}

// ---- pure helpers ------------------------------------------------------------

/// TCP state code (`MIB_TCP_STATE`) → the name netstat would print.
pub fn tcp_state(code: u32) -> &'static str {
    match code {
        1 => "CLOSED",
        2 => "LISTEN",
        3 => "SYN_SENT",
        4 => "SYN_RCVD",
        5 => "ESTABLISHED",
        6 => "FIN_WAIT1",
        7 => "FIN_WAIT2",
        8 => "CLOSE_WAIT",
        9 => "CLOSING",
        10 => "LAST_ACK",
        11 => "TIME_WAIT",
        12 => "DELETE_TCB",
        _ => "?",
    }
}

/// The handful of ports worth naming in a column this narrow.
const SERVICES: [(u16, &str); 38] = [
    (20, "ftp-data"),
    (21, "ftp"),
    (22, "ssh"),
    (23, "telnet"),
    (25, "smtp"),
    (53, "dns"),
    (67, "dhcp"),
    (68, "dhcp"),
    (80, "http"),
    (110, "pop3"),
    (123, "ntp"),
    (143, "imap"),
    (161, "snmp"),
    (389, "ldap"),
    (443, "https"),
    (445, "smb"),
    (465, "smtps"),
    (514, "syslog"),
    (587, "submission"),
    (636, "ldaps"),
    (853, "dot"),
    (993, "imaps"),
    (995, "pop3s"),
    (1194, "openvpn"),
    (1433, "mssql"),
    (1521, "oracle"),
    (1900, "ssdp"),
    (3306, "mysql"),
    (3389, "rdp"),
    (5060, "sip"),
    (5222, "xmpp"),
    (5353, "mdns"),
    (5432, "postgres"),
    (5672, "amqp"),
    (6379, "redis"),
    (8080, "http-alt"),
    (8443, "https-alt"),
    (51820, "wireguard"),
];

pub fn service(port: u16) -> Option<&'static str> {
    SERVICES.iter().find(|(p, _)| *p == port).map(|(_, n)| *n)
}

/// `443 https` when the port has a name, `51234` otherwise.
fn port_label(port: u16) -> String {
    match service(port) {
        Some(n) => format!("{port} {n}"),
        None => port.to_string(),
    }
}

pub fn is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

/// Addresses that never leave the building: RFC1918, link-local and IPv6 ULA.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || v4.is_link_local() || o[0] == 0 || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_unspecified() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// What GLOBE gets to see: the public remote addresses, one entry each. A
/// remote with several connections keeps its busiest one's rate and process
/// and counts as ESTABLISHED when any of them is.
pub fn remote_snapshot(conns: &[Conn], taken: Instant) -> ConnSnapshot {
    let mut by_ip: BTreeMap<IpAddr, RemoteConn> = BTreeMap::new();
    for c in conns {
        let ip = c.remote.ip();
        if is_loopback(ip) || is_private(ip) {
            continue;
        }
        let rate = c.rate_in.unwrap_or(0).saturating_add(c.rate_out.unwrap_or(0));
        let established = c.state == "ESTABLISHED";
        match by_ip.get_mut(&ip) {
            None => {
                by_ip.insert(ip, RemoteConn { ip, established, rate, process: c.process.clone() });
            }
            Some(r) => {
                r.established |= established;
                if rate > r.rate {
                    r.rate = rate;
                    r.process = c.process.clone();
                }
            }
        }
    }
    ConnSnapshot { taken, remotes: by_ip.into_values().collect() }
}

/// `LAN` for anything inside the house, empty for the public internet.
fn scope(ip: IpAddr) -> &'static str {
    if is_loopback(ip) {
        "local"
    } else if is_private(ip) {
        "LAN"
    } else {
        "public"
    }
}

/// Reverse-DNS suffix → who runs the box. Only names people misread:
/// `googleusercontent.com` is a Google Cloud customer, not Google.
const CLOUD_SUFFIXES: &[(&str, &str)] = &[
    ("googleusercontent.com", "Google Cloud"),
    ("1e100.net", "Google"),
    ("amazonaws.com", "AWS"),
    ("cloudfront.net", "AWS CloudFront"),
    ("awsglobalaccelerator.com", "AWS"),
    ("cloudapp.net", "Azure"),
    ("cloudapp.azure.com", "Azure"),
    ("azurefd.net", "Azure"),
    ("trafficmanager.net", "Azure"),
    ("msedge.net", "Microsoft"),
    ("akamaitechnologies.com", "Akamai"),
    ("akamaiedge.net", "Akamai"),
    ("fastly.net", "Fastly"),
    ("cloudflare.com", "Cloudflare"),
    ("hetzner.com", "Hetzner"),
    ("hetzner.de", "Hetzner"),
    ("digitalocean.com", "DigitalOcean"),
    ("linode.com", "Linode"),
    ("linodeusercontent.com", "Linode"),
    ("ovh.net", "OVH"),
    ("vultrusercontent.com", "Vultr"),
    ("scaleway.com", "Scaleway"),
    ("oraclecloud.com", "Oracle Cloud"),
];

/// IPv4 blocks that answer without a reverse name. ponytail: a handful of
/// well-known ranges, not the providers' JSON feeds; extend when one bites.
const CLOUD_V4: &[([u8; 4], u8, &str)] = &[
    ([1, 1, 1, 0], 24, "Cloudflare"),
    ([1, 0, 0, 0], 24, "Cloudflare"),
    ([104, 16, 0, 0], 12, "Cloudflare"),
    ([172, 64, 0, 0], 13, "Cloudflare"),
    ([162, 158, 0, 0], 15, "Cloudflare"),
    ([188, 114, 96, 0], 20, "Cloudflare"),
    ([8, 8, 8, 0], 24, "Google"),
    ([8, 8, 4, 0], 24, "Google"),
    ([34, 64, 0, 0], 10, "Google Cloud"),
    ([35, 184, 0, 0], 13, "Google Cloud"),
    ([9, 9, 9, 0], 24, "Quad9"),
    ([17, 0, 0, 0], 8, "Apple"),
    ([13, 107, 0, 0], 16, "Microsoft"),
    ([20, 190, 128, 0], 18, "Microsoft"),
    ([52, 96, 0, 0], 12, "Microsoft 365"),
    ([151, 101, 0, 0], 16, "Fastly"),
    ([199, 232, 0, 0], 16, "Fastly"),
];

/// Who runs the remote box, from its reverse name first, then a few
/// well-known address blocks. `None` for LAN, loopback and the unknown.
fn cloud(name: Option<&str>, ip: IpAddr) -> Option<&'static str> {
    if is_private(ip) || is_loopback(ip) {
        return None;
    }
    if let Some(n) = name {
        let n = n.to_ascii_lowercase();
        let hit = CLOUD_SUFFIXES
            .iter()
            .find(|(suf, _)| n == *suf || n.ends_with(&format!(".{suf}")));
        if let Some((_, who)) = hit {
            return Some(who);
        }
    }
    let IpAddr::V4(v4) = ip else { return None };
    let bits = u32::from(v4);
    CLOUD_V4
        .iter()
        .find(|(net, len, _)| {
            let mask = if *len == 0 { 0 } else { u32::MAX << (32 - len) };
            bits & mask == u32::from(Ipv4Addr::from(*net)) & mask
        })
        .map(|(_, _, who)| *who)
}

/// Rate from two counter samples. `None` for the first sample of a connection
/// and after a counter reset (a re-used tuple starts its bytes over), so a
/// wraparound never shows up as an absurd spike.
pub fn rate(prev: Option<u64>, now: Option<u64>, dt: Duration) -> Option<u64> {
    let (prev, now) = (prev?, now?);
    let secs = dt.as_secs_f64();
    if now < prev || secs <= 0.0 {
        return None;
    }
    Some(((now - prev) as f64 / secs) as u64)
}

/// `12s` / `5m` / `2h` / `3d`.
fn age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Compact byte rate for a narrow column: `1.2M`, `80k`, `512`, `·` for idle,
/// `—` when there is no counter to read.
fn rate_cell(r: Option<u64>) -> String {
    match r {
        None => "\u{2014}".to_string(),
        Some(0) => "\u{b7}".to_string(),
        Some(v) if v >= 1_048_576 => format!("{:.1}M", v as f64 / 1_048_576.0),
        Some(v) if v >= 1024 => format!("{}k", v / 1024),
        Some(v) => v.to_string(),
    }
}

fn rate_line(r: Option<u64>) -> String {
    match r {
        None => "\u{2014}".to_string(),
        Some(v) => format!("{}/s", bytes(v)),
    }
}

/// Reverse-DNS answers are untrusted text: printable characters only, nothing
/// that could repaint the terminal, capped length.
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

/// Case-insensitive substring over process, remote address and remote name.
pub fn matches_filter(c: &Conn, f: &str) -> bool {
    if f.is_empty() {
        return true;
    }
    let f = f.to_lowercase();
    c.process.to_lowercase().contains(&f)
        || c.remote.ip().to_string().contains(&f)
        || c.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&f))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Process,
    Remote,
    Traffic,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Process => Sort::Remote,
            Sort::Remote => Sort::Traffic,
            Sort::Traffic => Sort::Process,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Sort::Process => "process",
            Sort::Remote => "remote",
            Sort::Traffic => "traffic",
        }
    }
}

/// A rendered line: either a process header or one connection of the group
/// above it (the index points into the slice the rows were built from).
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Proc { name: String, count: usize, rate: Option<u64> },
    Conn(usize),
}

/// Total rate of one connection; `None` only when neither direction is known.
fn conn_rate(c: &Conn) -> Option<u64> {
    match (c.rate_in, c.rate_out) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// Descending, with the unknowns last — the order a "by traffic" sort means.
fn by_rate_desc(a: Option<u64>, b: Option<u64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Connections grouped under their process, both levels ordered by `sort`.
pub fn group_rows(conns: &[&Conn], sort: Sort) -> Vec<Row> {
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, c) in conns.iter().enumerate() {
        groups.entry(c.process.as_str()).or_default().push(i);
    }
    let mut groups: Vec<(&str, Vec<usize>)> = groups.into_iter().collect();
    let total = |idx: &[usize]| -> Option<u64> {
        let sum: Option<u64> =
            idx.iter().filter_map(|&i| conn_rate(conns[i])).reduce(|a, b| a.saturating_add(b));
        sum
    };
    match sort {
        Sort::Process => {
            for (_, idx) in &mut groups {
                idx.sort_by_key(|&i| (conns[i].remote.ip(), conns[i].remote.port()));
            }
        }
        Sort::Remote => {
            for (_, idx) in &mut groups {
                idx.sort_by_key(|&i| (conns[i].remote.ip(), conns[i].remote.port()));
            }
            groups.sort_by_key(|(name, idx)| {
                (idx.first().map(|&i| conns[i].remote.ip()), name.to_string())
            });
        }
        Sort::Traffic => {
            for (_, idx) in &mut groups {
                idx.sort_by(|&a, &b| by_rate_desc(conn_rate(conns[a]), conn_rate(conns[b])));
            }
            groups.sort_by(|(an, ai), (bn, bi)| by_rate_desc(total(ai), total(bi)).then(an.cmp(bn)));
        }
    }
    let mut rows = Vec::new();
    for (name, idx) in groups {
        rows.push(Row::Proc { name: name.to_string(), count: idx.len(), rate: total(&idx) });
        rows.extend(idx.into_iter().map(Row::Conn));
    }
    rows
}

// ---- pid → name cache --------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct ProcInfo {
    name: String,
    exe: Option<String>,
}

/// pid → process name, trusted for [`NAME_TTL`]. A pid is re-used eventually,
/// so an entry that is too old is simply asked again rather than kept.
#[derive(Default)]
pub struct NameCache {
    map: HashMap<u32, (ProcInfo, Instant)>,
}

impl NameCache {
    pub fn get(&self, pid: u32, now: Instant) -> Option<&ProcInfo> {
        self.map.get(&pid).filter(|(_, at)| now.duration_since(*at) < NAME_TTL).map(|(p, _)| p)
    }

    pub fn put(&mut self, pid: u32, info: ProcInfo, now: Instant) {
        self.map.insert(pid, (info, now));
    }

    pub fn sweep(&mut self, now: Instant) {
        self.map.retain(|_, (_, at)| now.duration_since(*at) < NAME_TTL);
    }
}

// ---- reverse DNS, off the scan thread -----------------------------------------

type DnsCache = Arc<Mutex<HashMap<IpAddr, (Option<String>, Instant)>>>;

/// Resolves remote addresses to names on [`DNS_POOL`] standing threads, so
/// the 3 s scan thread never waits on `getnameinfo`: it only reads whatever
/// is already in `cache` and drops a miss on `tx` for a resolver thread to
/// pick up later. A miss already queued (tracked in `inflight`) is not
/// queued twice, and a full queue just drops the enqueue for this cycle —
/// the address is still cold next cycle, so it is offered again.
struct DnsResolver {
    cache: DnsCache,
    inflight: Arc<Mutex<HashSet<IpAddr>>>,
    tx: SyncSender<IpAddr>,
}

impl DnsResolver {
    fn new() -> Self {
        let cache: DnsCache = Arc::new(Mutex::new(HashMap::new()));
        let inflight: Arc<Mutex<HashSet<IpAddr>>> = Arc::new(Mutex::new(HashSet::new()));
        let (tx, rx) = mpsc::sync_channel::<IpAddr>(DNS_QUEUE);
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..DNS_POOL {
            let rx = Arc::clone(&rx);
            let cache = Arc::clone(&cache);
            let inflight = Arc::clone(&inflight);
            thread::spawn(move || loop {
                // The blocking call: may take up to the OS resolver's own
                // timeout, but only this thread waits on it.
                let Ok(ip) = rx.lock().unwrap().recv() else { return };
                let name = reverse_dns(ip).map(|n| sanitize_name(&n)).filter(|n| !n.is_empty());
                cache.lock().unwrap().insert(ip, (name, Instant::now()));
                inflight.lock().unwrap().remove(&ip);
            });
        }
        Self { cache, inflight, tx }
    }

    /// Cached names for `ips` (whatever is fresh), and enqueues the misses.
    /// Never blocks.
    fn lookup(&self, ips: impl Iterator<Item = IpAddr>) -> HashMap<IpAddr, Option<String>> {
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap();
        cache.retain(|_, (_, at)| now.duration_since(*at) < DNS_TTL);
        let mut out = HashMap::new();
        let mut misses = Vec::new();
        for ip in ips {
            match cache.get(&ip) {
                Some((name, _)) => {
                    out.insert(ip, name.clone());
                }
                None => misses.push(ip),
            }
        }
        drop(cache);
        if misses.is_empty() {
            return out;
        }
        let mut inflight = self.inflight.lock().unwrap();
        for ip in misses {
            if inflight.insert(ip) && self.tx.try_send(ip).is_err() {
                inflight.remove(&ip);
            }
        }
        out
    }
}

// ---- the background worker ---------------------------------------------------

struct Prev {
    first_seen: Instant,
    bytes_in: Option<u64>,
    bytes_out: Option<u64>,
    estats_on: bool,
}

struct Worker {
    show_loopback: bool,
    interval: Duration,
    sys: sysinfo::System,
    names: NameCache,
    dns: DnsResolver,
    prev: HashMap<Key, Prev>,
    estats: Estats,
    last_at: Option<Instant>,
}

impl Worker {
    fn new(cfg: &CommsCfg, interval: Duration) -> Self {
        Self {
            show_loopback: cfg.show_loopback,
            interval,
            sys: sysinfo::System::new(),
            names: NameCache::default(),
            dns: DnsResolver::new(),
            prev: HashMap::new(),
            estats: Estats::Unknown,
            last_at: None,
        }
    }

    /// Fills in the process name of every pid in `conns`, refreshing only the
    /// pids that are not in the cache.
    fn fill_names(&mut self, conns: &mut [Conn], now: Instant) {
        let cold: Vec<sysinfo::Pid> = conns
            .iter()
            .map(|c| c.pid)
            .filter(|&pid| pid > 4 && self.names.get(pid, now).is_none())
            .collect::<std::collections::HashSet<u32>>()
            .into_iter()
            .map(|p| sysinfo::Pid::from_u32(p))
            .collect();
        if !cold.is_empty() {
            self.sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&cold), true);
            for pid in &cold {
                let info = match self.sys.process(*pid) {
                    Some(p) => ProcInfo {
                        name: p.name().to_string_lossy().trim_end_matches(".exe").to_string(),
                        exe: p.exe().map(|e| e.display().to_string()),
                    },
                    None => ProcInfo { name: format!("pid {}", pid.as_u32()), exe: None },
                };
                self.names.put(pid.as_u32(), info, now);
            }
        }
        for c in conns.iter_mut() {
            let info = match c.pid {
                0 => Some(ProcInfo { name: "Idle".into(), exe: None }),
                4 => Some(ProcInfo { name: "System".into(), exe: None }),
                pid => self.names.get(pid, now).cloned(),
            };
            match info {
                Some(i) => {
                    c.process = i.name;
                    c.exe = i.exe;
                }
                None => c.process = format!("pid {}", c.pid),
            }
        }
        self.names.sweep(now);
    }

    /// Byte counters for the IPv4 TCP connections, when ESTATS answers at all.
    fn fill_traffic(&mut self, conns: &mut [Conn], raw: &HashMap<Key, ffi::Row4>, dt: Duration) {
        if self.estats == Estats::Denied {
            return;
        }
        for c in conns.iter_mut() {
            let key = c.key();
            let Some(row) = raw.get(&key) else { continue };
            let known = self.prev.get(&key).is_some_and(|p| p.estats_on);
            if !known {
                let code = ffi::estats_enable(row);
                if code == ACCESS_DENIED {
                    self.estats = Estats::Denied;
                    return;
                }
                if code != 0 {
                    continue;
                }
                if let Some(p) = self.prev.get_mut(&key) {
                    p.estats_on = true;
                }
            }
            let Some((b_in, b_out)) = ffi::estats_read(row) else { continue };
            self.estats = Estats::Ok;
            let prev = self.prev.get(&key);
            c.rate_in = rate(prev.and_then(|p| p.bytes_in), Some(b_in), dt);
            c.rate_out = rate(prev.and_then(|p| p.bytes_out), Some(b_out), dt);
            c.bytes_in = Some(b_in);
            c.bytes_out = Some(b_out);
        }
    }

    /// Reverse DNS for the remotes: reads whatever the resolver pool already
    /// has cached and hands it the misses — never waits on a lookup itself.
    fn fill_names_dns(&mut self, conns: &mut [Conn]) {
        let ips = conns.iter().map(|c| c.remote.ip()).filter(|ip| !is_loopback(*ip));
        let names = self.dns.lookup(ips);
        for c in conns.iter_mut() {
            c.name = names.get(&c.remote.ip()).cloned().flatten();
        }
    }

    fn scan(&mut self) -> Snap {
        // `dt` is the gap between two table reads, not between two `scan()`
        // calls: nothing before this point may block (name resolution no
        // longer does), but measuring right at the read keeps it exact even
        // if that ever changes again.
        let (tcp, raw) = ffi::tcp_rows();
        let now = Instant::now();
        let dt = self.last_at.map(|t| now.duration_since(t)).unwrap_or(self.interval);
        self.last_at = Some(now);

        let mut listening = ffi::udp_bound();
        let mut conns = Vec::new();
        for r in tcp {
            if r.state == 2 {
                listening += 1;
                continue;
            }
            if !self.show_loopback && (is_loopback(r.remote.ip()) || is_loopback(r.local.ip())) {
                continue;
            }
            if r.remote.ip().is_unspecified() || r.remote.port() == 0 {
                continue;
            }
            conns.push(Conn {
                proto: Proto::Tcp,
                local: r.local,
                remote: r.remote,
                state: tcp_state(r.state),
                pid: r.pid,
                process: format!("pid {}", r.pid),
                exe: None,
                name: None,
                first_seen: now,
                bytes_in: None,
                bytes_out: None,
                rate_in: None,
                rate_out: None,
            });
        }
        // A connection that was already here keeps its original first_seen.
        for c in &mut conns {
            if let Some(p) = self.prev.get(&c.key()) {
                c.first_seen = p.first_seen;
            }
        }
        for c in &conns {
            self.prev.entry(c.key()).or_insert(Prev {
                first_seen: c.first_seen,
                bytes_in: None,
                bytes_out: None,
                estats_on: false,
            });
        }
        self.fill_names(&mut conns, now);
        self.fill_traffic(&mut conns, &raw, dt);
        self.fill_names_dns(&mut conns);

        // Forget the connections that are gone; remember this cycle's counters.
        let live: std::collections::HashSet<Key> = conns.iter().map(Conn::key).collect();
        self.prev.retain(|k, _| live.contains(k));
        for c in &conns {
            if let Some(p) = self.prev.get_mut(&c.key()) {
                if c.bytes_in.is_some() {
                    p.bytes_in = c.bytes_in;
                    p.bytes_out = c.bytes_out;
                }
            }
        }
        Snap { conns, listening, estats: self.estats }
    }
}

fn run(cfg: CommsCfg, tx: Sender<CommsEvent>, crx: Receiver<CommsCmd>) {
    let interval = Duration::from_secs(cfg.interval.max(MIN_INTERVAL));
    let mut w = Worker::new(&cfg, interval);
    let mut told_denied = false;
    loop {
        let snap = w.scan();
        if snap.estats == Estats::Denied && !told_denied {
            told_denied = true;
            let _ = tx.send(CommsEvent::Note(
                "comms: per-connection traffic needs an elevated pipboy.exe".into(),
            ));
        }
        if tx.send(CommsEvent::Snap(Box::new(snap))).is_err() {
            return;
        }
        match crx.recv_timeout(interval) {
            Ok(CommsCmd::Refresh) => {}
            Ok(CommsCmd::Loopback(on)) => w.show_loopback = on,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---- the module --------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Mode {
    List,
    /// Detail view of the connection with this key.
    Detail(Key),
    /// `FILTER> ` prompt; the list below already shows the match.
    Filter(String),
}

pub struct ConnView {
    /// `[comms] enabled`, read in `start`; `false` keeps this view out of the tab.
    pub enabled: bool,
    cfg: CommsCfg,
    conns: Vec<Conn>,
    listening: usize,
    estats: Estats,
    loading: bool,
    /// The selected connection, tracked by key since `rows()` regroups and
    /// re-sorts every cycle — a bare row index would drift under the
    /// selected row when a group above it changes size.
    sel: Option<Key>,
    /// Row index to fall back to when `sel`'s connection is gone, and the
    /// position new arrow-key movement starts from.
    sel_idx: usize,
    mode: Mode,
    sort: Sort,
    show_loopback: bool,
    filter: String,
    interval: Duration,
    body_height: Cell<u16>,
    rx: Option<Receiver<CommsEvent>>,
    tx: Option<Sender<CommsCmd>>,
}

impl ConnView {
    pub fn new() -> Self {
        let cfg = CommsCfg::default();
        Self {
            enabled: true,
            show_loopback: cfg.show_loopback,
            interval: Duration::from_secs(cfg.interval),
            cfg,
            conns: Vec::new(),
            listening: 0,
            estats: Estats::Unknown,
            loading: false,
            sel: None,
            sel_idx: 0,
            mode: Mode::List,
            sort: Sort::Process,
            filter: String::new(),
            body_height: Cell::new(10),
            rx: None,
            tx: None,
        }
    }

    fn visible(&self) -> Vec<&Conn> {
        self.conns.iter().filter(|c| matches_filter(c, &self.filter)).collect()
    }

    fn rows(&self) -> (Vec<&Conn>, Vec<Row>) {
        let v = self.visible();
        let rows = group_rows(&v, self.sort);
        (v, rows)
    }

    fn processes(&self) -> usize {
        self.conns.iter().map(|c| c.process.as_str()).collect::<std::collections::HashSet<_>>().len()
    }

    fn totals(&self) -> (Option<u64>, Option<u64>) {
        let sum = |f: fn(&Conn) -> Option<u64>| -> Option<u64> {
            self.conns.iter().filter_map(f).reduce(|a, b| a.saturating_add(b))
        };
        (sum(|c| c.rate_in), sum(|c| c.rate_out))
    }

    /// A connection that showed up within the last two cycles.
    fn fresh(&self, c: &Conn) -> bool {
        c.first_seen.elapsed() < self.interval * 2
    }

    fn find(&self, key: Key) -> Option<&Conn> {
        self.conns.iter().find(|c| c.key() == key)
    }

    fn send(&self, cmd: CommsCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// The row index `sel` currently points at: the row of its connection,
    /// re-found in this cycle's (regrouped, re-sorted) `rows`, or the nearest
    /// remembered index when that connection is gone.
    fn resolve_sel_idx(&self, conns: &[&Conn], rows: &[Row]) -> usize {
        if rows.is_empty() {
            return 0;
        }
        if let Some(key) = self.sel {
            if let Some(idx) = rows.iter().position(|r| matches!(r, Row::Conn(i) if conns[*i].key() == key)) {
                return idx;
            }
        }
        self.sel_idx.min(rows.len() - 1)
    }

    fn move_sel(&mut self, delta: i32) {
        let (conns, rows) = self.rows();
        if rows.is_empty() {
            self.sel = None;
            self.sel_idx = 0;
            return;
        }
        let cur = self.resolve_sel_idx(&conns, &rows) as i32;
        let new = (cur + delta).clamp(0, rows.len() as i32 - 1) as usize;
        let key = match rows.get(new) {
            Some(Row::Conn(i)) => conns.get(*i).map(|c| c.key()),
            _ => None,
        };
        self.sel_idx = new;
        self.sel = key;
    }

    fn title_line(&self, width: u16, t: Theme) -> Line<'static> {
        let (down, up) = self.totals();
        let traffic = match self.estats {
            Estats::Denied => "traffic: run as administrator",
            Estats::Ok => "traffic: ok",
            Estats::Unknown => "traffic: \u{2014}",
        };
        let mut head = format!(
            "CONN \u{b7} {} connections \u{b7} {} listening \u{b7} \u{2193} {} \u{2191} {} \u{b7} {traffic}",
            self.conns.len(),
            self.listening,
            rate_line(down),
            rate_line(up),
        );
        if !self.filter.is_empty() {
            head.push_str(&format!(" \u{b7} filter {}", self.filter));
        }
        if self.show_loopback {
            head.push_str(" \u{b7} loopback");
        }
        let mut spans = vec![Span::styled(truncate(&head, width as usize), t.title)];
        // The tab's other view, named only when there is room left for it.
        let used = spans[0].content.chars().count();
        if (width as usize).saturating_sub(used) >= V_HINT.chars().count() {
            spans.push(Span::styled(V_HINT, t.frame));
        }
        Line::from(spans)
    }

    fn name_w(&self, width: u16) -> usize {
        (width as usize).saturating_sub(PROC_W + REMOTE_W + PORT_W + STATE_W + AGE_W + 2 * RATE_W + 8).max(3)
    }

    fn column_header(&self, width: u16, t: Theme) -> Line<'static> {
        let s = if width < NARROW {
            format!("{} {} {}", pad("PROC", PROC_W), pad("REMOTE", REMOTE_W), pad("PORT", PORT_W))
        } else {
            format!(
                "{} {} {} {} {} {} {} {}",
                pad("PROC", PROC_W),
                pad("REMOTE", REMOTE_W),
                pad("NAME", self.name_w(width)),
                pad("PORT", PORT_W),
                pad("STATE", STATE_W),
                pad("AGE", AGE_W),
                pad("\u{2193}", RATE_W),
                pad("\u{2191}", RATE_W),
            )
        };
        Line::from(Span::styled(truncate(&s, width as usize), t.title.add_modifier(Modifier::UNDERLINED)))
    }

    fn proc_line(&self, name: &str, count: usize, r: Option<u64>, width: u16, t: Theme) -> Line<'static> {
        let mut s = format!("{name} \u{b7} {count}");
        if let Some(r) = r {
            s.push_str(&format!(" \u{b7} {}/s", bytes(r)));
        }
        Line::from(Span::styled(truncate(&s, width as usize), t.title))
    }

    fn conn_line(&self, c: &Conn, width: u16, t: Theme) -> Line<'static> {
        let remote = c.remote.ip().to_string();
        let style = if self.fresh(c) {
            t.value
        } else if c.state == "ESTABLISHED" {
            t.text.add_modifier(Modifier::BOLD)
        } else if matches!(c.state, "TIME_WAIT" | "CLOSE_WAIT" | "FIN_WAIT1" | "FIN_WAIT2" | "CLOSING") {
            t.frame
        } else {
            t.text
        };
        if width < NARROW {
            let s = format!(
                "{} {} {}",
                pad("", PROC_W),
                pad(&remote, REMOTE_W),
                pad(&port_label(c.remote.port()), PORT_W)
            );
            return Line::from(Span::styled(truncate(&s, width as usize), style));
        }
        let name = match (&c.name, cloud(None, c.remote.ip())) {
            (Some(n), _) => n.clone(),
            (None, Some(who)) => format!("{} · {who}", scope(c.remote.ip())),
            (None, None) => scope(c.remote.ip()).to_string(),
        };
        let s = format!(
            "{} {} {} {} {} {} {} {}",
            pad("", PROC_W),
            pad(&remote, REMOTE_W),
            pad(&name, self.name_w(width)),
            pad(&port_label(c.remote.port()), PORT_W),
            pad(c.state, STATE_W),
            pad(&age(c.first_seen.elapsed()), AGE_W),
            pad(&rate_cell(c.rate_in), RATE_W),
            pad(&rate_cell(c.rate_out), RATE_W),
        );
        Line::from(Span::styled(truncate(&s, width as usize), style))
    }

    fn draw_list(&self, f: &mut Frame, area: Rect, t: Theme) {
        let prompt_h = u16::from(matches!(self.mode, Mode::Filter(_)));
        let parts = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(prompt_h),
        ])
        .split(area);
        f.render_widget(Paragraph::new(self.title_line(parts[0].width, t)), parts[0]);
        f.render_widget(Paragraph::new(self.column_header(parts[1].width, t)), parts[1]);
        let (conns, rows) = self.rows();
        if rows.is_empty() {
            let (msg, style) = if self.loading {
                ("LOADING\u{2026}".to_string(), t.title)
            } else if !self.filter.is_empty() {
                (format!("nothing matches \"{}\"", self.filter), t.frame)
            } else {
                ("no connections".to_string(), t.frame)
            };
            if parts[2].height > 0 {
                let line = Rect {
                    x: parts[2].x,
                    y: parts[2].y + parts[2].height / 2,
                    width: parts[2].width,
                    height: 1,
                };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(truncate(&msg, parts[2].width as usize), style)))
                        .alignment(Alignment::Center),
                    line,
                );
            }
        } else {
            let items: Vec<ListItem> = rows
                .iter()
                .map(|r| match r {
                    Row::Proc { name, count, rate } => {
                        ListItem::new(self.proc_line(name, *count, *rate, parts[2].width, t))
                    }
                    Row::Conn(i) => ListItem::new(match conns.get(*i) {
                        Some(c) => self.conn_line(c, parts[2].width, t),
                        None => Line::from(""),
                    }),
                })
                .collect();
            let mut state = ListState::default();
            state.select(Some(self.resolve_sel_idx(&conns, &rows).min(rows.len() - 1)));
            f.render_stateful_widget(List::new(items).highlight_style(t.tab_active), parts[2], &mut state);
        }
        if let (Mode::Filter(input), true) = (&self.mode, parts[3].height > 0) {
            let line = Line::from(vec![Span::styled("FILTER> ", t.title), Span::raw(input.clone())]);
            f.render_widget(Paragraph::new(line), parts[3]);
            let x = (parts[3].x + 8 + input.chars().count() as u16)
                .min(parts[3].x + parts[3].width.saturating_sub(1));
            f.set_cursor_position((x, parts[3].y));
        }
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect, t: Theme, c: &Conn) {
        let w = area.width as usize;
        let proto = match c.proto {
            Proto::Tcp => "tcp",
        };
        let mut lines = vec![
            Line::from(Span::styled(truncate(&format!("{} {}", c.process, c.remote), w), t.title)),
            Line::from(Span::styled(format!("remote  {}", c.remote), t.value)),
            Line::from(truncate(&format!("name    {}", c.name.as_deref().unwrap_or("\u{2014}")), w)),
            Line::from(match cloud(c.name.as_deref(), c.remote.ip()) {
                Some(who) => format!("where   {} · {who}", scope(c.remote.ip())),
                None => format!("where   {}", scope(c.remote.ip())),
            }),
            Line::from(format!("port    {}", port_label(c.remote.port()))),
            Line::from(format!("local   {}", c.local)),
            Line::from(format!("proto   {proto}")),
            Line::from(format!("state   {}", c.state)),
            Line::from(format!("pid     {}", c.pid)),
        ];
        if let Some(exe) = &c.exe {
            lines.push(Line::from(truncate(&format!("path    {exe}"), w)));
        }
        lines.push(Line::from(format!("seen    {} ago", age(c.first_seen.elapsed()))));
        lines.push(Line::from(format!(
            "bytes   \u{2193} {} \u{2191} {}",
            c.bytes_in.map(bytes).unwrap_or_else(|| "\u{2014}".into()),
            c.bytes_out.map(bytes).unwrap_or_else(|| "\u{2014}".into()),
        )));
        lines.push(Line::from(format!(
            "rate    \u{2193} {} \u{2191} {}",
            rate_line(c.rate_in),
            rate_line(c.rate_out)
        )));
        if self.estats == Estats::Denied {
            lines.push(Line::from(Span::styled("traffic: run as administrator", t.warn)));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    /// The filter prompt's state machine, key by key.
    /// `Some(true)` = accept, `Some(false)` = cancel, `None` = keep typing.
    fn filter_key(input: &mut String, code: KeyCode) -> Option<bool> {
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

/// The CONN view's half of the [`Module`](crate::module::Module) contract: the
/// same bodies the trait impl had, as plain methods the `Wasteland` wrapper in
/// `super` forwards to.
impl ConnView {
    pub fn help(&self) -> &'static str {
        match self.mode {
            Mode::List => "v local net   ↑/↓ select   enter details   s sort   f filter   l loopback   r refresh",
            Mode::Detail(_) => "v local net   esc back   s sort   f filter   r refresh   q quit",
            Mode::Filter(_) => "type to filter · enter keep · esc clear",
        }
    }

    pub fn start(&mut self, ctx: &Ctx) {
        let (cfg, notice) = ctx.config.section::<CommsCfg>(ID);
        if let Some(n) = notice {
            let _ = ctx.notify.send(Notice::Footer(n));
        }
        // Switched off in config: no thread, no table reads, and the tab keeps
        // `v` to itself.
        self.enabled = cfg.enabled;
        if !cfg.enabled {
            return;
        }
        self.loading = true;
        self.cfg = cfg;
        self.cfg.interval = self.cfg.interval.max(MIN_INTERVAL);
        self.interval = Duration::from_secs(self.cfg.interval);
        self.show_loopback = self.cfg.show_loopback;
        let (tx_ev, rx_ev) = mpsc::channel();
        let (tx_cmd, rx_cmd) = mpsc::channel();
        self.rx = Some(rx_ev);
        self.tx = Some(tx_cmd);
        let cfg = self.cfg.clone();
        std::thread::spawn(move || run(cfg, tx_ev, rx_cmd));
    }

    pub fn poll(&mut self, ctx: &Ctx) -> usize {
        let mut n = 0;
        let Some(rx) = self.rx.take() else { return 0 };
        while let Ok(ev) = rx.try_recv() {
            n += 1;
            match ev {
                CommsEvent::Snap(s) => {
                    self.loading = false;
                    self.conns = s.conns;
                    self.listening = s.listening;
                    self.estats = s.estats;
                    let snap = remote_snapshot(&self.conns, Instant::now());
                    ctx.board.publish(CONNECTIONS_AT, snap.taken);
                    ctx.board.publish(CONNECTIONS, snap);
                    // The open detail view's connection can close between
                    // cycles — fall back to the list rather than show a ghost.
                    if let Mode::Detail(key) = self.mode {
                        if self.find(key).is_none() {
                            self.mode = Mode::List;
                        }
                    }
                    // `sel` re-finds its connection by key next draw/move; keep
                    // the fallback index roughly in place for when it can't.
                    let (conns, rows) = self.rows();
                    self.sel_idx = self.resolve_sel_idx(&conns, &rows);
                }
                CommsEvent::Note(msg) => {
                    let _ = ctx.notify.send(Notice::Footer(msg));
                }
            }
        }
        self.rx = Some(rx);
        n
    }

    pub fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> bool {
        // Ctrl+C must still quit while the prompt owns every other key.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if let Mode::Filter(input) = &mut self.mode {
            match ConnView::filter_key(input, key.code) {
                Some(true) => {
                    self.filter = input.clone();
                    self.mode = Mode::List;
                }
                Some(false) => {
                    self.filter.clear();
                    self.mode = Mode::List;
                }
                None => self.filter = input.clone(),
            }
            self.sel = None;
            self.sel_idx = 0;
            return true;
        }
        match key.code {
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-(self.body_height.get().max(1) as i32)),
            KeyCode::PageDown => self.move_sel(self.body_height.get().max(1) as i32),
            KeyCode::Enter => {
                let (conns, rows) = self.rows();
                let idx = self.resolve_sel_idx(&conns, &rows);
                if let Some(Row::Conn(i)) = rows.get(idx) {
                    if let Some(c) = conns.get(*i) {
                        self.mode = Mode::Detail(c.key());
                    }
                }
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if matches!(self.mode, Mode::Detail(_)) {
                    self.mode = Mode::List;
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.sel = None;
                    self.sel_idx = 0;
                } else {
                    return false;
                }
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.sel = None;
                self.sel_idx = 0;
                let _ = ctx.notify.send(Notice::Footer(format!("comms: sorted by {}", self.sort.label())));
            }
            KeyCode::Char('f') => self.mode = Mode::Filter(self.filter.clone()),
            KeyCode::Char('l') => {
                self.show_loopback = !self.show_loopback;
                self.send(CommsCmd::Loopback(self.show_loopback));
                let state = if self.show_loopback { "shown" } else { "hidden" };
                let _ = ctx.notify.send(Notice::Footer(format!("comms: loopback {state}")));
            }
            KeyCode::Char('r') => {
                self.loading = self.conns.is_empty();
                self.send(CommsCmd::Refresh);
            }
            _ => return false,
        }
        true
    }

    pub fn draw(&self, f: &mut Frame, area: Rect, t: Theme) {
        self.body_height.set(area.height.saturating_sub(2));
        match self.mode {
            Mode::Detail(key) => match self.find(key) {
                Some(c) => self.draw_detail(f, area, t, c),
                None => self.draw_list(f, area, t),
            },
            _ => self.draw_list(f, area, t),
        }
    }

    /// The one line CONN gets in WASTELAND's OVERVIEW block, under LOCAL NET's.
    pub fn summary_line(&self, width: u16) -> Line<'static> {
        let w = (width as usize).saturating_sub(1);
        let mut s = format!(" {} connections \u{b7} {} processes", self.conns.len(), self.processes());
        let top = self
            .conns
            .iter()
            .max_by(|a, b| by_rate_desc(conn_rate(b), conn_rate(a)))
            .filter(|c| conn_rate(c).is_some())
            .or_else(|| self.conns.iter().find(|c| c.state == "ESTABLISHED"));
        if let Some(c) = top {
            let name = c.name.clone().unwrap_or_else(|| c.remote.ip().to_string());
            s.push_str(&format!(" \u{b7} top {} \u{2192} {}", c.process, name));
        }
        Line::from(truncate(&s, w))
    }

    pub fn status(&self) -> String {
        format!(
            "comms {} conns, {} procs, estats={}",
            self.conns.len(),
            self.processes(),
            self.estats.label()
        )
    }
}

// ---- the connection tables, straight from iphlpapi ---------------------------

mod ffi {
    use super::{Key, Proto, MAX_ROWS};
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, GetPerTcpConnectionEStats, SetPerTcpConnectionEStats,
        TcpConnectionEstatsData, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_LH,
        MIB_TCPROW_LH_0, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, MIB_UDPROW_OWNER_PID,
        MIB_UDPTABLE_OWNER_PID, TCP_ESTATS_DATA_ROD_v0, TCP_ESTATS_DATA_RW_v0, TCP_TABLE_OWNER_PID_ALL,
        UDP_TABLE_OWNER_PID,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    const OK: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    /// One row of the connection table, already in safe types.
    pub struct TcpRow {
        pub local: SocketAddr,
        pub remote: SocketAddr,
        pub state: u32,
        pub pid: u32,
    }

    /// The five IPv4 fields ESTATS needs to find a connection again. Kept as
    /// plain integers so nothing but this module ever sees a raw MIB row.
    #[derive(Clone, Copy)]
    pub struct Row4 {
        state: u32,
        laddr: u32,
        lport: u32,
        raddr: u32,
        rport: u32,
    }

    /// The port fields hold a network-byte-order port in the low 16 bits.
    fn port(v: u32) -> u16 {
        u16::from_be(v as u16)
    }

    /// Calls a `GetExtended*Table` function into a buffer we own, growing it
    /// while Windows says it is too small. `None` = the table is unavailable,
    /// which is an empty list, not an error.
    fn table(af: u16, udp: bool) -> Option<Vec<u64>> {
        let mut size: u32 = 0;
        for _ in 0..4 {
            // u64 elements so the buffer is aligned for the MIB structs.
            let mut buf: Vec<u64> = vec![0; (size as usize / 8) + 2];
            let ptr = buf.as_mut_ptr().cast::<std::ffi::c_void>();
            let mut cap = (buf.len() * 8) as u32;
            let code = if udp {
                unsafe { GetExtendedUdpTable(ptr, &mut cap, 0, af as u32, UDP_TABLE_OWNER_PID, 0) }
            } else {
                unsafe { GetExtendedTcpTable(ptr, &mut cap, 0, af as u32, TCP_TABLE_OWNER_PID_ALL, 0) }
            };
            match code {
                OK => return Some(buf),
                ERROR_INSUFFICIENT_BUFFER => size = cap,
                _ => return None,
            }
        }
        None
    }

    /// `dwNumEntries` is read from the same buffer Windows just wrote into —
    /// trust it for row bounds too, but never past what the buffer we
    /// actually allocated could physically hold.
    fn row_cap<T>(reported: u32, buf: &[u64]) -> usize {
        let by_buf = (buf.len() * 8).saturating_sub(4) / std::mem::size_of::<T>();
        (reported as usize).min(MAX_ROWS).min(by_buf)
    }

    /// Every TCP connection of both families, plus the IPv4 rows ESTATS can
    /// be asked about.
    pub fn tcp_rows() -> (Vec<TcpRow>, HashMap<Key, Row4>) {
        let mut out = Vec::new();
        let mut raw = HashMap::new();
        if let Some(buf) = table(AF_INET, false) {
            unsafe {
                let t = buf.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
                let n = row_cap::<MIB_TCPROW_OWNER_PID>((*t).dwNumEntries, &buf);
                for r in std::slice::from_raw_parts((*t).table.as_ptr(), n) {
                    let r: &MIB_TCPROW_OWNER_PID = r;
                    let local = SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::from(r.dwLocalAddr.to_ne_bytes())),
                        port(r.dwLocalPort),
                    );
                    let remote = SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::from(r.dwRemoteAddr.to_ne_bytes())),
                        port(r.dwRemotePort),
                    );
                    out.push(TcpRow { local, remote, state: r.dwState, pid: r.dwOwningPid });
                    raw.insert(
                        Key { proto: Proto::Tcp, local, remote },
                        Row4 {
                            state: r.dwState,
                            laddr: r.dwLocalAddr,
                            lport: r.dwLocalPort,
                            raddr: r.dwRemoteAddr,
                            rport: r.dwRemotePort,
                        },
                    );
                }
            }
        }
        if let Some(buf) = table(AF_INET6, false) {
            unsafe {
                let t = buf.as_ptr().cast::<MIB_TCP6TABLE_OWNER_PID>();
                let n = row_cap::<MIB_TCP6ROW_OWNER_PID>((*t).dwNumEntries, &buf);
                for r in std::slice::from_raw_parts((*t).table.as_ptr(), n) {
                    let r: &MIB_TCP6ROW_OWNER_PID = r;
                    let local = SocketAddr::new(
                        IpAddr::V6(Ipv6Addr::from(r.ucLocalAddr)),
                        port(r.dwLocalPort),
                    );
                    let remote = SocketAddr::new(
                        IpAddr::V6(Ipv6Addr::from(r.ucRemoteAddr)),
                        port(r.dwRemotePort),
                    );
                    out.push(TcpRow { local, remote, state: r.dwState, pid: r.dwOwningPid });
                }
            }
        }
        (out, raw)
    }

    /// How many UDP sockets are bound right now. The table has no remote
    /// address, so these are only ever a count.
    pub fn udp_bound() -> usize {
        let mut n = 0;
        for af in [AF_INET, AF_INET6] {
            if let Some(buf) = table(af, true) {
                unsafe {
                    let t = buf.as_ptr().cast::<MIB_UDPTABLE_OWNER_PID>();
                    n += row_cap::<MIB_UDPROW_OWNER_PID>((*t).dwNumEntries, &buf);
                }
            }
        }
        n
    }

    fn mib(r: &Row4) -> MIB_TCPROW_LH {
        MIB_TCPROW_LH {
            Anonymous: MIB_TCPROW_LH_0 { dwState: r.state },
            dwLocalAddr: r.laddr,
            dwLocalPort: r.lport,
            dwRemoteAddr: r.raddr,
            dwRemotePort: r.rport,
        }
    }

    /// Turns the byte counters on for one connection. Returns the Win32 error
    /// code; `ERROR_ACCESS_DENIED` means the process is not elevated.
    pub fn estats_enable(r: &Row4) -> u32 {
        let row = mib(r);
        let rw = TCP_ESTATS_DATA_RW_v0 { EnableCollection: true };
        unsafe {
            SetPerTcpConnectionEStats(
                &row,
                TcpConnectionEstatsData,
                std::ptr::from_ref(&rw).cast::<u8>(),
                0,
                std::mem::size_of::<TCP_ESTATS_DATA_RW_v0>() as u32,
                0,
            )
        }
    }

    /// `(DataBytesIn, DataBytesOut)` for one connection, or `None` when the
    /// counters are not readable (not elevated, or the connection is gone).
    pub fn estats_read(r: &Row4) -> Option<(u64, u64)> {
        let row = mib(r);
        let mut rod: TCP_ESTATS_DATA_ROD_v0 = unsafe { std::mem::zeroed() };
        let code = unsafe {
            GetPerTcpConnectionEStats(
                &row,
                TcpConnectionEstatsData,
                std::ptr::null_mut(),
                0,
                0,
                std::ptr::null_mut(),
                0,
                0,
                std::ptr::from_mut(&mut rod).cast::<u8>(),
                0,
                std::mem::size_of::<TCP_ESTATS_DATA_ROD_v0>() as u32,
            )
        };
        (code == OK).then_some((rod.DataBytesIn, rod.DataBytesOut))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn conn(proc: &str, remote: &str, port: u16, state: &'static str, rin: Option<u64>) -> Conn {
        Conn {
            proto: Proto::Tcp,
            local: "192.168.1.9:50000".parse().unwrap(),
            remote: SocketAddr::new(remote.parse().unwrap(), port),
            state,
            pid: 1234,
            process: proc.to_string(),
            exe: Some(format!("C:\\Program Files\\{proc}.exe")),
            name: Some(format!("{proc}.example.net")),
            first_seen: Instant::now() - Duration::from_secs(300),
            bytes_in: rin.map(|_| 4096),
            bytes_out: rin.map(|_| 1024),
            rate_in: rin,
            rate_out: rin.map(|v| v / 2),
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

    fn loaded() -> ConnView {
        let mut m = ConnView::new();
        m.conns = vec![
            conn("chrome", "142.250.185.78", 443, "ESTABLISHED", Some(120_000)),
            conn("chrome", "142.250.185.99", 443, "TIME_WAIT", None),
            conn("ssh", "10.0.0.4", 22, "ESTABLISHED", Some(900)),
            conn("backup", "203.0.113.7", 993, "CLOSE_WAIT", Some(10)),
        ];
        m.listening = 12;
        m.estats = Estats::Ok;
        m
    }

    #[test]
    fn tcp_state_names_cover_the_table() {
        assert_eq!(tcp_state(2), "LISTEN");
        assert_eq!(tcp_state(5), "ESTABLISHED");
        assert_eq!(tcp_state(8), "CLOSE_WAIT");
        assert_eq!(tcp_state(11), "TIME_WAIT");
        assert_eq!(tcp_state(0), "?", "0 is not a state Windows reports");
        assert_eq!(tcp_state(99), "?");
    }

    #[test]
    fn well_known_ports_get_a_name() {
        assert_eq!(service(443), Some("https"));
        assert_eq!(service(22), Some("ssh"));
        assert_eq!(service(993), Some("imaps"));
        assert_eq!(service(5353), Some("mdns"));
        assert_eq!(service(853), Some("dot"));
        assert_eq!(service(51234), None);
        assert_eq!(port_label(443), "443 https");
        assert_eq!(port_label(51234), "51234");
        // Sorted-by-port table, so a duplicate or a typo shows up here.
        assert!(SERVICES.windows(2).all(|w| w[0].0 <= w[1].0), "keep SERVICES sorted by port");
    }

    #[test]
    fn snapshot_keeps_public_remotes_once_each() {
        let mut m = loaded();
        m.conns.push(conn("chrome", "142.250.185.78", 8443, "CLOSE_WAIT", Some(500_000)));
        m.conns.push(conn("x", "127.0.0.1", 80, "ESTABLISHED", Some(1)));
        m.conns.push(conn("x", "fe80::1", 80, "ESTABLISHED", Some(1)));
        let snap = remote_snapshot(&m.conns, Instant::now());
        let ips: Vec<String> = snap.remotes.iter().map(|r| r.ip.to_string()).collect();
        assert_eq!(ips, ["142.250.185.78", "142.250.185.99", "203.0.113.7"], "no LAN, loopback or link-local; one per ip");
        let g = &snap.remotes[0];
        assert!(g.established, "any ESTABLISHED connection counts");
        assert_eq!(g.rate, 750_000, "the busiest connection's in+out");
        assert_eq!(g.process, "chrome");
        assert!(!snap.remotes[1].established);
        assert_eq!(snap.remotes[1].rate, 0, "no counters → 0");
        assert!(remote_snapshot(&[], Instant::now()).remotes.is_empty());
    }

    #[test]
    fn private_ranges_are_told_from_public_ones() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(is_private(ip("192.168.1.9")));
        assert!(is_private(ip("10.0.0.4")));
        assert!(is_private(ip("172.16.5.1")));
        assert!(is_private(ip("169.254.1.1")), "link-local");
        assert!(is_private(ip("fd00::1")), "IPv6 ULA");
        assert!(is_private(ip("fe80::1")), "IPv6 link-local");
        assert!(!is_private(ip("142.250.185.78")));
        assert!(!is_private(ip("172.32.0.1")), "just outside 172.16/12");
        assert!(!is_private(ip("2606:4700::1111")));
        assert!(is_loopback(ip("127.0.0.1")) && is_loopback(ip("::1")));
        assert_eq!(scope(ip("10.0.0.4")), "LAN");
        assert_eq!(scope(ip("1.1.1.1")), "public");
        assert_eq!(scope(ip("127.0.0.1")), "local");
    }

    #[test]
    fn cloud_is_read_from_the_name_then_the_block() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(cloud(Some("163.66.149.34.bc.googleusercontent.com"), ip("34.149.66.163")), Some("Google Cloud"));
        assert_eq!(cloud(Some("ec2-3-4-5-6.compute-1.amazonaws.com"), ip("3.4.5.6")), Some("AWS"));
        assert_eq!(cloud(Some("notamazonaws.com"), ip("3.4.5.6")), None, "suffix must sit on a label boundary");
        assert_eq!(cloud(None, ip("1.1.1.1")), Some("Cloudflare"));
        assert_eq!(cloud(None, ip("104.31.7.7")), Some("Cloudflare"));
        assert_eq!(cloud(None, ip("17.253.1.1")), Some("Apple"));
        assert_eq!(cloud(None, ip("93.184.216.34")), None);
        assert_eq!(cloud(Some("nas.googleusercontent.com"), ip("192.168.1.2")), None, "LAN is never a cloud");
    }

    #[test]
    fn rows_group_by_process_and_sort_three_ways() {
        let m = loaded();
        let v = m.visible();
        let rows = group_rows(&v, Sort::Process);
        // Groups alphabetically: backup, chrome, ssh.
        let heads: Vec<String> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Proc { name, count, .. } => Some(format!("{name}/{count}")),
                Row::Conn(_) => None,
            })
            .collect();
        assert_eq!(heads, vec!["backup/1", "chrome/2", "ssh/1"]);
        assert_eq!(rows.len(), 3 + 4, "one header per group plus every connection");
        // Within chrome, the lower remote address comes first.
        let chrome: Vec<usize> = rows
            .iter()
            .skip_while(|r| !matches!(r, Row::Proc { name, .. } if name == "chrome"))
            .skip(1)
            .take(2)
            .filter_map(|r| match r {
                Row::Conn(i) => Some(*i),
                Row::Proc { .. } => None,
            })
            .collect();
        assert_eq!(
            (v[chrome[0]].remote.ip().to_string(), v[chrome[1]].remote.ip().to_string()),
            ("142.250.185.78".to_string(), "142.250.185.99".to_string())
        );

        // By traffic: the busiest process first, the one with no rate last.
        let rows = group_rows(&v, Sort::Traffic);
        let heads: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Proc { name, .. } => Some(name.as_str()),
                Row::Conn(_) => None,
            })
            .collect();
        assert_eq!(heads, vec!["chrome", "ssh", "backup"]);
        // chrome's own rows: the 120 kB/s one before the rate-less one.
        let first = rows.iter().find_map(|r| match r {
            Row::Conn(i) => Some(*i),
            Row::Proc { .. } => None,
        });
        assert_eq!(v[first.unwrap()].rate_in, Some(120_000));
        let last = rows.iter().rev().find_map(|r| match r {
            Row::Conn(i) => Some(*i),
            Row::Proc { .. } => None,
        });
        assert_eq!(v[last.unwrap()].process, "backup");

        // By remote: groups ordered by their lowest remote address.
        let rows = group_rows(&v, Sort::Remote);
        let heads: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Proc { name, .. } => Some(name.as_str()),
                Row::Conn(_) => None,
            })
            .collect();
        assert_eq!(heads, vec!["ssh", "chrome", "backup"]);

        // Both rate-less: the group header shows no rate at all.
        let none = [conn("idle", "203.0.113.9", 80, "ESTABLISHED", None)];
        let refs: Vec<&Conn> = none.iter().collect();
        assert_eq!(group_rows(&refs, Sort::Traffic)[0], Row::Proc {
            name: "idle".into(),
            count: 1,
            rate: None
        });
        assert!(group_rows(&[], Sort::Process).is_empty());
    }

    #[test]
    fn filter_matches_process_remote_and_name() {
        let c = conn("chrome", "142.250.185.78", 443, "ESTABLISHED", None);
        assert!(matches_filter(&c, ""), "an empty filter keeps everything");
        assert!(matches_filter(&c, "CHROME"), "case-insensitive on the process");
        assert!(matches_filter(&c, "142.250"), "substring of the address");
        assert!(matches_filter(&c, "example.net"), "substring of the rDNS name");
        assert!(!matches_filter(&c, "firefox"));
        let mut m = loaded();
        m.filter = "chrome".into();
        assert_eq!(m.visible().len(), 2);
        m.filter = "10.0.0".into();
        assert_eq!(m.visible().len(), 1);
        m.filter = "nothing".into();
        assert!(m.visible().is_empty());
    }

    #[test]
    fn rates_handle_the_first_sample_and_a_reset() {
        let dt = Duration::from_secs(3);
        assert_eq!(rate(Some(1000), Some(4000), dt), Some(1000));
        assert_eq!(rate(None, Some(4000), dt), None, "the first sample has nothing to subtract");
        assert_eq!(rate(Some(1000), None, dt), None, "no counter, no rate");
        assert_eq!(rate(Some(9000), Some(10), dt), None, "a counter that went backwards is a reset");
        assert_eq!(rate(Some(10), Some(10), dt), Some(0), "idle is a rate, not an unknown");
        assert_eq!(rate(Some(0), Some(10), Duration::ZERO), None, "no time passed, no rate");
        assert_eq!(rate_cell(None), "—");
        assert_eq!(rate_cell(Some(0)), "·");
        assert_eq!(rate_cell(Some(512)), "512");
        assert_eq!(rate_cell(Some(1000)), "1000", "below the 1024 threshold, not a misleading \"0k\"");
        assert_eq!(rate_cell(Some(1024)), "1k");
        assert_eq!(rate_cell(Some(20_480)), "20k");
        assert!(rate_cell(Some(2_097_152)).starts_with("2.0M"));
        assert_eq!(rate_line(None), "—");
        assert_eq!(age(Duration::from_secs(12)), "12s");
        assert_eq!(age(Duration::from_secs(300)), "5m");
        assert_eq!(age(Duration::from_secs(7200)), "2h");
        assert_eq!(age(Duration::from_secs(3 * 86_400)), "3d");
    }

    #[test]
    fn name_cache_expires_and_sweeps() {
        let mut c = NameCache::default();
        let t0 = Instant::now();
        let info = ProcInfo { name: "chrome".into(), exe: None };
        c.put(1234, info.clone(), t0);
        assert_eq!(c.get(1234, t0), Some(&info));
        assert_eq!(c.get(1234, t0 + NAME_TTL - Duration::from_secs(1)), Some(&info));
        assert_eq!(c.get(1234, t0 + NAME_TTL), None, "expired, ask sysinfo again");
        assert_eq!(c.get(9999, t0), None);
        c.sweep(t0 + NAME_TTL);
        assert_eq!(c.get(1234, t0), None, "the stale entry is gone for good");
    }

    #[test]
    fn dns_resolver_never_blocks_and_serves_the_cache() {
        let r = DnsResolver::new();
        let ip: IpAddr = "203.0.113.5".parse().unwrap();

        // Nothing cached yet: the lookup only enqueues the miss and returns
        // immediately — it never waits on `getnameinfo` itself.
        let start = Instant::now();
        let out = r.lookup(std::iter::once(ip));
        assert!(start.elapsed() < Duration::from_millis(200), "lookup must never block on DNS");
        assert!(out.get(&ip).is_none(), "not cached yet");

        // Once a resolver thread has written a result (simulated here rather
        // than waiting on a real lookup), a later lookup serves it from cache.
        r.cache.lock().unwrap().insert(ip, (Some("host.example.net".into()), Instant::now()));
        let out = r.lookup(std::iter::once(ip));
        assert_eq!(out.get(&ip).cloned().flatten(), Some("host.example.net".to_string()));
    }

    #[test]
    fn title_and_overview_read_like_the_spec() {
        let t = Theme::new(ThemeKind::Color);
        let m = loaded();
        let title = plain(&m.title_line(200, t));
        assert!(title.starts_with("CONN · 4 connections · 12 listening · ↓ "), "{title}");
        assert!(title.contains("traffic: ok"), "{title}");
        assert!(title.ends_with(" · v: LOCAL NET"), "the other view of the tab is named: {title}");
        let sum = plain(&m.summary_line(200));
        assert_eq!(sum.trim(), "4 connections · 3 processes · top chrome → chrome.example.net");
        assert!(plain(&m.summary_line(20)).chars().count() <= 19, "the summary is cut to the block width");
        assert_eq!(m.status(), "comms 4 conns, 3 procs, estats=ok");

        let mut denied = loaded();
        denied.estats = Estats::Denied;
        assert!(plain(&denied.title_line(200, t)).contains("traffic: run as administrator"));
        assert!(denied.status().ends_with("estats=denied"));
    }

    #[test]
    fn draws_loading_list_detail_and_filter() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = ConnView::new();
        m.loading = true;
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        assert!(screen(&term, 120, 40).contains("LOADING"));

        m = loaded();
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let s = screen(&term, 120, 40);
        assert!(s.contains("PROC") && s.contains("REMOTE") && s.contains("STATE"), "{s}");
        assert!(s.contains("142.250.185.78") && s.contains("443 https"), "{s}");
        assert!(s.contains("chrome · 2"), "the process header with its connection count: {s}");
        assert!(s.contains("ESTABLISHED") && s.contains("TIME_WAIT"), "{s}");

        // Detail of the first connection.
        let key = m.conns[0].key();
        m.mode = Mode::Detail(key);
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let s = screen(&term, 120, 40);
        assert!(s.contains("remote  142.250.185.78:443"), "{s}");
        assert!(s.contains("pid     1234") && s.contains("chrome.exe"), "{s}");
        assert!(s.contains("seen    5m ago"), "{s}");

        // A connection that closed while its detail was open falls back.
        m.conns.remove(0);
        assert!(m.find(key).is_none());
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        assert!(screen(&term, 120, 40).contains("PROC"), "back to the list");

        // Filter prompt.
        m = loaded();
        m.mode = Mode::Filter("chro".into());
        m.filter = "chro".into();
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let s = screen(&term, 120, 40);
        assert!(s.contains("FILTER> chro"), "{s}");
        assert!(!s.contains("10.0.0.4"), "the ssh row is filtered out: {s}");

        // Nothing matches.
        m.filter = "zzz".into();
        m.mode = Mode::List;
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        assert!(screen(&term, 120, 40).contains("nothing matches"));
    }

    #[test]
    fn tiny_areas_do_not_panic() {
        let t = Theme::new(ThemeKind::Color);
        let mut m = loaded();
        for (w, h) in [(40u16, 12u16), (1, 1), (80, 24)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| m.draw(f, f.area(), t)).unwrap();
            m.mode = Mode::Detail(m.conns[0].key());
            term.draw(|f| m.draw(f, f.area(), t)).unwrap();
            m.mode = Mode::Filter("x".into());
            term.draw(|f| m.draw(f, f.area(), t)).unwrap();
            m.mode = Mode::List;
        }
        // Narrow: only PROC, REMOTE and PORT.
        let mut term = Terminal::new(TestBackend::new(50, 12)).unwrap();
        term.draw(|f| m.draw(f, f.area(), t)).unwrap();
        let s = screen(&term, 50, 12);
        assert!(s.contains("PORT") && !s.contains("STATE"), "{s}");

        // Empty, with no data at all.
        let empty = ConnView::new();
        let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
        term.draw(|f| empty.draw(f, f.area(), t)).unwrap();
        assert!(screen(&term, 40, 12).contains("no connections"));
        let mut term = Terminal::new(TestBackend::new(1, 1)).unwrap();
        term.draw(|f| empty.draw(f, f.area(), t)).unwrap();
    }

    #[test]
    fn keys_move_select_sort_and_filter() {
        let (ctx, _notices) = crate::shell::test_ctx(toml::Table::new());
        let mut m = loaded();
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let idx = |m: &ConnView| {
            let (conns, rows) = m.rows();
            m.resolve_sel_idx(&conns, &rows)
        };
        assert!(m.on_key(key(KeyCode::Down), &ctx));
        assert_eq!(idx(&m), 1);
        assert!(m.on_key(key(KeyCode::Up), &ctx));
        assert!(m.on_key(key(KeyCode::Up), &ctx));
        assert_eq!(idx(&m), 0, "clamped at the top");
        // Row 0 is a process header: Enter does nothing there.
        assert!(m.on_key(key(KeyCode::Enter), &ctx));
        assert_eq!(m.mode, Mode::List);
        m.on_key(key(KeyCode::Down), &ctx);
        assert_eq!(idx(&m), 1);
        assert!(m.on_key(key(KeyCode::Enter), &ctx));
        assert!(matches!(m.mode, Mode::Detail(_)));
        assert!(m.on_key(key(KeyCode::Esc), &ctx));
        assert_eq!(m.mode, Mode::List);

        assert!(m.on_key(key(KeyCode::Char('s')), &ctx));
        assert_eq!(m.sort, Sort::Remote);
        m.on_key(key(KeyCode::Char('s')), &ctx);
        assert_eq!(m.sort, Sort::Traffic);
        m.on_key(key(KeyCode::Char('s')), &ctx);
        assert_eq!(m.sort, Sort::Process);

        assert!(m.on_key(key(KeyCode::Char('f')), &ctx));
        for c in "ssh".chars() {
            m.on_key(key(KeyCode::Char(c)), &ctx);
        }
        assert_eq!(m.filter, "ssh");
        assert_eq!(m.visible().len(), 1, "the list narrows while typing");
        m.on_key(key(KeyCode::Enter), &ctx);
        assert_eq!(m.mode, Mode::List);
        assert_eq!(m.filter, "ssh");
        m.on_key(key(KeyCode::Esc), &ctx);
        assert_eq!(m.filter, "", "esc clears an accepted filter");
        assert!(!m.on_key(key(KeyCode::Esc), &ctx), "with nothing to clear, esc is not ours");

        m.on_key(key(KeyCode::Char('f')), &ctx);
        m.on_key(key(KeyCode::Char('x')), &ctx);
        m.on_key(key(KeyCode::Esc), &ctx);
        assert_eq!((m.filter.as_str(), &m.mode), ("", &Mode::List), "esc cancels the prompt too");

        assert!(m.on_key(key(KeyCode::Char('l')), &ctx));
        assert!(m.show_loopback);
        assert!(!m.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), &ctx), "ctrl+c quits");
    }

    #[test]
    fn selection_survives_a_regroup_above_it() {
        let (ctx, _notices) = crate::shell::test_ctx(toml::Table::new());
        let mut m = loaded(); // rows: backup/1, chrome/2 (·78, ·99), ssh/1
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        for _ in 0..4 {
            m.on_key(down, &ctx);
        }
        let (conns, rows) = m.rows();
        let before = m.resolve_sel_idx(&conns, &rows);
        assert_eq!(before, 4, "landed on chrome's second connection");
        let Row::Conn(i) = rows[before] else { panic!("expected a connection row") };
        assert_eq!(conns[i].remote.ip().to_string(), "142.250.185.99");

        // A new process group sorts in above "chrome", pushing its rows down.
        m.conns.insert(0, conn("apple", "198.51.100.1", 80, "ESTABLISHED", None));
        let (conns, rows) = m.rows();
        let after = m.resolve_sel_idx(&conns, &rows);
        assert_ne!(after, before, "the row moved because a group was inserted above it");
        let Row::Conn(i) = rows[after] else { panic!("expected a connection row") };
        assert_eq!(conns[i].remote.ip().to_string(), "142.250.185.99", "same connection stays selected");
    }

    #[test]
    fn config_defaults_and_interval_floor() {
        let cfg = CommsCfg::default();
        assert_eq!((cfg.interval, cfg.show_loopback), (3, false));
        let table: toml::Table = toml::from_str("[comms]\ninterval = 0\nshow_loopback = true\n").unwrap();
        let (parsed, notice) = crate::module::ModuleConfig(table).section::<CommsCfg>("comms");
        assert!(notice.is_none());
        assert!(parsed.show_loopback);
        assert_eq!(parsed.interval.max(MIN_INTERVAL), 1, "never faster than a second");
    }

    #[test]
    fn untrusted_names_are_sanitized() {
        assert_eq!(sanitize_name("host.example.net"), "host.example.net");
        assert_eq!(sanitize_name("  bad\u{1b}[2Jname\u{202e} "), "bad[2Jname");
        assert_eq!(sanitize_name(&"a".repeat(80)).chars().count(), MAX_NAME);
    }
}
