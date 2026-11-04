//! Path check for the `digital` profile: is an audio path bit-transparent?
//!
//! The transmitter plays [`pattern_samples`], a periodic test signal in the
//! digital profile's sample layout ([`crate::phy::digital`]); the receiver
//! records the far end of the path and [`analyze`]s it. The report says
//! whether every sample arrived unchanged, whether the `digital` profile
//! would still work, and if not what the path did to the samples (gain,
//! word length, dither, sample-rate conversion, polarity, DC offset, noise).
//! [`PathImpairment`] simulates such paths for tests and `cricket pathcheck
//! sim`.
//!
//! One period ([`PERIOD`] samples, 100 ms at 48 kHz):
//!
//! | samples | content |
//! |---|---|
//! | 0–14 | a digital-profile frame carrying [`PROBE_PACKET`] (version `0xF`: never a message) |
//! | 15–31 | silence |
//! | 32–, even | random profile symbols −11…+11 (× 1024 codes) |
//! | 32–, odd | random codes −32768…32767 (all 16 low bits random) |
//!
//! Peak −48.2 dBFS. The pattern repeats exactly, so the receiver can align
//! to any point of it.

use crate::link::bytes_to_nibbles;
use crate::phy::digital::{self, DigitalRx, CODE_SCALE, STEP, TOLERANCE};
use crate::phy::PhyConfig;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Pattern period in samples.
pub const PERIOD: usize = 4800;
/// First sample of the random part of a period.
const RANDOM_START: usize = 32;
const SEED: u64 = 0x5EED_C0DE_D161_7A11;

/// The probe frame's packet: header `0xF4` (version 15, type Heartbeat),
/// SEQ 0, CRC-16. Structurally a frame, but no receiver delivers it.
pub const PROBE_PACKET: [u8; 4] = probe_packet();

const fn probe_packet() -> [u8; 4] {
    // CRC-16/CCITT of [0xF4, 0x00], computed at compile time.
    let data = [0xF4u8, 0x00];
    let mut crc: u16 = 0xFFFF;
    let mut i = 0;
    while i < 2 {
        crc ^= (data[i] as u16) << 8;
        let mut b = 0;
        while b < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            b += 1;
        }
        i += 1;
    }
    [0xF4, 0x00, (crc >> 8) as u8, crc as u8]
}

/// Small deterministic RNG (xorshift64*).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        Self(if s == 0 { 1 } else { s })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform integer in `lo..=hi`.
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next_u64() % (hi - lo + 1) as u64) as i32
    }

    /// Standard normal (Box–Muller).
    pub fn gaussian(&mut self) -> f64 {
        let u1 = self.uniform().max(f64::MIN_POSITIVE);
        let u2 = self.uniform();
        libm::sqrt(-2.0 * libm::log(u1)) * libm::cos(2.0 * core::f64::consts::PI * u2)
    }
}

/// What a pattern position carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Probe,
    Silence,
    Symbol,
    Lsb,
}

fn class(i: usize) -> Class {
    let probe_len = digital::frame_samples(PROBE_PACKET.len());
    match i % PERIOD {
        j if j < probe_len => Class::Probe,
        j if j < RANDOM_START => Class::Silence,
        j if j % 2 == 0 => Class::Symbol,
        _ => Class::Lsb,
    }
}

/// One period of the pattern as 24-bit codes.
pub fn pattern_period() -> Vec<i32> {
    let mut out = vec![0i32; PERIOD];
    let probe = digital::frame(&bytes_to_nibbles(&PROBE_PACKET));
    for (o, &x) in out.iter_mut().zip(&probe) {
        *o = digital::sample_code(x);
    }
    let mut rng = Rng::new(SEED);
    for (i, o) in out.iter_mut().enumerate().skip(RANDOM_START) {
        *o = match class(i) {
            Class::Symbol => rng.range(-11, 11) * STEP,
            _ => rng.range(-32_768, 32_767),
        };
    }
    out
}

