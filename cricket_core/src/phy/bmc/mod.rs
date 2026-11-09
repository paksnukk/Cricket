//! LTC-style physical layer: biphase-mark code (BMC).
//!
//! Status: **provisional** AML v1 profiles (spec §6.5; design history in
//! `docs/proposals/ltc-bmc.md`). Selected with
//! [`PhyConfig::ltc`](super::PhyConfig::ltc) and related constructors.
//!
//! # Line code
//!
//! Biphase mark, as SMPTE LTC and AES3 use: every bit cell starts with a
//! transition; a `1` has a second transition in the middle of the cell, a `0`
//! has none. Information is carried by *where the transitions are*, never by
//! the level, so the code is polarity-insensitive, DC-free and self-clocking.
//! Between two transitions the receiver sees one of two intervals: a half
//! cell (`H`, half of a `1`) or a full cell (`F`, a `0`).
//!
//! # Frame (one repetition)
//!
//! ```text
//! | preamble: P x '1' | sync: V V | body: 4 x N bits | postamble: '1' | tail |
//! ```
//!
//! * **Preamble** — `preamble_bits` ones: a square wave at the bit rate. The
//!   receiver measures the half-cell length on it (clock acquisition) and
//!   lets its envelope follower settle.
//! * **Sync** — two *code violations* `V`: intervals of 1.5 cells (3 half
//!   cells), first one level, then the other. Valid BMC data only ever
//!   produces `H` and `F` intervals, so `V V` cannot occur inside a body
//!   whatever the payload: the sync is unambiguous without a reserved data
//!   pattern or bit stuffing (AES3 uses violations for the same reason). Being
//!   a pair of opposite-level intervals it is DC-balanced and, like the rest
//!   of the code, polarity-free. The edge ending the second `V` is the start
//!   of the first body bit.
//! * **Body** — the link-layer nibbles, MSB first (exactly what the MFSK phy
//!   would send as tones), 4 bits per nibble.
//! * **Postamble** — one `1` bit: terminates the last body bit with a
//!   transition (its length could not be measured otherwise). The fast
//!   profiles (`ltc14400`, [`BmcParams::fast`]) send none: the transition
//!   into the tail half cell already terminates the last bit.
//! * **Tail** — one half cell at the last level, then a shaped fade to 0.
//!
//! The burst is followed by `gap_bits` of silence before the next repetition
//! or message.
//!
//! # Shaping
//!
//! Transitions are raised-cosine steps `edge_frac` half cells wide, placed at
//! exact (fractional) sample positions, so the signal is band-limited (LTC
//! specifies a 25 us +-5 rise time for the same reason) and any sample rate
//! works. Level: `amplitude / sqrt(2)`, i.e. the same RMS as an MFSK tone of
//! the same `amplitude` (fair SNR comparisons, lower peak).
//!
//! # Receiver
//!
//! DC blocker -> 2nd-order low-pass, then two paths on the filtered signal:
//!
//! * **Sync:** a correlator of half-cell box sums against the end of the
//!   preamble plus the two violations; the best peak above a threshold
//!   (snapped to the transition that ends the second violation when one is
//!   close) starts a body. It keeps running during a body, and a clearly
//!   stronger sync replaces a weaker lock, so a false lock never hides a
//!   real packet (the BMC equivalent of the MFSK "rewind on failed decode").
//! * **Body:** soft decisions on a half-cell time grid: each bit compares
//!   the integrals of its two half cells (`1` = opposite signs). The
//!   normalized decision margin is the bit's reliability; a nibble carries
//!   its weakest bit's margin (as `confidence_db`) for the FEC decoder, and
//!   a nibble with a dropout (no signal) is flagged erased. The grid is a
//!   second-order timing loop pulled by the transitions (peak envelope ->
//!   Schmitt trigger with an envelope-relative threshold -> interpolated
//!   zero-crossing times), so a spurious or missing transition only costs
//!   the bits it touches. With FEC a body is abandoned once it has more
//!   erased nibbles than the code can fill.
//!
//! Per-sample cost: the filters, envelope and Schmitt trigger as before,
//! one box-sum update (`half cell` adds), and the correlator (22 multiply-
//! adds plus 22 squares); per bit, two half-cell integrals. With
//! `sync_fractional` (fast profiles) each of the 22 boxes is an exact
//! half-cell integral of 2-3 weighted samples instead (~60 multiply-adds).

