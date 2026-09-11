//! STAT forrás: rendszer-pillanatképek 2 másodpercenként egy saját szálon.
use serde::Deserialize;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use sysinfo::{Disks, Networks, ProcessesToUpdate, System};

const INTERVAL: Duration = Duration::from_secs(2);

/// Sparkline-előzmény hossza (minta).
pub const HIST: usize = 60;

#[derive(Clone, Debug, Default)]
pub struct DiskInfo {
    pub mount: String,
    pub used: u64,
    pub total: u64,
    pub read_bps: u64,
    pub write_bps: u64,
}

#[derive(Clone, Debug, Default)]
pub struct GpuInfo {
    pub util: f32,
    pub mem_used: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BatteryInfo {
    pub pct: u8,
    pub charging: bool,
}

#[derive(Clone, Debug, Default)]
pub struct StatSnapshot {
    pub cpu_brand: String,
    pub cpu_total: f32,
    pub cpu_cores: Vec<f32>,
    pub cpu_mhz: u64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub disks: Vec<DiskInfo>,
    pub net_iface: String,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub uptime_s: u64,
    pub procs: usize,
    pub top: Vec<(String, f32)>,
    pub gpu: Option<GpuInfo>,
    pub battery: Option<BatteryInfo>,
}

/// A GPU (WMI) lekérés újrapróbálása 30 pollonként (~1 perc), ha az indulásnál nem sikerült.
pub fn should_retry_gpu(poll: u32) -> bool { poll % 30 == 0 }

pub fn spawn(tx: Sender<StatSnapshot>) {
    thread::spawn(move || {
        let mut sys = System::new_all();
        let mut disks = Disks::new_with_refreshed_list();
        let mut nets = Networks::new_with_refreshed_list();
        let bat = starship_battery::Manager::new().ok();

        let gpu_latest: Arc<Mutex<Option<GpuInfo>>> = Arc::new(Mutex::new(None));
        {
            let gpu_latest = gpu_latest.clone();
            thread::spawn(move || {
                // A WMI kapcsolat !Send, ezért ezen a szálon él; az első formatted lekérés lassú (~5 s), ez itt nem fogja vissza a STAT-ot.
                let mut gpu = Gpu::new();
                // poll 1-ről indul: az első Gpu::new() már megtörtént, újrapróbálás 30 pollonként
                let mut poll = 1u32;
                loop {
                    if gpu.is_none() && should_retry_gpu(poll) {
                        gpu = Gpu::new();
                    }
                    poll = poll.wrapping_add(1);
                    let v = gpu.as_ref().and_then(Gpu::read);
                    *gpu_latest.lock().unwrap_or_else(|e| e.into_inner()) = v;
                    thread::sleep(INTERVAL);
                }
            });
        }

        // Azonnali első pillanatkép: ne várjunk INTERVAL-t az induláskor.
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        sys.refresh_cpu_usage();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        disks.refresh(true);
        nets.refresh(true);
        let snap = snapshot(&sys, &disks, &nets, None, battery(bat.as_ref()));
        if tx.send(snap).is_err() {
            return;
        }

        loop {
            thread::sleep(INTERVAL);
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            sys.refresh_processes(ProcessesToUpdate::All, true);
            disks.refresh(true);
            nets.refresh(true);
            let gpu = gpu_latest.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let snap = snapshot(&sys, &disks, &nets, gpu, battery(bat.as_ref()));
            if tx.send(snap).is_err() {
                return;
            }
        }
    });
}

fn snapshot(sys: &System, disks: &Disks, nets: &Networks, gpu: Option<GpuInfo>, battery: Option<BatteryInfo>) -> StatSnapshot {
    let secs = INTERVAL.as_secs();
    let ncpu = sys.cpus().len().max(1) as f32;
    let mut top: Vec<(String, f32)> = sys
        .processes()
        .values()
        .map(|p| (p.name().to_string_lossy().trim_end_matches(".exe").to_string(), p.cpu_usage() / ncpu))
        .collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    top.truncate(3);

    let (net_iface, rx_bps, tx_bps) = nets
        .iter()
        .max_by_key(|(_, d)| d.received() + d.transmitted())
        .map(|(n, d)| (n.clone(), d.received() / secs, d.transmitted() / secs))
        .unwrap_or_default();

    StatSnapshot {
        cpu_brand: sys.cpus().first().map(|c| c.brand().to_string()).unwrap_or_default(),
        cpu_total: sys.global_cpu_usage(),
        cpu_cores: sys.cpus().iter().map(|c| c.cpu_usage()).collect(),
        cpu_mhz: sys.cpus().first().map(|c| c.frequency()).unwrap_or(0),
        mem_used: sys.used_memory(),
        mem_total: sys.total_memory(),
        swap_used: sys.used_swap(),
        swap_total: sys.total_swap(),
        disks: disks
            .iter()
            .filter(|d| d.total_space() > 0)
            .map(|d| DiskInfo {
                mount: d.mount_point().to_string_lossy().trim_end_matches('\\').to_string(),
                used: d.total_space().saturating_sub(d.available_space()),
                total: d.total_space(),
                read_bps: d.usage().read_bytes / secs,
                write_bps: d.usage().written_bytes / secs,
            })
            .collect(),
        net_iface,
        rx_bps,
        tx_bps,
        uptime_s: System::uptime(),
        procs: sys.processes().len(),
        top,
        gpu,
        battery,
    }
}

pub fn fmt_uptime(s: u64) -> String {
    format!("{}d {:02}:{:02}", s / 86400, s / 3600 % 24, s / 60 % 60)
}

// --- GPU: Windows GPU perf-számlálók WMI-n át (Intel Arc / bármely WDDM adapter) ---

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Engine {
    pub name: String,
    pub utilization_percentage: u64,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct AdapterMem {
    dedicated_usage: u64,
    shared_usage: u64,
}

/// A 3D motorok kihasználtságának összege (pid-enként jön), 100-ra vágva.
pub fn gpu_util(eng: &[Engine]) -> f32 {
    eng.iter()
        .filter(|e| e.name.contains("engtype_3D"))
        .map(|e| e.utilization_percentage)
        .sum::<u64>()
        .min(100) as f32
}

struct Gpu {
    con: wmi::WMIConnection,
}

impl Gpu {
    fn new() -> Option<Self> {
        wmi::WMIConnection::new().ok().map(|con| Self { con })
    }

    fn read(&self) -> Option<GpuInfo> {
        let eng: Vec<Engine> = self
            .con
            .raw_query("SELECT Name, UtilizationPercentage FROM Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine")
            .ok()?;
        let mem: Vec<AdapterMem> = self
            .con
            .raw_query("SELECT DedicatedUsage, SharedUsage FROM Win32_PerfFormattedData_GPUPerformanceCounters_GPUAdapterMemory")
            .ok()?;
        // Integrált GPU-n a dedikált 0, ilyenkor a megosztott memória a használat.
        let mem_used = mem
            .iter()
            .map(|m| if m.dedicated_usage > 0 { m.dedicated_usage } else { m.shared_usage })
            .max()
            .unwrap_or(0);
        Some(GpuInfo { util: gpu_util(&eng), mem_used })
    }
}

fn battery(m: Option<&starship_battery::Manager>) -> Option<BatteryInfo> {
    let b = m?.batteries().ok()?.next()?.ok()?;
    let pct = b.state_of_charge().get::<starship_battery::units::ratio::percent>();
    let charging = matches!(b.state(), starship_battery::State::Charging | starship_battery::State::Full);
    Some(BatteryInfo { pct: pct.round().clamp(0.0, 100.0) as u8, charging })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_format() {
        assert_eq!(fmt_uptime(3 * 86400 + 4 * 3600 + 12 * 60 + 5), "3d 04:12");
        assert_eq!(fmt_uptime(59), "0d 00:00");
    }

    #[test]
    fn gpu_engine_sum_only_3d() {
        let eng = vec![
            Engine { name: "pid_1_luid_0_phys_0_eng_0_engtype_3D".into(), utilization_percentage: 9 },
            Engine { name: "pid_2_luid_0_phys_0_eng_0_engtype_3D".into(), utilization_percentage: 4 },
            Engine { name: "pid_2_luid_0_phys_0_eng_1_engtype_Copy".into(), utilization_percentage: 50 },
        ];
        assert_eq!(gpu_util(&eng), 13.0);
    }

    #[test]
    fn gpu_retry_every_30_polls() {
        assert!(should_retry_gpu(0));
        assert!(!should_retry_gpu(1));
        assert!(should_retry_gpu(30));
        assert!(should_retry_gpu(60));
    }
}


// ---------------------------------------------------------------------------
// S.P.E.C.I.A.L. — pure mapping of machine facts onto a 1–10 character sheet.
//
// Every attribute is a one-line formula clamped to 1–10, and every unknown
// input stays `None` so the view prints `?` instead of a made-up number.
//   S  cores × GHz, log2-scaled:  2c×2GHz → 3, 16c×4GHz → 10
//   P  Wi-Fi networks in range (+1 when connected), 0.6 points each
//   E  uptime (a point per 8 h), averaged with the battery % when there is one
//   C  favourite tracks saved, 0.45 points each: 0 → 1, ≥ 20 → 10
//   I  RAM GB / 2, plus 2 points for a discrete GPU
//   A  free CPU % spread over 1–10, minus a point per 200 processes
//   L  deterministic hash of the day (YYYYMMDD) and the weather code
// ---------------------------------------------------------------------------

/// Letter and full name of the seven attributes, in S.P.E.C.I.A.L. order.
pub const ATTRS: [(char, &str); 7] = [
    ('S', "STRENGTH"),
    ('P', "PERCEPTION"),
    ('E', "ENDURANCE"),
    ('C', "CHARISMA"),
    ('I', "INTELLIGENCE"),
    ('A', "AGILITY"),
    ('L', "LUCK"),
];

/// A perk the machine's state has earned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perk {
    pub name: &'static str,
    pub flavor: &'static str,
}

/// Everything the sheet derives from; `None` (or a missing snapshot) → `?`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpecialInput<'a> {
    pub stat: Option<&'a StatSnapshot>,
    /// (networks in range, connected) from the WIFI blackboard entry.
    pub wifi: Option<(usize, bool)>,
    /// WMO weather code from the WEATHER blackboard entry.
    pub weather: Option<u8>,
    /// Count of tracks in the `Favorite tracks` note of `notes.md`.
    pub favorites: Option<usize>,
    /// Local hour 0–23 (Night Person).
    pub hour: u32,
    /// Local date as YYYYMMDD (Luck).
    pub ymd: i32,
}

