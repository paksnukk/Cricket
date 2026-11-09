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

/// Synthetic instrument signals (48 kHz mono) for the HF (instrument-audio)
/// profiles, whose tones ride on the instrument's own signal: what matters
/// is how much of each instrument's energy lies in and near 19–21.5 kHz.
/// See [`instrument`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instrument {
    /// Electric guitar on a passive pickup: Karplus–Strong strums and
    /// lead lines (a bright pick transient darkened by the string) through
    /// the pickup's resonant second-order low-pass (3–5 kHz, Q 2–4), so the
    /// spectrum at 20 kHz is ~30 dB under the resonance, not absent.
    Guitar,
    /// The same guitar on a bright active or piezo pickup: no resonance,
    /// only a one-pole 6 kHz roll-off (−10 dB at 20 kHz), so the pick
    /// transients reach the band. An upper bound for guitars.
    GuitarBright,
    /// Electric bass from E1 (41 Hz) through a 2.5 kHz, Q 2 pickup
    /// resonance.
    Bass,
    /// Bright polysynth: band-limited (PolyBLEP) sawtooth chords with an
    /// open filter and vibrato, so harmonics at 1/k reach 24 kHz and sweep
    /// through the tone slots. The worst case for keys.
    Synth,
    /// Electric-piano keys: decaying sine partials (fundamental, octave, a
    /// fast-fading bell partial) through a 4th-order 9 kHz low-pass.
    Keys,
    /// Cymbal / hi-hat bursts: metallic noise (high-passed at 6 kHz with
    /// two resonances) with fast decays, full-band to 24 kHz, as a sampler
    /// or drum machine on the same cable would produce.
    Cymbals,
}

/// A converter or link model for the HF profiles ([`ChannelSim::converter`]):
/// what happens to 19–30 kHz between the embedder and the box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Converter {
    /// A sharp 48 kHz converter (half-band decimator): flat to 0.4535 fs
    /// (21.77 kHz), −6 dB at Nyquist, −60 dB above (windowed-sinc FIR).
    Sharp48,
    /// A slow roll-off 48 kHz converter filter: the gentle model of
    /// [`Converters`] (one pole at 10 kHz and a second-order section at
    /// 16 kHz), which is −14 dB at 19 kHz and −26 dB at 21.5 kHz: a steep
    /// tilt across the band, not a wall.
    Slow48,
    /// A hop through a 44.1 kHz device with half-band converter filters
    /// (ADC and DAC): flat to 20.0 kHz (0.4535 × 44.1), −6 dB at 22.05 kHz
    /// each, so 20–21.5 kHz is tilted by up to 8 dB but not removed.
    Hop441,
    /// A sharp 44.1 kHz sample-rate conversion (a DAW or interface SRC
    /// with the stopband at Nyquist): flat to 20.0 kHz, −90 dB from
    /// 21.5 kHz. The tones from 20.17 kHz up are lost.
    Src441,
    /// A 19 kHz 8th-order Butterworth low-pass: an aptX-class wireless or
    /// codec link's analog-style band limit (−3 dB at 19 kHz, −9 dB at
    /// 21.5 kHz). A real codec also discards the band; see `Codec19Wall`.
    Codec19Lp8,
    /// A codec that codes nothing above 19 kHz: brick wall at 19 kHz
    /// (−60 dB from 19.5 kHz).
    Codec19Wall,
    /// A 96 kHz signal through a 48 kHz device and back: everything above
    /// 21.77 kHz is gone (the FIR of `Sharp48` applied in the 96 kHz
    /// domain). Removes the 24–29 kHz plan of the 96 kHz variant entirely.
    Hop96To48,
}

