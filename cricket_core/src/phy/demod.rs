use super::config::PhyConfig;
use super::filter::BandPass;
use super::goertzel::{
    best_tone, goertzel_power, goertzel_power_energy, rms_dbfs, tone_purity, ToneDetection,
};
use super::mfsk::{Acquisition, MfskParams};
use super::traits::PhyRx;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Phy lock state, as [`ModemRx`], `BmcRx` and
/// [`DigitalRx`](super::digital::DigitalRx) report it (the same rule for
/// all three).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// No body is being read and none was completed within the last
    /// [`LOCK_HOLD_MS`].
    Search,
    /// A body is being read (MFSK: the preamble matched; BMC: a sync was
    /// found), or the last body the phy completed (MFSK: and decoded;
    /// digital: a complete frame) ended less than [`LOCK_HOLD_MS`] ago. With
    /// heartbeats every 500 ms a live link stays `Locked` through idle time;
    /// a transmitter that stops shows `Search` after a second and `Dead` at
    /// link-dead time.
    Locked,
    /// No valid packet within `link_dead_ms` after the link was alive. Never
    /// produced by the phys (lock only); set by `CricketModemRx`, which owns
    /// link-dead detection.
    Dead,
}

/// How long after a completed body a phy still reports
/// [`LinkState::Locked`] (every phy).
pub const LOCK_HOLD_MS: u32 = 1000;

