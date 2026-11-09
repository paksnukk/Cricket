//! Bit-transparent digital phy: the experimental `digital` profile.
//!
//! Design and measurements: `docs/proposals/digital-transparent.md`
//! (candidate for AML v1 §6.6). For paths that deliver samples unchanged
//! (Dante / AES67 / MADI / AES3 routed without processing, USB interfaces in
//! bit-exact mode) the packet goes straight into PCM sample values, one
//! nibble per sample, like SMPTE ST 337 carries data in AES3 words. A whole
//! packet is 21 samples (0.44 ms at 48 kHz), so the codec adds almost no
//! latency.
//!
//! # Sample layout
//!
//! Samples are read as 24-bit two's complement *codes* (`code = x · 2²³` for
//! an `f32` sample `x`). A sample carries one *symbol* `v`, a small signed
//! integer, as `code = v · 1024`: the symbol sits in bits 10 and up, bits
//! 0–9 are zero.
//!
//! ```text
//!  24-bit code   23 ........ 14 | 13 12 11 10 | 9  8 | 7 ...... 0
//!                sign extension |   symbol v  | guard| zero (lost on 16-bit)
//!  16-bit word   15 ......... 6 |  5  4  3  2 | 1  0 |
//! ```
//!
//! * **Data symbols** are the packet nibbles as 4-bit two's complement
//!   (`0x0..=0x7` → 0..7, `0x8..=0xF` → −8..−1).
//! * **Control symbols** (sync, end) have magnitudes 9–11: no nibble ever
//!   maps to them, in either polarity, so a sync can never occur inside a
//!   body (a "code violation", as in AES3 and the BMC phy).
//! * **Level:** the largest symbol is 11 · 1024 = 11 264 codes, a peak of
//!   **−57.4 dBFS** (data alone: −60.2 dBFS). A misrouted burst is a faint
//!   tick, not a full-scale blast.
//! * **Word length:** bits 0–7 are zero, so a 16-bit path (which keeps the top
//!   16 of 24 bits) carries every symbol exactly; so do 20- and 24-bit paths
//!   and float paths with any power-of-two scaling.
//! * **Tolerance:** the receiver decides `v = round(code / 1024)` and accepts
//!   a sample only if `|code − v · 1024| ≤ 320` (1.25 LSB of a 16-bit word).
//!   That absorbs ±1 LSB16 of TPDF dither, a driver that converts with
//!   32767 or 2²³−1 instead of a power of two (with truncation), and noise up
//!   to about −100 dBFS; anything larger fails the sample, and a frame with a
//!   failed sample is dropped. A gain change of less than ~0.24 dB keeps
//!   every sample in its window (the bits arrive unchanged); a larger one
//!   fails the sync on every frame.
//!
//! # Frame
//!
//! ```text
//! | SYNC: +9 −11 +10 −9 +11 −10 | BODY: packet nibbles, high first | END: −11 |
//! ```
//!
//! The body is the link-layer packet itself (spec §4.1/§4.2: 7 bytes = 14
//! nibbles for Midi/Msc, 4 bytes = 8 nibbles for Heartbeat/Panic) without
//! padding, interleaving, FEC or whitening. Its length follows from the
//! type nibble (the second body sample); the END symbol must follow the
//! last nibble. A frame is 21 (or 15) samples; the transmitter sends every
//! packet twice back to back (K = 2) and nothing between frames, silence
//! (zero) otherwise. The receiver accepts the sync in either polarity and
//! then negates the frame, so a polarity-inverting path still works.
//!
//! # Receiver
//!
//! Per sample: one multiply, one round, a shift, a compare; on a sync value
//! six compares. A structurally complete frame (sync, body symbols in range,
//! END) is reported as [`RxEvent::Frame`]; the modem then applies the full
//! packet validation of spec §4.7 (CRC-16, version, type, SEQ, canonical
//! payload) and duplicate suppression.

