//! Pedal commands (**experimental prototype**): four short tone signatures in
//! 20.0–21.5 kHz, above guitar, bass and keys, so a footswitch or controller
//! can step a pedal's presets over the instrument cable while the music plays.
//!
//! This is not AML v1 and does not interoperate with it. A command carries no
//! payload, CRC or sequence number: the signature *is* the message, which is
//! what makes it short. Design and measurements:
//! `docs/proposals/pedal-commands.md`.
//!
//! - **TX** ([`PedalTx`]): a command is `L` tone bursts, one tone at a time,
//!   each with raised-cosine edges, followed by one symbol of silence. The
//!   default ([`PedalConfig::standard`]) is 4 bursts of 4 ms from 7 tones
//!   250 Hz apart, 16 ms of signal.
//! - **RX** ([`PedalRx`]): a Butterworth high-pass at 19.5 kHz removes the
//!   music, then a Goertzel bank checks every millisecond whether the last
//!   `L` symbol windows show one command's tones. Tone purity is measured
//!   against the energy of the *filtered* window, so the music does not count
//!   against it. All `L` windows must match (purity and confidence over the
//!   runner-up): without a CRC there is nothing to catch a near miss.
//! - **MIDI** ([`PresetState`]): MIDI has no "program up", so the receiver
//!   keeps the current preset and sends absolute Program Change (and Bank
//!   Select).
//!
//! The whole chain must run at 48 kHz (or higher) and pass 20–21.5 kHz:
//! at 44.1 kHz there is no room above 20 kHz.

use crate::transport::TransportMessage;
use alloc::vec;
use alloc::vec::Vec;

/// Most tones a [`PedalConfig`] may use.
pub const MAX_TONES: usize = 16;

/// The four commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PedalCommand {
    BankUp = 0,
    BankDown = 1,
    ProgramUp = 2,
    ProgramDown = 3,
}

impl PedalCommand {
    /// All commands, in codeword order (see [`PedalConfig::codewords`]).
    pub const ALL: [PedalCommand; 4] = [
        PedalCommand::BankUp,
        PedalCommand::BankDown,
        PedalCommand::ProgramUp,
        PedalCommand::ProgramDown,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    /// `bank-up`, `bank-down`, `program-up`, `program-down`.
    pub fn name(self) -> &'static str {
        match self {
            Self::BankUp => "bank-up",
            Self::BankDown => "bank-down",
            Self::ProgramUp => "program-up",
            Self::ProgramDown => "program-down",
        }
    }

    /// Inverse of [`name`](Self::name); case-insensitive, `_` = `-`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| {
            let want = c.name().as_bytes();
            name.len() == want.len()
                && name.bytes().zip(want.iter()).all(|(a, &b)| {
                    let a = a.to_ascii_lowercase();
                    a == b || (a == b'_' && b == b'-')
                })
        })
    }
}

/// Signal and receiver parameters. Both ends must use the same values.
#[derive(Debug, Clone, PartialEq)]
pub struct PedalConfig {
    pub sample_rate_hz: u32,
    /// Samples per symbol (one tone burst). Tones should be whole multiples
    /// of `sample_rate_hz / symbol_samples` so the Goertzel bins are
    /// orthogonal.
    pub symbol_samples: usize,
    /// Raised-cosine attack and release inside each symbol, in samples.
    pub ramp_samples: usize,
    pub tone_base_hz: f32,
    pub tone_step_hz: f32,
    pub n_tones: usize,
    /// Tone indices of each command, in [`PedalCommand::ALL`] order; all the
    /// same length `L`, pairwise different in every position.
    pub codewords: [Vec<u8>; 4],
    /// Peak amplitude of a burst (full scale = 1.0).
    pub amplitude: f32,
    /// Silent symbols after every command.
    pub gap_symbols: usize,
    /// RX: Butterworth high-pass cutoff and (even) order.
    pub highpass_hz: f32,
    pub highpass_order: usize,
    /// RX: detection step; must divide `symbol_samples`.
    pub hop_samples: usize,
    /// RX: minimum tone purity of every symbol window, against the energy of
    /// the high-passed window (1.0 = a pure steady tone).
    pub min_purity: f32,
    /// RX: minimum power of the expected tone over the runner-up, dB.
    pub min_confidence_db: f32,
    /// RX: high-passed windows quieter than this RMS (dBFS) are never
    /// detected.
    pub min_rms_dbfs: f32,
    /// RX: symbol windows of a command that may show *no* tone (a dropout,
    /// a click) while the rest match. A window showing a different tone
    /// always rejects. 0 (the default) requires every symbol.
    pub max_misses: usize,
}