#[derive(Debug, Clone)]
pub struct DemodResult {
    pub nibble: u8,
    pub power: f32,
    pub confidence_db: f32,
    /// Level-independent tone purity (0..~1).
    pub purity: f32,
    /// Body symbol that failed the detection gate but was kept as an
    /// erasure for FEC (only when `PhyConfig::fec` is enabled). `nibble` is
    /// then the best guess (0 if nothing was detected at all).
    pub erased: bool,
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum RxEvent {
    /// Demodulated body nibble after a successful preamble lock.
    /// `end_sample` is the absolute stream position (samples pushed since
    /// creation) one past the symbol period the nibble was read from.
    BodyNibble {
        result: DemodResult,
        end_sample: u64,
    },
    /// Preamble acquired; body collection starts.
    PreambleLocked { confidence_db: f32 },
    /// Lost lock mid-packet.
    SyncLost,
    Metrics {
        state: LinkState,
        confidence_db: f32,
    },
    /// A complete frame of the digital phy (packet bytes before validation).
    Frame(super::digital::DigitalFrame),
}

/// Joint acquisition: a preamble window that fails its gate *contradicts*
/// the lock when another tone wins it with at least this fraction of the
/// gate purity (spec §3.2.2). With the tone-band input filter, the best bin
/// of a noise window reaches a third of the gate purity routinely, so the
/// bar is the gate itself: a tone that would be detected as a symbol.
const CONTRADICTING_PURITY: f32 = 1.0;

/// Joint acquisition: minimum mean expected-tone purity of a candidate
/// search window, as a fraction of `min_tone_purity`. Measured: 0.8 keeps
/// every A/B cell (down to AWGN −4 dB) and cuts the work on unrelated MFSK
/// audio by a third; 1.0 already loses deliveries at −3/−4 dB.
const JOINT_MIN_MEAN_PURITY: f32 = 0.8;

/// Outcome of one joint-acquisition search step.
enum JointStep {
    NeedInput,
    /// No lock at this window: move on by one hop.
    Advance,
    Lock {
        trigger: u64,
        aligned: u64,
        confidence_db: f32,
    },
}

/// Alignments this close (samples) to the last failed attempt's are the
/// same attempt (see `ModemRx::is_failed_alignment`).
const FAILED_ALIGNMENT_TOLERANCE: u64 = 2;

/// Symbol clock tracking (`PhyConfig::rx_track_timing`): an early-late
/// gate on each body symbol that passed the detection gate. The tone's
/// power is measured in windows `hop / 2` samples early and late; a window
/// that sits on the symbol gives equal powers, a window late by `e` gives
/// `P(late) − P(early) ≈ −4 e P / N`. The estimate (times `TRACK_GAIN`, at
/// most `N / TRACK_MAX_DIV` samples) corrects the position of the next
/// symbol. Two percent of sample-rate error moves a 6 ms symbol by 6
/// samples per period, inside the ±9 the gate can follow.
const TRACK_GAIN: f32 = 0.75;
const TRACK_MAX_DIV: usize = 32;

/// A lock attempt in progress.
#[derive(Debug, Clone, Copy)]
struct Attempt {
    /// Search window that triggered it; a failed attempt resumes Search one
    /// hop after it.
    trigger: u64,
    /// Fine-aligned start of the preamble it reads.
    aligned: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RxPhase {
    Search,
    /// Collecting remaining preamble symbols after first matched.
    ConfirmPreamble {
        matched: usize,
        /// Preamble symbols after the first that failed but were tolerated
        /// (FEC only, see `FecMode::preamble_misses`).
        misses: usize,
    },
    /// Reading body nibbles for the current packet repetition.
    Body {
        collected: usize,
        expected: usize,
        /// Symbols kept as FEC erasures so far.
        erased: usize,
    },
    /// Body complete; processing is paused (input is only buffered) until
    /// the caller reports whether it decoded, via [`ModemRx::confirm`] or
    /// [`ModemRx::reject`].
    Verdict,
}

/// Streaming MFSK-16 receiver with preamble search and symbol clock.
///
/// All positions are absolute stream sample indices. `read` is the start of
/// the next symbol window. The buffer keeps `hop` samples of history behind
/// `read` (for backward fine alignment) and, while a lock attempt is in
/// progress, everything from `hop` before the window that triggered it, so a
/// failed attempt can be rewound: Search resumes at `trigger + hop` (one
/// symbol period after a rejected body, see [`reject`](Self::reject)), as
/// if the trigger window had been rejected, and no consumed sample is lost.
/// A hit that fine-aligns to the start the failed attempt read from is not
/// retried. Memory stays bounded by one packet (preamble + body periods)
/// plus history and the caller's chunk.
///
/// The attempt stays open after the last body nibble: the receiver pauses
/// until the caller (normally `CricketModemRx`) reports the packet decode
/// result. [`confirm`](Self::confirm) accepts the body and continues after
/// it; [`reject`](Self::reject) (CRC / FEC failure) rewinds like a failed
/// preamble, so a real packet that starts inside a false lock's body window
/// is still found. Both resume processing of the buffered input and return
/// its events. While paused, `push_samples` only buffers.
pub struct ModemRx {
    config: PhyConfig,
    /// The tone plan, timing and detection gates of `config`.
    params: MfskParams,
    tones: Vec<f32>,
    /// Tone-band input filter (spec §3.2): every pushed sample goes
    /// through it before it is buffered, so window energies, the silence
    /// floor and the purity gate see in-band energy only. `None` when the
    /// profile asks for no filter (`PhyConfig::rx_band_hz`).
    band: Option<BandPass>,
    buffer: VecDeque<f32>,
    phase: RxPhase,
    /// Stream position where the last confirmed (decoded) body ended.
    last_confirmed_end: Option<u64>,
    /// Total samples pushed since creation (never reset).
    samples_pushed: u64,
    /// Absolute position of the next symbol window.
    read: u64,
    /// The current lock attempt (ConfirmPreamble, Body or Verdict); `None`
    /// in Search.
    attempt: Option<Attempt>,
    /// Aligned preamble start of the last attempt that failed (preamble
    /// mismatch, body gate failure or rejected body). A hit whose fine
    /// alignment lands on the same start is not retried: it would read the
    /// same symbols and fail the same way (see
    /// [`is_failed_alignment`](Self::is_failed_alignment)). On
    /// preamble-like input (the preamble tones repeated back to back) every
    /// hop near a symbol start is a hit that aligns to that start, so
    /// without this each false body was read three to six times over.
    failed_alignment: Option<u64>,
    /// Fine alignment never moves before this position (end of the last
    /// completed packet body), so it cannot slide into a decoded packet.
    align_floor: u64,
    last_confidence_db: f32,
    /// Expected body nibble count for the current packet (set by caller via `expect_body_nibbles`).
    expected_body_nibbles: usize,
    /// Bits per symbol of the tone alphabet (`PhyConfig::bits_per_symbol`).
    bits: u32,
    /// Body symbols of the current attempt, on an alphabet of fewer than 16
    /// tones: the nibble events are regrouped from them once the body is
    /// complete (see [`pack`](super::pack)). Unused with 4 bits per symbol.
    body_symbols: Vec<DemodResult>,
}

impl ModemRx {
    /// Panics if `config` does not select [`Modulation::Mfsk`](super::Modulation::Mfsk)
    /// (the modem builds its phys through [`super::PhyFactory`], which
    /// cannot get this wrong).
    pub fn new(config: PhyConfig) -> Self {
        let params = config
            .mfsk()
            .cloned()
            .unwrap_or_else(|| panic!("ModemRx needs an MFSK PhyConfig"));
        let tones = params.tone_table();
        let band = params.rx_band_hz.map(|(lo, hi)| {
            BandPass::with_order(lo, hi, params.rx_band_order as usize, config.sample_rate_hz)
        });
        let bits = params.bits_per_symbol();
        Self {
            config,
            params,
            tones,
            band,
            buffer: VecDeque::with_capacity(4096),
            phase: RxPhase::Search,
            last_confirmed_end: None,
            samples_pushed: 0,
            read: 0,
            attempt: None,
            failed_alignment: None,
            align_floor: 0,
            last_confidence_db: 0.0,
            expected_body_nibbles: 16, // default; encoder pads to multiple of interleave depth
            bits,
            body_symbols: Vec::new(),
        }
    }

