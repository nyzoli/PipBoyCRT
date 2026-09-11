//! WIFI source: WlanAPI scanning on its own thread, plus the pure channel and
//! congestion math the view draws.
//!
//! `netsh` is deliberately not used: its output is localized. Everything
//! `unsafe` lives in the [`ffi`] module at the bottom — every WlanAPI return
//! code is checked, every buffer the API hands out is released by a `Drop`
//! guard (`WlanFreeMemory`) and the client handle by `Wlan`'s own `Drop`. "No
//! wireless adapter" is a state, not an error, and nothing here panics.
//!
//! Item counts are bounded twice: by the buffer's own `dwTotalSize` where the
//! API reports one (only `WLAN_BSS_LIST` does), and by [`MAX_ITEMS`] otherwise.
//! `MAX_ITEMS` alone is a plausibility cap, not a safety guarantee — it cannot
//! tell a short buffer from a long one, which is why the size bound comes first.

use std::sync::atomic::{compiler_fence, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// How long the driver gets to finish a scan before we read the BSS list.
const SCAN_WAIT: Duration = Duration::from_secs(4);
/// Plausibility cap on the item counts the API reports (guards a corrupt list).
const MAX_ITEMS: usize = 512;

/// The three Wi-Fi bands the tab knows about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Band {
    #[default]
    G2_4,
    G5,
    G6,
}

impl Band {
    pub fn label(self) -> &'static str {
        match self {
            Band::G2_4 => "2.4 GHz",
            Band::G5 => "5 GHz",
            Band::G6 => "6 GHz",
        }
    }

    /// Short form for the list rows.
    pub fn short(self) -> &'static str {
        match self {
            Band::G2_4 => "2.4",
            Band::G5 => "5",
            Band::G6 => "6",
        }
    }

    /// `b` cycles 2.4 → 5 → 6 → 2.4.
    pub fn next(self) -> Self {
        match self {
            Band::G2_4 => Band::G5,
            Band::G5 => Band::G6,
            Band::G6 => Band::G2_4,
        }
    }

    /// Channel spacing in MHz: 2.4 GHz channels are 5 MHz apart and overlap,
    /// 5 and 6 GHz channels are 20 MHz apart and do not.
    fn spacing_mhz(self) -> f32 {
        match self {
            Band::G2_4 => 5.0,
            _ => 20.0,
        }
    }

    /// Every channel on the chart's x axis.
    pub fn chart_channels(self) -> Vec<u16> {
        match self {
            Band::G2_4 => (1..=14).collect(),
            Band::G5 => (36..=64).step_by(4).chain((100..=144).step_by(4)).chain((149..=165).step_by(4)).collect(),
            Band::G6 => (1..=233).step_by(4).collect(),
        }
    }

    /// The channels worth recommending — [`best_channel`] picks from these.
    pub fn usual_channels(self) -> Vec<u16> {
        match self {
            Band::G2_4 => vec![1, 6, 11],
            _ => self.chart_channels(),
        }
    }
}

/// One basic service set (one radio of one access point).
#[derive(Clone, Debug, PartialEq)]
pub struct Bss {
    /// The SSID as text — lossy, so it is for display and grouping only.
    pub ssid: String,
    /// The SSID exactly as the driver reported it (≤ 32 bytes). A profile must
    /// be built from these, not from [`ssid`](Bss::ssid): a lossy U+FFFD would
    /// name a network that does not exist.
    pub ssid_raw: Vec<u8>,
    pub bssid: String,
    pub rssi_dbm: i32,
    pub freq_khz: u32,
    pub channel: u16,
    pub band: Band,
    pub width_mhz: u16,
    pub secured: bool,
    pub auth: String,
    pub connected: bool,
}

/// What the module publishes on the blackboard under `"wifi"`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WifiSnapshot {
    /// One entry per SSID, strongest first.
    pub networks: Vec<Bss>,
    pub connected: Option<String>,
}

