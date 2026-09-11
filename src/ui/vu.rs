//! Közös VU-csap: a dekódolt mintákon átengedő `Source`, ami RMS-szintet és
//! FFT-spektrumot küld a modul saját eseménycsatornájára.
//!
//! A RADIO és a MUSIC ugyanezt használja; a két modul nem hivatkozik egymásra,
//! csak erre a forrásrétegre. A rajzolás a `widgets::{vu_bar, spectrum}`-é.

use rodio::{ChannelCount, Sample, SampleRate, Source};
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// Spektrum-sávok száma.
pub const BANDS: usize = 16;
pub const FFT_N: usize = 1024;
/// Ennyi mintánként megy ki egy RMS-szint.
pub const VU_BLOCK: usize = 4096;

const MIN_HZ: f32 = 50.0;
const MAX_HZ: f32 = 14_000.0;
const DB_FLOOR: f32 = -60.0;

/// 1024 mintás Hann-ablakos FFT → 16 logaritmikus sáv 0–100 (dB-skála, -60 dB = 0).
pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    buf: Vec<f32>,
    scratch: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    edges: [usize; BANDS + 1],
    skip: u8,
}

impl Analyzer {
    pub fn new(sample_rate: u32) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_N);
        let window = (0..FFT_N).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_N as f32).cos()).collect();
        let mut edges = [0usize; BANDS + 1];
        let mut prev = 0usize;
        for (k, e) in edges.iter_mut().enumerate() {
            let hz = MIN_HZ * (MAX_HZ / MIN_HZ).powf(k as f32 / BANDS as f32);
            let computed = ((hz * FFT_N as f32 / sample_rate as f32) as usize).clamp(1, FFT_N / 2 - 1);
            *e = computed.max(prev + 1).min(FFT_N / 2 - 1);
            prev = *e;
        }
        let fft_scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        Self { fft, window, buf: Vec::with_capacity(FFT_N), scratch: vec![Complex::new(0.0, 0.0); FFT_N], fft_scratch, edges, skip: 0 }
    }

    /// Egy mono minta; ha összegyűlt egy blokk (minden 2. blokknál (~21/s 44,1 kHz-en) számol), a sávok.
    pub fn push(&mut self, s: f32) -> Option<[u8; BANDS]> {
        self.buf.push(s);
        if self.buf.len() < FFT_N {
            return None;
        }
        let compute = self.skip == 0;
        self.skip = (self.skip + 1) % 2;
        if !compute {
            self.buf.clear();
            return None;
        }
        for (i, c) in self.scratch.iter_mut().enumerate() {
            *c = Complex::new(self.buf[i] * self.window[i], 0.0);
        }
        self.buf.clear();
        self.fft.process_with_scratch(&mut self.scratch, &mut self.fft_scratch);
        let mut out = [0u8; BANDS];
        for k in 0..BANDS {
            let (a, b) = (self.edges[k], self.edges[k + 1].max(self.edges[k] + 1));
            let peak = self.scratch[a..b].iter().map(|c| c.norm()).fold(0.0f32, f32::max);
            let db = 20.0 * (peak / (FFT_N as f32 / 4.0)).max(1e-9).log10();
            out[k] = ((db - DB_FLOOR) / -DB_FLOOR * 100.0).clamp(0.0, 100.0) as u8;
        }
        Some(out)
    }
}

/// Átengedi a mintákat, és `block` mintánként elküldi az RMS-t; a bal csatornát
/// emellett egy FFT-elemzőbe vezeti. Az eseménytípus a hívóé: a két
/// konstruktor-függvény (pl. `RadioEvent::Level`) csomagolja be az értékeket.
pub struct Vu<S, E: 'static> {
    inner: S,
    tx: Sender<E>,
    block: usize,
    acc: f32,
    n: usize,
    analyzer: Analyzer,
    ch: u64,
    idx: u64,
    mk_level: fn(f32) -> E,
    mk_spectrum: fn([u8; BANDS]) -> E,
}

impl<S: Source, E> Vu<S, E> {
    pub fn new(inner: S, tx: Sender<E>, block: usize, mk_level: fn(f32) -> E, mk_spectrum: fn([u8; BANDS]) -> E) -> Self {
        let ch = inner.channels().get() as u64;
        let sample_rate = inner.sample_rate().get();
        Self {
            inner,
            tx,
            block,
            acc: 0.0,
            n: 0,
            analyzer: Analyzer::new(sample_rate),
            ch: ch.max(1),
            idx: 0,
            mk_level,
            mk_spectrum,
        }
    }
}

impl<S: Source, E> Iterator for Vu<S, E> {
    type Item = Sample;
    fn next(&mut self) -> Option<Sample> {
        let s = self.inner.next()?;
        self.acc += s * s;
        self.n += 1;
        if self.n >= self.block {
            let rms = (self.acc / self.n as f32).sqrt();
            let _ = self.tx.send((self.mk_level)(rms));
            self.acc = 0.0;
            self.n = 0;
        }
        if self.idx % self.ch == 0 {
            if let Some(b) = self.analyzer.push(s) {
                // ponytail: std mpsc send a hangszálon (~43/s) és az FFT egy next()-ben (~30 µs);
                // ha valaha akad, ring-buffer + külön elemző szál.
                let _ = self.tx.send((self.mk_spectrum)(b));
            }
        }
        self.idx += 1;
        Some(s)
    }
}

impl<S: Source, E> Source for Vu<S, E> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[derive(Debug, PartialEq)]
    enum Ev {
        Level(f32),
        Spectrum([u8; BANDS]),
    }

    #[test]
    fn vu_emits_rms_per_block() {
        let (tx, rx) = mpsc::channel();
        let src = rodio::source::SineWave::new(440.0);
        let mut vu = Vu::new(src, tx, 1000, Ev::Level, Ev::Spectrum);
        for _ in 0..1000 {
            vu.next();
        }
        let level = match rx.try_recv() {
            Ok(Ev::Level(l)) => l,
            other => panic!("nem Level jött: {other:?}"),
        };
        // teljes amplitúdójú szinusz RMS-e ~0,707
        assert!((level - 0.707).abs() < 0.05, "rms={level}");
    }

    #[test]
    fn analyzer_puts_low_tone_in_low_bands() {
        let mut an = Analyzer::new(44_100);
        let mut out = None;
        // 100 Hz szinusz: az energia az alsó sávokban
        for i in 0..(FFT_N * 8) {
            let s = (2.0 * std::f32::consts::PI * 100.0 * i as f32 / 44_100.0).sin();
            if let Some(b) = an.push(s) {
                out = Some(b);
            }
        }
        let b = out.expect("spectrum emitted");
        let low: u32 = b[..4].iter().map(|&x| x as u32).sum();
        let high: u32 = b[12..].iter().map(|&x| x as u32).sum();
        assert!(low > high + 100, "low={low} high={high}");
    }
}
