//! Physical-layer profile parameters (see docs/aml-v1-spec.md §3 and §6).

use super::bmc::{BmcParams, Modulation};
use crate::link::FecMode;
use alloc::vec;
use alloc::vec::Vec;

// Tone-purity thresholds (see `goertzel::tone_purity`). Measured on
// synthesized symbols: ~0.97 aligned, ~0.91 aligned at SNR 12 dB, >= 0.43 at a
// full search hop of misalignment (>= 0.39 at SNR 12 dB). White-noise windows
// peak at ~0.09 (robust, N = 288) and ~0.18 (fast, N = 144), so fast needs the
// higher threshold to keep margin above noise.
const PURITY_ROBUST: f32 = 0.25;
const PURITY_FAST: f32 = 0.3;

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

#[derive(Debug, Clone)]
pub struct PhyConfig {
    pub sample_rate_hz: u32,
    /// Line code: `Mfsk` (robust, robust_repeat, fast) or `Bmc` (the
    /// provisional LTC profiles, spec §6.5).
    pub modulation: Modulation,
    pub symbol_duration_ms: f32,
    pub guard_gap_ms: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub amplitude: f32,
    pub tone_base_hz: f32,
    pub tone_step_hz: f32,
    pub n_tones: usize,
    pub preamble: Vec<u8>,
    pub min_confidence_db: f32,
    /// Minimum level-independent tone purity (Goertzel power normalized by
    /// window energy, see `goertzel::tone_purity`; 1.0 = pure steady tone).
    pub min_tone_purity: f32,
    /// Silence floor: windows whose RMS is below this (dBFS, full-scale sine
    /// = -3 dBFS) are never detected as symbols.
    pub min_rms_dbfs: f32,
    pub repetition_k: usize,
    pub interleave_depth: usize,
    pub link_dead_ms: u32,
    pub heartbeat_interval_ms: u32,
    /// Body forward error correction (spec §4.6): `Rs16 { parity: 4 }` for
    /// robust and the `*_fec` LTC profiles, `None` (block-interleaved
    /// uncoded body, §4.3) otherwise.
    pub fec: FecMode,
    /// Preamble acquisition (MFSK only, spec §3.2).
    pub acquisition: Acquisition,
}

impl Default for PhyConfig {
    fn default() -> Self {
        Self::robust()
    }
}

impl PhyConfig {
    /// **Robust** (the default, spec §6.1): MFSK-16 at 6 ms + 0.5 ms guard,
    /// an RS(11,7)x2 GF(16) coded, whitened 22-nibble body
    /// (`FecMode::Rs16 { parity: 4 }`) sent once (K = 1), and joint preamble
    /// acquisition tolerating two missed preamble symbols.
    pub fn robust() -> Self {
        Self {
            repetition_k: 1,
            fec: FecMode::Rs16 { parity: 4 },
            acquisition: Acquisition::Joint { misses: 2 },
            ..Self::robust_repeat()
        }
    }

    /// **Robust repeat** (spec §6.2): Robust's tones and timing with the
    /// uncoded 16-nibble block-interleaved body sent twice (K = 2), and joint
    /// preamble acquisition tolerating one missed preamble symbol
    /// (docs/proposals/fec.md §10.4, "robust@joint1"). The choice for lines
    /// with glitches or dropouts of ~50 ms or more, which a single coded copy
    /// cannot bridge.
    pub fn robust_repeat() -> Self {
        Self {
            sample_rate_hz: 48_000,
            modulation: Modulation::Mfsk,
            symbol_duration_ms: 6.0,
            guard_gap_ms: 0.5,
            attack_ms: 0.3,
            release_ms: 0.3,
            amplitude: 0.8,
            tone_base_hz: 2500.0,
            tone_step_hz: 100.0,
            n_tones: 16,
            preamble: vec![0xA, 0x5, 0xC, 0x3],
            min_confidence_db: 3.0,
            min_tone_purity: PURITY_ROBUST,
            min_rms_dbfs: -60.0,
            repetition_k: 2,
            interleave_depth: 4,
            link_dead_ms: 2000,
            heartbeat_interval_ms: 500,
            fec: FecMode::None,
            acquisition: Acquisition::Joint { misses: 1 },
        }
    }

    /// Fast profile for clean dedicated chains (LTC-like). Targets ≤60 ms first-copy.
    /// 3 ms symbols, wider tone spacing (Goertzel resolution), short preamble, K=1.
    pub fn fast() -> Self {
        Self {
            sample_rate_hz: 48_000,
            modulation: Modulation::Mfsk,
            symbol_duration_ms: 3.0,
            guard_gap_ms: 0.0,
            attack_ms: 0.15,
            release_ms: 0.15,
            amplitude: 0.8,
            tone_base_hz: 2500.0,
            tone_step_hz: 200.0,
            n_tones: 16,
            preamble: vec![0xA, 0x5],
            min_confidence_db: 2.0,
            min_tone_purity: PURITY_FAST,
            min_rms_dbfs: -60.0,
            repetition_k: 1,
            interleave_depth: 4,
            link_dead_ms: 2000,
            heartbeat_interval_ms: 500,
            fec: FecMode::None,
            acquisition: Acquisition::FirstSymbol,
        }
    }

    /// LTC profile (provisional, spec §6.5): biphase-mark code at 4800 bit/s,
    /// uncoded 16-nibble body, K = 1.
    pub fn ltc() -> Self {
        Self::ltc_with_bit_rate(4800)
    }