/// `periods` periods of the pattern as `f32` samples (exact: code / 2²³).
pub fn pattern_samples(periods: usize) -> Vec<f32> {
    let one: Vec<f32> = pattern_period()
        .iter()
        .map(|&c| c as f32 / CODE_SCALE)
        .collect();
    let mut out = Vec::with_capacity(one.len() * periods);
    for _ in 0..periods {
        out.extend_from_slice(&one);
    }
    out
}

// ---------------------------------------------------------------------------
// Path simulator

/// How a path requantizes to a shorter word.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Requantize {
    /// Drop the low bits (two's complement truncation).
    Truncate,
    /// Round to nearest.
    Round,
    /// Add TPDF dither of ±1 LSB (peak), then round.
    Tpdf,
}

/// One processing step of a simulated audio path. Samples are 24-bit
/// codes in between; a step that computes in floating point rounds back to
/// a 24-bit integer at its end, as a real device's output would.
#[derive(Debug, Clone, PartialEq)]
pub enum PathImpairment {
    /// Linear gain in dB.
    Gain(f64),
    /// Negate every sample.
    Polarity,
    /// Insert this many samples of silence in front.
    Delay(usize),
    /// Add a constant, in 24-bit codes (256 = 1 LSB of a 16-bit word).
    DcOffset(i32),
    /// Reduce to `bits` (e.g. 16) and back to 24 bits.
    WordLength { bits: u32, mode: Requantize },
    /// A float stage scaled by 2²³ on the way in and by `scale` on the way
    /// out (e.g. 2²³ − 1), truncating toward zero or rounding.
    FloatScale { scale: f64, truncate: bool },
    /// A 16-bit stage converting `f32 → i16` with ×32767 (truncating or
    /// rounding) and `i16 → f32` with /32768.
    Int16Scale32767 { truncate: bool },
    /// Add Gaussian noise of this RMS level (dBFS); `seed` selects it.
    Noise { dbfs: f64, seed: u64 },
    /// Resample by `ratio` (output rate / input rate) with a windowed-sinc
    /// interpolator: sample-rate conversion or an unlocked clock.
    Resample(f64),
    /// Peaking EQ: centre (Hz at 48 kHz), gain (dB), Q.
    PeakingEq { hz: f64, db: f64, q: f64 },
}

impl PathImpairment {
    /// Named presets: the conditions of the fail-safe table
    /// (`docs/proposals/digital-transparent.md` §5).
    pub fn presets() -> Vec<(&'static str, Vec<PathImpairment>)> {
        use PathImpairment as P;
        let tpdf = P::WordLength {
            bits: 16,
            mode: Requantize::Tpdf,
        };
        vec![
            ("bit-exact", vec![]),
            ("delay-1", vec![P::Delay(1)]),
            ("polarity", vec![P::Polarity]),
            (
                "16bit-truncate",
                vec![P::WordLength {
                    bits: 16,
                    mode: Requantize::Truncate,
                }],
            ),
            ("16bit-tpdf", vec![tpdf.clone()]),
            (
                "float-2^23-1",
                vec![P::FloatScale {
                    scale: 8_388_607.0,
                    truncate: true,
                }],
            ),
            ("int16-32767", vec![P::Int16Scale32767 { truncate: true }]),
            (
                "noise-120dBFS",
                vec![P::Noise {
                    dbfs: -120.0,
                    seed: 1,
                }],
            ),
            (
                "noise-100dBFS",
                vec![P::Noise {
                    dbfs: -100.0,
                    seed: 2,
                }],
            ),
            ("dc+1lsb16", vec![P::DcOffset(256)]),
            ("gain+0.1dB", vec![P::Gain(0.1)]),
            ("gain-0.1dB", vec![P::Gain(-0.1)]),
            ("gain+0.5dB", vec![P::Gain(0.5)]),
            ("gain-1dB", vec![P::Gain(-1.0)]),
            ("gain-0.2dB+16bit-tpdf", vec![P::Gain(-0.2), tpdf]),
            (
                "noise-80dBFS",
                vec![P::Noise {
                    dbfs: -80.0,
                    seed: 3,
                }],
            ),
            ("dc+2lsb16", vec![P::DcOffset(512)]),
            ("drift+10ppm", vec![P::Resample(1.0 + 10e-6)]),
            (
                "src-48k-44k1-48k",
                vec![
                    P::Resample(44_100.0 / 48_000.0),
                    P::Resample(48_000.0 / 44_100.0),
                ],
            ),
            (
                "eq+3dB@10kHz",
                vec![P::PeakingEq {
                    hz: 10_000.0,
                    db: 3.0,
                    q: 1.0,
                }],
            ),
            (
                "eq+1dB@1kHz",
                vec![P::PeakingEq {
                    hz: 1000.0,
                    db: 1.0,
                    q: 1.0,
                }],
            ),
            (
                "8bit-truncate",
                vec![P::WordLength {
                    bits: 8,
                    mode: Requantize::Truncate,
                }],
            ),
        ]
    }
}