impl Default for PedalConfig {
    fn default() -> Self {
        Self::standard()
    }
}

impl PedalConfig {
    /// The default: 4 bursts of 4 ms (16 ms per command), 7 tones
    /// 20.0–21.5 kHz, 250 Hz apart, 1 ms raised-cosine edges.
    pub fn standard() -> Self {
        Self {
            sample_rate_hz: 48_000,
            symbol_samples: 192,
            ramp_samples: 48,
            tone_base_hz: 20_000.0,
            tone_step_hz: 250.0,
            n_tones: 7,
            // Chosen by search: every pair differs in every position, at
            // least 3 tones (750 Hz) between neighbours, no codeword runs
            // monotonically (a sweep cannot match it), a codeword shifted by
            // 1–3 symbols agrees with any codeword in at most one position,
            // and every tone is used 2 or 3 times.
            codewords: [
                vec![0, 3, 6, 1],
                vec![1, 6, 3, 0],
                vec![2, 5, 0, 4],
                vec![4, 1, 5, 2],
            ],
            amplitude: 0.1,
            gap_symbols: 1,
            highpass_hz: 19_500.0,
            highpass_order: 8,
            hop_samples: 48,
            min_purity: 0.5,
            min_confidence_db: 3.0,
            min_rms_dbfs: -100.0,
            max_misses: 0,
        }
    }

    /// [`standard`](Self::standard) with 3 bursts per command (12 ms).
    pub fn three_symbols() -> Self {
        Self {
            codewords: [vec![0, 4, 1], vec![1, 5, 0], vec![2, 6, 3], vec![6, 2, 5]],
            ..Self::standard()
        }
    }

    /// 4 bursts of 3 ms (12 ms per command): 5 tones 333 Hz apart
    /// (20.0–21.33 kHz), 0.75 ms edges.
    pub fn short_symbols() -> Self {
        Self {
            symbol_samples: 144,
            ramp_samples: 36,
            tone_step_hz: 48_000.0 / 144.0,
            n_tones: 5,
            codewords: [
                vec![0, 3, 1, 4],
                vec![1, 4, 0, 2],
                vec![2, 0, 4, 1],
                vec![4, 1, 3, 0],
            ],
            hop_samples: 36,
            ..Self::standard()
        }
    }