/// Centre frequency → band and channel number. `None` for anything outside the
/// known plans (so a odd reading is skipped instead of drawn in the wrong band).
pub fn channel_from_khz(khz: u32) -> Option<(Band, u16)> {
    // `ulChCenterFrequency` is documented in kHz; a driver reporting MHz is
    // tolerated rather than dropped.
    let mhz = if khz >= 1_000_000 { khz / 1000 } else { khz };
    match mhz {
        2412..=2472 if (mhz - 2412) % 5 == 0 => Some((Band::G2_4, ((mhz - 2412) / 5 + 1) as u16)),
        2484 => Some((Band::G2_4, 14)),
        5170..=5895 if mhz % 5 == 0 => Some((Band::G5, ((mhz - 5000) / 5) as u16)),
        5955..=7115 if (mhz - 5955) % 5 == 0 => Some((Band::G6, ((mhz - 5950) / 5) as u16)),
        _ => None,
    }
}

/// Channel width guessed from the PHY type; 20 MHz whenever it is not derivable.
pub fn width_from_phy(phy: i32) -> u16 {
    match phy {
        7 => 40,           // dot11_phy_type_ht
        8 | 10 | 11 => 80, // vht / he / eht — 160 exists but is not in the PHY id
        _ => 20,
    }
}

/// A BSS's share of a channel from its signal: -90 dBm → 0, -30 dBm → 1.
pub fn weight(rssi_dbm: i32) -> f32 {
    ((rssi_dbm + 90) as f32 / 60.0).clamp(0.0, 1.0)
}

/// One BSS's bell curve at a (possibly fractional) channel position: centred on
/// its channel, σ = half its width expressed in channels, scaled by [`weight`].
pub fn bss_load(b: &Bss, ch: f32) -> f32 {
    let sigma = (b.width_mhz as f32 / b.band.spacing_mhz() / 2.0).max(0.5);
    let d = ch - b.channel as f32;
    weight(b.rssi_dbm) * (-(d * d) / (2.0 * sigma * sigma)).exp()
}

/// The summed load of one band at a (possibly fractional) channel position.
pub fn load_at(band: Band, bss: &[Bss], ch: f32) -> f32 {
    bss.iter().filter(|b| b.band == band).map(|b| bss_load(b, ch)).sum()
}

/// Per-channel load of one band, over its whole chart channel set.
pub fn congestion(band: Band, bss: &[Bss]) -> Vec<(u16, f32)> {
    band.chart_channels().into_iter().map(|c| (c, load_at(band, bss, c as f32))).collect()
}

/// The least loaded of the band's usual channels; ties go to the lowest one, so
/// an empty band yields its first usual channel (2.4 GHz → 1).
pub fn best_channel(band: Band, bss: &[Bss]) -> u16 {
    let usual = band.usual_channels();
    congestion(band, bss)
        .into_iter()
        .filter(|(c, _)| usual.contains(c))
        .reduce(|a, b| if b.1 < a.1 - f32::EPSILON { b } else { a })
        .map(|(c, _)| c)
        .unwrap_or(1)
}

/// The strongest network on the air: its SSID and dBm.
pub fn best_network(bss: &[Bss]) -> Option<(String, i32)> {
    bss.iter().max_by_key(|b| b.rssi_dbm).map(|b| (b.ssid.clone(), b.rssi_dbm))
}

/// One entry per SSID (its strongest BSS), strongest network first.
pub fn networks(bss: &[Bss]) -> Vec<Bss> {
    let mut best: Vec<Bss> = Vec::new();
    for b in bss {
        match best.iter_mut().find(|x| x.ssid == b.ssid) {
            Some(x) => {
                let connected = x.connected || b.connected;
                if b.rssi_dbm > x.rssi_dbm {
                    *x = b.clone();
                }
                x.connected = connected;
            }
            None => best.push(b.clone()),
        }
    }
    best.sort_by(|a, b| b.rssi_dbm.cmp(&a.rssi_dbm).then_with(|| a.ssid.cmp(&b.ssid)));
    best
}

/// Which profile the generated XML should ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileAuth {
    Open,
    Wpa2Psk,
    Wpa3Sae,
    /// 802.1X / RADIUS. No passphrase exists for these — asking for one would
    /// invite a domain password into a PSK profile, so the tab refuses them.
    Enterprise,
}