fn round24(x: f64) -> f64 {
    libm::round(x).clamp(-8_388_608.0, 8_388_607.0)
}

/// Run `samples` through `path` (steps in order).
pub fn simulate(path: &[PathImpairment], samples: &[f32]) -> Vec<f32> {
    let mut c: Vec<f64> = samples
        .iter()
        .map(|&x| x as f64 * CODE_SCALE as f64)
        .collect();
    for step in path {
        c = apply(step, c);
    }
    c.iter().map(|&v| (v / CODE_SCALE as f64) as f32).collect()
}

fn apply(step: &PathImpairment, mut c: Vec<f64>) -> Vec<f64> {
    use PathImpairment as P;
    match *step {
        P::Gain(db) => {
            let g = libm::pow(10.0, db / 20.0);
            c.iter_mut().for_each(|v| *v = round24(*v * g));
        }
        P::Polarity => c.iter_mut().for_each(|v| *v = -*v),
        P::Delay(n) => {
            let mut out = vec![0.0; n];
            out.extend(c);
            c = out;
        }
        P::DcOffset(d) => c.iter_mut().for_each(|v| *v = round24(*v + d as f64)),
        P::WordLength { bits, mode } => {
            let q = libm::pow(2.0, 24.0 - bits as f64);
            let mut rng = Rng::new(bits as u64 ^ 0xD17E);
            for v in c.iter_mut() {
                let x = *v / q;
                let y = match mode {
                    Requantize::Truncate => libm::floor(x),
                    Requantize::Round => libm::round(x),
                    Requantize::Tpdf => libm::round(x + rng.uniform() + rng.uniform() - 1.0),
                };
                *v = round24(y * q);
            }
        }
        P::FloatScale { scale, truncate } => {
            for v in c.iter_mut() {
                let y = *v / CODE_SCALE as f64 * scale;
                *v = if truncate {
                    libm::trunc(y)
                } else {
                    libm::round(y)
                };
            }
        }
        P::Int16Scale32767 { truncate } => {
            for v in c.iter_mut() {
                let y = *v / CODE_SCALE as f64 * 32_767.0;
                let i = if truncate {
                    libm::trunc(y)
                } else {
                    libm::round(y)
                };
                *v = i * 256.0;
            }
        }
        P::Noise { dbfs, seed } => {
            let sigma = CODE_SCALE as f64 * libm::pow(10.0, dbfs / 20.0);
            let mut rng = Rng::new(seed);
            c.iter_mut()
                .for_each(|v| *v = round24(*v + sigma * rng.gaussian()));
        }
        P::Resample(ratio) => c = resample(&c, ratio),
        P::PeakingEq { hz, db, q } => {
            // RBJ peaking biquad at 48 kHz.
            let a = libm::pow(10.0, db / 40.0);
            let w0 = 2.0 * core::f64::consts::PI * hz / 48_000.0;
            let alpha = libm::sin(w0) / (2.0 * q);
            let cw = libm::cos(w0);
            let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a);
            let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a);
            let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
            for v in c.iter_mut() {
                let x0 = *v;
                let y0 = (b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2) / a0;
                x2 = x1;
                x1 = x0;
                y2 = y1;
                y1 = y0;
                *v = round24(y0);
            }
        }
    }
    c
}