    /// `standard`, `three_symbols` / `3sym`, `short_symbols` / `short`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "standard" => Some(Self::standard()),
            "three_symbols" | "3sym" => Some(Self::three_symbols()),
            "short_symbols" | "short" => Some(Self::short_symbols()),
            _ => None,
        }
    }

    /// Symbols per command (`L`).
    pub fn codeword_len(&self) -> usize {
        self.codewords[0].len()
    }

    pub fn tone_hz(&self, index: u8) -> f32 {
        self.tone_base_hz + self.tone_step_hz * index as f32
    }

    /// Samples of signal per command (without the trailing silence).
    pub fn command_samples(&self) -> usize {
        self.codeword_len() * self.symbol_samples
    }

    /// Samples one command holds the line, trailing silence included.
    pub fn total_samples(&self) -> usize {
        (self.codeword_len() + self.gap_symbols) * self.symbol_samples
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let l = self.codeword_len();
        if self.sample_rate_hz == 0 || self.symbol_samples < 8 {
            return Err("sample rate and symbol length must be set");
        }
        if self.n_tones < 4 || self.n_tones > MAX_TONES {
            return Err("n_tones must be 4..=16");
        }
        let top = self.tone_hz(self.n_tones as u8 - 1);
        if self.tone_base_hz <= 0.0 || top >= self.sample_rate_hz as f32 / 2.0 {
            return Err("tones must lie between 0 and Nyquist");
        }
        if 2 * self.ramp_samples > self.symbol_samples {
            return Err("ramps longer than the symbol");
        }
        if self.hop_samples == 0 || !self.symbol_samples.is_multiple_of(self.hop_samples) {
            return Err("hop_samples must divide symbol_samples");
        }
        if self.highpass_order < 2 || !self.highpass_order.is_multiple_of(2) {
            return Err("highpass_order must be even and >= 2");
        }
        if l < 2 {
            return Err("codewords need at least 2 symbols");
        }
        if self.max_misses >= l - 1 {
            return Err("max_misses must leave at least 2 symbols to match");
        }
        for (i, c) in self.codewords.iter().enumerate() {
            if c.len() != l {
                return Err("codewords must have the same length");
            }
            if c.iter().any(|&t| t as usize >= self.n_tones) {
                return Err("codeword tone out of range");
            }
            if c.windows(2).any(|w| w[0] == w[1]) {
                return Err("neighbouring symbols must use different tones");
            }
            for other in &self.codewords[i + 1..] {
                if c.iter().zip(other).any(|(a, b)| a == b) {
                    return Err("codewords must differ in every position");
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TX

/// Synthesizes commands.
#[derive(Debug, Clone)]
pub struct PedalTx {
    config: PedalConfig,
    envelope: Vec<f32>,
}

impl PedalTx {
    /// Panics if `config` is invalid (see [`PedalConfig::validate`]).
    pub fn new(config: PedalConfig) -> Self {
        config.validate().expect("invalid PedalConfig");
        let envelope = symbol_envelope(config.symbol_samples, config.ramp_samples);
        Self { config, envelope }
    }

    pub fn config(&self) -> &PedalConfig {
        &self.config
    }

    /// One command: `L` bursts and then `gap_symbols` of silence
    /// ([`PedalConfig::total_samples`] samples).
    pub fn synthesize(&self, command: PedalCommand) -> Vec<f32> {
        let c = &self.config;
        let mut out = Vec::with_capacity(c.total_samples());
        for &tone in &c.codewords[command.index()] {
            let w = 2.0 * core::f64::consts::PI * c.tone_hz(tone) as f64 / c.sample_rate_hz as f64;
            for (i, &env) in self.envelope.iter().enumerate() {
                out.push(c.amplitude * env * libm::sin(w * i as f64) as f32);
            }
        }
        out.resize(c.total_samples(), 0.0);
        out
    }
}

/// Flat top with raised-cosine attack and release of `ramp` samples each.
fn symbol_envelope(n: usize, ramp: usize) -> Vec<f32> {
    let mut env = vec![1.0f32; n];
    for i in 0..ramp {
        let v = 0.5 - 0.5 * libm::cos(core::f64::consts::PI * (i as f64 + 0.5) / ramp as f64);
        env[i] = v as f32;
        env[n - 1 - i] = v as f32;
    }
    env
}

// ---------------------------------------------------------------------------
// Filters

/// A Butterworth high- or low-pass as a cascade of biquads (bilinear
/// transform, prewarped at the cutoff), f32 state.
#[derive(Debug, Clone)]
pub struct Butterworth {
    sections: Vec<Biquad>,
}

#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Butterworth {
    /// High-pass of even `order` at `cutoff_hz`.
    pub fn highpass(order: usize, cutoff_hz: f32, sample_rate_hz: u32) -> Self {
        Self::design(order, cutoff_hz, sample_rate_hz, true)
    }

    /// Low-pass of even `order` at `cutoff_hz`.
    pub fn lowpass(order: usize, cutoff_hz: f32, sample_rate_hz: u32) -> Self {
        Self::design(order, cutoff_hz, sample_rate_hz, false)
    }

    fn design(order: usize, cutoff_hz: f32, sample_rate_hz: u32, high: bool) -> Self {
        assert!(order >= 2 && order.is_multiple_of(2), "order must be even");
        let w0 = 2.0 * core::f64::consts::PI * cutoff_hz as f64 / sample_rate_hz as f64;
        let (sin, cos) = (libm::sin(w0), libm::cos(w0));
        let sections = (0..order / 2)
            .map(|k| {
                let q = 1.0
                    / (2.0
                        * libm::cos(
                            core::f64::consts::PI * (2 * k + 1) as f64 / (2 * order) as f64,
                        ));
                let alpha = sin / (2.0 * q);
                let a0 = 1.0 + alpha;
                let (b0, b1) = if high {
                    ((1.0 + cos) / 2.0, -(1.0 + cos))
                } else {
                    ((1.0 - cos) / 2.0, 1.0 - cos)
                };
                Biquad {
                    b: [(b0 / a0) as f32, (b1 / a0) as f32, (b0 / a0) as f32],
                    a: [(-2.0 * cos / a0) as f32, ((1.0 - alpha) / a0) as f32],
                    z: [0.0; 2],
                }
            })
            .collect();
        Self { sections }
    }

    /// Filter one sample.
    pub fn process(&mut self, x: f32) -> f32 {
        let mut v = x;
        for s in self.sections.iter_mut() {
            // Transposed direct form II.
            let y = s.b[0] * v + s.z[0];
            s.z[0] = s.b[1] * v - s.a[0] * y + s.z[1];
            s.z[1] = s.b[2] * v - s.a[1] * y;
            v = y;
        }
        v
    }

    /// Filter `samples` in place.
    pub fn run(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            *s = self.process(*s);
        }
    }

    pub fn reset(&mut self) {
        for s in self.sections.iter_mut() {
            s.z = [0.0; 2];
        }
    }
}

// ---------------------------------------------------------------------------
// RX

/// A detected command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PedalDetection {
    pub command: PedalCommand,
    /// Input samples consumed when the command was recognized (the end of
    /// its last symbol window).
    pub at_sample: u64,
}

/// Receiver counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PedalRxStats {
    /// Symbol windows evaluated (one per hop).
    pub windows: u64,
    /// Windows that showed some tone through every gate.
    pub gated_windows: u64,
    /// `gated_windows` by tone.
    pub gated_per_tone: [u64; MAX_TONES],
    pub detections: u64,
}