use super::{
    BodyCoding, LinkState, PhyConfig, PhyFactory, PhyRx, PhyTx, RefreshPolicy, RxEvent,
    LOCK_HOLD_MS,
};
use crate::link::nibbles_to_bytes;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// Bit position of the symbol LSB in the 24-bit code.
pub const STEP_SHIFT: u32 = 10;
/// Codes per symbol step (1024 = 4 LSB of a 16-bit word).
pub const STEP: i32 = 1 << STEP_SHIFT;
/// `f32` sample = code / 2²³ (exact for every code with |code| < 2²⁴).
pub const CODE_SCALE: f32 = 8_388_608.0;
/// Largest accepted |code − v · STEP| (1.25 LSB of a 16-bit word).
pub const TOLERANCE: i32 = 320;
/// Sync symbols (opposite polarity: all negated).
pub const SYNC: [i8; 6] = [9, -11, 10, -9, 11, -10];
/// Number of sync samples.
pub const SYNC_LEN: usize = SYNC.len();
/// End-of-frame symbol.
pub const END: i8 = -11;
/// Smallest and largest control-symbol magnitude (never data).
const CONTROL_MIN: i32 = 9;
const CONTROL_MAX: i32 = 11;
/// Largest symbol magnitude on the wire.
pub const PEAK_SYMBOL: i32 = 11;
/// Peak sample value of a frame (−57.4 dBFS).
pub const PEAK: f32 = (PEAK_SYMBOL * STEP) as f32 / CODE_SCALE;
/// Largest packet (Midi / Msc) in bytes.
pub const MAX_PACKET_BYTES: usize = 7;
/// Least time between NoteOff refreshes (start to start): a message is
/// under 1 ms on air, so back-to-back refreshes (spec §5.5) would all fall
/// into one short dropout. Spaced, the 6 refreshes of a release span about
/// 120 ms (Fast: ~350 ms).
pub const REFRESH_SPACING_MS: u32 = 20;

/// Samples of one frame carrying `packet_bytes` bytes.
pub const fn frame_samples(packet_bytes: usize) -> usize {
    SYNC_LEN + 2 * packet_bytes + 1
}

/// Samples of the largest frame (21).
pub const MAX_FRAME_SAMPLES: usize = frame_samples(MAX_PACKET_BYTES);

/// Body nibbles of a packet whose header type nibble is `type_nibble`, or
/// `None` for a reserved type.
pub fn body_nibbles_for_type(type_nibble: u8) -> Option<usize> {
    match type_nibble {
        0x0 | 0x3 => Some(14),
        0x4 | 0x5 => Some(8),
        _ => None,
    }
}

/// Symbol of a nibble: 4-bit two's complement.
pub fn nibble_symbol(nibble: u8) -> i8 {
    (((nibble & 0x0F) << 4) as i8) >> 4
}

/// `f32` sample for symbol `v`.
pub fn symbol_sample(v: i8) -> f32 {
    (v as i32 * STEP) as f32 / CODE_SCALE
}

/// 24-bit code of an `f32` sample (rounded; clamped to ±2²⁴).
pub fn sample_code(x: f32) -> i32 {
    let c = libm::roundf(x * CODE_SCALE);
    if c.is_nan() {
        0
    } else {
        c.clamp(-16_777_216.0, 16_777_216.0) as i32
    }
}

/// Nearest symbol of `code` and the residual `code − v · STEP`.
pub fn classify(code: i32) -> (i32, i32) {
    let v = (code + STEP / 2) >> STEP_SHIFT;
    (v, code - (v << STEP_SHIFT))
}

/// Margin of a frame in dB: half a symbol step over its largest residual
/// (54.2 dB on a bit-exact path, ~6 dB at ±1 LSB16 of dither).
pub fn margin_db(max_residual: i32) -> f32 {
    20.0 * libm::log10f((STEP / 2) as f32 / max_residual.max(1) as f32)
}

/// One frame: sync, `body_nibbles` (high nibble of each byte first), end.
pub fn frame(body_nibbles: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(SYNC_LEN + body_nibbles.len() + 1);
    out.extend(SYNC.iter().map(|&s| symbol_sample(s)));
    out.extend(
        body_nibbles
            .iter()
            .map(|&n| symbol_sample(nibble_symbol(n))),
    );
    out.push(symbol_sample(END));
    out
}

/// Parameters of the digital phy. The sample layout is fixed by the wire
/// format (the constants of this module), so there is nothing to set yet;
/// the struct exists so [`Modulation::Digital`](super::Modulation::Digital)
/// can grow parameters without a breaking change. `DigitalParams::default()`
/// is the only way to make one outside the crate.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DigitalParams {}

impl PhyFactory for DigitalParams {
    fn name(&self) -> &str {
        "digital"
    }

