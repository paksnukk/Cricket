//! Parameters of the AML v1 MFSK phy (spec §3, §6.1–6.4, §6.6): the tone
//! plan, symbol timing and envelope, preamble and the receiver's detection
//! gates. The synthesizer is [`ModemTx`], the receiver [`ModemRx`].

use super::config::PhyConfig;
use super::demod::ModemRx;
use super::synth::ModemTx;
use super::traits::{BodyCoding, PhyFactory, PhyRx, PhyTx, RefreshPolicy};
use alloc::boxed::Box;
use alloc::vec::Vec;

/// How the MFSK receiver finds a preamble (receiver-side only; the wire
/// format is the same either way). Spec §3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Acquisition {
    /// Spec §3.2.1: gate on `preamble[0]` alone, fine-align on its tone, then
    /// confirm the remaining preamble symbols in order. Used by Fast (whose
    /// 2-symbol preamble is too short for joint detection).
    #[default]
    FirstSymbol,
    /// Spec §3.2.2: joint detection over the whole preamble
    /// (docs/proposals/fec.md §10): a search window is a candidate when at
    /// least `N - misses` preamble symbols show their expected tone, fine
    /// alignment maximizes the summed expected-tone power of all `N` symbols,
    /// and the lock is accepted when at most `misses` symbols (anywhere,
    /// including the first) fail the detection gate and at most one of those
    /// shows a clear *different* tone. Robust uses 2 misses (its coded body +
    /// CRC-16 verify the lock), Robust repeat 1.
    Joint { misses: u8 },
}

/// Tone-purity gate (see `goertzel::tone_purity`), measured on the tone-band
/// filtered input ([`MfskParams::rx_band_hz`]): ~0.97 for an aligned
/// synthesized symbol, ≥ 0.4 at a full search hop of misalignment, ~0.6 for
/// a symbol in white noise at −6 dB full-band SNR. Tuned on the MFSK
/// training table (`tests/fec.rs`, `mfsk_ab_table`): 0.2 is the lowest
/// value at which white noise and music-like audio still produce no phy
/// lock for any profile (at 0.15: robust 2, fast 13 locks per minute on
/// white noise); each step of 0.05 above it costs about 1 dB of AWGN cliff.
pub const PURITY_MFSK: f32 = 0.2;

/// Orthogonal tone step of an MFSK symbol of `symbol_duration_ms`: 1 / T
/// (spec §3.1), so that every other tone falls on a zero of a tone's
/// rectangular-window spectrum and leaks nothing into its Goertzel bin.
/// 6 ms → 166⅔ Hz, 3 ms → 333⅓ Hz.
pub fn orthogonal_step_hz(symbol_duration_ms: f32) -> f32 {
    1000.0 / symbol_duration_ms
}

/// Receiver band of an MFSK tone plan ([`MfskParams::rx_band_hz`]): from
/// `RX_BAND_LOW` × the lowest tone to `RX_BAND_HIGH` × the highest, so
/// the second-order band edges cost the outer tones less than 1 dB while
/// hum and rumble (≤ 300 Hz) are 40–60 dB down and hiss above the band is
/// gone.
pub(super) fn mfsk_rx_band(base_hz: f32, step_hz: f32, n_tones: usize) -> (f32, f32) {
    let top = base_hz + step_hz * (n_tones.max(1) - 1) as f32;
    (RX_BAND_LOW * base_hz, RX_BAND_HIGH * top)
}
const RX_BAND_LOW: f32 = 0.7;
const RX_BAND_HIGH: f32 = 1.3;

/// Parameters of the MFSK phy. The wire parameters (spec §3.1) are the
/// symbol timing and envelope, the tone plan and the preamble; the rest
/// (`min_*`, `rx_*`, `acquisition`) tune the reference receiver. Start
/// from a profile (`PhyConfig::robust().mfsk()` …) and change fields; the
/// struct may grow, so it cannot be built with a literal outside the crate.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct MfskParams {
    /// Tone duration, ms (6 on Robust, 3 on Fast).
    pub symbol_duration_ms: f32,
    /// Silence between symbols, ms.
    pub guard_gap_ms: f32,
    /// Envelope attack, ms.
    pub attack_ms: f32,
    /// Envelope release, ms.
    pub release_ms: f32,
    /// Symbol envelope: raised-cosine attack and release (the HF profiles,
    /// spec §6.6, whose leakage into the audible band must stay far under
    /// the tones) instead of the linear ramps of spec §3.1 (the baseband
    /// profiles). Wire format.
    pub raised_cosine_ramps: bool,
    /// Lowest tone, Hz.
    pub tone_base_hz: f32,
    /// Tone spacing, Hz ([`orthogonal_step_hz`] of the symbol duration).
    pub tone_step_hz: f32,
    /// Tones in the alphabet: 16 (4 bits per symbol) or 8 (3 bits,
    /// [`super::pack`]).
    pub n_tones: usize,
    /// Preamble, as tone indices.
    pub preamble: Vec<u8>,
    /// Detection gate: best tone over the runner-up, dB.
    pub min_confidence_db: f32,
    /// Minimum level-independent tone purity (Goertzel power normalized by
    /// window energy, see `goertzel::tone_purity`; 1.0 = pure steady tone).
    pub min_tone_purity: f32,
    /// Receiver only (spec §3.2): the receiver band-passes its input to
    /// `(low, high)` Hz (Butterworth each side) before any detection, so
    /// window energy, silence floor and tone purity are measured in the
    /// tone band only. `None` disables the filter.
    pub rx_band_hz: Option<(f32, f32)>,
    /// Receiver only: Butterworth order of each side of the
    /// [`rx_band_hz`](Self::rx_band_hz) filter (even, 2–8). 2 on the
    /// baseband profiles (spec §3.2); 8 on the HF profiles (spec §6.6),
    /// whose tones sit a few kHz above an instrument 30–60 dB stronger.
    pub rx_band_order: u8,
    /// Receiver only (spec §3.2.3): re-centre the symbol clock on every
    /// body symbol that passes the detection gate (±3 / ±6 samples), so
    /// the body survives a sample-rate mismatch of up to ~2 % between the
    /// ends. Costs four single-tone Goertzels per body symbol.
    pub rx_track_timing: bool,
    /// Preamble acquisition (spec §3.2).
    pub acquisition: Acquisition,
}