/// The rendered character sheet: seven attributes (`None` = unknown) and perks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Special {
    pub attrs: [Option<u8>; 7],
    pub perks: Vec<Perk>,
    /// Uptime in days + 1.
    pub level: u64,
    /// Battery percentage, 100 without a battery.
    pub hp: u8,
}

fn clamp10(v: f32) -> u8 {
    if v.is_nan() {
        return 1;
    }
    v.round().clamp(1.0, 10.0) as u8
}

/// STRENGTH: cores × GHz on a log2 scale — 1 → 1, 2c×2 GHz → 3, 16c×4 GHz → 10.
pub fn strength(cores: usize, mhz: u64) -> u8 {
    let power = cores as f32 * (mhz as f32 / 1000.0);
    clamp10(1.75 * power.max(1.0).log2() - 0.5)
}

/// PERCEPTION: how much of the ether is visible — 0.6 per Wi-Fi network, the connected one counts twice.
pub fn perception(networks: usize, connected: bool) -> u8 {
    clamp10(1.0 + (networks.saturating_add(connected as usize)) as f32 * 0.6)
}

/// ENDURANCE: a point per 8 h of uptime, averaged with the battery charge (1–10) when there is a battery.
pub fn endurance(uptime_s: u64, battery_pct: Option<u8>) -> u8 {
    let up = 1.0 + (uptime_s as f32 / 3600.0 / 8.0).min(9.0);
    match battery_pct {
        Some(p) => clamp10((up + 1.0 + p.min(100) as f32 * 0.09) / 2.0),
        None => clamp10(up),
    }
}

