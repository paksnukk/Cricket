//! Shared integration-test helpers: a deterministic audio channel simulator
//! and a chunked streaming decoder.
//!
//! Use from a test file with `mod support;`.

#![allow(dead_code)]

use cricket_core::modem::{CricketModemRx, ModemOutput};
use cricket_core::phy::{ModemTx, PhyConfig};
use cricket_core::transport::TransportMessage;
use std::f32::consts::PI;

/// Small deterministic xorshift64* RNG (no external crates).
#[derive(Debug, Clone)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    pub fn new(seed: u64) -> Self {
        // State must never be zero; mix the seed so nearby seeds diverge quickly.
        let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        Self {
            state: if mixed == 0 { 1 } else { mixed },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform integer in [0, n). `n` must be > 0.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Standard normal sample (Box-Muller).
    pub fn gaussian(&mut self) -> f64 {
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Program material that bleeds onto the AML channel (crosstalk, a badly
/// routed bus): the speech-like and music-like generators of the
/// false-accept harness ([`speech`], [`music`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    Speech,
    Music,
}

/// A noise gate on the bus: opens (over `attack_ms`) when the input peak
/// exceeds `threshold_dbfs`, stays open `hold_ms` after it last did, then
/// closes over `release_ms` (to silence).
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub threshold_dbfs: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
}

/// A peak compressor (`ratio` finite) or limiter (`ratio` = ∞) on the bus:
/// gain reduction `(level − threshold) · (1 − 1/ratio)` above
/// `threshold_dbfs`, smoothed with `attack_ms` when it grows and
/// `release_ms` when it recovers. No make-up gain (the receivers are
/// level-independent).
#[derive(Debug, Clone, Copy)]
pub struct Dynamics {
    pub threshold_dbfs: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl Dynamics {
    pub fn compressor(threshold_dbfs: f32, ratio: f32, attack_ms: f32, release_ms: f32) -> Self {
        Self {
            threshold_dbfs,
            ratio,
            attack_ms,
            release_ms,
        }
    }

    /// A brick-wall limiter: 0.1 ms attack.
    pub fn limiter(threshold_dbfs: f32, release_ms: f32) -> Self {
        Self {
            threshold_dbfs,
            ratio: f32::INFINITY,
            attack_ms: 0.1,
            release_ms,
        }
    }
}

/// Network-audio style packet loss: the signal is cut into blocks of
/// `block_ms` (Dante: 1 ms at 48 kHz, up to 5 ms over WAN-ish links); a
/// burst starts at a block with probability `per_second · block_ms / 1000`
/// and mutes `blocks` consecutive blocks.
#[derive(Debug, Clone, Copy)]
pub struct BurstLoss {
    pub block_ms: f32,
    pub blocks: usize,
    pub per_second: f32,
}

/// A chain of `stages` AD/DA conversions. Each stage is a steep anti-alias
/// low-pass (8th-order Butterworth at 20 kHz) or, with `slow_rolloff`, a
/// gentle one (one pole at 10 kHz + 2nd order at 16 kHz: a "slow roll-off"
/// converter filter, which tilts the top of the Fast band).
#[derive(Debug, Clone, Copy)]
pub struct Converters {
    pub stages: usize,
    pub slow_rolloff: bool,
}

/// Deterministic channel impairments applied to mono samples.
///
/// Processing order: symbol corruption (on the clean, grid-aligned TX
/// signal) → dropout → burst loss → clock drift → converter chain → 16 kHz
/// codec brick wall → low-pass(es) → high-pass → peaking EQ → program
/// bleed → soft clip → polarity → gain → gate → compressor → limiter → hum
/// → clicks → leading silence → AWGN → pink noise. Impairments that were
/// added later (burst loss, converters, codec, program bleed, dynamics,
/// pink noise) draw their randomness from their own streams and sit in the
/// chain so that a simulation without them is bit-identical to before.
#[derive(Debug, Clone)]
pub struct ChannelSim {
    pub config: PhyConfig,
    pub seed: u64,
    /// Leading silence inserted before the signal, in samples.
    pub offset_samples: usize,
    /// Linear gain.
    pub gain: f32,
    /// AWGN SNR in dB relative to the tone RMS (`amplitude * gain / sqrt(2)`).
    pub snr_db: Option<f32>,
    /// Receiver clock drift in ppm (positive = RX reads the signal faster).
    pub drift_ppm: f64,
    /// One-pole low-pass cutoff in Hz.
    pub lowpass_hz: Option<f32>,
    /// Probability per symbol period of replacing a tone with a different one.
    pub symbol_error_prob: f64,
    /// Second-order Butterworth low-pass cutoff in Hz (steeper band limit).
    pub lowpass2_hz: Option<f32>,
    /// One-pole high-pass cutoff in Hz (AC coupling / transformer).
    pub highpass_hz: Option<f32>,
    /// Peaking EQ: (centre Hz, gain dB, Q).
    pub peaking_eq: Option<(f32, f32, f32)>,
    /// Soft clipper `tanh(drive * x) / drive` (drive > 0; ~limiter).
    pub soft_clip_drive: Option<f32>,
    /// Invert polarity (pin 2/3 swap).
    pub invert: bool,
    /// Mains hum: (frequency Hz, level in dB relative to the tone RMS).
    pub hum: Option<(f32, f32)>,
    /// One muted stretch (e.g. a network audio packet loss) of this many
    /// samples at a uniformly random position inside the signal.
    pub dropout_samples: usize,
    /// Impulse noise (clicks, crackle): (mean clicks per second, peak level
    /// in dB relative to the signal peak). Each click is a 1-4 sample
    /// rectangular spike of random sign, added after the channel filters.
    pub clicks: Option<(f32, f32)>,
    /// Bursty packet loss, see [`BurstLoss`].
    pub burst_loss: Option<BurstLoss>,
    /// Cascaded converters, see [`Converters`].
    pub converters: Option<Converters>,
    /// A codec-like 16 kHz brick wall (8th-order Butterworth at 7.2 kHz).
    pub codec_16k: bool,
    /// Program bleed: (kind, level in dB relative to the tone RMS, long-term
    /// RMS of the material).
    pub program: Option<(Program, f32)>,
    pub gate: Option<Gate>,
    pub compressor: Option<Dynamics>,
    pub limiter: Option<Dynamics>,
    /// Pink noise SNR in dB relative to the tone RMS (total pink power).
    pub pink_snr_db: Option<f32>,
}

impl ChannelSim {
    /// Identity channel for `config`.
    pub fn new(config: PhyConfig, seed: u64) -> Self {
        Self {
            config,
            seed,
            offset_samples: 0,
            gain: 1.0,
            snr_db: None,
            drift_ppm: 0.0,
            lowpass_hz: None,
            symbol_error_prob: 0.0,
            lowpass2_hz: None,
            highpass_hz: None,
            peaking_eq: None,
            soft_clip_drive: None,
            invert: false,
            hum: None,
            dropout_samples: 0,
            clicks: None,
            burst_loss: None,
            converters: None,
            codec_16k: false,
            program: None,
            gate: None,
            compressor: None,
            limiter: None,
            pink_snr_db: None,
        }
    }

    pub fn burst_loss(mut self, block_ms: f32, blocks: usize, per_second: f32) -> Self {
        self.burst_loss = Some(BurstLoss {
            block_ms,
            blocks,
            per_second,
        });
        self
    }

    pub fn converters(mut self, stages: usize, slow_rolloff: bool) -> Self {
        self.converters = Some(Converters {
            stages,
            slow_rolloff,
        });
        self
    }

    pub fn codec_16k(mut self) -> Self {
        self.codec_16k = true;
        self
    }

    /// Program bleed at `rel_db` relative to the tone RMS.
    pub fn program(mut self, kind: Program, rel_db: f32) -> Self {
        self.program = Some((kind, rel_db));
        self
    }

    pub fn gate(
        mut self,
        threshold_dbfs: f32,
        attack_ms: f32,
        hold_ms: f32,
        release_ms: f32,
    ) -> Self {
        self.gate = Some(Gate {
            threshold_dbfs,
            attack_ms,
            hold_ms,
            release_ms,
        });
        self
    }

    pub fn compressor(mut self, d: Dynamics) -> Self {
        self.compressor = Some(d);
        self
    }

    pub fn limiter(mut self, d: Dynamics) -> Self {
        self.limiter = Some(d);
        self
    }

    pub fn pink_snr_db(mut self, snr_db: f32) -> Self {
        self.pink_snr_db = Some(snr_db);
        self
    }

    pub fn lowpass2_hz(mut self, cutoff_hz: f32) -> Self {
        self.lowpass2_hz = Some(cutoff_hz);
        self
    }

    pub fn highpass_hz(mut self, cutoff_hz: f32) -> Self {
        self.highpass_hz = Some(cutoff_hz);
        self
    }

    pub fn peaking_eq(mut self, centre_hz: f32, gain_db: f32, q: f32) -> Self {
        self.peaking_eq = Some((centre_hz, gain_db, q));
        self
    }

    pub fn soft_clip(mut self, drive: f32) -> Self {
        self.soft_clip_drive = Some(drive);
        self
    }

    pub fn invert(mut self) -> Self {
        self.invert = true;
        self
    }

    pub fn hum(mut self, freq_hz: f32, rel_db: f32) -> Self {
        self.hum = Some((freq_hz, rel_db));
        self
    }

    pub fn offset(mut self, samples: usize) -> Self {
        self.offset_samples = samples;
        self
    }

    pub fn gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }

    pub fn snr_db(mut self, snr_db: f32) -> Self {
        self.snr_db = Some(snr_db);
        self
    }

    pub fn drift_ppm(mut self, ppm: f64) -> Self {
        self.drift_ppm = ppm;
        self
    }

    pub fn lowpass_hz(mut self, cutoff_hz: f32) -> Self {
        self.lowpass_hz = Some(cutoff_hz);
        self
    }

    pub fn symbol_error_prob(mut self, p: f64) -> Self {
        self.symbol_error_prob = p;
        self
    }

    pub fn clicks(mut self, per_second: f32, rel_db: f32) -> Self {
        self.clicks = Some((per_second, rel_db));
        self
    }

    pub fn dropout_samples(mut self, samples: usize) -> Self {
        self.dropout_samples = samples;
        self
    }

    /// Apply all configured impairments. Deterministic for a given seed.
    pub fn apply(&self, input: &[f32]) -> Vec<f32> {
        let mut rng = XorShift64::new(self.seed);
        let mut samples = input.to_vec();

        if self.symbol_error_prob > 0.0 {
            corrupt_symbols(&self.config, &mut samples, self.symbol_error_prob, &mut rng);
        }
        if self.dropout_samples > 0 && samples.len() > self.dropout_samples {
            let start = rng.below(samples.len() - self.dropout_samples);
            samples[start..start + self.dropout_samples].fill(0.0);
        }
        let rate = self.config.sample_rate_hz;
        if let Some(b) = self.burst_loss {
            let mut burst_rng = XorShift64::new(self.seed ^ 0xB0B5_7105_5000_0001);
            burst_loss(&mut samples, b, rate, &mut burst_rng);
        }
        if self.drift_ppm != 0.0 {
            samples = resample_drift(&samples, self.drift_ppm);
        }
        if let Some(c) = self.converters {
            converter_chain(&mut samples, c, rate);
        }
        if self.codec_16k {
            brick_wall(&mut samples, 7_200.0, rate);
        }
        if let Some(cutoff) = self.lowpass_hz {
            one_pole_lowpass(&mut samples, cutoff, rate);
        }
        if let Some(cutoff) = self.lowpass2_hz {
            Biquad::lowpass(cutoff, rate).run(&mut samples);
        }
        if let Some(cutoff) = self.highpass_hz {
            one_pole_highpass(&mut samples, cutoff, rate);
        }
        if let Some((f, db, q)) = self.peaking_eq {
            Biquad::peaking(f, db, q, rate).run(&mut samples);
        }
        if let Some((kind, rel_db)) = self.program {
            let tone_rms = self.config.amplitude / 2f32.sqrt();
            let target = tone_rms * 10f32.powf(rel_db / 20.0);
            let bleed = program_material(
                kind,
                rate,
                samples.len(),
                target,
                self.seed ^ 0x9B0_6BA3_3D00_0002,
            );
            for (s, b) in samples.iter_mut().zip(bleed) {
                *s += b;
            }
        }
        if let Some(drive) = self.soft_clip_drive {
            for s in samples.iter_mut() {
                *s = (drive * *s).tanh() / drive;
            }
        }
        let gain = if self.invert { -self.gain } else { self.gain };
        if gain != 1.0 {
            for s in samples.iter_mut() {
                *s *= gain;
            }
        }
        if let Some(g) = self.gate {
            noise_gate(&mut samples, g, rate);
        }
        if let Some(d) = self.compressor {
            compress(&mut samples, d, rate);
        }
        if let Some(d) = self.limiter {
            compress(&mut samples, d, rate);
        }
        if let Some((f, rel_db)) = self.hum {
            let tone_rms = self.config.amplitude * self.gain / 2f32.sqrt();
            let amp = tone_rms * 2f32.sqrt() * 10f32.powf(rel_db / 20.0);
            let w = 2.0 * PI * f / rate as f32;
            for (i, s) in samples.iter_mut().enumerate() {
                *s += amp * (w * i as f32).sin();
            }
        }
        if let Some((per_second, rel_db)) = self.clicks {
            let peak = self.config.amplitude * self.gain * 10f32.powf(rel_db / 20.0);
            let p = per_second as f64 / rate as f64;
            let mut i = 0;
            while i < samples.len() {
                if rng.next_f64() < p {
                    let sign = if rng.below(2) == 0 { 1.0 } else { -1.0 };
                    let width = 1 + rng.below(4);
                    for s in samples.iter_mut().skip(i).take(width) {
                        *s += sign * peak;
                    }
                }
                i += 1;
            }
        }
        if self.offset_samples > 0 {
            let mut shifted = vec![0.0; self.offset_samples];
            shifted.extend_from_slice(&samples);
            samples = shifted;
        }
        if let Some(snr_db) = self.snr_db {
            let tone_rms = self.config.amplitude * self.gain / 2f32.sqrt();
            let sigma = tone_rms as f64 / 10f64.powf(snr_db as f64 / 20.0);
            for s in samples.iter_mut() {
                *s += (rng.gaussian() * sigma) as f32;
            }
        }
        if let Some(snr_db) = self.pink_snr_db {
            let tone_rms = self.config.amplitude * self.gain / 2f32.sqrt();
            let sigma = tone_rms / 10f32.powf(snr_db / 20.0);
            let mut pink_rng = XorShift64::new(self.seed ^ 0x9150_0000_0000_0003);
            let pink = pink_noise(samples.len(), sigma, &mut pink_rng);
            for (s, p) in samples.iter_mut().zip(pink) {
                *s += p;
            }
        }
        samples
    }
}

/// Mute `b.blocks` consecutive blocks of `b.block_ms` at random block
/// positions (see [`BurstLoss`]).
pub fn burst_loss(samples: &mut [f32], b: BurstLoss, sample_rate_hz: u32, rng: &mut XorShift64) {
    let block = ((b.block_ms / 1000.0) * sample_rate_hz as f32)
        .round()
        .max(1.0) as usize;
    let p = (b.per_second * b.block_ms / 1000.0) as f64;
    let mut i = 0;
    while i < samples.len() {
        if rng.next_f64() < p {
            let end = (i + block * b.blocks.max(1)).min(samples.len());
            samples[i..end].fill(0.0);
            i = end;
        } else {
            i += block;
        }
    }
}

/// Section Q values of an 8th-order Butterworth low-pass.
const BUTTERWORTH8_Q: [f32; 4] = [0.5098, 0.6013, 0.9000, 2.5629];

/// In-place 8th-order Butterworth low-pass (four cascaded biquads).
pub fn brick_wall(samples: &mut [f32], cutoff_hz: f32, sample_rate_hz: u32) {
    for q in BUTTERWORTH8_Q {
        Biquad::lowpass_q(cutoff_hz, q, sample_rate_hz).run(samples);
    }
}

/// See [`Converters`].
pub fn converter_chain(samples: &mut [f32], c: Converters, sample_rate_hz: u32) {
    for _ in 0..c.stages {
        if c.slow_rolloff {
            one_pole_lowpass(samples, 10_000.0, sample_rate_hz);
            Biquad::lowpass(16_000.0, sample_rate_hz).run(samples);
        } else {
            brick_wall(samples, 20_000.0, sample_rate_hz);
        }
    }
}

/// See [`Gate`]. The detector is a peak follower with a 0.5 ms decay.
pub fn noise_gate(samples: &mut [f32], g: Gate, sample_rate_hz: u32) {
    let rate = sample_rate_hz as f32;
    let thr = 10f32.powf(g.threshold_dbfs / 20.0);
    let coef = |ms: f32| {
        if ms <= 0.0 {
            1.0
        } else {
            1.0 - (-1000.0 / (ms * rate)).exp()
        }
    };
    let (a_att, a_rel) = (coef(g.attack_ms), coef(g.release_ms));
    let hold = (g.hold_ms / 1000.0 * rate) as usize;
    let env_decay = (-1000.0 / (0.5 * rate)).exp();
    let (mut env, mut gain, mut held) = (0.0f32, 0.0f32, 0usize);
    for s in samples.iter_mut() {
        env = s.abs().max(env * env_decay);
        let open = if env >= thr {
            held = hold;
            true
        } else if held > 0 {
            held -= 1;
            true
        } else {
            false
        };
        let target = if open { 1.0 } else { 0.0 };
        gain += (target - gain) * if target > gain { a_att } else { a_rel };
        *s *= gain;
    }
}

/// See [`Dynamics`]: a feed-forward peak compressor / limiter in the log
/// domain.
pub fn compress(samples: &mut [f32], d: Dynamics, sample_rate_hz: u32) {
    let rate = sample_rate_hz as f32;
    let coef = |ms: f32| {
        if ms <= 0.0 {
            1.0
        } else {
            1.0 - (-1000.0 / (ms * rate)).exp()
        }
    };
    let (a_att, a_rel) = (coef(d.attack_ms), coef(d.release_ms));
    let slope = if d.ratio.is_finite() {
        1.0 - 1.0 / d.ratio
    } else {
        1.0
    };
    let mut gain_db = 0.0f32;
    for s in samples.iter_mut() {
        let level_db = 20.0 * s.abs().max(1e-9).log10();
        let target = -((level_db - d.threshold_dbfs).max(0.0)) * slope;
        gain_db += (target - gain_db) * if target < gain_db { a_att } else { a_rel };
        *s *= 10f32.powf(gain_db / 20.0);
    }
}

/// Pink-ish noise (Paul Kellet's economy filter).
pub struct Pink {
    b: [f32; 3],
}

impl Pink {
    pub fn new() -> Self {
        Self { b: [0.0; 3] }
    }

    pub fn next(&mut self, white: f32) -> f32 {
        self.b[0] = 0.99765 * self.b[0] + white * 0.0990460;
        self.b[1] = 0.96300 * self.b[1] + white * 0.2965164;
        self.b[2] = 0.57000 * self.b[2] + white * 1.0526913;
        (self.b[0] + self.b[1] + self.b[2] + white * 0.1848) * 0.25
    }
}

impl Default for Pink {
    fn default() -> Self {
        Self::new()
    }
}

/// `n` samples of pink noise scaled to RMS `rms`.
pub fn pink_noise(n: usize, rms: f32, rng: &mut XorShift64) -> Vec<f32> {
    let mut pink = Pink::new();
    let mut out: Vec<f32> = (0..n).map(|_| pink.next(rng.gaussian() as f32)).collect();
    scale_to_rms(&mut out, rms);
    out
}

/// Scale `samples` so that their RMS is `rms` (no-op on silence).
pub fn scale_to_rms(samples: &mut [f32], rms: f32) {
    let energy: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let have = (energy / samples.len().max(1) as f64).sqrt() as f32;
    if have > 0.0 {
        let k = rms / have;
        for s in samples.iter_mut() {
            *s *= k;
        }
    }
}

/// `n` samples of program material of `kind` at long-term RMS `rms`.
pub fn program_material(
    kind: Program,
    sample_rate_hz: u32,
    n: usize,
    rms: f32,
    seed: u64,
) -> Vec<f32> {
    let rate = sample_rate_hz as usize;
    let seconds = n as f32 / rate as f32 + 0.05;
    let mut v = match kind {
        Program::Speech => speech(rate, seconds, seed),
        Program::Music => music(rate, seconds, seed),
    };
    v.truncate(n);
    scale_to_rms(&mut v, rms);
    v
}

/// Music-like: 1–3 voices of harmonic tones (random notes, ADSR envelopes,
/// some square-ish timbres), drum-like noise hits and a pink-noise bed.
pub fn music(rate: usize, seconds: f32, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let n = (seconds * rate as f32) as usize;
    let mut out = vec![0f32; n];
    for voice in 0..3 {
        let mut t = rng.below(rate / 4);
        while t < n {
            let dur = rate / 10 + rng.below(rate / 2);
            let note = 36 + rng.below(60) as i32 + voice * 5;
            let f0 = 440.0 * 2f32.powf((note - 69) as f32 / 12.0);
            let square = rng.below(3) == 0;
            let harmonics = 1 + rng.below(12);
            let level = 0.05 + 0.15 * rng.next_f64() as f32;
            for (k, s) in out.iter_mut().skip(t).take(dur).enumerate() {
                let env = (k as f32 / 480.0).min(1.0) * (-(k as f32) / dur as f32 * 2.0).exp();
                let ph = 2.0 * PI * f0 * k as f32 / rate as f32;
                let mut v = 0.0;
                for h in 1..=harmonics {
                    if square && h % 2 == 0 {
                        continue;
                    }
                    v += (ph * h as f32).sin() / h as f32;
                }
                *s += level * env * v;
            }
            t += dur + rng.below(rate / 8);
        }
    }
    // Drum hits and a pink bed.
    let mut pink = Pink::new();
    let mut hit = 0usize;
    let mut hit_level = 0.0f32;
    for (i, s) in out.iter_mut().enumerate() {
        if i >= hit {
            hit = i + rate / 8 + rng.below(rate / 2);
            hit_level = 0.1 + 0.3 * rng.next_f64() as f32;
        }
        let w = rng.gaussian() as f32;
        let since = (i + rate / 2).saturating_sub(hit) as f32;
        let drum = hit_level * (-since / 2400.0).exp() * w;
        *s += 0.02 * pink.next(w) + drum * (since < 9600.0) as u8 as f32;
    }
    out
}

/// Speech-like: a pitch-varying glottal pulse train through three formant
/// resonators, syllabic envelope with pauses, and fricative noise bursts.
pub fn speech(rate: usize, seconds: f32, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let n = (seconds * rate as f32) as usize;
    let mut out = vec![0f32; n];
    let mut t = 0usize;
    let mut phase = 0.0f32;
    while t < n {
        let syll = rate / 8 + rng.below(rate / 4);
        let voiced = rng.below(4) != 0;
        let formants = [
            300.0 + 500.0 * rng.next_f64() as f32,
            900.0 + 1400.0 * rng.next_f64() as f32,
            2300.0 + 900.0 * rng.next_f64() as f32,
        ];
        let mut res: Vec<Biquad> = formants
            .iter()
            .map(|&f| Biquad::peaking(f, 18.0, 6.0, rate as u32))
            .collect();
        let f0 = 90.0 + 140.0 * rng.next_f64() as f32;
        let mut seg = vec![0f32; syll.min(n - t)];
        for (k, s) in seg.iter_mut().enumerate() {
            let x = if voiced {
                let f = f0 * (1.0 + 0.1 * (k as f32 / syll as f32 - 0.5));
                phase += f / rate as f32;
                if phase >= 1.0 {
                    phase -= 1.0;
                    1.0
                } else {
                    0.0
                }
            } else {
                0.3 * rng.gaussian() as f32
            };
            let env = (PI * k as f32 / syll as f32).sin();
            *s = 0.3 * env * x;
        }
        for r in &mut res {
            r.run(&mut seg);
        }
        out[t..t + seg.len()].copy_from_slice(&seg);
        t += syll + if rng.below(3) == 0 { rate / 5 } else { 0 };
    }
    out
}

/// Replace whole symbol periods (assumed aligned to index 0, as produced by
/// `encode_to_samples`) with a different MFSK tone, with probability `p` per
/// period. Silent periods are left untouched.
pub fn corrupt_symbols(config: &PhyConfig, samples: &mut [f32], p: f64, rng: &mut XorShift64) {
    let period = config.samples_per_symbol_period();
    let sym = config.symbol_samples();
    let tones = config.tone_table();
    let tx = ModemTx::new(config.clone());
    let silence_power = 1e-6 * sym as f32;

    for start in (0..samples.len()).step_by(period) {
        let end = (start + period).min(samples.len());
        if end - start < sym || rng.next_f64() >= p {
            continue;
        }
        let window = &samples[start..start + sym];
        let (current, power) = strongest_tone(window, &tones, config.sample_rate_hz);
        if power < silence_power {
            continue;
        }
        let replacement = (current as usize + 1 + rng.below(tones.len() - 1)) % tones.len();
        let audio = tx.synthesize_nibbles(&[replacement as u8]);
        samples[start..end].copy_from_slice(&audio[..end - start]);
    }
}

/// Linear-interpolation resampling simulating a receiver clock offset of `ppm`.
pub fn resample_drift(input: &[f32], ppm: f64) -> Vec<f32> {
    if input.len() < 2 {
        return input.to_vec();
    }
    let ratio = 1.0 + ppm * 1e-6;
    let last = (input.len() - 1) as f64;
    let mut out = Vec::with_capacity((input.len() as f64 / ratio) as usize + 1);
    let mut i = 0usize;
    loop {
        let pos = i as f64 * ratio;
        if pos > last {
            break;
        }
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let a = input[idx];
        let b = if idx + 1 < input.len() {
            input[idx + 1]
        } else {
            a
        };
        out.push(a + (b - a) * frac);
        i += 1;
    }
    out
}

/// In-place one-pole IIR low-pass with cutoff `cutoff_hz`.
pub fn one_pole_lowpass(samples: &mut [f32], cutoff_hz: f32, sample_rate_hz: u32) {
    let alpha = 1.0 - (-2.0 * PI * cutoff_hz / sample_rate_hz as f32).exp();
    let mut y = 0.0f32;
    for s in samples.iter_mut() {
        y += alpha * (*s - y);
        *s = y;
    }
}

/// In-place one-pole high-pass (DC blocker) with cutoff `cutoff_hz`.
pub fn one_pole_highpass(samples: &mut [f32], cutoff_hz: f32, sample_rate_hz: u32) {
    let r = (-2.0 * PI * cutoff_hz / sample_rate_hz as f32).exp();
    let (mut x1, mut y1) = (0.0f32, 0.0f32);
    for s in samples.iter_mut() {
        let y = *s - x1 + r * y1;
        x1 = *s;
        y1 = y;
        *s = y;
    }
}

/// RBJ-cookbook biquad (direct form I).
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
}

