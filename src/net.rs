//! NET forrás: ICMP ping-hurkok, link-adatok és tracert egy saját szálon.
use crate::modules::net::NetCfg;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub const HIST: usize = 120;
pub const WINDOW: usize = 60;
const PING_TIMEOUT_MS: u32 = 2000;
const PING_PERIOD: Duration = Duration::from_secs(1);
const LINK_PERIOD: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq)]
pub struct WifiInfo { pub ssid: String, pub signal: u8 }

#[derive(Clone, Debug, PartialEq)]
pub struct IfaceInfo { pub ip: String, pub gateway: String, pub dns: Option<String> }

#[derive(Clone, Debug)]
pub enum NetEvent {
    Resolved { target: usize, addr: Option<String> },
    Ping { target: usize, rtt: Option<u32> },
    Wifi(Option<WifiInfo>),
    Iface(Option<IfaceInfo>),
    TraceLine(String),
    TraceDone,
    Log(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum NetCmd { TraceStart, TraceStop }

#[derive(Clone, Debug)]
pub struct Stats { pub min: u32, pub avg: u32, pub max: u32, pub loss_pct: u8 }

/// min/átlag/max a sikeres mintákból, veszteség % az összesből. `None`, ha nincs minta.
pub fn stats(samples: impl Iterator<Item = Option<u32>>) -> Option<Stats> {
    let v: Vec<Option<u32>> = samples.collect();
    if v.is_empty() { return None; }
    let ok: Vec<u32> = v.iter().flatten().copied().collect();
    let loss_pct = ((v.len() - ok.len()) * 100 / v.len()) as u8;
    let (min, max) = (ok.iter().copied().min().unwrap_or(0), ok.iter().copied().max().unwrap_or(0));
    let avg = if ok.is_empty() { 0 } else { (ok.iter().map(|&x| x as u64).sum::<u64>() / ok.len() as u64) as u32 };
    Some(Stats { min, avg, max, loss_pct })
}

fn value_after_colon(line: &str) -> Option<(&str, &str)> {
    let (label, value) = line.split_once(':')?;
    Some((label.trim().trim_end_matches(['.', ' ']).trim(), value.trim()))
}

fn is_v4(s: &str) -> bool { s.parse::<Ipv4Addr>().is_ok() }

fn is_gateway_label(l: &str) -> bool {
    // "átjáró" a HU OEM-kódlapon (CP852) String::from_utf8_lossy után olvashatatlanná válik,
    // de az ASCII "alap" (Alapértelmezett) prefix túléli a torzítást.
    l.contains("gateway") || l.starts_with("alap")
}

/// `ipconfig` kimenetéből az első adapter, amelynek van IPv4 átjárója (angol és magyar Windows).
pub fn parse_ipconfig(out: &str) -> Option<IfaceInfo> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in out.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() { blocks.push(Vec::new()); }
        if let Some(b) = blocks.last_mut() { b.push(line); }
    }
    for b in blocks {
        let (mut ip, mut gw, mut dns) = (None, None, None);
        let mut last_label: Option<String> = None;
        for line in b {
            match value_after_colon(line) {
                Some((label, value)) => {
                    let l = label.to_lowercase();
                    last_label = Some(l.clone());
                    if !is_v4(value) { continue; }
                    if l.contains("ipv4") && ip.is_none() { ip = Some(value.to_string()); }
                    else if is_gateway_label(&l) && gw.is_none() { gw = Some(value.to_string()); }
                    else if l.contains("dns") && dns.is_none() { dns = Some(value.to_string()); }
                }
                None => {
                    // Folytatás sor (pl. további átjáró/DNS cím, kettőspont nélkül, behúzva):
                    // az előző címke értékének tekintjük, ha IPv4-nek elemezhető.
                    let trimmed = line.trim();
                    if line.starts_with(' ') && !trimmed.is_empty() && is_v4(trimmed) {
                        if let Some(l) = &last_label {
                            if is_gateway_label(l) && gw.is_none() { gw = Some(trimmed.to_string()); }
                            else if l.contains("dns") && dns.is_none() { dns = Some(trimmed.to_string()); }
                        }
                    }
                }
            }
        }
        if let (Some(ip), Some(gateway)) = (ip, gw) { return Some(IfaceInfo { ip, gateway, dns }); }
    }
    None
}