mod rx;
mod tx;

pub use rx::BmcRx;
pub use tx::BmcTx;

/// Physical-layer modulation selected by [`PhyConfig`](super::PhyConfig).
#[derive(Debug, Clone, PartialEq)]
pub enum Modulation {
    /// AML v1 MFSK-16 (Goertzel bank). Used by robust, robust_repeat and fast.
    Mfsk,
    /// Biphase-mark code (LTC-style, provisional), see the module docs.
    Bmc(BmcParams),
    /// Packet nibbles as PCM sample values (bit-transparent paths only,
    /// experimental), see [`crate::phy::digital`].
    Digital,
}

/// Parameters of the BMC phy. The wire parameters (spec §6.5) are
/// `bit_rate`, `preamble_bits`, `postamble_bits`, `gap_bits` and
/// `edge_frac`; the rest tune the reference receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct BmcParams {
    /// Bits per second (one bit = one BMC cell).
    pub bit_rate: u32,
    /// Number of `1` bits before the sync.
    pub preamble_bits: usize,
    /// Number of `1` bits after the body.
    pub postamble_bits: usize,
    /// Silence after each burst, in bit cells.
    pub gap_bits: usize,
    /// Raised-cosine transition width as a fraction of a half cell (0..=1).
    pub edge_frac: f32,
    /// RX low-pass cutoff as a multiple of the bit rate.
    pub rx_lowpass_ratio: f32,
    /// RX DC-blocker cutoff as a fraction of the bit rate.
    pub rx_highpass_ratio: f32,
    /// Schmitt threshold as a fraction of the peak envelope.
    pub hysteresis: f32,
    /// Peak-envelope release time constant, in bit cells.
    pub envelope_release_bits: f32,
    /// Preamble half cells (before the two sync violations) in the RX
    /// sync correlator template.
    pub sync_preamble_half_cells: usize,
    /// Sync correlator threshold: normalized correlation (0..=1) a peak must
    /// reach. A clean sync scores ~1; body data can reach 0.82 (two
    /// mismatched half cells of 22), which is why a lock is only replaced by
    /// a stronger sync.
    pub sync_threshold: f32,
    /// While a body is read, a new sync replaces the lock only if its
    /// squared correlation exceeds the lock's by this much.
    pub resync_margin: f32,
    /// A sync whose correlation reaches this replaces any lock (body data
    /// cannot exceed 0.82 on a clean line).
    pub resync_certain: f32,
    /// Sync correlator boxes exactly one half cell long, at exact
    /// (interpolated) half-cell delays, instead of rounded to whole samples.
    /// Needed when a half cell is only ~1.5-2.5 samples (bit rates above
    /// 9600 at 48 kHz); off for the original profiles (unchanged receiver).
    pub sync_fractional: bool,
    /// After a completed body, hold syncs to the replacement rule (as if
    /// the body were still being read) until the next burst's sync could
    /// first arrive. The end of a burst can correlate like a sync (~0.85);
    /// when real syncs score only ~0.9 (bit rates above 9600 at 48 kHz)
    /// such a false lock would otherwise hide the next packet of a chord.
    pub post_body_guard: bool,
    /// A transition within this many half cells of the sync correlation
    /// peak sets the body start (0 = use the peak).
    pub sync_snap: f32,
    /// Accepted deviation of the bit rate from nominal (0.05 = +-5 %).
    pub max_rate_error: f32,
    /// Part of each half cell left out of the soft-decision integration
    /// next to the mid-cell point, as a fraction of a half cell.
    pub rx_window_trim: f32,
    /// Same, next to the cell boundaries.
    pub rx_window_outer_trim: f32,
    /// Body timing loop: phase correction per transition (fraction of the
    /// measured offset).
    pub pll_phase_gain: f32,
    /// Body timing loop: relative half-cell correction per transition and
    /// half cell of offset.
    pub pll_rate_gain: f32,
    /// A bit is decided this many half cells after its cell ends (at least
    /// 1.5: the decision also uses the first half of the next cell).
    pub decision_delay: f32,
    /// Body timing loop: only transitions within this many half cells of a
    /// grid point steer it.
    pub pll_window: f32,
    /// Body timing loop: weight reduction towards the window edge (0 = none,
    /// 1 = linear down to zero).
    pub pll_taper: f32,
    /// A bit whose integrated level falls below this fraction of the
    /// running bit level is a dropout: its nibble is erased.
    pub dropout_ratio: f32,
    /// A completed body whose mean nibble margin (weakest bit per nibble,
    /// 0..=1) is below this is dropped as a false lock. Noise scores
    /// ~0.1-0.25, a real packet at 0 dB SNR ~0.4-0.6.
    pub min_body_margin: f32,
    /// ... or whose transitions fall within 0.25 half cells of the timing
    /// grid less often than this.
    pub min_on_grid: f32,
    /// A nibble whose weakest bit margin (0..=1) is below this is flagged
    /// as an erasure (only matters with FEC; 0 disables).
    pub erase_margin: f32,
}