/// Windowed-sinc (Hann, 32 taps) resampler; output sample `k` is the input
/// at time `k / ratio`, low-passed below the lower of the two Nyquists.
fn resample(c: &[f64], ratio: f64) -> Vec<f64> {
    const HALF: i64 = 16;
    let cutoff = ratio.min(1.0) * 0.95;
    let n_out = (c.len() as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(n_out);
    for k in 0..n_out {
        let t = k as f64 / ratio;
        let center = libm::floor(t) as i64;
        let mut acc = 0.0;
        for j in (center - HALF + 1)..=(center + HALF) {
            if j < 0 || j as usize >= c.len() {
                continue;
            }
            let d = t - j as f64;
            let x = core::f64::consts::PI * d * cutoff;
            let sinc = if libm::fabs(x) < 1e-12 {
                1.0
            } else {
                libm::sin(x) / x
            };
            let w = 0.5 + 0.5 * libm::cos(core::f64::consts::PI * d / HALF as f64);
            acc += c[j as usize] * cutoff * sinc * w;
        }
        out.push(round24(acc));
    }
    out
}

// ---------------------------------------------------------------------------
// Analysis

/// Overall answer of [`analyze`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The test signal was not found (wrong channel, muted, TX not running,
    /// or a path that changes the signal beyond recognition).
    NoSignal,
    /// Every sample arrived unchanged (possibly with inverted polarity).
    BitExact,
    /// Not bit-exact, but every profile symbol and probe frame survived:
    /// the `digital` profile works, with less margin.
    DigitalOk,
    /// Fewer than 1 in 1000 profile symbols fail: the `digital` profile
    /// works, but some frames are lost (and with both copies hit, a few
    /// messages per million).
    Marginal,
    /// The `digital` profile does not work on this path.
    Unusable,
}

/// Result of [`analyze`].
#[derive(Debug, Clone, PartialEq)]
pub struct PathReport {
    pub verdict: Verdict,
    pub sample_rate_hz: u32,
    /// Samples analysed (aligned with the pattern).
    pub compared: usize,
    /// Of those, samples equal to the pattern (after polarity).
    pub exact: usize,
    /// Normalized correlation with the pattern at the best alignment.
    pub correlation: f64,
    /// The pattern arrived negated.
    pub inverted: bool,
    /// Index of the first analysed sample in the capture.
    pub start: usize,
    /// Pattern position of the first analysed sample (0..PERIOD).
    pub phase: usize,
    /// Least-squares gain of received over sent, dB.
    pub gain_db: f64,
    /// Mean of received − gain × sent, in 24-bit codes.
    pub dc_codes: f64,
    /// RMS of what remains, dBFS (`None` if exactly zero).
    pub residual_dbfs: Option<f64>,
    /// Lowest bit that was ever non-zero in the received random samples
    /// (8 = 16-bit path, 4 = 20-bit, 0 = 24-bit or more).
    pub low_zero_bits: u32,
    /// Received samples that are not whole 24-bit codes (float processing
    /// or a word longer than 24 bits).
    pub fractional: usize,
    /// How a 16/20-bit word length was reached, if one was detected.
    pub requantize: Option<(u32, Requantize)>,
    /// Drift of the alignment over the capture, ppm (`None`: below what the
    /// capture can resolve).
    pub drift_ppm: Option<f64>,
    /// Errors of exactly ±1 code (24-bit LSB), e.g. a float stage scaled by
    /// 2²³ − 1 with truncation.
    pub lsb24_errors: usize,
    /// Profile symbols decided correctly by the digital receiver rule.
    pub symbols_ok: usize,
    pub symbols: usize,
    /// Largest |residual| of a correctly decided symbol, in codes (the
    /// tolerance is [`TOLERANCE`]).
    pub worst_symbol_residual: i32,
    /// Probe frames decoded by the digital receiver / expected.
    pub probes_ok: usize,
    pub probes_expected: usize,
}