/// CHARISMA: saved favourite tracks — 0 → 1, 20 or more → 10.
pub fn charisma(favorites: usize) -> u8 {
    clamp10(1.0 + favorites as f32 * 0.45)
}

/// INTELLIGENCE: half a point per RAM GB, plus 2 for a discrete GPU.
pub fn intelligence(mem_total: u64, gpu: bool) -> u8 {
    let gb = mem_total as f32 / 1024.0 / 1024.0 / 1024.0;
    clamp10(gb * 0.5 + if gpu { 2.0 } else { 0.0 })
}

/// AGILITY: free CPU % spread over 1–10, minus a point per 200 processes.
pub fn agility(cpu_total: f32, procs: usize) -> u8 {
    let free = (100.0 - cpu_total.clamp(0.0, 100.0)) / 100.0;
    clamp10(1.0 + free * 9.0 - procs as f32 / 200.0)
}

/// LUCK: a deterministic hash of the day (YYYYMMDD) and the weather code — same day, same sky, same luck.
pub fn luck(ymd: i32, weather_code: u8) -> u8 {
    let mut h = (ymd as u32).wrapping_mul(2654435761) ^ (weather_code as u32).wrapping_mul(40503);
    h ^= h >> 13;
    h = h.wrapping_mul(1274126177);
    ((h ^ (h >> 16)) % 10) as u8 + 1
}