/// Streaming command detector. Feed it the instrument signal as it arrives.
#[derive(Debug, Clone)]
pub struct PedalRx {
    config: PedalConfig,
    filter: Butterworth,
    coeffs: Vec<f32>,
    /// Ring of the last `symbol_samples` high-passed samples; `write` is the
    /// oldest.
    window: Vec<f32>,
    write: usize,
    pos: u64,
    /// Samples until the next observation.
    hop_left: usize,
    /// Ring of per-hop observations (gated tone or none), newest at
    /// `obs_next - 1`.
    obs: Vec<Option<u8>>,
    obs_next: usize,
    obs_count: u64,
    hops_per_symbol: usize,
    quiet_until: u64,
    min_energy: f32,
    confidence_ratio: f32,
    stats: PedalRxStats,
}

impl PedalRx {
    /// Panics if `config` is invalid (see [`PedalConfig::validate`]).
    pub fn new(config: PedalConfig) -> Self {
        config.validate().expect("invalid PedalConfig");
        let n = config.symbol_samples;
        let coeffs = (0..config.n_tones)
            .map(|t| {
                let w = 2.0 * core::f64::consts::PI * config.tone_hz(t as u8) as f64
                    / config.sample_rate_hz as f64;
                (2.0 * libm::cos(w)) as f32
            })
            .collect();
        let hops_per_symbol = n / config.hop_samples;
        let obs_len = (config.codeword_len() - 1) * hops_per_symbol + 1;
        Self {
            filter: Butterworth::highpass(
                config.highpass_order,
                config.highpass_hz,
                config.sample_rate_hz,
            ),
            coeffs,
            window: vec![0.0; n],
            write: 0,
            pos: 0,
            hop_left: n,
            obs: vec![None; obs_len],
            obs_next: 0,
            obs_count: 0,
            hops_per_symbol,
            quiet_until: 0,
            min_energy: libm::powf(10.0, config.min_rms_dbfs / 10.0) * n as f32,
            confidence_ratio: libm::powf(10.0, config.min_confidence_db / 10.0),
            stats: PedalRxStats::default(),
            config,
        }
    }

    pub fn config(&self) -> &PedalConfig {
        &self.config
    }

    pub fn stats(&self) -> &PedalRxStats {
        &self.stats
    }