/// Profile kind from what the scan reported for the network.
///
/// Only the names [`ffi::auth_name`] produces are matched, and the enterprise
/// algorithms (`WPA`, `WPA2`/RSNA, `WPA3-ENT` — the ones *without* `-PSK` or
/// `-SAE`) map to [`ProfileAuth::Enterprise`] rather than to a PSK profile.
pub fn profile_auth(secured: bool, auth: &str) -> ProfileAuth {
    match auth {
        _ if !secured => ProfileAuth::Open,
        "WPA-PSK" | "WPA2-PSK" => ProfileAuth::Wpa2Psk,
        "WPA3-SAE" => ProfileAuth::Wpa3Sae,
        "WPA" | "WPA2" | "WPA3" | "WPA3-ENT" => ProfileAuth::Enterprise,
        // "shared" (WEP), "WPA-None", "OWE", "unknown": no profile we can build.
        _ => ProfileAuth::Enterprise,
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// The raw SSID as text, if it is text at all: valid UTF-8 with no embedded NUL
/// (a NUL cannot survive the trip through a wide string, so it takes the hex
/// path too).
fn ssid_text(ssid_raw: &[u8]) -> Option<&str> {
    std::str::from_utf8(ssid_raw).ok().filter(|s| !s.contains('\0'))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        s.push_str(&format!("{b:02X}"));
        s
    })
}

/// The `<name>` of the generated profile — also the name `WlanConnect` asks for.
/// A non-text SSID is named by its hex, which keeps the name printable, unique
/// and free of characters the profile store would reject.
pub fn profile_name(ssid_raw: &[u8]) -> String {
    match ssid_text(ssid_raw) {
        Some(s) => s.to_string(),
        None => hex(ssid_raw),
    }
}

/// WLAN profile XML for `WlanSetProfile`. The passphrase exists only inside the
/// returned `String`, which the caller wipes with [`zero`] right after use.
///
/// The SSID goes in raw: `<name>` for text, `<hex>` for anything else. Going
/// through `from_utf8_lossy` would put U+FFFD in the profile and the radio would
/// then look for a network nobody is broadcasting.
pub fn profile_xml(ssid_raw: &[u8], auth: ProfileAuth, password: &str) -> String {
    let name = xml_escape(&profile_name(ssid_raw));
    let ssid_el = match ssid_text(ssid_raw) {
        Some(s) => format!("<name>{}</name>", xml_escape(s)),
        None => format!("<hex>{}</hex>", hex(ssid_raw)),
    };
    let (algo, cipher) = match auth {
        ProfileAuth::Open => ("open", "none"),
        ProfileAuth::Wpa2Psk => ("WPA2PSK", "AES"),
        ProfileAuth::Wpa3Sae => ("WPA3SAE", "AES"),
        // Unreachable in practice: `connect` refuses Enterprise before it gets
        // here. Named explicitly so a new variant is a compile error, not a
        // silently mis-generated profile.
        ProfileAuth::Enterprise => ("open", "none"),
    };
    let key = match auth {
        ProfileAuth::Wpa2Psk | ProfileAuth::Wpa3Sae => format!(
            "<sharedKey><keyType>passPhrase</keyType><protected>false</protected><keyMaterial>{}</keyMaterial></sharedKey>",
            xml_escape(password)
        ),
        _ => String::new(),
    };
    format!(
        "<?xml version=\"1.0\"?>\
<WLANProfile xmlns=\"http://www.microsoft.com/networking/WLAN/profile/v1\">\
<name>{name}</name>\
<SSIDConfig><SSID>{ssid_el}</SSID></SSIDConfig>\
<connectionType>ESS</connectionType><connectionMode>auto</connectionMode>\
<MSM><security>\
<authEncryption><authentication>{algo}</authentication><encryption>{cipher}</encryption><useOneX>false</useOneX></authEncryption>\
{key}</security></MSM></WLANProfile>"
    )
}