/// `netsh wlan show interfaces` → SSID + jel % (angol "Signal", magyar "Jel").
pub fn parse_netsh(out: &str) -> Option<WifiInfo> {
    let (mut ssid, mut signal) = (None, None);
    for line in out.lines() {
        let Some((label, value)) = value_after_colon(line) else { continue };
        let l = label.to_lowercase();
        if l == "ssid" && ssid.is_none() { ssid = Some(value.to_string()); }
        else if (l.starts_with("signal") || l.starts_with("jel")) && signal.is_none() {
            signal = value.trim_end_matches('%').trim().parse::<u8>().ok();
        }
    }
    Some(WifiInfo { ssid: ssid?, signal: signal? })
}

/// "gateway" → az átjáró címe; IPv4 literál → önmaga; hostnév → DNS-feloldás (első IPv4).
pub fn resolve_target(spec: &str, gateway: Option<&str>) -> Option<String> {
    if spec.eq_ignore_ascii_case("gateway") { return gateway.map(str::to_string); }
    if is_v4(spec) { return Some(spec.to_string()); }
    (spec, 0u16).to_socket_addrs().ok()?.find(|a| a.is_ipv4()).map(|a| a.ip().to_string())
}

fn run_cmd(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

pub fn spawn(cfg: NetCfg, tx: Sender<NetEvent>) -> Sender<NetCmd> {
    let (ctx, crx) = mpsc::channel();
    thread::spawn(move || run(cfg, tx, crx));
    ctx
}

fn run(cfg: NetCfg, tx: Sender<NetEvent>, crx: Receiver<NetCmd>) {
    let iface = run_cmd("ipconfig", &[]).and_then(|o| parse_ipconfig(&o));
    let _ = tx.send(NetEvent::Iface(iface.clone()));
    let wifi = run_cmd("netsh", &["wlan", "show", "interfaces"]).and_then(|o| parse_netsh(&o));
    let _ = tx.send(NetEvent::Wifi(wifi));
    let gw = iface.as_ref().map(|i| i.gateway.clone());
    let mut first_addr: Option<String> = None;
    for (i, spec) in cfg.targets.iter().enumerate() {
        let addr = resolve_target(spec, gw.as_deref());
        let _ = tx.send(NetEvent::Resolved { target: i, addr: addr.clone() });
        if let Some(a) = addr {
            if first_addr.is_none() { first_addr = Some(a.clone()); }
            let tx = tx.clone();
            thread::spawn(move || ping_loop(i, a, tx));
        }
    }
    let mut trace: Option<Child> = None;
    let mut next_link = Instant::now() + LINK_PERIOD;
    loop {
        if Instant::now() >= next_link {
            let wifi = run_cmd("netsh", &["wlan", "show", "interfaces"]).and_then(|o| parse_netsh(&o));
            let _ = tx.send(NetEvent::Wifi(wifi));
            let iface = run_cmd("ipconfig", &[]).and_then(|o| parse_ipconfig(&o));
            let _ = tx.send(NetEvent::Iface(iface));
            next_link = Instant::now() + LINK_PERIOD;
        }
        match crx.recv_timeout(Duration::from_millis(500)) {
            Ok(NetCmd::TraceStart) => {
                stop_trace(&mut trace);
                let Some(target) = first_addr.clone() else {
                    let _ = tx.send(NetEvent::Log("no target to trace".into()));
                    let _ = tx.send(NetEvent::TraceDone);
                    continue;
                };
                match Command::new("tracert").args(["-d", "-w", "1000", "-h", "30", &target]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
                    Ok(mut child) => {
                        if let Some(out) = child.stdout.take() {
                            let tx = tx.clone();
                            thread::spawn(move || {
                                for line in BufReader::new(out).lines().map_while(Result::ok) {
                                    let line = line.trim().to_string();
                                    if !line.is_empty() { let _ = tx.send(NetEvent::TraceLine(line)); }
                                }
                                let _ = tx.send(NetEvent::TraceDone);
                            });
                        }
                        trace = Some(child);
                    }
                    Err(e) => {
                        let _ = tx.send(NetEvent::Log(format!("tracert: {e}")));
                        let _ = tx.send(NetEvent::TraceDone);
                    }
                }
            }
            Ok(NetCmd::TraceStop) => stop_trace(&mut trace),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => { stop_trace(&mut trace); return; }
        }
    }
}

fn stop_trace(trace: &mut Option<Child>) {
    if let Some(mut c) = trace.take() { let _ = c.kill(); let _ = c.wait(); }
}

fn ping_loop(target: usize, addr: String, tx: Sender<NetEvent>) {
    let Ok(ip) = addr.parse::<Ipv4Addr>() else { return };
    let Some(pinger) = Pinger::new() else {
        let _ = tx.send(NetEvent::Log("ICMP handle unavailable".into()));
        return;
    };
    loop {
        let started = Instant::now();
        let rtt = pinger.ping(ip, PING_TIMEOUT_MS);
        if tx.send(NetEvent::Ping { target, rtt }).is_err() { return; }
        if let Some(left) = PING_PERIOD.checked_sub(started.elapsed()) { thread::sleep(left); }
    }
}

// ---- ICMP echo a Windows iphlpapi-n át (nem kell admin, nem kell raw socket) ----
pub mod icmp {
    use std::net::Ipv4Addr;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::NetworkManagement::IpHelper::{IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY};

    pub struct Pinger { h: HANDLE }

    impl Pinger {
        pub fn new() -> Option<Self> {
            let h = unsafe { IcmpCreateFile() };
            if h.is_null() || h as isize == -1 { None } else { Some(Self { h }) }
        }

        /// Egy echo; `Some(rtt_ms)` sikerkor, `None` timeoutnál vagy hibánál.
        pub fn ping(&self, ip: Ipv4Addr, timeout_ms: u32) -> Option<u32> {
            let data = [0x50u8; 32];
            let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 8];
            let addr = u32::from_ne_bytes(ip.octets()); // hálózati bájtsorrend, ahogy az API várja
            let n = unsafe {
                IcmpSendEcho(self.h, addr, data.as_ptr().cast(), data.len() as u16, std::ptr::null(),
                    reply.as_mut_ptr().cast(), reply.len() as u32, timeout_ms)
            };
            if n == 0 { return None; }
            let r = unsafe { std::ptr::read_unaligned(reply.as_ptr() as *const ICMP_ECHO_REPLY) };
            (r.Status == 0).then_some(r.RoundTripTime)
        }
    }

    impl Drop for Pinger {
        fn drop(&mut self) { unsafe { IcmpCloseHandle(self.h); } }
    }
}
use icmp::Pinger;

