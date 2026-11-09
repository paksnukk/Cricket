//! Physical-layer profile parameters (see docs/aml-v1-spec.md §3 and §6),
//! the profile catalog with the user-facing names ([`PROFILES`]), and the
//! transmit level ([`DEFAULT_TX_LEVEL_DBFS`]).
//!
//! Profiles have two kinds of names. The Rust constructors and the spec use
//! the wire-format names (`robust`, `ltc_fec`, …); operators see the tier
//! names of the README table (`analog-safe`, `clean-line`, …), which say
//! what line a profile is for. [`PhyConfig::from_name`] accepts both.

use super::bmc::{BmcParams, Modulation};
use crate::link::FecMode;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Default transmit level of the MFSK and BMC (LTC) profiles: peak −12 dBFS
/// (amplitude [`DEFAULT_AMPLITUDE`], full scale = 1.0). The receivers are
/// level-independent down to the −60 dBFS silence floor, so this leaves
/// 12 dB of headroom on the bus (no clipping in a limiter or a hot analog
/// stage) and still decodes with a fader 30 dB down. The spec lists the
/// level as a SHOULD (§6); [`PhyConfig::with_tx_level_dbfs`] changes it.
/// The `digital` profile has its own fixed level ([`super::digital::PEAK`]).
pub const DEFAULT_TX_LEVEL_DBFS: f32 = -12.0;
/// [`DEFAULT_TX_LEVEL_DBFS`] as a peak amplitude (−12.04 dBFS).
pub const DEFAULT_AMPLITUDE: f32 = 0.25;

/// Peak amplitude of a level in dBFS (0 dBFS = 1.0).
pub fn amplitude_from_dbfs(dbfs: f32) -> f32 {
    libm::powf(10.0, dbfs / 20.0)
}

/// Level in dBFS of a peak amplitude (0 dBFS = 1.0; never below −200).
pub fn dbfs_from_amplitude(amplitude: f32) -> f32 {
    20.0 * libm::log10f(amplitude.abs().max(1e-10))
}

// Tone-purity thresholds (see `goertzel::tone_purity`). Measured on
// synthesized symbols: ~0.97 aligned, ~0.91 aligned at SNR 12 dB, >= 0.43 at a
// full search hop of misalignment (>= 0.39 at SNR 12 dB). White-noise windows
// peak at ~0.09 (robust, N = 288) and ~0.18 (fast, N = 144), so fast needs the
// higher threshold to keep margin above noise.
const PURITY_ROBUST: f32 = 0.25;
const PURITY_FAST: f32 = 0.3;

/// Receiver band of an MFSK tone plan (`PhyConfig::rx_band_hz`): from
/// `RX_BAND_LOW` × the lowest tone to `RX_BAND_HIGH` × the highest, so
/// the second-order band edges cost the outer tones less than 1 dB while
/// hum and rumble (≤ 300 Hz) are 40–60 dB down and hiss above the band is
/// gone.
fn mfsk_rx_band(base_hz: f32, step_hz: f32, n_tones: usize) -> (f32, f32) {
    let top = base_hz + step_hz * (n_tones.max(1) - 1) as f32;
    (RX_BAND_LOW * base_hz, RX_BAND_HIGH * top)
}
const RX_BAND_LOW: f32 = 0.7;
const RX_BAND_HIGH: f32 = 1.3;

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
    /// Receiver only (spec §3.2): the MFSK receiver band-passes its input to
    /// `(low, high)` Hz (second-order Butterworth each side) before any
    /// detection, so window energy, silence floor and tone purity are
    /// measured in the tone band only. `None` disables the filter.
    pub rx_band_hz: Option<(f32, f32)>,
    /// Receiver only (spec §3.2.3): re-centre the symbol clock on every
    /// body symbol that passes the detection gate (±3 / ±6 samples), so
    /// the body survives a sample-rate mismatch of up to ~2 % between the
    /// ends. Costs four single-tone Goertzels per body symbol.
    pub rx_track_timing: bool,
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
    /// Receiver only: also run the digital-frame detector next to this
    /// profile's own receiver (profile name suffix `+digital`, e.g.
    /// `ltc9600+digital`), so a transmitter can switch between this profile
    /// and `digital` without reconfiguring the receiver. Always on for the
    /// `digital` profile itself (its only receiver). Experimental, see
    /// `docs/proposals/digital-transparent.md`.
    pub digital_detect: bool,
}