impl MfskParams {
    /// Samples of one tone at `sample_rate_hz`.
    pub fn symbol_samples(&self, sample_rate_hz: u32) -> usize {
        libm::roundf((self.symbol_duration_ms / 1000.0) * sample_rate_hz as f32) as usize
    }

    /// Samples of the guard gap at `sample_rate_hz`.
    pub fn gap_samples(&self, sample_rate_hz: u32) -> usize {
        libm::roundf((self.guard_gap_ms / 1000.0) * sample_rate_hz as f32) as usize
    }

    /// Tone plus gap, in samples.
    pub fn samples_per_symbol_period(&self, sample_rate_hz: u32) -> usize {
        self.symbol_samples(sample_rate_hz) + self.gap_samples(sample_rate_hz)
    }

    /// Search hop of the receiver: a quarter symbol.
    pub fn hop_samples(&self, sample_rate_hz: u32) -> usize {
        (self.symbol_samples(sample_rate_hz) / 4).max(1)
    }

    /// Frequency of tone `index`.
    pub fn tone_hz(&self, index: u8) -> f32 {
        self.tone_base_hz + (index as f32) * self.tone_step_hz
    }

    /// All tones of the plan.
    pub fn tone_table(&self) -> Vec<f32> {
        (0..self.n_tones as u8).map(|i| self.tone_hz(i)).collect()
    }

    /// Bits one symbol carries: 4 with 16 tones, 3 with 8 ([`super::pack`]).
    pub fn bits_per_symbol(&self) -> u32 {
        super::pack::bits_per_symbol(self.n_tones)
    }

    /// Symbols of a body of `body_nibbles` nibbles on the wire (22 → 30 on
    /// an 8-tone plan; equal to the nibble count on 16 tones).
    pub fn body_symbols(&self, body_nibbles: usize) -> usize {
        super::pack::symbols_for_nibbles(body_nibbles, self.bits_per_symbol())
    }

    /// The receiver band ([`rx_band_hz`](Self::rx_band_hz)) that fits this
    /// tone plan, or `None` without tones.
    pub fn default_rx_band_hz(&self) -> Option<(f32, f32)> {
        (self.n_tones > 0).then(|| mfsk_rx_band(self.tone_base_hz, self.tone_step_hz, self.n_tones))
    }
}

impl PhyFactory for MfskParams {
    fn name(&self) -> &str {
        "mfsk"
    }

    fn clone_box(&self) -> Box<dyn PhyFactory> {
        Box::new(self.clone())
    }

    fn tx(&self, config: &PhyConfig) -> Box<dyn PhyTx> {
        Box::new(ModemTx::new(config.clone()))
    }

    fn rx(&self, config: &PhyConfig) -> Box<dyn PhyRx> {
        Box::new(ModemRx::new(config.clone()))
    }

    fn body_coding(&self, config: &PhyConfig) -> BodyCoding {
        BodyCoding::from_fec(config.fec)
    }

    /// Preamble and body symbol periods.
    fn frame_samples(&self, config: &PhyConfig, body_nibbles: usize) -> usize {
        (self.preamble.len() + self.body_symbols(body_nibbles))
            * self.samples_per_symbol_period(config.sample_rate_hz)
    }

    /// One symbol period of silence after each burst.
    fn gap_samples(&self, config: &PhyConfig) -> usize {
        self.samples_per_symbol_period(config.sample_rate_hz)
    }

    fn symbol_duration_ms(&self, _config: &PhyConfig) -> f32 {
        self.symbol_duration_ms
    }

    fn guard_gap_ms(&self, _config: &PhyConfig) -> f32 {
        self.guard_gap_ms
    }

    fn bits_per_symbol(&self, _config: &PhyConfig) -> u32 {
        self.bits_per_symbol()
    }

    fn first_copy_ms(&self, _config: &PhyConfig, body_nibbles: usize) -> f32 {
        let symbols = self.preamble.len() + self.body_symbols(body_nibbles);
        symbols as f32 * (self.symbol_duration_ms + self.guard_gap_ms)
    }

    /// The Fibonacci spread: messages are 57–273 ms, consecutive copies
    /// would fall into one dropout, and six back to back held the line for
    /// 0.35–1.6 s. Coded (Robust): a repeat waits one message time of idle
    /// line (spec §5.5).
    fn refresh_policy(&self, _config: &PhyConfig) -> RefreshPolicy {
        RefreshPolicy::SPREAD
    }
}