    /// Body symbols to read for the expected nibbles (equal on 16 tones;
    /// 30 for 22 nibbles on 8 tones).
    fn expected_body_symbols(&self) -> usize {
        super::pack::symbols_for_nibbles(self.expected_body_nibbles, self.bits)
    }

    /// Enter the body phase of a lock attempt.
    fn start_body(&mut self) {
        self.body_symbols.clear();
        self.phase = RxPhase::Body {
            collected: 0,
            expected: self.expected_body_symbols(),
            erased: 0,
        };
    }

    /// Emit a body symbol: directly as a nibble on a 16-tone alphabet;
    /// on a smaller one it is buffered, and when the body is complete each
    /// nibble is assembled from the symbols that carry its bits, erased if
    /// any of them was, with the least confidence and purity among them.
    fn emit_body_symbol(
        &mut self,
        result: DemodResult,
        last: bool,
        end_sample: u64,
        events: &mut Vec<RxEvent>,
    ) {
        if self.bits >= 4 {
            events.push(RxEvent::BodyNibble { result, end_sample });
            return;
        }
        self.body_symbols.push(result);
        if !last {
            return;
        }
        let symbols: Vec<u8> = self.body_symbols.iter().map(|r| r.nibble).collect();
        let nibbles = super::pack::unpack_symbols(&symbols, self.bits, self.expected_body_nibbles);
        for (i, &nibble) in nibbles.iter().enumerate() {
            let (first, last) = super::pack::symbols_of_nibble(i, self.bits);
            let parts = &self.body_symbols[first.min(symbols.len())..(last + 1).min(symbols.len())];
            let result = parts.iter().fold(
                DemodResult {
                    nibble,
                    power: f32::INFINITY,
                    confidence_db: f32::INFINITY,
                    purity: f32::INFINITY,
                    erased: parts.is_empty(),
                },
                |acc, r| DemodResult {
                    nibble,
                    power: acc.power.min(r.power),
                    confidence_db: acc.confidence_db.min(r.confidence_db),
                    purity: acc.purity.min(r.purity),
                    erased: acc.erased || r.erased,
                },
            );
            events.push(RxEvent::BodyNibble { result, end_sample });
        }
        self.body_symbols.clear();
    }

    pub fn config(&self) -> &PhyConfig {
        &self.config
    }

    /// The tone plan, timing and detection gates.
    pub fn params(&self) -> &MfskParams {
        &self.params
    }

    /// See [`LinkState`]: `Locked` while a body is read (or awaits its
    /// verdict) and for [`LOCK_HOLD_MS`] after a confirmed body.
    pub fn link_state(&self) -> LinkState {
        let reading = matches!(self.phase, RxPhase::Body { .. } | RxPhase::Verdict);
        let hold = self.config.sample_rate_hz as u64 * LOCK_HOLD_MS as u64 / 1000;
        let recent = self
            .last_confirmed_end
            .is_some_and(|end| self.samples_pushed.saturating_sub(end) < hold);
        if reading || recent {
            LinkState::Locked
        } else {
            LinkState::Search
        }
    }

    pub fn last_confidence_db(&self) -> f32 {
        self.last_confidence_db
    }

    pub fn set_expected_body_nibbles(&mut self, n: usize) {
        self.expected_body_nibbles = n;
    }

    /// True after the last body nibble of a lock attempt, until
    /// [`confirm`](Self::confirm) or [`reject`](Self::reject) is called.
    pub fn awaiting_verdict(&self) -> bool {
        self.phase == RxPhase::Verdict
    }