/// Overwrite a secret in place, then empty it, so the plaintext is gone before
/// the buffer is freed or reused.
///
/// The whole *allocation* is overwritten, not just the live `len()` bytes: a
/// `Backspace` only moves the length back, so the byte it "removed" is still
/// sitting in the buffer. Best effort — the compiler may have copied the string
/// elsewhere, and a `push` past the capacity already left a stale copy behind
/// (which is why the caller reserves the capacity up front).
pub fn zero(s: &mut String) {
    let cap = s.capacity();
    // SAFETY: NUL bytes are valid UTF-8, so the `String` stays well-formed;
    // `resize` up to the existing capacity cannot reallocate.
    let v = unsafe { s.as_mut_vec() };
    v.fill(0);
    v.resize(cap, 0);
    // Keep the writes from being optimised away as stores to a dead buffer.
    compiler_fence(Ordering::SeqCst);
    v.clear();
}

/// Test helper: the whole allocation behind `s`, including the bytes past its
/// length. Only sound to call on a buffer [`zero`] has initialised end to end.
#[cfg(test)]
#[allow(clippy::ptr_arg)] // `capacity()` is the whole point; `&str` cannot give it
pub fn raw_buffer(s: &String) -> &[u8] {
    // SAFETY: the caller guarantees all `capacity` bytes were written by `zero`.
    unsafe { std::slice::from_raw_parts(s.as_ptr(), s.capacity()) }
}

/// News from the scan thread.
#[derive(Clone, Debug)]
pub enum WifiEvent {
    /// A finished scan: every BSS the driver reported.
    Scan(Vec<Bss>),
    NoAdapter,
    Error(String),
    /// The profile was written and `WlanConnect` accepted the request. That is
    /// **not** a successful association: the handshake runs asynchronously and
    /// its result never comes back through this API. The next scan's `connected`
    /// flag is the only truth about whether the join worked.
    Joining(String),
    ConnectFailed(String),
}

/// Commands towards the scan thread. Deliberately **not** `Debug`: the connect
/// command carries a passphrase that must never reach a log or panic message.
pub enum WifiCmd {
    Rescan,
    Connect { ssid: String, ssid_raw: Vec<u8>, auth: ProfileAuth, password: String },
}

/// Start the scan thread; the returned sender takes [`WifiCmd`]s.
pub fn spawn(interval: Duration, tx: Sender<WifiEvent>) -> Sender<WifiCmd> {
    let (ctx, crx) = mpsc::channel();
    thread::spawn(move || run(interval, tx, crx));
    ctx
}