impl Biquad {
    /// Butterworth low-pass.
    pub fn lowpass(cutoff_hz: f32, sample_rate_hz: u32) -> Self {
        Self::lowpass_q(cutoff_hz, std::f32::consts::FRAC_1_SQRT_2, sample_rate_hz)
    }

    /// Second-order low-pass with quality `q` (a section of a higher-order
    /// Butterworth, see `brick_wall`).
    pub fn lowpass_q(cutoff_hz: f32, q: f32, sample_rate_hz: u32) -> Self {
        let w0 = 2.0 * PI * cutoff_hz / sample_rate_hz as f32;
        let alpha = w0.sin() / (2.0 * q);
        let c = w0.cos();
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 - c) / 2.0 / a0, (1.0 - c) / a0, (1.0 - c) / 2.0 / a0],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
        }
    }

    /// Peaking EQ of `gain_db` at `centre_hz` with quality `q`.
    pub fn peaking(centre_hz: f32, gain_db: f32, q: f32, sample_rate_hz: u32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * centre_hz / sample_rate_hz as f32;
        let alpha = w0.sin() / (2.0 * q);
        let c = w0.cos();
        let a0 = 1.0 + alpha / a;
        Self {
            b: [
                (1.0 + alpha * a) / a0,
                -2.0 * c / a0,
                (1.0 - alpha * a) / a0,
            ],
            a: [-2.0 * c / a0, (1.0 - alpha / a) / a0],
        }
    }

    pub fn run(&self, samples: &mut [f32]) {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for s in samples.iter_mut() {
            let x = *s;
            let y =
                self.b[0] * x + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
            x2 = x1;
            x1 = x;
            y2 = y1;
            y1 = y;
            *s = y;
        }
    }
}