/// Normalized correlation of `y[start..start + PERIOD]` with the pattern at
/// pattern offset `lag` (`y[start + n]` against `pattern[(n + lag) % P]`);
/// `er` is the pattern's energy.
fn correlate(y: &[f64], start: usize, r: &[f64], er: f64, lag: usize) -> f64 {
    let mut acc = 0.0;
    let mut ey = 0.0;
    for n in 0..PERIOD {
        let a = y[start + n];
        acc += a * r[(n + lag) % PERIOD];
        ey += a * a;
    }
    if ey == 0.0 {
        0.0
    } else {
        acc / libm::sqrt(ey * er)
    }
}

/// Analyse a recording of the far end of a path carrying the pattern.
/// Needs at least ~2.5 periods (0.25 s at 48 kHz) of signal; a few seconds
/// give a meaningful drift estimate.
pub fn analyze(samples: &[f32], sample_rate_hz: u32) -> PathReport {
    let y: Vec<f64> = samples
        .iter()
        .map(|&x| x as f64 * CODE_SCALE as f64)
        .collect();
    let r: Vec<f64> = pattern_period().iter().map(|&c| c as f64).collect();
    let er: f64 = r.iter().map(|v| v * v).sum();
    let mut report = PathReport {
        verdict: Verdict::NoSignal,
        sample_rate_hz,
        compared: 0,
        exact: 0,
        correlation: 0.0,
        inverted: false,
        start: 0,
        phase: 0,
        gain_db: 0.0,
        dc_codes: 0.0,
        residual_dbfs: None,
        low_zero_bits: 0,
        fractional: 0,
        requantize: None,
        drift_ppm: None,
        lsb24_errors: 0,
        symbols_ok: 0,
        symbols: 0,
        worst_symbol_residual: 0,
        probes_ok: 0,
        probes_expected: 0,
    };

    // Signal onset and end (the pattern never has 64 zeros in a row).
    let Some(onset) = y.iter().position(|v| v.abs() >= 256.0) else {
        return report;
    };
    let mut end = y.len();
    let mut zeros = 0usize;
    for (i, v) in y.iter().enumerate().skip(onset) {
        if *v == 0.0 {
            zeros += 1;
            if zeros >= 64 {
                end = i + 1 - zeros;
                break;
            }
        } else {
            zeros = 0;
        }
    }
    // Skip a few samples of a possible fade-in, then align on one period.
    let start = onset + 64;
    if end < start + 2 * PERIOD {
        return report;
    }
    let (mut best_lag, mut best) = (0usize, 0.0f64);
    for lag in 0..PERIOD {
        let c = correlate(&y, start, &r, er, lag);
        if c.abs() > best.abs() {
            best = c;
            best_lag = lag;
        }
    }
    report.correlation = best.abs();
    report.inverted = best < 0.0;
    if report.correlation < 0.5 {
        return report;
    }
    let sign = if report.inverted { -1.0 } else { 1.0 };
    report.start = start;
    report.phase = best_lag;
    let expected = |i: usize| sign * r[(i - start + best_lag) % PERIOD];

    // Alignment over time: per period, the best lag near the previous one,
    // refined by parabolic interpolation, gives the accumulated delay.
    let mut positions = Vec::new();
    let mut lag = best_lag as i64;
    let mut k = 0;
    let p = PERIOD as i64;
    while start + (k + 1) * PERIOD < end && k < 200 {
        let s = start + k * PERIOD;
        let c = |l: i64| correlate(&y, s, &r, er, l.rem_euclid(p) as usize).abs();
        let mut best_l = lag;
        let mut best_c = c(lag);
        for l in [lag - 2, lag - 1, lag + 1, lag + 2] {
            let v = c(l);
            if v > best_c {
                best_c = v;
                best_l = l;
            }
        }
        lag = best_l;
        let (c0, c1, c2) = (c(lag - 1), best_c, c(lag + 1));
        let denom = c0 - 2.0 * c1 + c2;
        let frac = if denom.abs() > 1e-12 {
            0.5 * (c0 - c2) / denom
        } else {
            0.0
        };
        // Received sample s + n carries pattern position n + lag + frac: the
        // path has delayed it by -(lag + frac) relative to the start.
        positions.push((s as f64, -((lag - best_lag as i64) as f64 + frac)));
        k += 1;
    }
    if positions.len() >= 3 {
        let n = positions.len() as f64;
        let mx = positions.iter().map(|p| p.0).sum::<f64>() / n;
        let my = positions.iter().map(|p| p.1).sum::<f64>() / n;
        let sxy: f64 = positions.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        let sxx: f64 = positions.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
        let slope = sxy / sxx;
        // Resolution: a few hundredths of a sample over the capture.
        let span = positions.last().map_or(0.0, |p| p.0) - positions[0].0;
        if (slope * span).abs() > 0.05 {
            report.drift_ppm = Some(slope * 1e6);
        }
    }

    // Sample-by-sample comparison.
    let (mut sxy, mut sxx) = (0.0f64, 0.0f64);
    let mut low_bits = 0i64;
    let mut quant: [[usize; 3]; 2] = [[0; 3]; 2]; // [16, 20] x [trunc, round, tpdf-like]
    let mut quant_n = 0usize;
    for (i, &got) in y.iter().enumerate().take(end).skip(start) {
        let want = expected(i);
        report.compared += 1;
        if got == want {
            report.exact += 1;
        }
        if got != libm::round(got) {
            report.fractional += 1;
        }
        let diff = got - want;
        if diff.abs() == 1.0 {
            report.lsb24_errors += 1;
        }
        sxy += got * want;
        sxx += want * want;
        match class(i - start + best_lag) {
            Class::Lsb => {
                low_bits |= libm::round(got) as i64;
                quant_n += 1;
                for (w, bits) in [16u32, 20].iter().enumerate() {
                    let q = libm::pow(2.0, 24.0 - *bits as f64);
                    let t = libm::floor(want / q) * q;
                    let rd = libm::round(want / q) * q;
                    quant[w][0] += (got == t) as usize;
                    quant[w][1] += (got == rd) as usize;
                    quant[w][2] += ((got - rd).abs() <= q) as usize;
                }
            }
            Class::Symbol => {
                report.symbols += 1;
                let v_want = (want / STEP as f64) as i32 * if report.inverted { -1 } else { 1 };
                let (v, res) = digital::classify(digital::sample_code(samples[i]));
                let v = if report.inverted { -v } else { v };
                if v == v_want && res.abs() <= TOLERANCE {
                    report.symbols_ok += 1;
                    report.worst_symbol_residual = report.worst_symbol_residual.max(res.abs());
                }
            }
            _ => {}
        }
    }
    let gain = if sxx > 0.0 { sxy / sxx } else { 1.0 };
    report.gain_db = 20.0 * libm::log10(gain.abs().max(1e-12));
    let n = report.compared as f64;
    let mut dc = 0.0;
    for (i, &got) in y.iter().enumerate().take(end).skip(start) {
        dc += got - gain * expected(i);
    }
    dc /= n;
    report.dc_codes = dc;
    let mut e2 = 0.0;
    for (i, &got) in y.iter().enumerate().take(end).skip(start) {
        let e = got - gain * expected(i) - dc;
        e2 += e * e;
    }
    let rms = libm::sqrt(e2 / n);
    report.residual_dbfs = (rms > 0.0).then(|| 20.0 * libm::log10(rms / CODE_SCALE as f64));
    report.low_zero_bits = if low_bits == 0 {
        24
    } else {
        low_bits.trailing_zeros().min(24)
    };
    for (w, bits) in [16u32, 20].iter().enumerate() {
        if report.low_zero_bits >= 24 - bits && quant_n > 0 && report.requantize.is_none() {
            let frac = |c: usize| c as f64 / quant_n as f64;
            report.requantize = if frac(quant[w][0]) > 0.999 {
                Some((*bits, Requantize::Truncate))
            } else if frac(quant[w][1]) > 0.999 {
                Some((*bits, Requantize::Round))
            } else if frac(quant[w][2]) > 0.999 {
                Some((*bits, Requantize::Tpdf))
            } else {
                None
            };
        }
    }

    // The digital receiver itself, on the probe frames.
    let mut rx = DigitalRx::new(PhyConfig::digital().with_sample_rate(sample_rate_hz));
    for &x in &samples[start..end] {
        if let Some(f) = rx.push(x) {
            if f.packet() == PROBE_PACKET {
                report.probes_ok += 1;
            }
        }
    }
    let probe_len = digital::frame_samples(PROBE_PACKET.len());
    let first_probe = (PERIOD - best_lag) % PERIOD;
    if end >= start + first_probe + probe_len {
        report.probes_expected = (end - start - first_probe - probe_len) / PERIOD + 1;
    }

    report.verdict = if report.exact == report.compared {
        Verdict::BitExact
    } else if report.symbols_ok == report.symbols
        && report.probes_ok == report.probes_expected
        && report.probes_expected > 0
    {
        Verdict::DigitalOk
    } else if report.symbol_error_rate() < 1e-3
        && report.probes_ok * 100 >= report.probes_expected * 95
        && report.probes_expected > 0
    {
        Verdict::Marginal
    } else {
        Verdict::Unusable
    };
    report
}