    fn clone_box(&self) -> Box<dyn PhyFactory> {
        Box::new(*self)
    }

    fn tx(&self, config: &PhyConfig) -> Box<dyn PhyTx> {
        Box::new(DigitalTx::new(config.clone()))
    }

    fn rx(&self, config: &PhyConfig) -> Box<dyn PhyRx> {
        Box::new(DigitalRx::new(config.clone()))
    }

    /// The packet nibbles as they are: the frame carries its length in the
    /// type nibble, there is no pad, interleave or code.
    fn body_coding(&self, _config: &PhyConfig) -> BodyCoding {
        BodyCoding::Raw
    }

    /// The largest frame ([`MAX_FRAME_SAMPLES`]), whatever the body.
    fn frame_samples(&self, _config: &PhyConfig, _body_nibbles: usize) -> usize {
        MAX_FRAME_SAMPLES
    }

    /// None: frames are delimited by their sync and END symbols.
    fn gap_samples(&self, _config: &PhyConfig) -> usize {
        0
    }

    /// One sample at the profile's 48 kHz (the timing unit of the generic
    /// helpers, whatever the stream rate).
    fn symbol_duration_ms(&self, _config: &PhyConfig) -> f32 {
        1000.0 / 48_000.0
    }

    /// The first six free slots, [`REFRESH_SPACING_MS`] apart.
    fn refresh_policy(&self, _config: &PhyConfig) -> RefreshPolicy {
        RefreshPolicy::consecutive_spaced(REFRESH_SPACING_MS)
    }

    /// The level is the sample layout.
    fn fixed_tx_level(&self) -> bool {
        true
    }

    fn needs_bit_exact_path(&self) -> bool {
        true
    }
}

/// Transmit side of the digital phy.
pub struct DigitalTx {
    config: PhyConfig,
}

impl DigitalTx {
    pub fn new(config: PhyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &PhyConfig {
        &self.config
    }

    /// One frame for the packet nibbles `body_nibbles` (see [`frame`]).
    pub fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32> {
        frame(body_nibbles)
    }
}

impl PhyTx for DigitalTx {
    fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32> {
        DigitalTx::synthesize_framed(self, body_nibbles)
    }
}

/// A structurally complete frame (before packet validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigitalFrame {
    /// Packet bytes; `len` of them are valid.
    pub bytes: [u8; MAX_PACKET_BYTES],
    pub len: usize,
    /// The sync arrived negated (polarity-inverting path).
    pub inverted: bool,
    /// Stream position one past the END sample.
    pub end_sample: u64,
    /// Largest |residual| over the frame's samples, in codes (0 = exact).
    pub max_residual: i32,
}