/// Reverse DNS for one address (v4 or v6). Blocking: `getnameinfo` has no
/// timeout of its own, so this call may take up to the OS resolver's own
/// timeout. A per-call detached thread used to enforce a 1 s ceiling here,
/// but `getnameinfo` doesn't stop when its caller stops waiting — it kept
/// running (and blocking) past the deadline, one leaked thread per lookup.
/// Callers that need a ceiling now run this on their own resolver thread
/// (a standing one, not spawned per call) and simply move on without it,
/// same as the leaked thread would have — but without spawning a new one
/// every cycle. `None` = no PTR record.
///
/// Privacy: this asks whatever resolver Windows is configured to use, so the
/// address being looked up leaves the machine. Nothing else does.
pub fn reverse_dns(ip: std::net::IpAddr) -> Option<String> {
    rdns::host_name(ip)
}

mod rdns {
    use std::net::IpAddr;
    use windows_sys::Win32::Networking::WinSock::{
        getnameinfo, WSAStartup, AF_INET, AF_INET6, NI_NAMEREQD, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, WSADATA,
    };

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

    /// `NI_NAMEREQD` means "a name or nothing", so an address is never echoed
    /// back as its own host name.
    pub fn host_name(ip: IpAddr) -> Option<String> {
        if !winsock() {
            return None;
        }
        let mut host = [0u8; 256];
        let code = match ip {
            IpAddr::V4(v4) => {
                let mut sa: SOCKADDR_IN = unsafe { std::mem::zeroed() };
                sa.sin_family = AF_INET;
                // Network byte order, the same way `Pinger` hands an address over.
                sa.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.octets());
                unsafe {
                    getnameinfo(
                        std::ptr::from_ref(&sa).cast::<SOCKADDR>(),
                        std::mem::size_of::<SOCKADDR_IN>() as i32,
                        host.as_mut_ptr(),
                        host.len() as u32,
                        std::ptr::null_mut(),
                        0,
                        NI_NAMEREQD as i32,
                    )
                }
            }
            IpAddr::V6(v6) => {
                let mut sa: SOCKADDR_IN6 = unsafe { std::mem::zeroed() };
                sa.sin6_family = AF_INET6;
                sa.sin6_addr.u.Byte = v6.octets();
                unsafe {
                    getnameinfo(
                        std::ptr::from_ref(&sa).cast::<SOCKADDR>(),
                        std::mem::size_of::<SOCKADDR_IN6>() as i32,
                        host.as_mut_ptr(),
                        host.len() as u32,
                        std::ptr::null_mut(),
                        0,
                        NI_NAMEREQD as i32,
                    )
                }
            }
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

    const IPCONFIG_EN: &str = "\r\nWindows IP Configuration\r\n\r\n\r\nEthernet adapter Ethernet:\r\n\r\n   Media State . . . . . . . . . . . : Media disconnected\r\n\r\nWireless LAN adapter Wi-Fi:\r\n\r\n   IPv4 Address. . . . . . . . . . . : 192.168.1.42\r\n   Subnet Mask . . . . . . . . . . . : 255.255.255.0\r\n   Default Gateway . . . . . . . . . : 192.168.1.1\r\n";
    const IPCONFIG_HU: &str = "\r\nVezeték nélküli LAN adapter Wi-Fi:\r\n\r\n   IPv4-cím. . . . . . . . . . . . . : 10.0.0.7\r\n   Alhálózati maszk  . . . . . . . . : 255.255.255.0\r\n   Alapértelmezett átjáró. . . . . . : 10.0.0.1\r\n";
    const NETSH_EN: &str = "\r\nThere is 1 interface on the system:\r\n\r\n    Name                   : Wi-Fi\r\n    State                  : connected\r\n    SSID                   : NLM-5G\r\n    BSSID                  : aa:bb:cc:dd:ee:ff\r\n    Signal                 : 82%\r\n";
    const NETSH_HU: &str = "    Név                    : Wi-Fi\r\n    SSID                   : Otthon\r\n    BSSID                  : 00:11:22:33:44:55\r\n    Jel                    : 64%\r\n";
    // HU ipconfig, OEM-kódlap (CP852) `String::from_utf8_lossy` utáni torzítása szimulálva:
    // minden nem-ASCII karakter U+FFFD-re cserélve, ahogy éles gépen ténylegesen látszana.
    const IPCONFIG_HU_MANGLED: &str = "\r\nVezet\u{FFFD}k n\u{FFFD}lk\u{FFFD}li LAN adapter Wi-Fi:\r\n\r\n   IPv4-c\u{FFFD}m. . . . : 10.0.0.7\r\n   Alap\u{FFFD}rtelmezett \u{FFFD}tj\u{FFFD}r\u{FFFD}. . . : 10.0.0.1\r\n";
    const IPCONFIG_MULTILINE_GATEWAY: &str = "\r\nWireless LAN adapter Wi-Fi:\r\n\r\n   IPv4 Address. . . . . . . . . . . : 192.168.0.5\r\n   Default Gateway . . . . . . . . . : fe80::1%14\r\n                                       192.168.0.1\r\n";

    #[test]
    fn ipconfig_picks_adapter_with_gateway() {
        let i = parse_ipconfig(IPCONFIG_EN).unwrap();
        assert_eq!(i.ip, "192.168.1.42");
        assert_eq!(i.gateway, "192.168.1.1");
        let i = parse_ipconfig(IPCONFIG_HU).unwrap();
        assert_eq!(i.gateway, "10.0.0.1");
        assert!(parse_ipconfig("nothing here").is_none());
    }

    #[test]
    fn ipconfig_mangled_hu_oem_codepage_still_matches_gateway() {
        let i = parse_ipconfig(IPCONFIG_HU_MANGLED).unwrap();
        assert_eq!(i.ip, "10.0.0.7");
        assert_eq!(i.gateway, "10.0.0.1");
    }

    #[test]
    fn ipconfig_continuation_line_gives_gateway() {
        let i = parse_ipconfig(IPCONFIG_MULTILINE_GATEWAY).unwrap();
        assert_eq!(i.ip, "192.168.0.5");
        assert_eq!(i.gateway, "192.168.0.1");
    }

    #[test]
    fn netsh_ssid_and_signal() {
        let w = parse_netsh(NETSH_EN).unwrap();
        assert_eq!((w.ssid.as_str(), w.signal), ("NLM-5G", 82));
        let w = parse_netsh(NETSH_HU).unwrap();
        assert_eq!((w.ssid.as_str(), w.signal), ("Otthon", 64));
        assert!(parse_netsh("").is_none());
    }

    #[test]
    fn stats_with_losses() {
        let s = stats([Some(10), None, Some(30), Some(20)].into_iter()).unwrap();
        assert_eq!((s.min, s.avg, s.max, s.loss_pct), (10, 20, 30, 25));
        assert!(stats(std::iter::empty()).is_none());
        let s = stats([None, None].into_iter()).unwrap();
        assert_eq!((s.loss_pct, s.max), (100, 0));
    }

    #[test]
    fn resolve_literal_and_keyword() {
        assert_eq!(resolve_target("1.1.1.1", Some("10.0.0.1")), Some("1.1.1.1".into()));
        assert_eq!(resolve_target("gateway", Some("10.0.0.1")), Some("10.0.0.1".into()));
        assert_eq!(resolve_target("gateway", None), None);
    }
}