    /// The completed body decoded: keep it, continue searching after it, and
    /// process buffered input. No-op (beyond processing) unless
    /// [`awaiting_verdict`](Self::awaiting_verdict).
    pub fn confirm(&mut self) -> Vec<RxEvent> {
        if self.phase == RxPhase::Verdict {
            self.phase = RxPhase::Search;
            self.attempt = None;
            self.failed_alignment = None;
            self.align_floor = self.read;
            self.last_confirmed_end = Some(self.read);
            self.trim(self.params.hop_samples(self.config.sample_rate_hz));
        }
        self.process()
    }

    /// The completed body failed to decode: treat the lock as false, rewind
    /// Search to just after the window that triggered it (re-scanning the
    /// buffered body audio), and process buffered input. No-op (beyond
    /// processing) unless [`awaiting_verdict`](Self::awaiting_verdict).
    pub fn reject(&mut self) -> Vec<RxEvent> {
        if self.phase == RxPhase::Verdict {
            let hop = self.params.hop_samples(self.config.sample_rate_hz);
            self.phase = RxPhase::Search;
            self.rewind_failed(hop);
            self.trim(hop);
        }
        self.process()
    }

    pub fn reset(&mut self) {
        self.phase = RxPhase::Search;
        self.last_confirmed_end = None;
        self.buffer.clear();
        if let Some(band) = &mut self.band {
            band.reset();
        }
        self.read = self.samples_pushed;
        self.attempt = None;
        self.failed_alignment = None;
        self.align_floor = self.samples_pushed;
    }

    /// Total samples pushed since creation (absolute stream position of the
    /// input head).
    pub fn samples_pushed(&self) -> u64 {
        self.samples_pushed
    }

    /// Absolute stream position of the first sample still buffered.
    fn base(&self) -> u64 {
        self.samples_pushed - self.buffer.len() as u64
    }

    /// Whether samples `[at, at + len)` have been pushed.
    fn available(&self, at: u64, len: usize) -> bool {
        at + len as u64 <= self.samples_pushed
    }

    /// Copy of the buffered samples `[at, at + len)`.
    fn window_at(&self, at: u64, len: usize) -> Vec<f32> {
        let start = (at - self.base()) as usize;
        self.buffer.range(start..start + len).copied().collect()
    }

    /// Drop samples no longer needed: keep `hop` of history behind the
    /// current read position, or behind the trigger of a pending lock attempt.
    fn trim(&mut self, hop: usize) {
        let keep_from = self.attempt.map_or(self.read, |a| a.trigger).min(self.read);
        let keep_from = keep_from.saturating_sub(hop as u64);
        let drop = keep_from.saturating_sub(self.base()) as usize;
        self.buffer.drain(0..drop.min(self.buffer.len()));
    }

    /// Abandon the current lock attempt and rewind Search to just after the
    /// window that triggered it.
    fn fail_attempt(&mut self, hop: usize, events: &mut Vec<RxEvent>) {
        self.phase = RxPhase::Search;
        events.push(RxEvent::SyncLost);
        self.rewind_failed(hop);
    }

    /// Close the current attempt as failed: remember its alignment (see
    /// `failed_alignment`) and resume Search one hop after its trigger
    /// window.
    fn rewind_failed(&mut self, hop: usize) {
        let trigger = match self.attempt.take() {
            Some(a) => {
                self.failed_alignment = Some(a.aligned);
                a.trigger
            }
            None => self.read,
        };
        self.read = trigger + hop as u64;
    }

    /// Whether a lock aligned at `at` would repeat the last failed attempt:
    /// the same start within [`FAILED_ALIGNMENT_TOLERANCE`]. On a clean
    /// signal every trigger window of one preamble aligns to the same
    /// sample. The tolerance stays tight (not a hop) on purpose: a retry
    /// at a slightly different alignment sometimes decodes what the first
    /// attempt could not (in noise the alignments jitter; with wrong tones
    /// a quarter-symbol offset can turn a confident wrong symbol into an
    /// erasure the FEC can fill), and the A/B harness delivers a trial or
    /// two per 400 fewer in those cells with any wider rule. On preamble
    /// tones this leaves the joint-acquisition profiles one extra attempt
    /// per preamble occurrence, from the half-overlap window whose
    /// alignment range starts a hop past the rejected start.
    fn is_failed_alignment(&self, at: u64) -> bool {
        self.failed_alignment
            .is_some_and(|failed| at.abs_diff(failed) <= FAILED_ALIGNMENT_TOLERANCE)
    }