/// Bit rates [`BmcParams::new`] accepts; a value outside is clamped. Below
/// 300 bit/s a half cell is longer than 80 samples at 48 kHz (the receiver's
/// history alone would take hundreds of KB); above 100 000 bit/s a half
/// cell is shorter than one sample at every sample rate in use. A bit rate
/// of 0 would make every cell length infinite.
pub const BIT_RATE_RANGE: core::ops::RangeInclusive<u32> = 300..=100_000;

impl BmcParams {
    /// Defaults for `bit_rate` (all other parameters are bit-rate relative).
    /// `bit_rate` is clamped to [`BIT_RATE_RANGE`]; read it back from the
    /// result when it matters.
    pub fn new(bit_rate: u32) -> Self {
        let bit_rate = bit_rate.clamp(*BIT_RATE_RANGE.start(), *BIT_RATE_RANGE.end());
        Self {
            bit_rate,
            preamble_bits: 8,
            postamble_bits: 1,
            gap_bits: 4,
            edge_frac: 0.5,
            rx_lowpass_ratio: 1.5,
            rx_highpass_ratio: 0.05,
            hysteresis: 0.3,
            envelope_release_bits: 8.0,
            sync_preamble_half_cells: 16,
            sync_threshold: 0.75,
            sync_snap: 0.5,
            resync_margin: 0.05,
            resync_certain: 0.95,
            sync_fractional: false,
            post_body_guard: false,
            max_rate_error: 0.05,
            rx_window_trim: 0.15,
            rx_window_outer_trim: 0.15,
            pll_phase_gain: 0.3,
            pll_rate_gain: 0.02,
            decision_delay: 1.5,
            pll_window: 0.45,
            pll_taper: 0.5,
            dropout_ratio: 0.25,
            min_body_margin: 0.3,
            min_on_grid: 0.6,
            erase_margin: 0.0,
        }
    }

    /// Parameters of the fast profiles for clean digital lines (`ltc14400`,
    /// `ltc14400_fec`; docs/proposals/ltc-bmc.md §10), where a half cell is
    /// only ~1.7 samples at 48 kHz. Wire: full-width (sinusoidal) edges,
    /// so an all-ones preamble is a pure tone at the bit rate and the burst
    /// stays inside a 20 kHz converter passband; no postamble (the tail
    /// half cell terminates the last bit); one cell of silence between
    /// bursts. Receiver: fractional sync boxes, a slower rate loop and the
    /// post-body sync guard.
    pub fn fast(bit_rate: u32) -> Self {
        Self {
            postamble_bits: 0,
            gap_bits: 1,
            edge_frac: 1.0,
            pll_rate_gain: 0.01,
            sync_fractional: true,
            post_body_guard: true,
            ..Self::new(bit_rate)
        }
    }

    /// Samples per half cell (fractional) at `sample_rate_hz`.
    pub fn half_cell_samples(&self, sample_rate_hz: u32) -> f32 {
        sample_rate_hz as f32 / (2.0 * self.bit_rate as f32)
    }