fn run(interval: Duration, tx: Sender<WifiEvent>, crx: Receiver<WifiCmd>) {
    let mut next = Instant::now();
    loop {
        if Instant::now() >= next {
            let ev = match ffi::Wlan::open() {
                None => WifiEvent::NoAdapter,
                Some(w) => match w.scan() {
                    Ok(list) => WifiEvent::Scan(list),
                    Err(e) => WifiEvent::Error(e),
                },
            };
            if tx.send(ev).is_err() {
                return;
            }
            next = Instant::now() + interval;
        }
        match crx.recv_timeout(Duration::from_millis(250)) {
            Ok(WifiCmd::Rescan) => next = Instant::now(),
            Ok(WifiCmd::Connect { ssid, ssid_raw, auth, mut password }) => {
                let ev = match ffi::Wlan::open() {
                    None => WifiEvent::NoAdapter,
                    Some(w) => match w.connect(&ssid_raw, auth, &password) {
                        Ok(()) => WifiEvent::Joining(ssid),
                        Err(e) => WifiEvent::ConnectFailed(e),
                    },
                };
                zero(&mut password);
                let _ = tx.send(ev);
                // The association is asynchronous and its result is not reported
                // back, so rescan soon: the `*` marker is what confirms the join.
                next = Instant::now() + Duration::from_secs(5);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---- WlanAPI (wlanapi.dll, raw-dylib — no import library needed) ----
mod ffi {
    use super::{channel_from_khz, width_from_phy, Bss, ProfileAuth, MAX_ITEMS, SCAN_WAIT};
    use std::collections::HashMap;
    use std::ptr::{null, null_mut};
    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::NetworkManagement::WiFi::*;

    /// `ERROR_SUCCESS`; every WlanAPI call returns a Win32 error code.
    const OK: u32 = 0;

    /// What the available-network list adds to a BSS: security and connectedness.
    type Security = (bool, String, bool);

    /// A buffer WlanAPI allocated for us; released in `Drop`.
    struct Mem<T>(*mut T);

    impl<T> Drop for Mem<T> {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WlanFreeMemory(self.0.cast()) }
            }
        }
    }

    /// An open WlanAPI client handle bound to the first wireless interface.
    pub struct Wlan {
        h: HANDLE,
        iface: GUID,
    }

    impl Drop for Wlan {
        fn drop(&mut self) {
            unsafe { WlanCloseHandle(self.h, null()) };
        }
    }

    impl Wlan {
        /// Opens the client handle and binds it to the first wireless interface.
        /// `None` means no WLAN service or no wireless adapter — not an error.
        pub fn open() -> Option<Self> {
            let (mut version, mut h): (u32, HANDLE) = (0, null_mut());
            if unsafe { WlanOpenHandle(WLAN_API_VERSION_2_0, null(), &mut version, &mut h) } != OK {
                return None;
            }
            // From here on `me`'s Drop closes the handle on every exit path.
            let mut me = Self { h, iface: GUID::from_u128(0) };
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
            if unsafe { WlanEnumInterfaces(h, null(), &mut list) } != OK {
                return None;
            }
            let _mem = Mem(list);
            let first = unsafe {
                let n = ((*list).dwNumberOfItems as usize).min(MAX_ITEMS);
                std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), n).first()?.InterfaceGuid
            };
            me.iface = first;
            Some(me)
        }

        /// Triggers a scan, waits for the driver, then merges the BSS list with
        /// the available-network list (which carries the security info).
        pub fn scan(&self) -> Result<Vec<Bss>, String> {
            // A refused scan is not fatal: the driver's cached BSS list is still
            // worth showing, so only a failing *list* call is an error.
            if unsafe { WlanScan(self.h, &self.iface, null(), null(), null()) } == OK {
                std::thread::sleep(SCAN_WAIT);
            }
            let secured = self.available()?;
            self.bss_list(&secured)
        }

        /// SSID → (secured, auth name, connected).
        fn available(&self) -> Result<HashMap<String, Security>, String> {
            let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = null_mut();
            let code = unsafe { WlanGetAvailableNetworkList(self.h, &self.iface, 0, null(), &mut list) };
            if code != OK {
                return Err(format!("network list 0x{code:08X}"));
            }
            let _mem = Mem(list);
            let mut out: HashMap<String, Security> = HashMap::new();
            unsafe {
                let n = ((*list).dwNumberOfItems as usize).min(MAX_ITEMS);
                for net in std::slice::from_raw_parts((*list).Network.as_ptr(), n) {
                    let ssid = String::from_utf8_lossy(&ssid_bytes(&net.dot11Ssid)).into_owned();
                    if ssid.is_empty() {
                        continue;
                    }
                    let connected = net.dwFlags & WLAN_AVAILABLE_NETWORK_CONNECTED != 0;
                    let entry = out.entry(ssid).or_insert_with(|| {
                        (net.bSecurityEnabled != 0, auth_name(net.dot11DefaultAuthAlgorithm).to_string(), false)
                    });
                    entry.2 |= connected;
                }
            }
            Ok(out)
        }

        fn bss_list(&self, secured: &HashMap<String, Security>) -> Result<Vec<Bss>, String> {
            let mut list: *mut WLAN_BSS_LIST = null_mut();
            let code = unsafe {
                WlanGetNetworkBssList(self.h, &self.iface, null(), dot11_BSS_type_infrastructure, 0, null(), &mut list)
            };
            if code != OK {
                return Err(format!("bss list 0x{code:08X}"));
            }
            let _mem = Mem(list);
            let mut out = Vec::new();
            unsafe {
                // The buffer's own size is the real bound; MAX_ITEMS only keeps
                // a nonsense `dwTotalSize` from producing a nonsense slice.
                let room = ((*list).dwTotalSize as usize)
                    .saturating_sub(std::mem::offset_of!(WLAN_BSS_LIST, wlanBssEntries))
                    / std::mem::size_of::<WLAN_BSS_ENTRY>();
                let n = ((*list).dwNumberOfItems as usize).min(room).min(MAX_ITEMS);
                for e in std::slice::from_raw_parts((*list).wlanBssEntries.as_ptr(), n) {
                    let ssid_raw = ssid_bytes(&e.dot11Ssid);
                    if ssid_raw.is_empty() {
                        continue; // hidden network: nothing useful to list
                    }
                    let ssid = String::from_utf8_lossy(&ssid_raw).into_owned();
                    let Some((band, channel)) = channel_from_khz(e.ulChCenterFrequency) else { continue };
                    let (sec, auth, connected) =
                        secured.get(&ssid).cloned().unwrap_or((false, "unknown".to_string(), false));
                    out.push(Bss {
                        ssid,
                        ssid_raw,
                        bssid: mac(&e.dot11Bssid),
                        rssi_dbm: e.lRssi,
                        freq_khz: e.ulChCenterFrequency,
                        channel,
                        band,
                        width_mhz: width_from_phy(e.dot11BssPhyType),
                        secured: sec,
                        auth,
                        connected,
                    });
                }
            }
            Ok(out)
        }

        /// Writes a generated profile, then asks the driver to connect to it.
        ///
        /// The passphrase only ever lives in local buffers, which are wiped
        /// before returning. `Ok(())` means the *request* was accepted — the
        /// association happens afterwards and is not reported here.
        pub fn connect(&self, ssid_raw: &[u8], auth: ProfileAuth, password: &str) -> Result<(), String> {
            if auth == ProfileAuth::Enterprise {
                return Err("enterprise networks are not supported".to_string());
            }
            let mut xml = super::profile_xml(ssid_raw, auth, password);
            let mut xml_w = to_wide(&xml);
            // The XML carries the key: gone from the heap before the API call.
            super::zero(&mut xml);
            let profile_w = to_wide(&super::profile_name(ssid_raw));
            let mut reason = 0u32;
            let code =
                unsafe { WlanSetProfile(self.h, &self.iface, 0, xml_w.as_ptr(), null(), 1, null(), &mut reason) };
            xml_w.fill(0);
            if code != OK {
                return Err(format!("profile 0x{code:08X} (reason {reason})"));
            }
            let params = WLAN_CONNECTION_PARAMETERS {
                wlanConnectionMode: wlan_connection_mode_profile,
                strProfile: profile_w.as_ptr(),
                pDot11Ssid: null_mut(),
                pDesiredBssidList: null_mut(),
                dot11BssType: dot11_BSS_type_infrastructure,
                dwFlags: 0,
            };
            let code = unsafe { WlanConnect(self.h, &self.iface, &params, null()) };
            if code != OK {
                return Err(format!("connect 0x{code:08X}"));
            }
            Ok(())
        }
    }

    /// The SSID exactly as reported: the length is clamped to the 32-byte array
    /// and trailing padding NULs are dropped. No lossy conversion happens here —
    /// an SSID that is not UTF-8 must survive intact as far as the profile XML.
    fn ssid_bytes(s: &DOT11_SSID) -> Vec<u8> {
        let n = (s.uSSIDLength as usize).min(s.ucSSID.len());
        let end = s.ucSSID[..n].iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
        s.ucSSID[..end].to_vec()
    }

    fn mac(b: &[u8; 6]) -> String {
        let mut out = String::with_capacity(17);
        for (i, x) in b.iter().enumerate() {
            if i > 0 {
                out.push(':');
            }
            out.push_str(&format!("{x:02x}"));
        }
        out
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn auth_name(a: DOT11_AUTH_ALGORITHM) -> &'static str {
        match a {
            DOT11_AUTH_ALGO_80211_OPEN => "open",
            DOT11_AUTH_ALGO_80211_SHARED_KEY => "shared",
            DOT11_AUTH_ALGO_WPA => "WPA",
            DOT11_AUTH_ALGO_WPA_PSK => "WPA-PSK",
            DOT11_AUTH_ALGO_WPA_NONE => "WPA-None",
            DOT11_AUTH_ALGO_RSNA => "WPA2",
            DOT11_AUTH_ALGO_RSNA_PSK => "WPA2-PSK",
            DOT11_AUTH_ALGO_WPA3 => "WPA3",
            DOT11_AUTH_ALGO_WPA3_SAE => "WPA3-SAE",
            DOT11_AUTH_ALGO_OWE => "OWE",
            DOT11_AUTH_ALGO_WPA3_ENT => "WPA3-ENT",
            _ => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bss(ssid: &str, ch: u16, band: Band, rssi: i32) -> Bss {
        Bss {
            ssid: ssid.to_string(),
            ssid_raw: ssid.as_bytes().to_vec(),
            bssid: "00:11:22:33:44:55".into(),
            rssi_dbm: rssi,
            freq_khz: 0,
            channel: ch,
            band,
            width_mhz: 20,
            secured: true,
            auth: "WPA2-PSK".into(),
            connected: false,
        }
    }

    #[test]
    fn channels_of_the_three_bands() {
        assert_eq!(channel_from_khz(2_412_000), Some((Band::G2_4, 1)));
        assert_eq!(channel_from_khz(2_437_000), Some((Band::G2_4, 6)));
        assert_eq!(channel_from_khz(2_462_000), Some((Band::G2_4, 11)));
        assert_eq!(channel_from_khz(2_484_000), Some((Band::G2_4, 14)));
        assert_eq!(channel_from_khz(5_180_000), Some((Band::G5, 36)));
        assert_eq!(channel_from_khz(5_745_000), Some((Band::G5, 149)));
        assert_eq!(channel_from_khz(5_955_000), Some((Band::G6, 1)));
        assert_eq!(channel_from_khz(5_180), Some((Band::G5, 36)), "a MHz-reporting driver is tolerated");
        assert_eq!(channel_from_khz(0), None);
        assert_eq!(channel_from_khz(3_000_000), None);
    }

    #[test]
    fn width_defaults_to_20_mhz() {
        assert_eq!(width_from_phy(7), 40);
        assert_eq!(width_from_phy(8), 80);
        assert_eq!(width_from_phy(11), 80);
        assert_eq!(width_from_phy(4), 20);
        assert_eq!(width_from_phy(-1), 20);
    }

    #[test]
    fn congestion_peaks_on_the_occupied_channel() {
        let aps = [bss("a", 6, Band::G2_4, -50)];
        let load = congestion(Band::G2_4, &aps);
        assert_eq!(load.len(), 14);
        let at = |c: u16| load.iter().find(|(ch, _)| *ch == c).unwrap().1;
        assert!(at(6) > at(4) && at(4) > at(1), "a bell curve around channel 6");
        assert!(at(1) < 0.1, "±5 channels away is nearly free");
        // A 20 MHz AP spans ±2 channels on 2.4 GHz (σ = 2), so ch 8 still counts.
        assert!(at(8) > 0.5 * at(6));
        assert!(congestion(Band::G5, &aps).iter().all(|(_, v)| *v == 0.0), "other bands stay empty");
    }

    #[test]
    fn best_channel_prefers_the_quiet_one() {
        assert_eq!(best_channel(Band::G2_4, &[]), 1, "empty air → the first usual channel");
        assert_eq!(best_channel(Band::G2_4, &[bss("a", 6, Band::G2_4, -40)]), 1, "1 and 11 tie, the lower wins");
        let crowded = [
            bss("a", 1, Band::G2_4, -40),
            bss("b", 6, Band::G2_4, -40),
            bss("c", 11, Band::G2_4, -75),
        ];
        assert_eq!(best_channel(Band::G2_4, &crowded), 11, "the lightest of 1/6/11");
        assert_eq!(best_channel(Band::G5, &[]), 36);
    }

    #[test]
    fn networks_dedupe_by_ssid_and_sort_by_signal() {
        let mut weak = bss("home", 1, Band::G2_4, -80);
        weak.connected = true;
        let all = [bss("far", 11, Band::G2_4, -85), weak, bss("home", 36, Band::G5, -45)];
        let n = networks(&all);
        assert_eq!(n.len(), 2);
        assert_eq!((n[0].ssid.as_str(), n[0].rssi_dbm, n[0].band), ("home", -45, Band::G5));
        assert!(n[0].connected, "the connected flag survives the merge");
        assert_eq!(n[1].ssid, "far");
        assert_eq!(best_network(&all), Some(("home".to_string(), -45)));
        assert_eq!(best_network(&[]), None);
    }

    #[test]
    fn profile_xml_escapes_ssid_and_passphrase() {
        let xml = profile_xml(b"A&B <net>", ProfileAuth::Wpa2Psk, "p\"a&ss<1>");
        assert!(xml.contains("<name>A&amp;B &lt;net&gt;</name>"));
        assert!(xml.contains("<keyMaterial>p&quot;a&amp;ss&lt;1&gt;</keyMaterial>"));
        assert!(xml.contains("<authentication>WPA2PSK</authentication>"));
        assert!(!xml.contains("A&B"), "the raw ampersand never reaches the XML");

        let sae = profile_xml(b"x", ProfileAuth::Wpa3Sae, "pw");
        assert!(sae.contains("<authentication>WPA3SAE</authentication>"));

        let open = profile_xml(b"x", ProfileAuth::Open, "");
        assert!(open.contains("<authentication>open</authentication>"));
        assert!(!open.contains("sharedKey"), "an open profile carries no key element");
    }

    #[test]
    fn a_non_utf8_ssid_goes_into_the_profile_as_hex() {
        let xml = profile_xml(&[0xE9, 0x41], ProfileAuth::Wpa2Psk, "pw");
        assert!(xml.contains("<SSID><hex>E941</hex></SSID>"), "{xml}");
        assert!(!xml.contains('\u{FFFD}'), "no replacement character reaches the profile");
        assert_eq!(profile_name(&[0xE9, 0x41]), "E941");
        // An embedded NUL cannot survive a wide string, so it takes the hex path.
        let nul = profile_xml(b"a\0b", ProfileAuth::Open, "");
        assert!(nul.contains("<hex>610062</hex>"), "{nul}");
        assert!(!nul.contains('\0'), "the XML never carries a NUL");
        // Plain text still uses <name>, and the profile is named after it.
        assert!(profile_xml(b"net", ProfileAuth::Open, "").contains("<SSID><name>net</name></SSID>"));
        assert_eq!(profile_name(b"net"), "net");
    }

    #[test]
    fn profile_auth_from_the_scanned_algorithm() {
        assert_eq!(profile_auth(false, "open"), ProfileAuth::Open);
        assert_eq!(profile_auth(true, "WPA-PSK"), ProfileAuth::Wpa2Psk);
        assert_eq!(profile_auth(true, "WPA2-PSK"), ProfileAuth::Wpa2Psk);
        assert_eq!(profile_auth(true, "WPA3-SAE"), ProfileAuth::Wpa3Sae);
        assert_eq!(profile_auth(false, "WPA3-SAE"), ProfileAuth::Open, "not secured wins");
        // The 802.1X algorithms must never become a PSK profile.
        for ent in ["WPA", "WPA2", "WPA3", "WPA3-ENT"] {
            assert_eq!(profile_auth(true, ent), ProfileAuth::Enterprise, "{ent} is not a PSK network");
        }
        // Nor anything we cannot build a profile for at all.
        for odd in ["shared", "WPA-None", "OWE", "unknown"] {
            assert_eq!(profile_auth(true, odd), ProfileAuth::Enterprise, "{odd} has no PSK profile");
        }
    }

    #[test]
    fn zero_wipes_the_whole_allocation_not_just_the_live_bytes() {
        let mut s = String::with_capacity(32);
        s.push_str("hunter2");
        s.pop(); // the '2' is above `len` now, but still in the buffer
        let cap = s.capacity();
        zero(&mut s);
        assert!(s.is_empty());
        assert_eq!(s.capacity(), cap, "zeroing must not reallocate");
        assert!(raw_buffer(&s).iter().all(|b| *b == 0), "even the bytes past len are gone");
        assert_eq!(raw_buffer(&s).len(), cap);
    }
}