    /// Feed one sample; returns a command when one has just been recognized.
    /// No allocation.
    pub fn push_sample(&mut self, x: f32) -> Option<PedalDetection> {
        let y = self.filter.process(x);
        self.window[self.write] = y;
        self.write += 1;
        if self.write == self.window.len() {
            self.write = 0;
        }
        self.pos += 1;
        self.hop_left -= 1;
        if self.hop_left > 0 {
            return None;
        }
        self.hop_left = self.config.hop_samples;

        let o = self.observe();
        self.obs[self.obs_next] = o;
        self.obs_next = (self.obs_next + 1) % self.obs.len();
        self.obs_count += 1;
        if self.obs_count < self.obs.len() as u64 || self.pos < self.quiet_until {
            return None;
        }
        // The window that ended `k` symbols ago is `k * hops_per_symbol`
        // observations back; the ring holds exactly the oldest one needed.
        let l = self.config.codeword_len();
        let len = self.obs.len();
        let newest = self.obs_next + len - 1;
        let command = PedalCommand::ALL.into_iter().find(|c| {
            let mut misses = 0;
            for (j, &tone) in self.config.codewords[c.index()].iter().enumerate() {
                let back = (l - 1 - j) * self.hops_per_symbol;
                match self.obs[(newest - back) % len] {
                    Some(t) if t == tone => {}
                    Some(_) => return false,
                    None => misses += 1,
                }
            }
            misses <= self.config.max_misses
        })?;
        // One command per codeword: nearby hops that still match are the
        // same transmission.
        self.quiet_until = self.pos + self.config.command_samples() as u64;
        self.stats.detections += 1;
        Some(PedalDetection {
            command,
            at_sample: self.pos,
        })
    }

    /// Feed a block of samples; returns the commands recognized in it.
    pub fn push_samples(&mut self, samples: &[f32]) -> Vec<PedalDetection> {
        samples
            .iter()
            .filter_map(|&x| self.push_sample(x))
            .collect()
    }

    /// The tone the current window shows through every gate, if any.
    fn observe(&mut self) -> Option<u8> {
        let n_tones = self.config.n_tones;
        let mut q1 = [0.0f32; MAX_TONES];
        let mut q2 = [0.0f32; MAX_TONES];
        let (q1s, q2s, coeffs) = (&mut q1[..n_tones], &mut q2[..n_tones], &self.coeffs[..]);
        let mut energy = 0.0f32;
        let (newer, older) = self.window.split_at(self.write);
        for &x in older.iter().chain(newer) {
            energy += x * x;
            for ((a, b), &c) in q1s.iter_mut().zip(q2s.iter_mut()).zip(coeffs) {
                let q0 = c * *a - *b + x;
                *b = *a;
                *a = q0;
            }
        }
        self.stats.windows += 1;
        if energy < self.min_energy {
            return None;
        }
        let (mut best, mut best_p, mut runner) = (0usize, 0.0f32, 0.0f32);
        for t in 0..n_tones {
            let p = q1[t] * q1[t] + q2[t] * q2[t] - q1[t] * q2[t] * self.coeffs[t];
            if p > best_p {
                runner = best_p;
                best_p = p;
                best = t;
            } else if p > runner {
                runner = p;
            }
        }
        let purity = best_p / (energy * self.window.len() as f32 * 0.5);
        if purity < self.config.min_purity || best_p < runner * self.confidence_ratio {
            return None;
        }
        self.stats.gated_windows += 1;
        self.stats.gated_per_tone[best] += 1;
        Some(best as u8)
    }
}

// ---------------------------------------------------------------------------
// MIDI

/// How a preset number becomes MIDI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BankMode {
    /// Program Change = `bank × programs_per_bank + slot` (at most 128
    /// presets in all).
    Flat,
    /// Bank Select MSB (CC 0) = bank, then Program Change = slot.
    BankSelect,
}

/// The receiving pedal's preset layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetMap {
    /// MIDI channel 0–15 (MIDI channel 1 is `0`).
    pub channel: u8,
    pub banks: u8,
    pub programs_per_bank: u8,
    pub mode: BankMode,
}

impl Default for PresetMap {
    /// Channel 1, 32 banks of 4, flat Program Change 0–127.
    fn default() -> Self {
        Self {
            channel: 0,
            banks: 32,
            programs_per_bank: 4,
            mode: BankMode::Flat,
        }
    }
}

impl PresetMap {
    pub fn presets(&self) -> u16 {
        self.banks as u16 * self.programs_per_bank as u16
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.channel > 15 {
            return Err("channel must be 0..=15");
        }
        if self.banks == 0 || self.programs_per_bank == 0 {
            return Err("banks and programs_per_bank must be >= 1");
        }
        if self.banks > 128 || self.programs_per_bank > 128 {
            return Err("banks and programs_per_bank must be <= 128");
        }
        if self.mode == BankMode::Flat && self.presets() > 128 {
            return Err("flat mode holds at most 128 presets");
        }
        Ok(())
    }
}