/// Deterministic channel impairments applied to mono samples.
///
/// Processing order: symbol corruption (on the clean, grid-aligned TX
/// signal) → dropout → burst loss → clock drift → converter chain → 16 kHz
/// codec brick wall → low-pass(es) → high-pass → peaking EQ → program
/// bleed → instrument → soft clip → polarity → gain → gate → compressor →
/// limiter → PSU spurs → hiss floor → HF converter model → hum → clicks →
/// leading silence → AWGN → pink noise. Impairments that were added later
/// (burst loss, converters, codec, program bleed, dynamics, pink noise, the
/// instrument sources, spurs, hiss and converter models of the HF profiles)
/// draw their randomness from their own streams and sit in the chain so
/// that a simulation without them is bit-identical to before.
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
    /// The instrument whose signal the tones ride on (HF profiles): kind and
    /// its long-term RMS in dBFS (an absolute level: the tones are placed
    /// by `config.amplitude`, not relative to it).
    pub instrument: Option<(Instrument, f32)>,
    /// Steady spurs (switch-mode power supplies, backlight inverters):
    /// (frequency Hz, level dBFS peak), added at the box input.
    pub spurs: Vec<(f32, f32)>,
    /// A pedalboard noise floor: (RMS dBFS, pink instead of white), added
    /// at the box input.
    pub hiss: Option<(f32, bool)>,
    /// A converter or link model between embedder and box, see
    /// [`Converter`].
    pub converter: Option<Converter>,
    /// Use the windowed-sinc resampler for `drift_ppm` instead of linear
    /// interpolation (which attenuates 20 kHz by up to 12 dB and so is
    /// wrong for the HF profiles).
    pub drift_sinc: bool,
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
            instrument: None,
            spurs: Vec::new(),
            hiss: None,
            converter: None,
            drift_sinc: false,
        }
    }

    /// The tones ride on `kind` at `rms_dbfs` (−18 dBFS is a typical
    /// instrument level with −6 dBFS peaks).
    pub fn instrument(mut self, kind: Instrument, rms_dbfs: f32) -> Self {
        self.instrument = Some((kind, rms_dbfs));
        self
    }

    /// Add a steady spur at `hz`, `dbfs` peak.
    pub fn spur(mut self, hz: f32, dbfs: f32) -> Self {
        self.spurs.push((hz, dbfs));
        self
    }

    /// A white (or pink) noise floor of `rms_dbfs`.
    pub fn hiss(mut self, rms_dbfs: f32, pink: bool) -> Self {
        self.hiss = Some((rms_dbfs, pink));
        self
    }

    pub fn converter(mut self, model: Converter) -> Self {
        self.converter = Some(model);
        self
    }

    /// Receiver clock offset of `ppm` applied with the windowed-sinc
    /// resampler (for the HF profiles; see `drift_sinc`).
    pub fn drift_ppm_sinc(mut self, ppm: f64) -> Self {
        self.drift_ppm = ppm;
        self.drift_sinc = true;
        self
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
            samples = if self.drift_sinc {
                resample_drift_sinc(&samples, self.drift_ppm)
            } else {
                resample_drift(&samples, self.drift_ppm)
            };
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
        if let Some((kind, rms_dbfs)) = self.instrument {
            let music = instrument(
                kind,
                rate,
                samples.len(),
                rms_dbfs,
                self.seed ^ 0x1A57_0000_0000_0004,
            );
            for (s, m) in samples.iter_mut().zip(music) {
                *s += m;
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
        if !self.spurs.is_empty() {
            let mut spur_rng = XorShift64::new(self.seed ^ 0x5B0E_0000_0000_0005);
            for &(hz, dbfs) in &self.spurs {
                let amp = 10f32.powf(dbfs / 20.0);
                let w = 2.0 * PI * hz / rate as f32;
                let phase0 = 2.0 * PI * spur_rng.next_f64() as f32;
                for (i, s) in samples.iter_mut().enumerate() {
                    *s += amp * (w * i as f32 + phase0).sin();
                }
            }
        }
        if let Some((rms_dbfs, pink)) = self.hiss {
            let sigma = 10f32.powf(rms_dbfs / 20.0);
            let mut hiss_rng = XorShift64::new(self.seed ^ 0x4155_0000_0000_0006);
            if pink {
                let floor = pink_noise(samples.len(), sigma, &mut hiss_rng);
                for (s, p) in samples.iter_mut().zip(floor) {
                    *s += p;
                }
            } else {
                for s in samples.iter_mut() {
                    *s += (hiss_rng.gaussian() as f32) * sigma;
                }
            }
        }
        if let Some(model) = self.converter {
            apply_converter(&mut samples, model, rate);
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

/// See [`Converter`].
pub fn apply_converter(samples: &mut [f32], model: Converter, sample_rate_hz: u32) {
    let fs = sample_rate_hz as f32;
    match model {
        // A half-band decimator seen from its own output rate: flat to
        // 0.4535 fs, −6 dB at 0.5 fs. Modelled as a windowed sinc whose
        // transition runs from 0.4535 fs to 0.5 fs.
        Converter::Sharp48 => {
            let taps = fir_lowpass(0.4535 * fs, 0.0465 * fs, 60.0, sample_rate_hz);
            fir_filter(samples, &taps);
        }
        Converter::Slow48 => converter_chain(
            samples,
            Converters {
                stages: 1,
                slow_rolloff: true,
            },
            sample_rate_hz,
        ),
        // The 44.1 kHz device's half-band filters (in and out), in this
        // sample rate's domain: flat to 20.0 kHz, −6 dB at 22.05 kHz each.
        Converter::Hop441 => {
            let taps = fir_lowpass(20_000.0, 2_050.0, 60.0, sample_rate_hz);
            fir_filter(samples, &taps);
            fir_filter(samples, &taps);
        }
        Converter::Src441 => {
            let taps = fir_lowpass(20_000.0, 750.0, 90.0, sample_rate_hz);
            fir_filter(samples, &taps);
        }
        Converter::Codec19Lp8 => brick_wall(samples, 19_000.0, sample_rate_hz),
        Converter::Codec19Wall => {
            let taps = fir_lowpass(19_000.0, 500.0, 60.0, sample_rate_hz);
            fir_filter(samples, &taps);
        }
        // The 48 kHz device's filter seen from 96 kHz: flat to 21.77 kHz,
        // −6 dB at 24 kHz, gone above.
        Converter::Hop96To48 => {
            let taps = fir_lowpass(21_768.0, 2_232.0, 60.0, sample_rate_hz);
            fir_filter(samples, &taps);
        }
    }
}

/// Kaiser-windowed sinc low-pass: `pass_hz` is the end of the passband
/// (≤ −0.1 dB there), the response reaches −6 dB at `pass_hz + transition_hz`
/// and `stop_db` below full scale at `pass_hz + 2 · transition_hz`.
pub fn fir_lowpass(
    pass_hz: f32,
    transition_hz: f32,
    stop_db: f32,
    sample_rate_hz: u32,
) -> Vec<f32> {
    let fs = sample_rate_hz as f64;
    let cutoff = (pass_hz + transition_hz) as f64 / fs; // −6 dB point, cycles per sample
    let width = 2.0 * transition_hz as f64 / fs; // full transition in cycles per sample
    let a = stop_db as f64;
    let beta = if a > 50.0 {
        0.1102 * (a - 8.7)
    } else if a >= 21.0 {
        0.5842 * (a - 21.0).powf(0.4) + 0.07886 * (a - 21.0)
    } else {
        0.0
    };
    let n = (((a - 8.0) / (2.285 * 2.0 * std::f64::consts::PI * width)).ceil() as usize).max(3) | 1;
    let m = (n - 1) as f64 / 2.0;
    let bessel = |x: f64| {
        // I0 by its power series.
        let mut sum = 1.0;
        let mut term = 1.0;
        let q = x * x / 4.0;
        for k in 1..50 {
            term *= q / (k as f64 * k as f64);
            sum += term;
            if term < 1e-12 * sum {
                break;
            }
        }
        sum
    };
    let i0b = bessel(beta);
    let mut taps: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - m;
            let sinc = if t.abs() < 1e-9 {
                2.0 * cutoff
            } else {
                (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t)
            };
            let r = t / m;
            let w = bessel(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
            sinc * w
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    for t in taps.iter_mut() {
        *t /= sum;
    }
    taps.into_iter().map(|t| t as f32).collect()
}

/// In-place FIR filtering (zero-phase alignment is not needed: the
/// receiver finds the symbol clock itself, and a group delay of half the
/// filter is a delay like any other in the chain).
pub fn fir_filter(samples: &mut [f32], taps: &[f32]) {
    let input = samples.to_vec();
    let n = taps.len();
    for (i, out) in samples.iter_mut().enumerate() {
        let mut acc = 0.0f32;
        let start = (i + 1).saturating_sub(n);
        for (j, &x) in input[start..=i].iter().enumerate() {
            // x = input[start + j] pairs with taps[i - (start + j)].
            acc += x * taps[i - start - j];
        }
        *out = acc;
    }
}

/// Energy of `x` below 17 kHz (a windowed-sinc low-pass with 100 dB of
/// stopband from 18 kHz, so tones at 19 kHz and above do not count), dB.
pub fn audible_energy_db(x: &[f32], sample_rate_hz: u32) -> f32 {
    let mut y = x.to_vec();
    let taps = fir_lowpass(17_000.0, 500.0, 100.0, sample_rate_hz);
    fir_filter(&mut y, &taps);
    let e: f64 = y.iter().map(|&v| v as f64 * v as f64).sum();
    10.0 * e.max(1e-30).log10() as f32
}

/// Intermodulation budget of the HF profiles: `tones` added to `program`
/// and both passed through a tanh drive stage `tanh(drive · x) / drive`.
/// Returns the energy of what the tones add to the drive's output *below
/// 17 kHz* (their intermodulation and aliasing products, the part a
/// listener hears), relative to the energy of the driven program below
/// 17 kHz, in dB. The level budget: tones at a level `L` cost roughly
/// `2 L` of residue for a cubic nonlinearity.
pub fn imd_residual_db(program: &[f32], tones: &[f32], drive: f32, sample_rate_hz: u32) -> f32 {
    let n = program.len().min(tones.len());
    let drive_fn = |x: f32| (drive * x).tanh() / drive;
    let clean: Vec<f32> = program[..n].iter().map(|&p| drive_fn(p)).collect();
    let residual: Vec<f32> = program[..n]
        .iter()
        .zip(&tones[..n])
        .zip(&clean)
        .map(|((&p, &t), &c)| drive_fn(p + t) - c)
        .collect();
    audible_energy_db(&residual, sample_rate_hz) - audible_energy_db(&clean, sample_rate_hz)
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

// ---------------------------------------------------------------------------
// Instrument sources (HF profiles). The Karplus–Strong string and the
// strum / lead / chord patterns come from the pedal-command prototype's
// test harness (branch `claude/tender-johnson-pfysx4`); the pickup
// resonances, the band-limited synth, the keys and the cymbals are new.

/// `n` samples of `kind` at long-term RMS `rms_dbfs`, deterministic in
/// `seed`. Peaks are typically 10–14 dB above the RMS.
pub fn instrument(
    kind: Instrument,
    sample_rate_hz: u32,
    n: usize,
    rms_dbfs: f32,
    seed: u64,
) -> Vec<f32> {
    let rate = sample_rate_hz as usize;
    let mut v = match kind {
        Instrument::Guitar => guitar(rate, n, seed),
        Instrument::GuitarBright => guitar_bright(rate, n, seed),
        Instrument::Bass => bass(rate, n, seed),
        Instrument::Synth => synth(rate, n, seed),
        Instrument::Keys => keys(rate, n, seed),
        Instrument::Cymbals => cymbals(rate, n, seed),
    };
    scale_to_rms(&mut v, 10f32.powf(rms_dbfs / 20.0));
    v
}

fn midi_hz(note: u8) -> f32 {
    440.0 * 2f32.powf((note as f32 - 69.0) / 12.0)
}

/// Karplus–Strong string: a white-noise burst (a bright pick, energy up to
/// Nyquist) that the averaging loop darkens over time. Rings for at most
/// 2 s.
fn pluck(
    out: &mut [f32],
    rate: usize,
    start: usize,
    freq: f32,
    amp: f32,
    decay: f32,
    rng: &mut XorShift64,
) {
    if start >= out.len() {
        return;
    }
    let period = ((rate as f32 / freq).round() as usize).max(2);
    let mut buf: Vec<f32> = (0..period)
        .map(|_| rng.next_f64() as f32 * 2.0 - 1.0)
        .collect();
    let mut i = 0;
    for o in out[start..].iter_mut().take(2 * rate) {
        let a = buf[i];
        let b = buf[(i + 1) % period];
        *o += amp * a;
        buf[i] = decay * 0.5 * (a + b);
        i = (i + 1) % period;
    }
}

/// Open chords from low E (82 Hz) up.
const GUITAR_CHORDS: [&[u8]; 6] = [
    &[40, 47, 52, 56, 59, 64],
    &[45, 52, 57, 61, 64],
    &[50, 57, 62, 66],
    &[43, 47, 50, 55, 59, 67],
    &[48, 52, 55, 60, 64],
    &[40, 47, 52, 55, 59, 64],
];

/// Strummed chords and lead lines on a passive pickup: the pickup's
/// inductance and the cable's capacitance form a resonant second-order
/// low-pass (3–5 kHz, Q 2–4, drawn from the seed), so the top of the
/// spectrum falls at 12 dB per octave above it: ~−30 dB at 20 kHz.
fn guitar(rate: usize, n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let resonance_hz = 3_000.0 + 2_000.0 * rng.next_f64() as f32;
    let q = 2.0 + 2.0 * rng.next_f64() as f32;
    let mut out = guitar_strings(rate, n, &mut rng);
    Biquad::lowpass_q(resonance_hz, q, rate as u32).run(&mut out);
    out
}

/// See [`Instrument::GuitarBright`].
fn guitar_bright(rate: usize, n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let mut out = guitar_strings(rate, n, &mut rng);
    one_pole_lowpass(&mut out, 6_000.0, rate as u32);
    out
}

/// The strings and the playing, before any pickup: strummed chords and
/// lead lines.
fn guitar_strings(rate: usize, n: usize, rng: &mut XorShift64) -> Vec<f32> {
    let mut out = vec![0.0f32; n];
    let mut t = 0;
    while t < n {
        if rng.below(4) == 0 {
            for _ in 0..2 + rng.below(5) {
                let note = 60 + rng.below(29) as u8;
                pluck(&mut out, rate, t, midi_hz(note), 0.8, 0.997, rng);
                t += rate / 8 + rng.below(rate / 8);
            }
        } else {
            let chord = GUITAR_CHORDS[rng.below(GUITAR_CHORDS.len())];
            let up = rng.below(2) == 1;
            for k in 0..chord.len() {
                let note = if up {
                    chord[chord.len() - 1 - k]
                } else {
                    chord[k]
                };
                pluck(
                    &mut out,
                    rate,
                    t + k * rate / 100,
                    midi_hz(note),
                    0.5,
                    0.996,
                    rng,
                );
            }
            t += rate / 4 + rng.below(rate / 2);
        }
    }
    out
}

/// Electric bass from E1 (41 Hz) on a pickup resonating at 2.5 kHz, Q 2.
fn bass(rate: usize, n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let mut out = vec![0.0f32; n];
    let mut t = 0;
    while t < n {
        let note = 28 + rng.below(25) as u8;
        pluck(&mut out, rate, t, midi_hz(note), 1.0, 0.995, &mut rng);
        t += rate / 4 + rng.below(rate / 4);
    }
    Biquad::lowpass_q(2_500.0, 2.0, rate as u32).run(&mut out);
    out
}

/// PolyBLEP correction for a sawtooth discontinuity at phase 0 (`t` is the
/// phase in [0, 1), `dt` the phase increment per sample).
fn poly_blep(t: f64, dt: f64) -> f64 {
    if t < dt {
        let t = t / dt;
        t + t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + t + t + 1.0
    } else {
        0.0
    }
}

/// Polysynth chords of band-limited (PolyBLEP) sawtooths with an open
/// filter and 5.5 Hz vibrato: every harmonic up to Nyquist is present at
/// 1 / k and sweeps ±0.4 % with the vibrato, so lines keep crossing the
/// tone slots. Notes last 0.5–2 s with a 10 ms attack and a 100 ms release.
fn synth(rate: usize, n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let mut out = vec![0.0f32; n];
    let mut t = 0;
    while t < n {
        let dur = rate / 2 + rng.below(3 * rate / 2);
        let root = 48 + rng.below(24) as u8;
        let len = dur.min(n - t);
        for note in [root, root + 4, root + 7, root + 11] {
            let f0 = midi_hz(note) as f64;
            let mut phase = rng.next_f64();
            for k in 0..len {
                let env = (k as f32 / (0.01 * rate as f32)).min(1.0)
                    * ((dur - k) as f32 / (0.1 * rate as f32)).min(1.0);
                let lfo = (2.0 * std::f64::consts::PI * 5.5 * (t + k) as f64 / rate as f64).sin();
                let dt = f0 * (1.0 + 0.004 * lfo) / rate as f64;
                phase += dt;
                phase -= phase.floor();
                let saw = 2.0 * phase - 1.0 - poly_blep(phase, dt);
                out[t + k] += 0.25 * env * saw as f32;
            }
        }
        t += dur;
    }
    out
}

/// Electric-piano keys: a fundamental (1.2 s decay), its octave and a
/// fast-fading bell partial at 7.1 × f0, through a 4th-order 9 kHz
/// low-pass (an e-piano, organ or pad has next to nothing above 10 kHz).
fn keys(rate: usize, n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let mut out = vec![0.0f32; n];
    let mut t = 0;
    while t < n {
        let dur = rate * 3 / 10 + rng.below(rate * 12 / 10);
        let len = (dur + rate / 2).min(n - t);
        let voices = 1 + rng.below(4);
        for _ in 0..voices {
            let f0 = midi_hz(48 + rng.below(36) as u8);
            let vel = 0.3 + 0.7 * rng.next_f64() as f32;
            let partials = [(1.0f32, 1.0f32, 1.2f32), (2.0, 0.3, 0.6), (7.1, 0.2, 0.08)];
            for (k, o) in out[t..t + len].iter_mut().enumerate() {
                let s = k as f32 / rate as f32;
                let attack = (s / 0.002).min(1.0);
                // Key release: the note is damped after `dur`.
                let damp = if k > dur {
                    (-((k - dur) as f32) / (0.05 * rate as f32)).exp()
                } else {
                    1.0
                };
                let mut v = 0.0;
                for (mult, amp, tau) in partials {
                    v += amp * (-s / tau).exp() * (2.0 * PI * f0 * mult * s).sin();
                }
                *o += vel * attack * damp * v;
            }
        }
        t += dur;
    }
    for q in [0.5412f32, 1.3066] {
        Biquad::lowpass_q(9_000.0, q, rate as u32).run(&mut out);
    }
    out
}

/// Cymbal and hi-hat hits every 125–500 ms: white noise with exponential
/// decays of 40–250 ms, high-passed at 6 kHz (4th order) with two metallic
/// resonances (+10 dB at 8.5 and 11 kHz, Q 4). Full-band to Nyquist.
fn cymbals(rate: usize, n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let mut out = vec![0.0f32; n];
    let mut i = 0usize;
    while i < n {
        let tau = 0.04 + 0.21 * rng.next_f64() as f32;
        let level = 0.2 + 0.8 * rng.next_f64() as f32;
        let next = (i + rate / 8 + rng.below(3 * rate / 8)).min(n);
        for (k, o) in out[i..next].iter_mut().enumerate() {
            *o = rng.gaussian() as f32 * level * (-(k as f32) / (tau * rate as f32)).exp();
        }
        i = next;
    }
    for q in [0.5412f32, 1.3066] {
        Biquad::highpass_q(6_000.0, q, rate as u32).run(&mut out);
    }
    Biquad::peaking(8_500.0, 10.0, 4.0, rate as u32).run(&mut out);
    Biquad::peaking(11_000.0, 10.0, 4.0, rate as u32).run(&mut out);
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

/// Receiver clock offset of `ppm` (positive = RX reads faster) by
/// Hann-windowed sinc interpolation (64 taps), flat to 22 kHz. Linear
/// interpolation ([`resample_drift`]) attenuates 20 kHz by up to 12 dB
/// and modulates it with the sub-sample phase, so it cannot be used for the
/// HF profiles. (From the pedal-command prototype's test harness.)
pub fn resample_drift_sinc(input: &[f32], ppm: f64) -> Vec<f32> {
    const HALF: i64 = 32;
    let ratio = 1.0 + ppm * 1e-6;
    if input.len() < 2 {
        return input.to_vec();
    }
    let n_out = ((input.len() as f64 - 1.0) / ratio) as usize;
    (0..n_out)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos.floor() as i64;
            let frac = pos - idx as f64;
            let mut acc = 0.0f64;
            for k in -HALF + 1..=HALF {
                let j = idx + k;
                if j < 0 || j >= input.len() as i64 {
                    continue;
                }
                let t = k as f64 - frac;
                let sinc = if t.abs() < 1e-12 {
                    1.0
                } else {
                    (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t)
                };
                let w = 0.5 + 0.5 * (std::f64::consts::PI * t / HALF as f64).cos();
                acc += input[j as usize] as f64 * sinc * w;
            }
            acc as f32
        })
        .collect()
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

    /// Second-order high-pass with quality `q`.
    pub fn highpass_q(cutoff_hz: f32, q: f32, sample_rate_hz: u32) -> Self {
        let w0 = 2.0 * PI * cutoff_hz / sample_rate_hz as f32;
        let alpha = w0.sin() / (2.0 * q);
        let c = w0.cos();
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0],
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