impl Default for PhyConfig {
    fn default() -> Self {
        Self::robust()
    }
}

impl PhyConfig {
    /// **Robust** (the default, spec §6.1; user-facing name `analog-safe`):
    /// MFSK-16 at 6 ms + 0.5 ms guard,
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

    /// **Robust repeat** (spec §6.2; user-facing name `analog-safe-repeat`):
    /// Robust's tones and timing with the
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
            amplitude: DEFAULT_AMPLITUDE,
            tone_base_hz: 2500.0,
            tone_step_hz: 100.0,
            n_tones: 16,
            preamble: vec![0xA, 0x5, 0xC, 0x3],
            min_confidence_db: 3.0,
            min_tone_purity: PURITY_ROBUST,
            min_rms_dbfs: -60.0,
            rx_band_hz: Some(mfsk_rx_band(2500.0, 100.0, 16)),
            rx_track_timing: true,
            repetition_k: 2,
            interleave_depth: 4,
            link_dead_ms: 2000,
            heartbeat_interval_ms: 500,
            fec: FecMode::None,
            acquisition: Acquisition::Joint { misses: 1 },
            digital_detect: false,
        }
    }

    /// Fast profile for clean dedicated chains (user-facing name
    /// `analog-fast`). Targets ≤60 ms first-copy: 3 ms symbols, wider tone
    /// spacing (Goertzel resolution), short preamble, K=1, no body FEC.
    pub fn fast() -> Self {
        Self {
            sample_rate_hz: 48_000,
            modulation: Modulation::Mfsk,
            symbol_duration_ms: 3.0,
            guard_gap_ms: 0.0,
            attack_ms: 0.15,
            release_ms: 0.15,
            amplitude: DEFAULT_AMPLITUDE,
            tone_base_hz: 2500.0,
            tone_step_hz: 200.0,
            n_tones: 16,
            preamble: vec![0xA, 0x5],
            min_confidence_db: 2.0,
            min_tone_purity: PURITY_FAST,
            min_rms_dbfs: -60.0,
            rx_band_hz: Some(mfsk_rx_band(2500.0, 200.0, 16)),
            rx_track_timing: true,
            repetition_k: 1,
            interleave_depth: 4,
            link_dead_ms: 2000,
            heartbeat_interval_ms: 500,
            fec: FecMode::None,
            acquisition: Acquisition::FirstSymbol,
            digital_detect: false,
        }
    }

    /// LTC profile (provisional, spec §6.5; user-facing name `ltc`, in the
    /// advanced group): biphase-mark code at 4800 bit/s, uncoded 16-nibble
    /// body, K = 1.
    pub fn ltc() -> Self {
        Self::ltc_with_bit_rate(4800)
    }

    /// BMC profile at `bit_rate` bit/s (spec §6.5: 2400, 4800 or 9600). The symbol fields describe
    /// one nibble (4 bits) so generic helpers stay meaningful; the MFSK-only
    /// fields (tones, preamble, purity) are unused.
    pub fn ltc_with_bit_rate(bit_rate: u32) -> Self {
        // `BmcParams::new` clamps the bit rate (`bmc::BIT_RATE_RANGE`).
        let params = BmcParams::new(bit_rate);
        let bit_rate = params.bit_rate;
        Self {
            sample_rate_hz: 48_000,
            modulation: Modulation::Bmc(params),
            symbol_duration_ms: 4000.0 / bit_rate as f32,
            guard_gap_ms: 0.0,
            attack_ms: 0.0,
            release_ms: 0.0,
            amplitude: DEFAULT_AMPLITUDE,
            tone_base_hz: 0.0,
            tone_step_hz: 0.0,
            n_tones: 0,
            preamble: Vec::new(),
            min_confidence_db: 0.0,
            min_tone_purity: 0.0,
            min_rms_dbfs: -60.0,
            rx_band_hz: None,
            rx_track_timing: false,
            repetition_k: 1,
            interleave_depth: 4,
            link_dead_ms: 2000,
            heartbeat_interval_ms: 500,
            fec: FecMode::None,
            acquisition: Acquisition::FirstSymbol,
            digital_detect: false,
        }
    }

    /// [`ltc`](Self::ltc) with the RS(11,7)x2 GF(16) coded, whitened body
    /// (`FecMode::Rs16 { parity: 4 }`, 22 instead of 16 nibbles; spec §6.5).
    /// User-facing name `clean-line`; at 9600 bit/s `clean-line-fast`.
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
    /// RS16 parity-4 body (`ltc14400_fec`; user-facing name `digital-line`).
    pub fn ltc_fast_fec_with_bit_rate(bit_rate: u32) -> Self {
        Self {
            fec: FecMode::Rs16 { parity: 4 },
            ..Self::ltc_fast_with_bit_rate(bit_rate)
        }
    }

    /// **Digital** (experimental, `docs/proposals/digital-transparent.md`;
    /// user-facing name `digital-bit-exact`): packet nibbles carried
    /// directly in PCM sample values for
    /// bit-transparent digital paths (Dante / AES67 / MADI / AES3 without
    /// processing, USB interfaces in bit-exact mode). A frame is 21 samples
    /// at a peak of −57.4 dBFS, sent twice back to back (K = 2). The symbol
    /// fields describe one sample so generic helpers stay meaningful; the
    /// MFSK-only fields are unused.
    pub fn digital() -> Self {
        Self {
            sample_rate_hz: 48_000,
            modulation: Modulation::Digital,
            symbol_duration_ms: 1000.0 / 48_000.0,
            guard_gap_ms: 0.0,
            attack_ms: 0.0,
            release_ms: 0.0,
            amplitude: super::digital::PEAK,
            tone_base_hz: 0.0,
            tone_step_hz: 0.0,
            n_tones: 0,
            preamble: Vec::new(),
            min_confidence_db: 0.0,
            min_tone_purity: 0.0,
            min_rms_dbfs: -120.0,
            rx_band_hz: None,
            rx_track_timing: false,
            repetition_k: 2,
            interleave_depth: 4,
            link_dead_ms: 2000,
            heartbeat_interval_ms: 500,
            fec: FecMode::None,
            acquisition: Acquisition::FirstSymbol,
            digital_detect: true,
        }
    }

    /// Profile by user-facing or wire-format name (case-insensitive, `-` =
    /// `_`; see [`PROFILES`]): `analog-safe` = `robust` (and its former name
    /// `robust_fec`), `analog-safe-repeat` = `robust_repeat`, `analog-fast` =
    /// `fast`, `clean-line` = `ltc_fec`, `clean-line-fast` = `ltc9600_fec`,
    /// `digital-line` = `ltc14400_fec`, `digital-bit-exact` = `digital`, plus
    /// the uncoded `ltc`, `ltc2400`, `ltc9600`, `ltc14400` and the coded
    /// `ltc2400_fec`. A `+digital` suffix (e.g. `ltc9600+digital`) selects
    /// the profile with [`digital_detect`](Self::digital_detect) on.
    ///
    /// Unknown names are `None`: a typo must never silently select another
    /// profile (both ends have to match, and a mismatch means no MIDI).
    pub fn from_name(name: &str) -> Option<Self> {
        let key = normalize(name);
        if let Some(base) = key.strip_suffix(DIGITAL_SUFFIX) {
            let mut config = Self::from_name(base)?;
            config.digital_detect = true;
            return Some(config);
        }
        PROFILES
            .iter()
            .find(|p| p.matches(&key))
            .map(|p| (p.build)())
    }

    /// The canonical user-facing name of `name` (`"ROBUST"` →
    /// `"analog-safe"`, `"ltc9600-fec+digital"` → `"clean-line-fast+digital"`),
    /// or `None` for an unknown name.
    pub fn canonical_name(name: &str) -> Option<String> {
        let key = normalize(name);
        let (base, suffix) = match key.strip_suffix(DIGITAL_SUFFIX) {
            Some(base) => (base, DIGITAL_SUFFIX),
            None => (key.as_str(), ""),
        };
        let info = PROFILES.iter().find(|p| p.matches(base))?;
        Some(format!("{}{}", info.name, suffix))
    }

    /// Set the transmit level (peak, dBFS; 0 = full scale) of an MFSK or
    /// BMC profile. The `digital` profile's level is part of its wire
    /// format and is left unchanged.
    pub fn with_tx_level_dbfs(mut self, dbfs: f32) -> Self {
        if !matches!(self.modulation, Modulation::Digital) {
            self.amplitude = amplitude_from_dbfs(dbfs);
        }
        self
    }

    /// The transmit level (peak, dBFS) this profile sends at.
    pub fn tx_level_dbfs(&self) -> f32 {
        dbfs_from_amplitude(self.amplitude)
    }

    pub fn with_sample_rate(mut self, sample_rate_hz: u32) -> Self {
        self.sample_rate_hz = sample_rate_hz;
        self
    }

    /// Estimated first-copy on-wire duration for a max-size body (16 nibbles,
    /// or the FEC-coded body length) + preamble.
    pub fn estimated_first_copy_ms(&self) -> f32 {
        let body_nibbles = self.fec.body_nibbles();
        match &self.modulation {
            Modulation::Bmc(p) => return p.frame_ms(body_nibbles),
            Modulation::Digital => {
                return super::digital::MAX_FRAME_SAMPLES as f32 * 1000.0
                    / self.sample_rate_hz as f32
            }
            Modulation::Mfsk => {}
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

const DIGITAL_SUFFIX: &str = "+digital";

/// Lower case, `-` → `_`, surrounding whitespace removed.
fn normalize(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

/// Which selector group a profile belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileTier {
    /// The tiers of the README table: one profile per kind of line, each
    /// with body error correction (or bit-exact digital frames).
    Standard,
    /// Uncoded variants (and `ltc2400_fec`) under their wire-format names,
    /// for operators who know their line.
    Advanced,
}

/// One entry of the profile catalog ([`PROFILES`]): the user-facing name,
/// the older names still accepted, and the one-line guidance the GUI and
/// `cricket profiles` show.
#[derive(Debug, Clone, Copy)]
pub struct ProfileInfo {
    /// Canonical user-facing name (`analog-safe`, …; the advanced group
    /// keeps its wire-format names).
    pub name: &'static str,
    /// Older names [`PhyConfig::from_name`] still accepts (`_` form; `-` is
    /// equivalent). The first one is the spec / constructor name.
    pub aliases: &'static [&'static str],
    pub tier: ProfileTier,
    /// Display name ("Analog Safe").
    pub display: &'static str,
    /// The line it is meant for.
    pub line: &'static str,
    /// Processing allowed on the bus.
    pub processing: &'static str,
    /// MIDI-in → MIDI-out latency as measured in-process with
    /// `cricket loopback` (an ideal audio device; real interfaces add their
    /// own buffering), or the airtime where it was not measured.
    pub latency: &'static str,
    /// When to pick it.
    pub use_when: &'static str,
    /// Body error correction (RS16) or bit-exact digital frames, as opposed
    /// to an uncoded body.
    pub coded: bool,
    /// The profile's parameters.
    pub build: fn() -> PhyConfig,
}

impl ProfileInfo {
    /// `key` is a normalized name (see `normalize`).
    fn matches(&self, key: &str) -> bool {
        normalize(self.name) == key || self.aliases.contains(&key)
    }

    /// The default profile.
    pub fn is_default(&self) -> bool {
        self.name == DEFAULT_PROFILE
    }
}

/// User-facing name of the default profile.
pub const DEFAULT_PROFILE: &str = "analog-safe";

/// Every selectable profile, standard tiers first (in the order of the
/// README table), then the advanced group.
pub static PROFILES: &[ProfileInfo] = &[
    ProfileInfo {
        name: "analog-safe",
        aliases: &["robust", "robust_fec"],
        tier: ProfileTier::Standard,
        display: "Analog Safe",
        line: "any analog or digital bus, also one you don't control",
        processing: "EQ, compression or a limiter on the bus are tolerated",
        latency: "~0.3 s (measured in-process: mean ~0.27 s, p95 ~0.35 s)",
        use_when: "the default: cues and notes over a chain you don't fully trust",
        coded: true,
        build: PhyConfig::robust,
    },
    ProfileInfo {
        name: "analog-safe-repeat",
        aliases: &["robust_repeat"],
        tier: ProfileTier::Standard,
        display: "Analog Safe Repeat",
        line: "any bus with glitches or dropouts of 50 ms or more",
        processing: "as Analog Safe",
        latency: "~0.35 s (measured in-process: mean ~0.35 s, p95 ~0.5 s)",
        use_when: "the line drops out for 50 ms or more: every message is sent twice",
        coded: false,
        build: PhyConfig::robust_repeat,
    },
    ProfileInfo {
        name: "analog-fast",
        aliases: &["fast"],
        tier: ProfileTier::Standard,
        display: "Analog Fast",
        line: "a clean, dedicated analog line",
        processing: "no EQ, compression or limiter; no error correction",
        latency: "~80 ms (measured in-process: mean ~80 ms, p95 ~115 ms)",
        use_when: "playing notes over a dedicated analog line you set up yourself",
        coded: false,
        build: PhyConfig::fast,
    },
    ProfileInfo {
        name: "clean-line",
        aliases: &["ltc_fec", "ltc4800_fec"],
        tier: ProfileTier::Standard,
        display: "Clean Line",
        line: "a dedicated line, analog or digital (biphase mark like timecode, 4800 bit/s)",
        processing: "none: no EQ, dynamics or noise reduction; short dropouts and clicks are repaired",
        latency: "~20 ms (measured in-process: mean ~21 ms, p95 ~24 ms)",
        use_when: "a dedicated line without processing, when Analog Fast is too slow",
        coded: true,
        build: PhyConfig::ltc_fec,
    },
    ProfileInfo {
        name: "clean-line-fast",
        aliases: &["ltc9600_fec"],
        tier: ProfileTier::Standard,
        display: "Clean Line Fast",
        line: "a dedicated digital line, or analog with at least 6 kHz of bandwidth (9600 bit/s)",
        processing: "none",
        latency: "~10 ms (measured in-process: mean ~11 ms)",
        use_when: "Clean Line on a digital path, when 20 ms is too slow",
        coded: true,
        build: || PhyConfig::ltc_fec_with_bit_rate(9600),
    },
    ProfileInfo {
        name: "digital-line",
        aliases: &["ltc14400_fec"],
        tier: ProfileTier::Standard,
        display: "Digital Line",
        line: "a digital line (Dante, MADI, AES) at 48 or 96 kHz; needs 16 kHz of bandwidth (14400 bit/s)",
        processing: "none; survives converters and sample-rate conversion",
        latency: "~7 ms (measured in-process: mean ~7 ms, p95 ~7.5 ms)",
        use_when: "the fastest profile that does not need a bit-exact path",
        coded: true,
        build: || PhyConfig::ltc_fast_fec_with_bit_rate(14400),
    },
    ProfileInfo {
        name: "digital-bit-exact",
        aliases: &["digital"],
        tier: ProfileTier::Standard,
        display: "Digital Bit-Exact",
        line: "a bit-transparent digital path: samples arrive unchanged (check with `cricket pathcheck`)",
        processing: "none at all: no gain, mixing, EQ or sample-rate conversion",
        latency: "< 1 ms (measured in-process: mean ~0.6 ms)",
        use_when: "`cricket pathcheck` reported bit-exact; experimental",
        coded: true,
        build: PhyConfig::digital,
    },
    ProfileInfo {
        name: "ltc",
        aliases: &["ltc4800"],
        tier: ProfileTier::Advanced,
        display: "LTC 4800",
        line: "a dedicated clean line (4800 bit/s, uncoded)",
        processing: "none, and no dropouts or clicks",
        latency: "~16 ms (measured in-process)",
        use_when: "Clean Line without error correction",
        coded: false,
        build: PhyConfig::ltc,
    },
    ProfileInfo {
        name: "ltc2400",
        aliases: &[],
        tier: ProfileTier::Advanced,
        display: "LTC 2400",
        line: "a dedicated clean analog line with little bandwidth (2400 bit/s, uncoded)",
        processing: "none, and no dropouts or clicks",
        latency: "not measured in-process",
        use_when: "a narrow analog line where 4800 bit/s does not get through",
        coded: false,
        build: || PhyConfig::ltc_with_bit_rate(2400),
    },
    ProfileInfo {
        name: "ltc2400_fec",
        aliases: &[],
        tier: ProfileTier::Advanced,
        display: "LTC 2400 FEC",
        line: "as ltc2400, with error correction",
        processing: "none; short dropouts and clicks are repaired",
        latency: "not measured in-process",
        use_when: "Clean Line on a narrow analog line",
        coded: true,
        build: || PhyConfig::ltc_fec_with_bit_rate(2400),
    },
    ProfileInfo {
        name: "ltc9600",
        aliases: &[],
        tier: ProfileTier::Advanced,
        display: "LTC 9600",
        line: "a dedicated digital line (9600 bit/s, uncoded)",
        processing: "none, and no dropouts or clicks",
        latency: "~8 ms (measured in-process)",
        use_when: "Clean Line Fast without error correction",
        coded: false,
        build: || PhyConfig::ltc_with_bit_rate(9600),
    },
    ProfileInfo {
        name: "ltc14400",
        aliases: &[],
        tier: ProfileTier::Advanced,
        display: "LTC 14400",
        line: "a digital line at 48 or 96 kHz (14400 bit/s, uncoded)",
        processing: "none, and no dropouts or clicks",
        latency: "~5.5 ms (measured in-process)",
        use_when: "Digital Line without error correction",
        coded: false,
        build: || PhyConfig::ltc_fast_with_bit_rate(14400),
    },
];

/// Catalog entry of a user-facing or wire-format name (a `+digital` suffix
/// is ignored), or `None` for an unknown name.
pub fn profile_info(name: &str) -> Option<&'static ProfileInfo> {
    let key = normalize(name);
    let base = key.strip_suffix(DIGITAL_SUFFIX).unwrap_or(&key);
    PROFILES.iter().find(|p| p.matches(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_aliases_select_the_same_profile() {
        for (a, b) in [
            ("analog-safe", "robust"),
            ("Analog_Safe", "robust_fec"),
            ("analog-safe-repeat", "robust_repeat"),
            ("analog-fast", "fast"),
            ("clean-line", "ltc_fec"),
            ("clean-line", "ltc4800_fec"),
            ("clean-line-fast", "ltc9600_fec"),
            ("digital-line", "ltc14400_fec"),
            ("digital-bit-exact", "digital"),
            ("ltc", "ltc4800"),
        ] {
            let x = PhyConfig::from_name(a).unwrap_or_else(|| panic!("{a}"));
            let y = PhyConfig::from_name(b).unwrap_or_else(|| panic!("{b}"));
            assert_eq!(x.fec, y.fec, "{a} {b}");
            assert_eq!(x.repetition_k, y.repetition_k, "{a} {b}");
            assert_eq!(x.symbol_duration_ms, y.symbol_duration_ms, "{a} {b}");
            assert_eq!(x.amplitude, y.amplitude, "{a} {b}");
        }
        assert_eq!(
            PhyConfig::canonical_name("ROBUST").as_deref(),
            Some("analog-safe")
        );
        assert_eq!(
            PhyConfig::canonical_name("ltc9600-fec+digital").as_deref(),
            Some("clean-line-fast+digital")
        );
        assert!(
            PhyConfig::from_name("clean_line+digital")
                .unwrap()
                .digital_detect
        );
        assert_eq!(profile_info("Digital").unwrap().name, "digital-bit-exact");
        assert!(PROFILES.iter().filter(|p| p.is_default()).count() == 1);
    }

    #[test]
    fn unknown_names_are_rejected() {
        for name in ["robsut", "", "nonsense", "analog", "+digital", "ltc7200"] {
            assert!(PhyConfig::from_name(name).is_none(), "{name:?}");
            assert!(PhyConfig::canonical_name(name).is_none(), "{name:?}");
            assert!(profile_info(name).is_none(), "{name:?}");
        }
    }

    #[test]
    fn catalog_names_are_unique_and_normalized() {
        let mut seen: Vec<String> = Vec::new();
        for p in PROFILES {
            for n in core::iter::once(p.name).chain(p.aliases.iter().copied()) {
                let key = normalize(n);
                assert!(!seen.contains(&key), "duplicate name {n}");
                seen.push(key);
            }
            // Aliases are stored in the `_` form they are compared in.
            for a in p.aliases {
                assert_eq!(normalize(a), *a, "{a}");
            }
        }
    }

    #[test]
    fn tx_level() {
        let c = PhyConfig::robust();
        assert!((c.tx_level_dbfs() - DEFAULT_TX_LEVEL_DBFS).abs() < 0.1);
        let quiet = PhyConfig::ltc_fec().with_tx_level_dbfs(-20.0);
        assert!((quiet.amplitude - 0.1).abs() < 1e-4);
        let full = PhyConfig::fast().with_tx_level_dbfs(0.0);
        assert!((full.amplitude - 1.0).abs() < 1e-6);
        // The digital profile's level is wire format.
        let d = PhyConfig::digital().with_tx_level_dbfs(-6.0);
        assert_eq!(d.amplitude, PhyConfig::digital().amplitude);
    }
}