/// The perks the current state has earned, in a stable order.
pub fn perks(i: &SpecialInput) -> Vec<Perk> {
    let mut out = Vec::new();
    let mut add = |name, flavor| out.push(Perk { name, flavor });
    if i.hour >= 22 || i.hour < 6 {
        add("Night Person", "sharper after dark, like everyone worth knowing");
    }
    if i.stat.and_then(|s| s.battery.as_ref()).is_some_and(|b| b.pct > 80) {
        add("Rad Resistant", "cells topped up: the rads bounce off");
    }
    if i.stat.is_some_and(|s| s.disks.iter().any(|d| d.total > 0 && d.used as f32 / d.total as f32 > 0.90)) {
        add("Scrounger", "you never throw anything away. Ever.");
    }
    if i.favorites.is_some_and(|f| f >= 10) {
        add("Cap Collector", "a tin full of favourite tracks");
    }
    if i.stat.is_some_and(|s| s.uptime_s > 7 * 86_400) {
        add("Ghoulish", "still running long after the sirens stopped");
    }
    if i.wifi.is_some_and(|(n, _)| n == 0) {
        add("Lone Wanderer", "no networks in range: the wasteland is yours");
    }
    if matches!(i.weather, Some(0 | 1)) {
        add("Sunny Disposition", "clear skies, and it shows");
    }
    out
}

impl Special {
    /// Build the sheet; a missing source leaves its attributes `None`.
    pub fn new(i: &SpecialInput) -> Self {
        let s = i.stat;
        let bat = s.and_then(|s| s.battery.as_ref());
        Self {
            attrs: [
                s.map(|s| strength(s.cpu_cores.len(), s.cpu_mhz)),
                i.wifi.map(|(n, c)| perception(n, c)),
                s.map(|s| endurance(s.uptime_s, bat.map(|b| b.pct))),
                i.favorites.map(charisma),
                s.map(|s| intelligence(s.mem_total, s.gpu.is_some())),
                s.map(|s| agility(s.cpu_total, s.procs)),
                i.weather.map(|c| luck(i.ymd, c)),
            ],
            perks: perks(i),
            level: s.map_or(0, |s| s.uptime_s / 86_400) + 1,
            hp: bat.map_or(100, |b| b.pct),
        }
    }
}

#[cfg(test)]
mod special_tests {
    use super::*;

    #[test]
    fn attribute_bounds_and_anchors() {
        assert_eq!(strength(0, 0), 1);
        assert_eq!(strength(2, 2000), 3);
        assert_eq!(strength(16, 4000), 10);
        assert_eq!(strength(256, 5000), 10);

        assert_eq!(perception(0, false), 1);
        assert_eq!(perception(0, true), 2);
        assert_eq!(perception(9999, true), 10);

        assert_eq!(endurance(0, None), 1);
        assert_eq!(endurance(u64::MAX / 2, None), 10);
        assert_eq!(endurance(0, Some(0)), 1);
        assert_eq!(endurance(u64::MAX / 2, Some(100)), 10);

        assert_eq!(charisma(0), 1);
        assert_eq!(charisma(20), 10);
        assert_eq!(charisma(usize::MAX), 10);

        assert_eq!(intelligence(0, false), 1);
        assert_eq!(intelligence(0, true), 2);
        assert_eq!(intelligence(u64::MAX, true), 10);

        assert_eq!(agility(0.0, 0), 10);
        assert_eq!(agility(100.0, usize::MAX), 1);
        assert_eq!(agility(f32::NAN, 0), 1, "NaN CPU is a 1, not a panic");
    }