impl DigitalFrame {
    pub fn packet(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Counters of a [`DigitalRx`] (for false-accept analysis and diagnostics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DigitalRxCounters {
    /// Syncs found (either polarity).
    pub syncs: u64,
    /// Frames completed (sync, in-range body, END).
    pub frames: u64,
    /// Syncs whose frame failed (a sample out of tolerance or range, a
    /// reserved type nibble, or a wrong END).
    pub aborted: u64,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Search,
    /// Reading body nibble `got` of `need` (0 = not known yet).
    Body {
        inverted: bool,
        need: usize,
        got: usize,
    },
    End {
        inverted: bool,
    },
}

/// Marker in the symbol history for a sample outside the tolerance window.
const INVALID: i32 = i32::MIN;

/// Receive side of the digital phy (see the module docs).
pub struct DigitalRx {
    sample_rate_hz: u32,
    state: State,
    /// Last `SYNC_LEN` symbols (`INVALID` if out of tolerance), ring.
    recent: [i32; SYNC_LEN],
    recent_residual: [i32; SYNC_LEN],
    head: usize,
    nibbles: [u8; 2 * MAX_PACKET_BYTES],
    frame_residual: i32,
    samples: u64,
    last_frame_end: Option<u64>,
    last_margin_db: f32,
    counters: DigitalRxCounters,
}

impl DigitalRx {
    pub fn new(config: PhyConfig) -> Self {
        Self {
            sample_rate_hz: config.sample_rate_hz,
            state: State::Search,
            recent: [INVALID; SYNC_LEN],
            recent_residual: [0; SYNC_LEN],
            head: 0,
            nibbles: [0; 2 * MAX_PACKET_BYTES],
            frame_residual: 0,
            samples: 0,
            last_frame_end: None,
            last_margin_db: 0.0,
            counters: DigitalRxCounters::default(),
        }
    }

    pub fn counters(&self) -> DigitalRxCounters {
        self.counters
    }

    pub fn samples_pushed(&self) -> u64 {
        self.samples
    }

    /// See [`LinkState`]: `Locked` for [`LOCK_HOLD_MS`] after a complete
    /// frame, `Search` otherwise (a frame is only 21 samples: there is no
    /// "reading" state worth reporting).
    pub fn link_state(&self) -> LinkState {
        let hold = self.sample_rate_hz as u64 * LOCK_HOLD_MS as u64 / 1000;
        match self.last_frame_end {
            Some(end) if self.samples.saturating_sub(end) < hold => LinkState::Locked,
            _ => LinkState::Search,
        }
    }

    /// [`margin_db`] of the last complete frame.
    pub fn last_confidence_db(&self) -> f32 {
        self.last_margin_db
    }

    /// Back to search with the symbol history and the lock cleared; the
    /// counters and the sample count go on.
    pub fn reset(&mut self) {
        self.state = State::Search;
        self.recent = [INVALID; SYNC_LEN];
        self.head = 0;
        self.last_frame_end = None;
    }

    /// Feed samples; returns one [`RxEvent::Frame`] per complete frame.
    pub fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent> {
        let mut events = Vec::new();
        for &x in samples {
            if let Some(frame) = self.push(x) {
                events.push(RxEvent::Frame(frame));
            }
        }
        events
    }

    /// Feed one sample; returns the frame it completes, if any.
    pub fn push(&mut self, x: f32) -> Option<DigitalFrame> {
        let (v, r) = classify(sample_code(x));
        let ok = r.abs() <= TOLERANCE;
        self.samples += 1;
        let mut out = None;

        match self.state {
            State::Search => {}
            State::Body {
                inverted,
                mut need,
                got,
            } => {
                let s = if inverted { -v } else { v };
                if ok && (-8..=7).contains(&s) {
                    self.nibbles[got] = (s & 0x0F) as u8;
                    self.frame_residual = self.frame_residual.max(r.abs());
                    let got = got + 1;
                    if got == 2 {
                        need = body_nibbles_for_type(self.nibbles[1]).unwrap_or(0);
                    }
                    self.state = if got == 2 && need == 0 {
                        self.counters.aborted += 1;
                        State::Search
                    } else if got == need {
                        State::End { inverted }
                    } else {
                        State::Body {
                            inverted,
                            need,
                            got,
                        }
                    };
                } else {
                    self.counters.aborted += 1;
                    self.state = State::Search;
                }
            }
            State::End { inverted } => {
                let s = if inverted { -v } else { v };
                if ok && s == END as i32 {
                    let n = body_nibbles_for_type(self.nibbles[1]).unwrap_or(0);
                    let packet = nibbles_to_bytes(&self.nibbles[..n]);
                    let mut bytes = [0u8; MAX_PACKET_BYTES];
                    bytes[..packet.len()].copy_from_slice(&packet);
                    let max_residual = self.frame_residual.max(r.abs());
                    self.counters.frames += 1;
                    self.last_frame_end = Some(self.samples);
                    self.last_margin_db = margin_db(max_residual);
                    out = Some(DigitalFrame {
                        bytes,
                        len: packet.len(),
                        inverted,
                        end_sample: self.samples,
                        max_residual,
                    });
                } else {
                    self.counters.aborted += 1;
                }
                self.state = State::Search;
            }
        }

        self.recent[self.head] = if ok { v } else { INVALID };
        self.recent_residual[self.head] = r.abs();
        self.head = (self.head + 1) % SYNC_LEN;
        if ok && (CONTROL_MIN..=CONTROL_MAX).contains(&v.abs()) {
            if let Some(inverted) = self.sync_polarity() {
                self.counters.syncs += 1;
                self.frame_residual = self.recent_residual.iter().copied().max().unwrap_or(0);
                self.state = State::Body {
                    inverted,
                    need: 0,
                    got: 0,
                };
            }
        }
        out
    }

    /// Whether the last `SYNC_LEN` symbols are the sync (`Some(false)`) or
    /// the negated sync (`Some(true)`).
    fn sync_polarity(&self) -> Option<bool> {
        let at = |i: usize| self.recent[(self.head + i) % SYNC_LEN];
        let first = at(0);
        let inverted = if first == SYNC[0] as i32 {
            false
        } else if first == -(SYNC[0] as i32) {
            true
        } else {
            return None;
        };
        let sign = if inverted { -1 } else { 1 };
        (1..SYNC_LEN)
            .all(|i| at(i) == sign * SYNC[i] as i32)
            .then_some(inverted)
    }
}

/// A frame phy: its frames carry their length and a sync can never occur
/// inside a valid body, so there is nothing to expect, confirm or rewind
/// (the trait defaults).
impl PhyRx for DigitalRx {
    fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent> {
        DigitalRx::push_samples(self, samples)
    }

    fn link_state(&self) -> LinkState {
        DigitalRx::link_state(self)
    }

    fn last_confidence_db(&self) -> f32 {
        DigitalRx::last_confidence_db(self)
    }

    fn samples_pushed(&self) -> u64 {
        DigitalRx::samples_pushed(self)
    }

    fn reset(&mut self) {
        DigitalRx::reset(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn rx() -> DigitalRx {
        DigitalRx::new(PhyConfig::digital())
    }

    #[test]
    fn layout_constants() {
        assert_eq!(MAX_FRAME_SAMPLES, 21);
        assert_eq!(frame_samples(4), 15);
        // −57.4 dBFS peak.
        let peak_db = 20.0 * libm::log10f(PEAK);
        assert!((peak_db + 57.4).abs() < 0.05, "{peak_db}");
        // Control symbols are never data, in either polarity.
        for n in 0..16u8 {
            let v = nibble_symbol(n) as i32;
            assert!((-8..=7).contains(&v));
            for &c in SYNC.iter().chain(core::iter::once(&END)) {
                assert_ne!(v, c as i32);
                assert_ne!(-v, c as i32);
            }
        }
        // Every symbol survives 16-bit truncation and f32 exactly.
        for v in -11i8..=11 {
            let x = symbol_sample(v);
            let code = sample_code(x);
            assert_eq!(code, v as i32 * STEP);
            assert_eq!(code & 0xFF, 0);
            assert_eq!(classify(code), (v as i32, 0));
        }
    }

    #[test]
    fn frame_roundtrip_both_polarities_any_offset() {
        let packet = [0x10u8, 0x02, 0x93, 60, 100, 0xAB, 0xCD];
        let nibbles = crate::link::bytes_to_nibbles(&packet);
        for inverted in [false, true] {
            for lead in 0..8 {
                let mut audio = vec![0.0f32; lead];
                audio.extend(frame(&nibbles));
                audio.extend(frame(&nibbles));
                if inverted {
                    audio.iter_mut().for_each(|s| *s = -*s);
                }
                let mut r = rx();
                let frames: Vec<_> = audio.iter().filter_map(|&x| r.push(x)).collect();
                assert_eq!(frames.len(), 2);
                for (k, f) in frames.iter().enumerate() {
                    assert_eq!(f.packet(), &packet);
                    assert_eq!(f.inverted, inverted);
                    assert_eq!(f.max_residual, 0);
                    assert_eq!(f.end_sample, (lead + 21 * (k + 1)) as u64);
                }
            }
        }
    }

    #[test]
    fn silence_and_loud_audio_find_nothing() {
        let mut r = rx();
        for i in 0..48_000 {
            let x = 0.5 * libm::sinf(i as f32 * 0.3);
            assert!(r.push(x).is_none());
            assert!(r.push(0.0).is_none());
        }
        assert_eq!(r.counters().syncs, 0);
    }

    #[test]
    fn out_of_tolerance_sample_drops_the_frame() {
        let nibbles = crate::link::bytes_to_nibbles(&[0x14, 0x01, 0x12, 0x34]);
        for k in 0..15 {
            let mut audio = frame(&nibbles);
            audio[k] += (TOLERANCE + 1) as f32 / CODE_SCALE;
            let mut r = rx();
            assert!(audio.iter().all(|&x| r.push(x).is_none()), "sample {k}");
            let mut audio = frame(&nibbles);
            audio[k] += TOLERANCE as f32 / CODE_SCALE;
            let mut r = rx();
            assert_eq!(audio.iter().filter_map(|&x| r.push(x)).count(), 1);
        }
    }
}