fn strongest_tone(window: &[f32], tones: &[f32], sample_rate_hz: u32) -> (u8, f32) {
    let mut best = (0u8, 0.0f32);
    for (i, &freq) in tones.iter().enumerate() {
        let w = 2.0 * PI * freq / sample_rate_hz as f32;
        let coeff = 2.0 * w.cos();
        let (mut q1, mut q2) = (0.0f32, 0.0f32);
        for &x in window {
            let q0 = coeff * q1 - q2 + x;
            q2 = q1;
            q1 = q0;
        }
        let power = q1 * q1 + q2 * q2 - q1 * q2 * coeff;
        if power > best.1 {
            best = (i as u8, power);
        }
    }
    best
}

/// Trailing silence fed by [`decode_chunked`], in milliseconds. Longer than a
/// full Robust transmission (2 x 20 symbols of 6.5 ms ~= 150 ms), so any packet
/// whose start is in the input can finish even if the input is truncated.
pub const FLUSH_MS: usize = 250;

/// Feed `samples` followed by [`FLUSH_MS`] of silence into a fresh
/// `CricketModemRx` in chunks of exactly `chunk_size` samples (only the final
/// silence chunk may be shorter) and collect decoded messages.
pub fn decode_chunked(
    config: PhyConfig,
    samples: &[f32],
    chunk_size: usize,
) -> Vec<TransportMessage> {
    assert!(chunk_size > 0, "chunk_size must be > 0");
    let flush = config.sample_rate_hz as usize * FLUSH_MS / 1000;
    let mut stream = Vec::with_capacity(samples.len() + flush);
    stream.extend_from_slice(samples);
    stream.resize(samples.len() + flush, 0.0);

    let mut rx = CricketModemRx::new(config);
    let mut messages = Vec::new();
    for chunk in stream.chunks(chunk_size) {
        for out in rx.push_samples(chunk) {
            if let ModemOutput::Message(m) = out {
                messages.push(m);
            }
        }
    }
    messages
}