    #[test]
    fn luck_is_deterministic_per_day_and_sky() {
        assert_eq!(luck(20260910, 3), luck(20260910, 3));
        assert_ne!(luck(20260910, 3), luck(20260911, 3));
        for d in 0..400 {
            let v = luck(20260101 + d, (d % 100) as u8);
            assert!((1..=10).contains(&v), "{v} out of range");
        }
    }

    fn snap() -> StatSnapshot {
        StatSnapshot { cpu_cores: vec![0.0; 8], cpu_mhz: 3000, mem_total: 16 << 30, ..Default::default() }
    }

    #[test]
    fn missing_data_is_unknown_not_zero() {
        let sp = Special::new(&SpecialInput::default());
        assert_eq!(sp.attrs, [None; 7], "no snapshot, wifi, weather or favourites → all ?");
        assert_eq!((sp.level, sp.hp), (1, 100));

        let s = snap();
        let sp = Special::new(&SpecialInput { stat: Some(&s), ..Default::default() });
        assert!(sp.attrs[0].is_some(), "the snapshot feeds STRENGTH");
        assert!(sp.attrs[1].is_none() && sp.attrs[3].is_none() && sp.attrs[6].is_none(), "the other sources are still ?");
    }

    #[test]
    fn every_perk_rule_fires_only_on_its_own_condition() {
        let none = SpecialInput { hour: 12, ..Default::default() };
        assert!(perks(&none).is_empty());
        let names = |i: &SpecialInput| perks(i).iter().map(|p| p.name).collect::<Vec<_>>();

        assert_eq!(names(&SpecialInput { hour: 23, ..none }), ["Night Person"]);
        assert_eq!(names(&SpecialInput { hour: 5, ..none }), ["Night Person"]);
        assert!(perks(&SpecialInput { hour: 21, ..none }).is_empty());
        assert!(perks(&SpecialInput { hour: 6, ..none }).is_empty());

        let mut s = snap();
        s.battery = Some(BatteryInfo { pct: 81, charging: false });
        assert_eq!(names(&SpecialInput { stat: Some(&s), ..none }), ["Rad Resistant"]);
        s.battery = Some(BatteryInfo { pct: 80, charging: false });
        assert!(perks(&SpecialInput { stat: Some(&s), ..none }).is_empty());

        let mut s = snap();
        s.disks = vec![DiskInfo { used: 91, total: 100, ..Default::default() }];
        assert_eq!(names(&SpecialInput { stat: Some(&s), ..none }), ["Scrounger"]);
        s.disks = vec![DiskInfo { used: 90, total: 100, ..Default::default() }];
        assert!(perks(&SpecialInput { stat: Some(&s), ..none }).is_empty());
        s.disks = vec![DiskInfo::default()];
        assert!(perks(&SpecialInput { stat: Some(&s), ..none }).is_empty(), "an empty disk is not 100% full");

        assert_eq!(names(&SpecialInput { favorites: Some(10), ..none }), ["Cap Collector"]);
        assert!(perks(&SpecialInput { favorites: Some(9), ..none }).is_empty());

        let mut s = snap();
        s.uptime_s = 7 * 86_400 + 1;
        assert_eq!(names(&SpecialInput { stat: Some(&s), ..none }), ["Ghoulish"]);
        s.uptime_s = 7 * 86_400;
        assert!(perks(&SpecialInput { stat: Some(&s), ..none }).is_empty());

        assert_eq!(names(&SpecialInput { wifi: Some((0, false)), ..none }), ["Lone Wanderer"]);
        assert!(perks(&SpecialInput { wifi: Some((1, true)), ..none }).is_empty());

        assert_eq!(names(&SpecialInput { weather: Some(0), ..none }), ["Sunny Disposition"]);
        assert_eq!(names(&SpecialInput { weather: Some(1), ..none }), ["Sunny Disposition"]);
        assert!(perks(&SpecialInput { weather: Some(2), ..none }).is_empty());
    }

    #[test]
    fn level_is_uptime_days_and_hp_is_the_battery() {
        let mut s = snap();
        s.uptime_s = 3 * 86_400;
        s.battery = Some(BatteryInfo { pct: 42, charging: false });
        let sp = Special::new(&SpecialInput { stat: Some(&s), ..Default::default() });
        assert_eq!((sp.level, sp.hp), (4, 42));
    }
}