    fn preamble0_ok(&self, det: &DemodResult) -> bool {
        det.nibble == self.params.preamble[0]
            && self.level_ok(det)
            && det.confidence_db >= self.params.min_confidence_db
    }

    pub fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent> {
        // A NaN or infinite sample (a glitching driver) must not poison the
        // filter state, so it is read as silence.
        let clean = samples.iter().map(|&s| if s.is_finite() { s } else { 0.0 });
        match &mut self.band {
            Some(band) => self.buffer.extend(clean.map(|s| band.process(s))),
            None => self.buffer.extend(clean),
        }
        self.samples_pushed += samples.len() as u64;
        self.process()
    }

    fn process(&mut self) -> Vec<RxEvent> {
        let mut events = Vec::new();
        let sym = self.params.symbol_samples(self.config.sample_rate_hz);
        let period = self
            .params
            .samples_per_symbol_period(self.config.sample_rate_hz);
        let hop = self.params.hop_samples(self.config.sample_rate_hz);

        loop {
            match self.phase {
                RxPhase::Search if self.joint_misses().is_some() => {
                    match self.joint_search_step(sym, period, hop) {
                        JointStep::NeedInput => break,
                        JointStep::Advance => {
                            self.read += hop as u64;
                            self.trim(hop);
                        }
                        JointStep::Lock {
                            trigger,
                            aligned,
                            confidence_db,
                        } => {
                            let n = self.params.preamble.len() as u64;
                            self.attempt = Some(Attempt { trigger, aligned });
                            self.read = aligned + n * period as u64;
                            self.start_body();
                            self.last_confidence_db = confidence_db;
                            events.push(RxEvent::PreambleLocked { confidence_db });
                            self.trim(hop);
                        }
                    }
                }
                RxPhase::Search => {
                    if !self.available(self.read, sym) {
                        break;
                    }
                    let trigger = self.read;
                    // Cheap pre-check: the gate needs the preamble[0] tone's
                    // purity >= min_tone_purity, so skip the 16-tone bank when
                    // that single tone (bit-identical power and energy) fails.
                    let hit = self.expected_purity(trigger, sym, self.params.preamble[0])
                        >= self.params.min_tone_purity
                        && self
                            .detect_nibble(&self.window_at(trigger, sym))
                            .is_some_and(|det| self.preamble0_ok(&det));
                    if hit {
                        // Fine alignment looks up to one symbol ahead and the lock
                        // needs a full period behind that. Wait for all of it (the
                        // window is re-detected on the next call) so that alignment
                        // never depends on how the caller chunks its input.
                        if !self.available(trigger, sym + period) {
                            break;
                        }
                        let aligned = self.best_preamble0_position(trigger, sym, hop);
                        debug_assert!(self.available(aligned, period));
                        // Re-check the gate on the aligned window, unless
                        // this alignment was just tried and failed.
                        if let Some(det) =
                            self.detect_nibble(&self.window_at(aligned, sym))
                                .filter(|det| {
                                    self.preamble0_ok(det) && !self.is_failed_alignment(aligned)
                                })
                        {
                            self.attempt = Some(Attempt { trigger, aligned });
                            self.read = aligned + period as u64;
                            self.phase = RxPhase::ConfirmPreamble {
                                matched: 1,
                                misses: 0,
                            };
                            self.last_confidence_db = det.confidence_db;
                            self.trim(hop);
                            continue;
                        }
                    }
                    self.read = trigger + hop as u64;
                    self.trim(hop);
                }
                RxPhase::ConfirmPreamble { matched, misses } => {
                    if !self.available(self.read, period) {
                        break;
                    }
                    let window = self.window_at(self.read, sym);
                    let expect = self.params.preamble[matched];
                    let det = self.detect_nibble(&window);
                    let good = det.as_ref().is_some_and(|det| {
                        det.nibble == expect
                            && self.level_ok(det)
                            && det.confidence_db >= self.params.min_confidence_db * 0.5
                    });
                    // With FEC the coded body + CRC verify the lock, so a
                    // bad preamble symbol after the first may be tolerated.
                    let tolerate = !good && misses < self.config.fec.preamble_misses();
                    if good || tolerate {
                        self.read += period as u64;
                        let next = matched + 1;
                        let misses = misses + usize::from(tolerate);
                        let confidence_db = det.map_or(0.0, |d| d.confidence_db);
                        if good {
                            self.last_confidence_db = confidence_db;
                        }
                        if next >= self.params.preamble.len() {
                            self.start_body();
                            events.push(RxEvent::PreambleLocked { confidence_db });
                        } else {
                            self.phase = RxPhase::ConfirmPreamble {
                                matched: next,
                                misses,
                            };
                        }
                        continue;
                    }
                    // Preamble mismatch → rewind and search again
                    self.fail_attempt(hop, &mut events);
                    self.trim(hop);
                }
                RxPhase::Body {
                    collected,
                    expected,
                    erased,
                } => {
                    let track = self.params.rx_track_timing;
                    let lookahead = if track { hop / 2 } else { 0 };
                    if !self.available(self.read, period.max(sym + lookahead)) {
                        break;
                    }
                    let window = self.window_at(self.read, sym);
                    let det = self.detect_nibble(&window);
                    let gate_ok = det.as_ref().is_some_and(|det| {
                        self.level_ok(det)
                            && det.confidence_db >= self.params.min_confidence_db * 0.5
                    });
                    // Symbol clock tracking (spec §3.2.3): a symbol that
                    // passed the gate re-centres the clock on its tone, so a
                    // sample-rate mismatch between the ends does not walk the
                    // body windows off the symbols.
                    let shift = match (&det, gate_ok && track) {
                        (Some(d), true) => self.timing_shift(self.read, sym, hop, d.nibble),
                        _ => 0,
                    };
                    let coded = self.config.fec.max_body_erasures() > 0;
                    // With FEC, a symbol that fails the gate becomes an
                    // erasure instead of aborting, up to what the code can
                    // fill. Without FEC the best guess is kept: the CRC-16 and
                    // the packet checks (spec §4.7) decide, and a wrong guess
                    // costs nothing that an abort would not, since a rejected
                    // body rewinds like a failed preamble.
                    let keep = gate_ok || !coded || erased < self.config.fec.max_body_erasures();
                    if keep {
                        let mut result = det.unwrap_or(DemodResult {
                            nibble: 0,
                            power: 0.0,
                            confidence_db: 0.0,
                            purity: 0.0,
                            erased: true,
                        });
                        result.erased = !gate_ok;
                        self.read = self.read.saturating_add_signed(shift) + period as u64;
                        if gate_ok {
                            self.last_confidence_db = result.confidence_db;
                        }
                        let next = collected + 1;
                        self.emit_body_symbol(result, next >= expected, self.read, &mut events);
                        if next >= expected {
                            // Keep the attempt (and its audio) until the
                            // caller's verdict.
                            self.phase = RxPhase::Verdict;
                            break;
                        }
                        self.phase = RxPhase::Body {
                            collected: next,
                            expected,
                            erased: erased + usize::from(!gate_ok),
                        };
                        continue;
                    }
                    // Missed body symbol → rewind and search again
                    self.fail_attempt(hop, &mut events);
                    self.trim(hop);
                }
                RxPhase::Verdict => break,
            }
        }

        events.push(RxEvent::Metrics {
            state: self.link_state(),
            confidence_db: self.last_confidence_db,
        });
        events
    }