    /// BMC profile at `bit_rate` bit/s (spec §6.5: 2400, 4800 or 9600). The symbol fields describe
    /// one nibble (4 bits) so generic helpers stay meaningful; the MFSK-only
    /// fields (tones, preamble, purity) are unused.
    pub fn ltc_with_bit_rate(bit_rate: u32) -> Self {
        Self {
            sample_rate_hz: 48_000,
            modulation: Modulation::Bmc(BmcParams::new(bit_rate)),
            symbol_duration_ms: 4000.0 / bit_rate as f32,
            guard_gap_ms: 0.0,
            attack_ms: 0.0,
            release_ms: 0.0,
            amplitude: 0.8,
            tone_base_hz: 0.0,
            tone_step_hz: 0.0,
            n_tones: 0,
            preamble: Vec::new(),
            min_confidence_db: 0.0,
            min_tone_purity: 0.0,
            min_rms_dbfs: -60.0,
            repetition_k: 1,
            interleave_depth: 4,
            link_dead_ms: 2000,
            heartbeat_interval_ms: 500,
            fec: FecMode::None,
            acquisition: Acquisition::FirstSymbol,
        }
    }

    /// [`ltc`](Self::ltc) with the RS(11,7)x2 GF(16) coded, whitened body
    /// (`FecMode::Rs16 { parity: 4 }`, 22 instead of 16 nibbles; spec §6.5).
    pub fn ltc_fec() -> Self {
        Self::ltc_fec_with_bit_rate(4800)
    }

    /// BMC profile at `bit_rate` bit/s with the RS16 parity-4 body.
    pub fn ltc_fec_with_bit_rate(bit_rate: u32) -> Self {
        Self {
            fec: FecMode::Rs16 { parity: 4 },
            ..Self::ltc_with_bit_rate(bit_rate)
        }
    }

    /// Fast LTC profile for clean digital lines (Dante, MADI, AES; not part
    /// of spec §6.5 yet): BMC at `bit_rate` with [`BmcParams::fast`],
    /// uncoded 16-nibble body. `ltc14400` is 14400 bit/s.
    pub fn ltc_fast_with_bit_rate(bit_rate: u32) -> Self {
        Self {
            modulation: Modulation::Bmc(BmcParams::fast(bit_rate)),
            ..Self::ltc_with_bit_rate(bit_rate)
        }
    }

    /// [`ltc_fast_with_bit_rate`](Self::ltc_fast_with_bit_rate) with the
    /// RS16 parity-4 body (`ltc14400_fec`).
    pub fn ltc_fast_fec_with_bit_rate(bit_rate: u32) -> Self {
        Self {
            fec: FecMode::Rs16 { parity: 4 },
            ..Self::ltc_fast_with_bit_rate(bit_rate)
        }
    }

    /// Profile by name (case-insensitive, `-` = `_`). `robust_fec` is a
    /// deprecated alias of `robust` (its former name); unknown names mean
    /// the default, robust.
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().replace('-', "_").as_str() {
            "fast" => Self::fast(),
            "robust_repeat" => Self::robust_repeat(),
            "ltc" => Self::ltc(),
            "ltc2400" => Self::ltc_with_bit_rate(2400),
            "ltc4800" => Self::ltc_with_bit_rate(4800),
            "ltc9600" => Self::ltc_with_bit_rate(9600),
            "ltc_fec" | "ltc4800_fec" => Self::ltc_fec(),
            "ltc2400_fec" => Self::ltc_fec_with_bit_rate(2400),
            "ltc9600_fec" => Self::ltc_fec_with_bit_rate(9600),
            "ltc14400" => Self::ltc_fast_with_bit_rate(14400),
            "ltc14400_fec" => Self::ltc_fast_fec_with_bit_rate(14400),
            // "robust", its deprecated alias "robust_fec", and unknown names.
            _ => Self::robust(),
        }
    }

    pub fn with_sample_rate(mut self, sample_rate_hz: u32) -> Self {
        self.sample_rate_hz = sample_rate_hz;
        self
    }

    /// Estimated first-copy on-wire duration for a max-size body (16 nibbles,
    /// or the FEC-coded body length) + preamble.
    pub fn estimated_first_copy_ms(&self) -> f32 {
        let body_nibbles = self.fec.body_nibbles();
        if let Modulation::Bmc(p) = &self.modulation {
            return p.frame_ms(body_nibbles);
        }
        let symbols = self.preamble.len() + body_nibbles;
        let period_ms = self.symbol_duration_ms + self.guard_gap_ms;
        symbols as f32 * period_ms
    }

    pub fn symbol_samples(&self) -> usize {
        libm::roundf((self.symbol_duration_ms / 1000.0) * self.sample_rate_hz as f32) as usize
    }

    pub fn gap_samples(&self) -> usize {
        libm::roundf((self.guard_gap_ms / 1000.0) * self.sample_rate_hz as f32) as usize
    }

    pub fn samples_per_symbol_period(&self) -> usize {
        self.symbol_samples() + self.gap_samples()
    }

    pub fn hop_samples(&self) -> usize {
        (self.symbol_samples() / 4).max(1)
    }

    pub fn tone_hz(&self, index: u8) -> f32 {
        self.tone_base_hz + (index as f32) * self.tone_step_hz
    }

    pub fn tone_table(&self) -> Vec<f32> {
        (0..self.n_tones as u8).map(|i| self.tone_hz(i)).collect()
    }
}