/// The current preset, stepped by commands.
///
/// `ProgramUp`/`ProgramDown` step through all presets (crossing into the
/// next or previous bank); `BankUp`/`BankDown` keep the slot and change the
/// bank. Both wrap around. A missed or doubled command leaves the state off
/// by one until [`set_index`](Self::set_index) restores it from an absolute
/// source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetState {
    map: PresetMap,
    index: u16,
}

impl PresetState {
    pub fn new(map: PresetMap) -> Result<Self, &'static str> {
        map.validate()?;
        Ok(Self { map, index: 0 })
    }

    pub fn map(&self) -> &PresetMap {
        &self.map
    }

    /// Preset number `0..presets()`.
    pub fn index(&self) -> u16 {
        self.index
    }

    pub fn bank(&self) -> u8 {
        (self.index / self.map.programs_per_bank as u16) as u8
    }

    pub fn slot(&self) -> u8 {
        (self.index % self.map.programs_per_bank as u16) as u8
    }

    /// Step by `command`; returns the MIDI to send for the new preset.
    pub fn apply(&mut self, command: PedalCommand) -> Vec<TransportMessage> {
        let n = self.map.presets();
        let per_bank = self.map.programs_per_bank as u16 % n;
        let step = match command {
            PedalCommand::ProgramUp => 1 % n,
            PedalCommand::ProgramDown => n - 1 % n,
            PedalCommand::BankUp => per_bank,
            PedalCommand::BankDown => (n - per_bank) % n,
        };
        self.index = (self.index + step) % n;
        self.messages()
    }

    /// Jump to an absolute preset (e.g. from a resync message); `None` if
    /// out of range.
    pub fn set_index(&mut self, index: u16) -> Option<Vec<TransportMessage>> {
        if index >= self.map.presets() {
            return None;
        }
        self.index = index;
        Some(self.messages())
    }

    /// MIDI that selects the current preset.
    pub fn messages(&self) -> Vec<TransportMessage> {
        let channel = self.map.channel;
        match self.map.mode {
            BankMode::Flat => vec![TransportMessage::ProgramChange {
                channel,
                program: self.index as u8,
            }],
            BankMode::BankSelect => vec![
                TransportMessage::ControlChange {
                    channel,
                    controller: 0,
                    value: self.bank(),
                },
                TransportMessage::ProgramChange {
                    channel,
                    program: self.slot(),
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_valid() {
        for c in [
            PedalConfig::standard(),
            PedalConfig::three_symbols(),
            PedalConfig::short_symbols(),
        ] {
            c.validate().unwrap();
        }
    }

    #[test]
    fn command_names_round_trip() {
        for c in PedalCommand::ALL {
            assert_eq!(PedalCommand::from_name(c.name()), Some(c));
        }
        assert_eq!(
            PedalCommand::from_name("Bank_Up"),
            Some(PedalCommand::BankUp)
        );
        assert_eq!(PedalCommand::from_name("bank"), None);
    }

    #[test]
    fn validate_rejects_codewords_sharing_a_position() {
        let mut c = PedalConfig::standard();
        c.codewords[1][2] = c.codewords[0][2];
        assert!(c.validate().is_err());
    }

    #[test]
    fn each_command_decodes_alone_with_bounded_latency() {
        for config in [
            PedalConfig::standard(),
            PedalConfig::three_symbols(),
            PedalConfig::short_symbols(),
        ] {
            let tx = PedalTx::new(config.clone());
            for cmd in PedalCommand::ALL {
                let mut audio = vec![0.0f32; 1000];
                audio.extend(tx.synthesize(cmd));
                audio.extend(vec![0.0f32; 1000]);
                let mut rx = PedalRx::new(config.clone());
                let got = rx.push_samples(&audio);
                assert_eq!(got.len(), 1, "{cmd:?}: {got:?}");
                assert_eq!(got[0].command, cmd);
                let latency = got[0].at_sample - 1000;
                let signal = config.command_samples() as u64;
                assert!(
                    latency <= signal + config.hop_samples as u64,
                    "{cmd:?}: {latency} samples for {signal} of signal"
                );
            }
        }
    }

    #[test]
    fn max_misses_bridges_one_muted_symbol_only() {
        let config = PedalConfig::standard();
        let tx = PedalTx::new(config.clone());
        let n = config.symbol_samples;
        let mut one = vec![0.0f32; 1000];
        one.extend(tx.synthesize(PedalCommand::ProgramUp));
        one.extend(vec![0.0f32; 1000]);
        let mut two = one.clone();
        one[1000 + n..1000 + 2 * n].fill(0.0);
        two[1000 + n..1000 + 3 * n].fill(0.0);
        let detect = |misses: usize, x: &[f32]| {
            let mut rx = PedalRx::new(PedalConfig {
                max_misses: misses,
                ..config.clone()
            });
            rx.push_samples(x)
                .iter()
                .map(|d| d.command)
                .collect::<Vec<_>>()
        };
        assert!(detect(0, &one).is_empty());
        assert_eq!(detect(1, &one), vec![PedalCommand::ProgramUp]);
        assert!(detect(1, &two).is_empty());
        let mut c = config.clone();
        c.max_misses = 3;
        assert!(c.validate().is_err());
    }

    #[test]
    fn highpass_passes_band_and_rejects_music() {
        let gain = |f: f32| {
            let mut hp = Butterworth::highpass(8, 19_500.0, 48_000);
            let x: Vec<f32> = (0..4800)
                .map(|i| libm::sinf(2.0 * core::f32::consts::PI * f * i as f32 / 48_000.0))
                .collect();
            let mut y = x.clone();
            hp.run(&mut y);
            let e = |s: &[f32]| s[2400..].iter().map(|v| v * v).sum::<f32>();
            10.0 * libm::log10f(e(&y) / e(&x))
        };
        // Analytic: -0.6 dB at 20 kHz, -55 dB at 15 kHz, -158 dB at 5 kHz
        // (f32 arithmetic floors the last one near -90 dB).
        assert!(gain(20_000.0) > -1.0, "{}", gain(20_000.0));
        assert!(gain(21_500.0) > -0.5, "{}", gain(21_500.0));
        assert!(gain(15_000.0) < -50.0, "{}", gain(15_000.0));
        assert!(gain(5_000.0) < -80.0, "{}", gain(5_000.0));
    }

    #[test]
    fn preset_state_steps_and_wraps() {
        let mut s = PresetState::new(PresetMap::default()).unwrap();
        assert_eq!(
            s.apply(PedalCommand::ProgramDown),
            vec![TransportMessage::ProgramChange {
                channel: 0,
                program: 127
            }]
        );
        s.apply(PedalCommand::ProgramUp);
        s.apply(PedalCommand::BankUp);
        s.apply(PedalCommand::ProgramUp);
        assert_eq!((s.index(), s.bank(), s.slot()), (5, 1, 1));
        s.apply(PedalCommand::BankDown);
        s.apply(PedalCommand::BankDown);
        assert_eq!((s.index(), s.bank(), s.slot()), (125, 31, 1));
        assert_eq!(s.set_index(128), None);
        assert!(s.set_index(7).is_some());
        assert_eq!(s.index(), 7);
    }

    #[test]
    fn bank_select_mode_sends_cc0_then_slot() {
        let map = PresetMap {
            channel: 2,
            banks: 100,
            programs_per_bank: 3,
            mode: BankMode::BankSelect,
        };
        let mut s = PresetState::new(map).unwrap();
        s.apply(PedalCommand::BankUp);
        assert_eq!(
            s.apply(PedalCommand::ProgramUp),
            vec![
                TransportMessage::ControlChange {
                    channel: 2,
                    controller: 0,
                    value: 1
                },
                TransportMessage::ProgramChange {
                    channel: 2,
                    program: 1
                },
            ]
        );
        assert!(PresetState::new(PresetMap {
            mode: BankMode::Flat,
            ..map
        })
        .is_err());
    }

    #[test]
    fn single_preset_map_does_not_panic() {
        let map = PresetMap {
            channel: 0,
            banks: 1,
            programs_per_bank: 1,
            mode: BankMode::Flat,
        };
        let mut s = PresetState::new(map).unwrap();
        for c in PedalCommand::ALL {
            s.apply(c);
            assert_eq!(s.index(), 0);
        }
    }
}