    /// Tolerated preamble misses under joint acquisition (`None` =
    /// first-symbol acquisition).
    fn joint_misses(&self) -> Option<usize> {
        match self.params.acquisition {
            Acquisition::FirstSymbol => None,
            Acquisition::Joint { misses } => Some(misses as usize),
        }
    }

    /// Purity of `nibble`'s tone in the window `[at, at + sym)`, or 0 below
    /// the silence floor. Equals `detect_nibble`'s purity when that tone wins.
    fn expected_purity(&self, at: u64, sym: usize, nibble: u8) -> f32 {
        let start = (at - self.base()) as usize;
        let (power, energy) = goertzel_power_energy(
            self.buffer.range(start..start + sym).copied(),
            sym,
            self.params.tone_hz(nibble),
            self.config.sample_rate_hz,
        );
        if rms_dbfs(energy, sym) < self.config.min_rms_dbfs {
            return 0.0;
        }
        tone_purity(power, energy, sym)
    }

    /// One joint-acquisition search step at window position `self.read`.
    ///
    /// 1. **Candidate** (one Goertzel + energy per preamble window
    ///    `read + i * period`): at most `misses` windows have an expected-tone
    ///    purity below `min_tone_purity` (stops at the first excess miss), and
    ///    the purities sum to at least `0.8 * N * min_tone_purity`.
    /// 2. **Fine alignment** on the summed expected-tone power of all `N`
    ///    windows ([`best_preamble_position`](Self::best_preamble_position)).
    /// 3. **Accept** (full tone bank per aligned window): at most `misses`
    ///    symbols fail the gate (nibble, purity, `0.5 * min_confidence_db`), at
    ///    least one passes, and at most one failed window is won by another
    ///    tone with at least half the gate purity, so a stream of unrelated
    ///    tones needs three matching symbols in sequence while noise or a
    ///    dropout may take out two.
    fn joint_search_step(&self, sym: usize, period: usize, hop: usize) -> JointStep {
        let misses = self.joint_misses().unwrap_or(0);
        let n = self.params.preamble.len();
        let min_purity = self.params.min_tone_purity;
        let trigger = self.read;
        let span = (n - 1) * period + sym;
        if !self.available(trigger, span) {
            return JointStep::NeedInput;
        }
        let mut failed = 0;
        let mut sum = 0.0;
        for (i, &nibble) in self.params.preamble.iter().enumerate() {
            let p = self.expected_purity(trigger + (i * period) as u64, sym, nibble);
            sum += p;
            if p < min_purity {
                failed += 1;
                if failed > misses {
                    return JointStep::Advance;
                }
            }
        }
        if sum < JOINT_MIN_MEAN_PURITY * min_purity * n as f32 {
            return JointStep::Advance;
        }
        // Alignment looks up to one symbol ahead; wait for all of it so the
        // result never depends on input chunking (re-evaluated next call).
        if !self.available(trigger, sym + span) {
            return JointStep::NeedInput;
        }
        let aligned = self.best_preamble_position(trigger, sym, hop, period);
        if self.is_failed_alignment(aligned) {
            return JointStep::Advance;
        }
        let mut bad = 0;
        let mut contradicting = 0;
        let mut confidence_db = 0.0;
        for (i, &nibble) in self.params.preamble.iter().enumerate() {
            let det = self.detect_nibble(&self.window_at(aligned + (i * period) as u64, sym));
            match det {
                Some(d)
                    if d.nibble == nibble
                        && self.level_ok(&d)
                        && d.confidence_db >= self.params.min_confidence_db * 0.5 =>
                {
                    confidence_db = d.confidence_db;
                    continue;
                }
                Some(d) if d.nibble != nibble && d.purity >= CONTRADICTING_PURITY * min_purity => {
                    contradicting += 1;
                }
                _ => {}
            }
            bad += 1;
            if bad > misses || bad >= n || contradicting > 1 {
                return JointStep::Advance;
            }
        }
        JointStep::Lock {
            trigger,
            aligned,
            confidence_db,
        }
    }