fn requantize_name(m: Requantize) -> &'static str {
    match m {
        Requantize::Truncate => "truncation",
        Requantize::Round => "rounding",
        Requantize::Tpdf => "±1 LSB requantization error (dither, or a 32767-scaled conversion)",
    }
}

impl PathReport {
    /// Fraction of profile symbols the digital receiver would get wrong.
    pub fn symbol_error_rate(&self) -> f64 {
        (self.symbols - self.symbols_ok) as f64 / self.symbols.max(1) as f64
    }

    /// Expected share of frames lost (21 samples each) and of messages lost
    /// with both of their copies hit, from the symbol error rate.
    pub fn expected_losses(&self) -> (f64, f64) {
        let frame = 1.0 - libm::pow(1.0 - self.symbol_error_rate(), 21.0);
        (frame, frame * frame)
    }

    /// Human-readable report, one line per entry.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let ms = |n: usize| n as f64 * 1000.0 / self.sample_rate_hz.max(1) as f64;
        if self.verdict == Verdict::NoSignal {
            out.push(format!(
                "Test signal: NOT FOUND (best correlation {:.2})",
                self.correlation
            ));
            out.push(
                "Verdict: NO SIGNAL. Check routing and channel, that `cricket pathcheck send` \
                 is running on the far end, and that nothing mutes or heavily processes the \
                 channel."
                    .into(),
            );
            return out;
        }
        out.push(format!(
            "Test signal: found (correlation {:.4}, polarity {})",
            self.correlation,
            if self.inverted { "INVERTED" } else { "normal" }
        ));
        out.push(format!(
            "Compared: {} samples ({:.1} s)",
            self.compared,
            ms(self.compared) / 1000.0
        ));
        out.push(format!(
            "Bit-exact samples: {} of {} ({:.3} %)",
            self.exact,
            self.compared,
            100.0 * self.exact as f64 / self.compared.max(1) as f64
        ));
        out.push(format!(
            "Digital profile symbols decoded: {} of {} (worst error {} of {} codes allowed)",
            self.symbols_ok, self.symbols, self.worst_symbol_residual, TOLERANCE
        ));
        out.push(format!(
            "Digital probe frames decoded: {} of {}",
            self.probes_ok, self.probes_expected
        ));