    /// Half cells in one burst carrying `body_nibbles` nibbles: preamble,
    /// sync (2 x 3), body, postamble and the one-half-cell tail.
    pub fn frame_half_cells(&self, body_nibbles: usize) -> usize {
        2 * self.preamble_bits + 6 + 8 * body_nibbles + 2 * self.postamble_bits + 1
    }

    /// Samples one burst occupies, including the shaped fade at the end.
    pub fn frame_samples(&self, sample_rate_hz: u32, body_nibbles: usize) -> usize {
        let half = self.half_cell_samples(sample_rate_hz);
        // The TX leads by half an edge and the final fade takes another half.
        let len = self.frame_half_cells(body_nibbles) as f32 + self.edge_frac.clamp(0.0, 1.0);
        libm::ceilf(len * half) as usize + 1
    }

    /// Silence after each burst, in samples.
    pub fn gap_samples(&self, sample_rate_hz: u32) -> usize {
        libm::roundf(2.0 * self.gap_bits as f32 * self.half_cell_samples(sample_rate_hz)) as usize
    }

    /// Burst + trailing gap, in samples.
    pub fn repetition_samples(&self, sample_rate_hz: u32, body_nibbles: usize) -> usize {
        self.frame_samples(sample_rate_hz, body_nibbles) + self.gap_samples(sample_rate_hz)
    }

    /// Burst duration in milliseconds (without the gap).
    pub fn frame_ms(&self, body_nibbles: usize) -> f32 {
        self.frame_half_cells(body_nibbles) as f32 * 500.0 / self.bit_rate as f32
    }
}

/// Transition intervals of one burst, in half cells (1 = `H`, 2 = `F`,
/// 3 = sync violation `V`). The burst starts at a level, each interval is
/// followed by a transition, and the tail half cell follows the last one.
pub(crate) fn frame_intervals(params: &BmcParams, body_nibbles: &[u8]) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(params.frame_half_cells(body_nibbles.len()));
    for _ in 0..params.preamble_bits {
        out.extend_from_slice(&[1, 1]);
    }
    out.extend_from_slice(&[3, 3]);
    for &n in body_nibbles {
        for shift in (0..4).rev() {
            if (n >> shift) & 1 == 1 {
                out.extend_from_slice(&[1, 1]);
            } else {
                out.push(2);
            }
        }
    }
    for _ in 0..params.postamble_bits {
        out.extend_from_slice(&[1, 1]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phy::PhyConfig;
    use alloc::vec;
    use alloc::vec::Vec;

    fn decode_stream(config: &PhyConfig, samples: &[f32], chunk: usize) -> Vec<u8> {
        let mut rx = BmcRx::new(config.clone());
        rx.set_expected_body_nibbles(16);
        let mut nibbles = Vec::new();
        for c in samples.chunks(chunk) {
            for ev in rx.push_samples(c) {
                if let crate::phy::RxEvent::BodyNibble { result, .. } = ev {
                    nibbles.push(result.nibble);
                }
            }
        }
        nibbles
    }

    #[test]
    fn frame_layout_counts() {
        let p = BmcParams::new(4800);
        let body = [0u8; 16];
        let iv = frame_intervals(&p, &body);
        let total: usize = iv.iter().map(|&u| u as usize).sum::<usize>() + 1;
        assert_eq!(total, p.frame_half_cells(16));
        assert_eq!(p.half_cell_samples(48_000), 5.0);
    }

    #[test]
    fn roundtrip_all_nibbles_both_polarities() {
        for rate in [2400, 4800, 9600] {
            let config = PhyConfig::ltc_with_bit_rate(rate);
            let body: Vec<u8> = (0..16).collect();
            let tx = BmcTx::new(config.clone());
            let mut audio = vec![0.0; 37];
            audio.extend(tx.synthesize_framed(&body));
            audio.extend(vec![0.0; 200]);
            assert_eq!(decode_stream(&config, &audio, 64), body, "rate {rate}");
            let inverted: Vec<f32> = audio.iter().map(|s| -s).collect();
            assert_eq!(
                decode_stream(&config, &inverted, 7),
                body,
                "rate {rate} inv"
            );
        }
    }

    #[test]
    fn silence_produces_nothing() {
        let config = PhyConfig::ltc();
        assert!(decode_stream(&config, &vec![0.0; 48_000], 256).is_empty());
    }
}
