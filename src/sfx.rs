//! Szintetizált hangeffektek az időzítő lejáratához (nincs hangfájl).
use rodio::{ChannelCount, Sample, SampleRate, Source};
use std::time::Duration;

pub const SAMPLE_RATE: u32 = 44_100;

fn sr() -> SampleRate { SampleRate::new(SAMPLE_RATE).expect("sample rate") }
fn mono() -> ChannelCount { ChannelCount::new(1).expect("channels") }

/// Geiger-kattogás: rövid zajimpulzusok, gyakoriságuk `rate_start`→`rate_end` Hz-ig lineárisan nő.
pub struct Clicks { rate_start: f32, rate_end: f32, total: u64, pos: u64, burst_left: u32, seed: u32 }

impl Clicks {
    pub fn new(rate_start: f32, rate_end: f32, dur: Duration) -> Self {
        Self { rate_start, rate_end, total: (dur.as_secs_f32() * SAMPLE_RATE as f32) as u64, pos: 0, burst_left: 0, seed: 0x9E3779B9 }
    }
    fn rnd(&mut self) -> f32 {
        self.seed ^= self.seed << 13; self.seed ^= self.seed >> 17; self.seed ^= self.seed << 5;
        (self.seed as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

impl Iterator for Clicks {
    type Item = Sample;
    fn next(&mut self) -> Option<Sample> {
        if self.pos >= self.total { return None; }
        let p = self.pos as f32 / self.total as f32;
        let rate = self.rate_start + (self.rate_end - self.rate_start) * p;
        self.pos += 1;
        if self.burst_left == 0 && self.rnd().abs() < rate / SAMPLE_RATE as f32 {
            self.burst_left = (SAMPLE_RATE / 500) as u32; // 2 ms
        }
        if self.burst_left > 0 {
            self.burst_left -= 1;
            Some(self.rnd() * 0.6)
        } else {
            Some(0.0)
        }
    }
}

impl Source for Clicks {
    fn current_span_len(&self) -> Option<usize> { None }
    fn channels(&self) -> ChannelCount { mono() }
    fn sample_rate(&self) -> SampleRate { sr() }
    fn total_duration(&self) -> Option<Duration> { Some(Duration::from_secs_f32(self.total as f32 / SAMPLE_RATE as f32)) }
}

/// Kétütemű sziréna: 660/880 Hz, 0,25 s váltással.
pub struct Siren { total: u64, pos: u64 }

impl Siren {
    pub fn new(dur: Duration) -> Self { Self { total: (dur.as_secs_f32() * SAMPLE_RATE as f32) as u64, pos: 0 } }
}

impl Iterator for Siren {
    type Item = Sample;
    fn next(&mut self) -> Option<Sample> {
        if self.pos >= self.total { return None; }
        let t = self.pos as f32 / SAMPLE_RATE as f32;
        self.pos += 1;
        let f = if ((t * 4.0) as u32) % 2 == 0 { 660.0 } else { 880.0 };
        Some((2.0 * std::f32::consts::PI * f * t).sin() * 0.4)
    }
}

impl Source for Siren {
    fn current_span_len(&self) -> Option<usize> { None }
    fn channels(&self) -> ChannelCount { mono() }
    fn sample_rate(&self) -> SampleRate { sr() }
    fn total_duration(&self) -> Option<Duration> { Some(Duration::from_secs_f32(self.total as f32 / SAMPLE_RATE as f32)) }
}

/// A négy fázis hangja egymás után: 2,4 s gyorsuló kattogás, 1,6 s sziréna+kattogás, 26 s halk kattogás.
pub fn alarm_sequence() -> Vec<Box<dyn Source + Send>> {
    vec![
        Box::new(Clicks::new(3.0, 25.0, Duration::from_millis(2400))),
        Box::new(Siren::new(Duration::from_millis(1600)).mix(Clicks::new(25.0, 25.0, Duration::from_millis(1600)))),
        Box::new(Clicks::new(8.0, 8.0, Duration::from_secs(26)).amplify(0.5)),
    ]
}

/// Geiger-kitörés: 1,5 s sűrű, véletlen kattogás (DOSIMETER túladagolás).
pub fn geiger_burst() -> Vec<Box<dyn Source + Send>> {
    vec![Box::new(Clicks::new(18.0, 40.0, Duration::from_millis(1500)))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_have_energy_and_length() {
        let c = Clicks::new(20.0, 20.0, Duration::from_secs(1));
        let samples: Vec<f32> = c.collect();
        assert_eq!(samples.len(), SAMPLE_RATE as usize);
        let nonzero = samples.iter().filter(|s| s.abs() > 0.01).count();
        assert!(nonzero > 900 && nonzero < 2800, "nonzero={nonzero}");
    }

    #[test]
    fn siren_alternates_and_ends() {
        let s = Siren::new(Duration::from_millis(500));
        let v: Vec<f32> = s.collect();
        assert_eq!(v.len(), SAMPLE_RATE as usize / 2);
        assert!(v.iter().any(|x| *x > 0.3) && v.iter().any(|x| *x < -0.3));
    }

    #[test]
    fn alarm_sequence_is_about_30s() {
        let total: f32 = alarm_sequence().iter().map(|s| s.total_duration().map(|d| d.as_secs_f32()).unwrap_or(0.0)).sum();
        assert!((28.0..=31.0).contains(&total), "total={total} (a Mix total_duration-je lehet None)");
    }
}