    /// Early-late clock correction (samples) for the body symbol read at
    /// `at` showing tone `nibble`; see `TRACK_GAIN`. Needs samples up to
    /// `at + sym + hop / 2`.
    fn timing_shift(&self, at: u64, sym: usize, hop: usize, nibble: u8) -> i64 {
        let d = (hop / 2).max(1) as u64;
        if at < d || at - d < self.base() {
            return 0;
        }
        let target = self.params.tone_hz(nibble);
        let power = |start: u64| {
            goertzel_power(
                &self.window_at(start, sym),
                target,
                self.config.sample_rate_hz,
            )
        };
        let (early, on, late) = (power(at - d), power(at), power(at + d));
        if on <= 0.0 {
            return 0;
        }
        // Lateness of the window: `x = -(N / 4) (late - early) / P`; the
        // correction is `-x`.
        let max = (sym / TRACK_MAX_DIV).max(1) as f32;
        let shift = (sym as f32 / 4.0) * (late - early) / on * TRACK_GAIN;
        libm::roundf(shift.clamp(-max, max)) as i64
    }

    fn detect_nibble(&self, window: &[f32]) -> Option<DemodResult> {
        let ToneDetection {
            index,
            power,
            confidence_db,
            purity,
            energy,
        } = best_tone(window, &self.tones, self.config.sample_rate_hz)?;
        if rms_dbfs(energy, window.len()) < self.config.min_rms_dbfs {
            return None;
        }
        Some(DemodResult {
            nibble: index,
            power,
            confidence_db,
            purity,
            erased: false,
        })
    }

    /// Level-independent gate (silence floor is applied in `detect_nibble`).
    fn level_ok(&self, det: &DemodResult) -> bool {
        det.purity >= self.params.min_tone_purity
    }