        let mut findings = Vec::new();
        if self.inverted {
            findings.push(
                "Polarity inverted (harmless: the digital receiver detects and undoes it)".into(),
            );
        }
        if self.exact != self.compared {
            if let Some(ppm) = self.drift_ppm {
                findings.push(format!(
                    "Sample clock drifts by {:+.1} ppm: sample-rate conversion or unlocked \
                     clocks (sync both ends to one word clock / PTP leader, disable SRC)",
                    ppm
                ));
            } else if self.correlation < 0.999 {
                findings.push(format!(
                    "Waveform changed (correlation {:.4}): filtering, EQ, sample-rate \
                     conversion or strong noise",
                    self.correlation
                ));
            }
            if self.low_zero_bits >= 4 && self.low_zero_bits < 24 {
                let bits = 24 - self.low_zero_bits;
                match self.requantize {
                    Some((b, m)) => findings.push(format!(
                        "Word length: {} bit, reduced by {} (the digital profile only needs \
                         16 bits)",
                        b,
                        requantize_name(m)
                    )),
                    None => findings.push(format!(
                        "Word length: {} bit (low {} bits always zero)",
                        bits, self.low_zero_bits
                    )),
                }
            }
            if self.gain_db.abs() >= 0.0005 {
                findings.push(format!(
                    "Gain change: {:+.3} dB (fader, trim or volume not at unity)",
                    self.gain_db
                ));
            }
            if self.dc_codes.abs() >= 0.5 && self.requantize.is_none() {
                findings.push(format!(
                    "DC offset: {:+.1} (24-bit LSB) = {:+.2} LSB of 16 bits",
                    self.dc_codes,
                    self.dc_codes / 256.0
                ));
            }
            if self.fractional > 0 {
                findings.push(format!(
                    "{} samples are not whole 24-bit values (float processing or a longer word \
                     with dither)",
                    self.fractional
                ));
            }
            if self.lsb24_errors * 10 > (self.compared - self.exact) * 9 {
                findings.push(
                    "Errors are all ±1 LSB of 24 bits: a conversion that scales by 2^23-1 (or \
                     32767) instead of a power of two, with truncation"
                        .into(),
                );
            }
            if let Some(db) = self.residual_dbfs {
                if self.requantize.is_none() && self.lsb24_errors * 10 <= self.compared {
                    findings.push(format!(
                        "Residual after gain and offset: {:.1} dBFS RMS (noise, dither or \
                         processing)",
                        db
                    ));
                }
            }
        }
        for f in findings {
            out.push(format!("  - {}", f));
        }
        out.push(match self.verdict {
            Verdict::BitExact => {
                "Verdict: BIT-EXACT. The `digital` profile will work on this path.".into()
            }
            Verdict::DigitalOk => format!(
                "Verdict: NOT BIT-EXACT, but the `digital` profile survives it (margin: worst \
                 error {} of {} codes). Fix the cause above if you can; any further change \
                 will break the link (safely: no wrong MIDI, just no MIDI).",
                self.worst_symbol_residual, TOLERANCE
            ),
            Verdict::Marginal => {
                let (frame, msg) = self.expected_losses();
                format!(
                    "Verdict: MARGINAL. The `digital` profile works, but {:.0} in a million \
                     samples leave the tolerance: about {:.2} % of frames are dropped and {:.1} \
                     in a million messages lose both copies. Fix the cause above.",
                    self.symbol_error_rate() * 1e6,
                    frame * 100.0,
                    msg * 1e6
                )
            }
            Verdict::Unusable => "Verdict: NOT USABLE for the `digital` profile (its receiver \
                                  would drop frames). Fix the cause above or use ltc9600_fec / \
                                  robust on this path."
                .into(),
            Verdict::NoSignal => unreachable!(),
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_packet_has_valid_crc_and_invalid_version() {
        let crc = crate::link::crc16_ccitt(&PROBE_PACKET[..2]);
        assert_eq!(PROBE_PACKET[2], (crc >> 8) as u8);
        assert_eq!(PROBE_PACKET[3], crc as u8);
        assert_eq!(
            crate::link::decode_packet_bytes(&PROBE_PACKET),
            Err(crate::link::PacketDecodeError::BadVersion(0xF))
        );
    }

    #[test]
    fn pattern_level_and_exactness() {
        let p = pattern_period();
        let peak = p.iter().map(|c| c.abs()).max().unwrap();
        assert!(peak <= 32_768);
        for &c in &p {
            let x = c as f32 / CODE_SCALE;
            assert_eq!(digital::sample_code(x), c);
        }
    }

    #[test]
    fn clean_path_is_bit_exact() {
        let mut audio = vec![0.0f32; 1234];
        audio.extend(pattern_samples(5));
        let r = analyze(&audio, 48_000);
        assert_eq!(r.verdict, Verdict::BitExact, "{:?}", r);
        assert!(!r.inverted);
        assert_eq!(r.probes_ok, r.probes_expected);
        assert!(r.probes_expected >= 4);
    }
}