    /// Symbol-boundary fine alignment around a preamble[0] hit at `trigger`.
    ///
    /// Maximizes the preamble[0] tone's Goertzel power over window start
    /// positions in `[trigger - hop, trigger + sym]`: backward by up to one
    /// search hop (clamped to buffered history and `align_floor`), forward by
    /// up to one symbol, because a window only partly covering the symbol can
    /// already pass the gate (purity is roughly the covered fraction). Coarse
    /// grid of `hop / 2`, then halving refinement around the best point down
    /// to one sample. The power peaks where the window coincides with the
    /// symbol, so the residual error is a sample or two on a clean signal.
    /// Needs samples up to `trigger + 2 * sym`.
    fn best_preamble0_position(&self, trigger: u64, sym: usize, hop: usize) -> u64 {
        let target = self.params.tone_hz(self.params.preamble[0]);
        let power =
            |at: u64| goertzel_power(&self.window_at(at, sym), target, self.config.sample_rate_hz);
        self.maximize_position(trigger, sym, hop, power)
    }

    /// Joint-acquisition fine alignment: like
    /// [`best_preamble0_position`](Self::best_preamble0_position), but
    /// maximizes the expected-tone power summed over all preamble symbols
    /// (`at + i * period`), so a weak or missing symbol does not decide the
    /// symbol clock. Needs samples up to `trigger + 2 * sym + (N - 1) * period`.
    fn best_preamble_position(&self, trigger: u64, sym: usize, hop: usize, period: usize) -> u64 {
        let power = |at: u64| -> f32 {
            self.params
                .preamble
                .iter()
                .enumerate()
                .map(|(i, &nibble)| {
                    let start = (at + (i * period) as u64 - self.base()) as usize;
                    goertzel_power_energy(
                        self.buffer.range(start..start + sym).copied(),
                        sym,
                        self.params.tone_hz(nibble),
                        self.config.sample_rate_hz,
                    )
                    .0
                })
                .sum()
        };
        self.maximize_position(trigger, sym, hop, power)
    }

    /// Window start in `[trigger - hop, trigger + sym]` (clamped to buffered
    /// history and `align_floor`) maximizing `power`: coarse grid of `hop / 2`
    /// through `trigger`, earliest first (ties keep the earlier), then halving
    /// refinement down to one sample.
    fn maximize_position(
        &self,
        trigger: u64,
        sym: usize,
        hop: usize,
        power: impl Fn(u64) -> f32,
    ) -> u64 {
        let lo = trigger
            .saturating_sub(hop as u64)
            .max(self.base())
            .max(self.align_floor.min(trigger));
        let hi = trigger + sym as u64;
        let mut best = trigger;
        let mut best_power = power(trigger);
        let mut consider = |at: u64, best: &mut u64| {
            if (lo..=hi).contains(&at) && at != *best {
                let p = power(at);
                if p > best_power {
                    best_power = p;
                    *best = at;
                }
            }
        };
        let step = (hop / 2).max(1) as u64;
        // Coarse grid through `trigger`, earliest first (ties keep the earlier).
        let mut at = trigger - (trigger - lo) / step * step;
        while at <= hi {
            consider(at, &mut best);
            at += step;
        }
        let mut step = step / 2;
        while step >= 1 {
            let center = best;
            if let Some(before) = center.checked_sub(step) {
                consider(before, &mut best);
            }
            consider(center + step, &mut best);
            step /= 2;
        }
        best
    }
}

/// The MFSK receiver pauses after the last body nibble until the modem
/// reports the decode result: `confirm` continues after the body, `reject`
/// rewinds the search into it (see [`ModemRx`]).
impl PhyRx for ModemRx {
    fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent> {
        ModemRx::push_samples(self, samples)
    }

    fn link_state(&self) -> LinkState {
        ModemRx::link_state(self)
    }

    fn last_confidence_db(&self) -> f32 {
        ModemRx::last_confidence_db(self)
    }

    fn samples_pushed(&self) -> u64 {
        ModemRx::samples_pushed(self)
    }

    fn set_expected_body_nibbles(&mut self, n: usize) {
        ModemRx::set_expected_body_nibbles(self, n)
    }

    fn confirm(&mut self) -> Vec<RxEvent> {
        ModemRx::confirm(self)
    }

    fn reject(&mut self) -> Vec<RxEvent> {
        ModemRx::reject(self)
    }

    fn reset(&mut self) {
        ModemRx::reset(self)
    }
}
