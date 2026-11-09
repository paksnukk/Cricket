use super::{BmcParams, Modulation};
use crate::phy::{DemodResult, LinkState, PhyConfig, RxEvent, LOCK_HOLD_MS};
use alloc::vec;
use alloc::vec::Vec;
use core::f32::consts::PI;

/// Absolute stream position: sample index + fraction in `[0, 1)`.
#[derive(Debug, Clone, Copy)]
struct Pos {
    index: u64,
    frac: f32,
}

impl Pos {
    /// `self - earlier` in samples.
    fn since(self, earlier: Pos) -> f32 {
        (self.index as i64 - earlier.index as i64) as f32 + (self.frac - earlier.frac)
    }
}

/// Direct-form-I biquad.
#[derive(Debug, Clone)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// RBJ-cookbook Butterworth (Q = 1/sqrt 2) low-pass.
    fn lowpass(cutoff_hz: f32, sample_rate_hz: u32) -> Self {
        let fc = cutoff_hz.min(0.45 * sample_rate_hz as f32);
        let w0 = 2.0 * PI * fc / sample_rate_hz as f32;
        let alpha = libm::sinf(w0) / (2.0 * core::f32::consts::FRAC_1_SQRT_2);
        let cw = libm::cosf(w0);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - cw) / 2.0 / a0,
            b1: (1.0 - cw) / a0,
            b2: (1.0 - cw) / 2.0 / a0,
            a1: -2.0 * cw / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// Clear the filter state (coefficients are kept).
    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// Interval class in half cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interval {
    Half,
    Full,
    Violation,
    Invalid,
}

fn classify(units: f32) -> Interval {
    if units < 0.5 {
        Interval::Invalid
    } else if units < 1.5 {
        Interval::Half
    } else if units < 2.5 {
        Interval::Full
    } else if units < 3.5 {
        Interval::Violation
    } else {
        Interval::Invalid
    }
}

/// Sync peak candidate: squared normalized correlation and body start.
#[derive(Debug, Clone, Copy)]
struct SyncPeak {
    rho: f32,
    at: Pos,
}

/// Body being read on a half-cell time grid anchored at the sync.
#[derive(Debug, Clone)]
struct Body {
    /// Stream index all body times are relative to (sync edge, integer part).
    base: u64,
    /// Grid point `ref_grid` (in half cells from the body start) lies at
    /// relative time `ref_t` (samples).
    ref_t: f32,
    ref_grid: i32,
    /// Current half-cell estimate (samples).
    half: f32,
    /// Next bit to decide.
    bit: i32,
    acc: u8,
    acc_bits: u8,
    /// Smallest bit margin in the current nibble (0..=1).
    nibble_margin: f32,
    nibble_erased: bool,
    nibbles: usize,
    erasures: usize,
    /// Correlation of the sync that started this body.
    sync_rho: f32,
    /// Transitions seen in the body, and those close to a grid point.
    edges: u16,
    edges_on_grid: u16,
    /// Typical `|s1| + |s2|` of a bit (dropout reference).
    bit_energy: f32,
    margin_sum: f32,
}

impl Body {
    fn grid(&self, g: i32) -> f32 {
        self.ref_t + (g - self.ref_grid) as f32 * self.half
    }
}

/// Streaming biphase-mark receiver emitting the same [`RxEvent`]s as the
/// MFSK [`ModemRx`](crate::phy::ModemRx) (one `BodyNibble` per 4 bits).
///
/// **Sync** is a correlator on the filtered signal: half-cell box sums `B_j`
/// are matched against the end of the preamble plus the two code violations
/// (`... + - + - - - - + + +`, sign-free). The best peak of the normalized
/// correlation `|sum c_j B_j| / sqrt(N sum B_j^2)` above `sync_threshold`
/// (refined to a fraction of a sample, then snapped to the transition that
/// ends the second violation) marks the body start. It keeps running while a
/// body is read: a clearly stronger sync replaces a weaker lock, so a false
/// lock never hides a real packet and nothing has to be rewound.
///
/// **Body** bits are soft decisions on a half-cell grid: each half cell of
/// the filtered signal is integrated (`s1`, `s2`) and a bit is `1` when the
/// halves differ in sign (`|s1 - s2| > |s1 + s2|`). The normalized
/// difference of the two is the bit margin (1 = ideal, 0 = coin toss); a
/// nibble's reliability is its smallest bit margin, and a nibble containing
/// a dropout (no signal) is flagged erased. The grid is a second-order loop
/// driven by the Schmitt-trigger transitions near grid points, so a spurious
/// or missing transition costs at most the bits it touches, never the rest
/// of the body. With FEC, a body with more erased nibbles than the code can
/// fill is abandoned; without FEC the CRC decides.
pub struct BmcRx {
    config: PhyConfig,
    params: BmcParams,
    half_nominal: f32,
    // Front end
    hp_r: f32,
    hp_x1: f32,
    hp_y1: f32,
    lp: Biquad,
    env: f32,
    env_decay: f32,
    floor: f32,
    prev_y: f32,
    /// Filtered-signal history (ring, power-of-two length) for integration.
    history: Vec<f32>,
    /// Schmitt state: `Some(true)` high, `Some(false)` low, `None` gated.
    high: Option<bool>,
    last_zc_up: Option<Pos>,
    last_zc_down: Option<Pos>,
    /// Last transitions (ring), to snap a sync peak to the true edge.
    recent_edges: [Option<Pos>; 8],
    recent_next: usize,
    samples_pushed: u64,
    // Sync correlator
    /// Half-cell box sums (ring, power-of-two length).
    boxes: Vec<f32>,
    box_len: usize,
    /// Template taps: (delay in samples, sign), newest half cell first.
    taps: Vec<(usize, f32)>,
    /// `sync_fractional`: the template as weights over history delays, for
    /// boxes ending 0, 1/2 and 3/2 samples before the end of the newest
    /// sample ([`FRAC_BACKS`]). In each set box `j` (same order as `taps`)
    /// is the exact half-cell integral `j` to `j + 1` half cells before
    /// that end (sample `d` covers `[d, d + 1]` samples before the newest
    /// sample's end). Empty otherwise.
    frac_boxes: Vec<Vec<Vec<(usize, f32)>>>,
    /// Last two squared normalized correlations.
    rho1: f32,
    rho2: f32,
    pending: Option<SyncPeak>,
    /// `post_body_guard`: until this position (where the next burst's sync
    /// can first be), syncs are held to the replacement rule against the
    /// last body's sync correlation (the second field), as if that body were
    /// still being read.
    guard: Option<(Pos, f32)>,
    // Decoder
    body: Option<Body>,
    /// Stream position where the last completed body ended.
    last_body_end: Option<u64>,
    expected_body_nibbles: usize,
    max_erasures: usize,
    last_confidence_db: f32,
}

/// Box end offsets of [`BmcRx::frac_boxes`] (samples back from the newest
/// sample's end): the per-sample output, and the half-sample points either
/// side of the sample before it (a peak candidate).
const FRAC_BACKS: [f32; 3] = [0.0, 0.5, 1.5];

/// Sample weights of a box `len` samples long that ends `start` samples
/// before the end of the newest sample (delay `d` covers `[d, d + 1]`).
fn box_weights(start: f32, len: f32) -> Vec<(usize, f32)> {
    let end = start + len;
    let mut w = Vec::new();
    let mut d = libm::floorf(start) as usize;
    while (d as f32) < end {
        let overlap = (d as f32 + 1.0).min(end) - (d as f32).max(start);
        if overlap > 1e-6 {
            w.push((d, overlap));
        }
        d += 1;
    }
    w
}

/// Nibble reliability in dB from a bit margin `m` in `0..=1`
/// (`20 log10((1 + m) / (1 - m))`, capped at 40 dB).
fn margin_db(m: f32) -> f32 {
    let m = m.clamp(0.0, 1.0);
    (20.0 * libm::log10f((1.0 + m) / (1.0 - m).max(1e-3))).min(40.0)
}

impl BmcRx {
    /// Panics if `config` does not select [`Modulation::Bmc`].
    pub fn new(config: PhyConfig) -> Self {
        let params = match &config.modulation {
            Modulation::Bmc(p) => p.clone(),
            _ => panic!("BmcRx needs a BMC PhyConfig"),
        };
        let sr = config.sample_rate_hz;
        let bit_rate = params.bit_rate as f32;
        let half_nominal = params.half_cell_samples(sr);
        let hp_r = libm::expf(-2.0 * PI * params.rx_highpass_ratio * bit_rate / sr as f32);
        let lp = Biquad::lowpass(params.rx_lowpass_ratio * bit_rate, sr);
        let release_samples = (params.envelope_release_bits * 2.0 * half_nominal).max(1.0);
        let env_decay = libm::expf(-1.0 / release_samples);
        // Silence floor: min_rms_dbfs is relative to a full-scale sine
        // (-3 dBFS RMS = peak 1.0); the envelope is a peak.
        let floor = libm::powf(10.0, (config.min_rms_dbfs + 3.0) / 20.0);
        // History: a bit decision looks back ~3 half cells (+ rate error).
        let history_len =
            (((4.5 + params.decision_delay) * half_nominal) as usize + 8).next_power_of_two();
        let max_erasures = config.fec.max_body_erasures();

        // Sync template, newest half cell first: second violation (3 x q),
        // first violation (3 x p), then the preamble alternating q, p, ...
        let n_taps = 6 + params.sync_preamble_half_cells;
        let taps: Vec<(usize, f32)> = (0..n_taps)
            .map(|j| {
                let sign = match j {
                    0..=2 => -1.0,
                    3..=5 => 1.0,
                    _ if (j - 6) % 2 == 0 => -1.0,
                    _ => 1.0,
                };
                (libm::roundf(j as f32 * half_nominal) as usize, sign)
            })
            .collect();
        let box_len = (libm::roundf(half_nominal) as usize).max(1);
        let boxes_len = (taps[n_taps - 1].0 + 1).next_power_of_two();
        // Rounded boxes and taps are exact at 4800 and 2400 bit/s (48 kHz)
        // but smear the template when a half cell is only 1.5-2.5 samples.
        let frac_boxes: Vec<Vec<Vec<(usize, f32)>>> = if params.sync_fractional {
            FRAC_BACKS
                .iter()
                .map(|&back| {
                    (0..n_taps)
                        .map(|j| box_weights(back + j as f32 * half_nominal, half_nominal))
                        .collect()
                })
                .collect()
        } else {
            Vec::new()
        };
        let frac_span = (n_taps as f32 * half_nominal + FRAC_BACKS[2]) as usize + 3;
        let history_len = if params.sync_fractional {
            history_len.max(frac_span.next_power_of_two())
        } else {
            history_len
        };
        Self {
            config,
            params,
            half_nominal,
            hp_r,
            hp_x1: 0.0,
            hp_y1: 0.0,
            lp,
            env: 0.0,
            env_decay,
            floor,
            prev_y: 0.0,
            history: vec![0.0; history_len.max(box_len.next_power_of_two())],
            high: None,
            last_zc_up: None,
            last_zc_down: None,
            recent_edges: [None; 8],
            recent_next: 0,
            samples_pushed: 0,
            boxes: vec![0.0; boxes_len],
            box_len,
            taps,
            frac_boxes,
            rho1: 0.0,
            rho2: 0.0,
            pending: None,
            guard: None,
            body: None,
            last_body_end: None,
            expected_body_nibbles: 16,
            max_erasures,
            last_confidence_db: 0.0,
        }
    }

    pub fn config(&self) -> &PhyConfig {
        &self.config
    }

    /// See [`LinkState`]: `Locked` while a body is read and for
    /// [`LOCK_HOLD_MS`] after a completed body.
    pub fn link_state(&self) -> LinkState {
        let hold = self.config.sample_rate_hz as u64 * LOCK_HOLD_MS as u64 / 1000;
        let recent = self
            .last_body_end
            .is_some_and(|end| self.samples_pushed.saturating_sub(end) < hold);
        if self.body.is_some() || recent {
            LinkState::Locked
        } else {
            LinkState::Search
        }
    }

    /// Mean nibble reliability of the last packet (see [`margin_db`]; an
    /// ideal line is ~30-40 dB, a coin-toss bit 0 dB).
    pub fn last_confidence_db(&self) -> f32 {
        self.last_confidence_db
    }

    pub fn set_expected_body_nibbles(&mut self, n: usize) {
        self.expected_body_nibbles = n;
    }

    pub fn samples_pushed(&self) -> u64 {
        self.samples_pushed
    }

    /// Back to search with the front end (filters, envelope, transition
    /// memory) and the sync correlator cleared; the sample count goes on.
    pub fn reset(&mut self) {
        self.body = None;
        self.pending = None;
        self.guard = None;
        self.rho1 = 0.0;
        self.rho2 = 0.0;
        self.last_body_end = None;
        self.high = None;
        self.recent_edges = [None; 8];
        self.hp_x1 = 0.0;
        self.hp_y1 = 0.0;
        self.lp.reset();
        self.env = 0.0;
        self.prev_y = 0.0;
        self.last_zc_up = None;
        self.last_zc_down = None;
        self.history.fill(0.0);
        self.boxes.fill(0.0);
    }

    pub fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent> {
        let mut events = Vec::new();
        for &x in samples {
            self.push_one(x, &mut events);
        }
        events.push(RxEvent::Metrics {
            state: self.link_state(),
            confidence_db: self.last_confidence_db,
        });
        events
    }

    fn push_one(&mut self, x: f32, events: &mut Vec<RxEvent>) {
        let index = self.samples_pushed;
        self.samples_pushed += 1;

        // A non-finite sample (a glitching driver, a NaN from upstream DSP)
        // would otherwise sit in the IIR filter state for good and silence
        // the receiver until it is recreated; treat it as a dropout sample.
        let x = if x.is_finite() { x } else { 0.0 };
        // DC blocker, then low-pass.
        let hp = x - self.hp_x1 + self.hp_r * self.hp_y1;
        self.hp_x1 = x;
        self.hp_y1 = hp;
        let mut y = self.lp.process(hp);
        if !y.is_finite() {
            // Overflow on absurd (but finite) input: restart the filters.
            self.hp_x1 = 0.0;
            self.hp_y1 = 0.0;
            self.lp.reset();
            y = 0.0;
        }
        let mask = self.history.len() - 1;
        self.history[index as usize & mask] = y;

        let active = self.front_end(index, y);
        self.correlate(index, active, events);

        // Bit decision once its whole cell, and the transitions that time
        // it, are in the history.
        if let Some(body) = &self.body {
            let decide_at = body.grid(2 * body.bit + 2) + self.params.decision_delay * body.half;
            if (index - body.base) as f32 >= decide_at {
                self.decide_bit(events);
            }
        }
    }

    /// Envelope, Schmitt trigger and transition timing. Returns false while
    /// the input is below the silence floor.
    fn front_end(&mut self, index: u64, y: f32) -> bool {
        // Interpolated zero crossings between samples index-1 and index.
        let prev = self.prev_y;
        if prev < 0.0 && y >= 0.0 && index > 0 {
            self.last_zc_up = Some(Pos {
                index: index - 1,
                frac: prev / (prev - y),
            });
        } else if prev >= 0.0 && y < 0.0 && index > 0 {
            self.last_zc_down = Some(Pos {
                index: index - 1,
                frac: prev / (prev - y),
            });
        }
        self.prev_y = y;

        let a = libm::fabsf(y);
        self.env = if a > self.env {
            a
        } else {
            self.env * self.env_decay
        };
        if self.env < self.floor {
            // A body keeps running: the silence shows up as erasures.
            if self.high.take().is_some() {
                self.recent_edges = [None; 8];
            }
            return false;
        }

        let th = self.params.hysteresis * self.env;
        let flipped_to = if y > th && self.high != Some(true) {
            Some(true)
        } else if y < -th && self.high != Some(false) {
            Some(false)
        } else {
            None
        };
        let Some(level) = flipped_to else {
            return true;
        };
        let was = self.high.replace(level);
        if was.is_none() {
            // Leaving the gate: signal onset, not a code transition.
            return true;
        }
        let zc = if level {
            self.last_zc_up
        } else {
            self.last_zc_down
        };
        let Some(edge) = zc else {
            return true;
        };
        self.track_edge(edge);
        self.recent_edges[self.recent_next] = Some(edge);
        self.recent_next = (self.recent_next + 1) % self.recent_edges.len();
        true
    }

    /// Sync correlator, one output per sample (see the type docs).
    fn correlate(&mut self, index: u64, active: bool, events: &mut Vec<RxEvent>) {
        let hmask = self.history.len() - 1;
        let fractional = self.params.sync_fractional;
        let mut rho = 0.0;
        if fractional {
            if active {
                rho = self.frac_rho(index, 0);
            }
        } else {
            let mut b = 0.0;
            for k in 0..self.box_len as u64 {
                if k <= index {
                    b += self.history[(index - k) as usize & hmask];
                }
            }
            let bmask = self.boxes.len() - 1;
            self.boxes[index as usize & bmask] = b;

            if active {
                let mut c = 0.0;
                let mut energy = 0.0;
                for &(delay, sign) in &self.taps {
                    let v = self.boxes[(index as usize).wrapping_sub(delay) & bmask];
                    c += sign * v;
                    energy += v * v;
                }
                // Normalized correlation, squared: c^2 / (N sum v^2) <= 1.
                let norm = self.taps.len() as f32 * energy;
                if norm > 1e-12 {
                    rho = c * c / norm;
                }
            }
        }

        // Local maximum at index - 1?
        let (r2, r1, r0) = (self.rho2, self.rho1, rho);
        self.rho2 = r1;
        self.rho1 = r0;
        let th = self.params.sync_threshold;
        if index >= 2 && r1 >= th * th && r1 > r2 && r1 >= r0 {
            let den = r2 - 2.0 * r1 + r0;
            let delta = if den < 0.0 {
                (0.5 * (r2 - r0) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            // Box sums end half a sample after their last sample.
            let t = delta + 0.5;
            let at = if t >= 1.0 {
                Pos {
                    index,
                    frac: t - 1.0,
                }
            } else {
                Pos {
                    index: index - 1,
                    frac: t,
                }
            };
            // With fractional boxes the peak can fall between two samples,
            // where it is up to ~0.1 higher at 1.7 samples per half cell:
            // also evaluate the two half-sample points next to it, so a real
            // sync scores ~0.95 or more (needed by the replacement rules).
            let r1 = if fractional {
                r1.max(self.frac_rho(index, 1)).max(self.frac_rho(index, 2))
            } else {
                r1
            };
            if self.pending.is_none_or(|p| r1 > p.rho) {
                self.pending = Some(SyncPeak { rho: r1, at });
            }
        }

        // Accept the best peak once no better one followed for 3.5 half
        // cells (side peaks sit 2 and 3 half cells before the true one).
        if let Some(peak) = self.pending {
            let since = Pos { index, frac: 0.0 }.since(peak.at);
            if since >= 3.5 * self.half_nominal {
                self.pending = None;
                self.on_sync(peak, events);
            }
        }
    }

    /// Squared normalized sync correlation (`sync_fractional`) for boxes
    /// ending `FRAC_BACKS[set]` samples before the end of sample `index`.
    fn frac_rho(&self, index: u64, set: usize) -> f32 {
        let hmask = self.history.len() - 1;
        let mut c = 0.0;
        let mut energy = 0.0;
        for (w, &(_, sign)) in self.frac_boxes[set].iter().zip(&self.taps) {
            let mut v = 0.0;
            for &(d, wt) in w {
                v += wt * self.history[(index as usize).wrapping_sub(d) & hmask];
            }
            c += sign * v;
            energy += v * v;
        }
        // Normalized correlation, squared: c^2 / (N sum v^2) <= 1.
        let norm = self.taps.len() as f32 * energy;
        if norm > 1e-12 {
            c * c / norm
        } else {
            0.0
        }
    }

    /// Grid timing loop: a transition near a half-cell grid point pulls the
    /// grid towards it (phase) and nudges the half-cell length (rate).
    fn track_edge(&mut self, edge: Pos) {
        let kp = self.params.pll_phase_gain;
        let ki = self.params.pll_rate_gain;
        let Some(body) = &mut self.body else { return };
        let t = (edge.index as i64 - body.base as i64) as f32 + edge.frac;
        let u = (t - body.ref_t) / body.half + body.ref_grid as f32;
        let g = libm::roundf(u);
        let err = u - g;
        let window = self.params.pll_window;
        body.edges = body.edges.saturating_add(1);
        if libm::fabsf(err) < 0.25 {
            body.edges_on_grid = body.edges_on_grid.saturating_add(1);
        }
        if libm::fabsf(err) > window || g < 0.0 {
            return; // Too far off the grid to be a code transition.
        }
        // Transitions near the edge of the window (ISI, noise) count less.
        let err = err * (1.0 - self.params.pll_taper * libm::fabsf(err) / window);
        let g = g as i32;
        body.ref_t = body.grid(g) + kp * err * body.half;
        body.ref_grid = g;
        let half = body.half * (1.0 + ki * err);
        let max_dev = 1.5 * self.params.max_rate_error;
        body.half = half.clamp(
            self.half_nominal * (1.0 - max_dev),
            self.half_nominal * (1.0 + max_dev),
        );
    }

    fn on_sync(&mut self, peak: SyncPeak, events: &mut Vec<RxEvent>) {
        // Inside a body (or its trailing guard) only a clearly stronger
        // sync, or one that body data cannot produce, takes over.
        let held = match (&self.body, self.guard) {
            (Some(body), _) => Some(body.sync_rho),
            (None, Some((until, rho))) if peak.at.since(until) < 0.0 => Some(rho),
            _ => None,
        };
        if let Some(lock_rho) = held {
            let certain = self.params.resync_certain * self.params.resync_certain;
            if peak.rho < lock_rho + self.params.resync_margin && peak.rho < certain {
                return;
            }
        }
        self.guard = None;
        // The body starts at the nominal rate; a rate error is left to the
        // timing loop (a preamble estimate is too noisy at low SNR).
        let half = self.half_nominal;
        let at = self.sync_edge(peak.at);
        let bit_energy = self.env * self.window_fraction() * 2.0 * half;
        self.body = Some(Body {
            base: at.index,
            ref_t: at.frac,
            ref_grid: 0,
            half,
            bit: 0,
            acc: 0,
            acc_bits: 0,
            nibble_margin: 1.0,
            nibble_erased: false,
            nibbles: 0,
            erasures: 0,
            sync_rho: peak.rho,
            edges: 0,
            edges_on_grid: 0,
            bit_energy,
            margin_sum: 0.0,
        });
        events.push(RxEvent::PreambleLocked {
            confidence_db: margin_db(peak.rho),
        });
    }

    /// Body start for a correlation peak at `at`: preferably a transition
    /// preceded by two violation-length intervals (the end of the second
    /// violation) within 1.5 half cells, else the nearest transition within
    /// `sync_snap` half cells, else the peak itself.
    fn sync_edge(&self, at: Pos) -> Pos {
        let half = self.half_nominal;
        let n = self.recent_edges.len();
        // Chronological order, oldest first.
        let edge = |k: usize| self.recent_edges[(self.recent_next + k) % n];
        let mut best: Option<(f32, Pos)> = None;
        for k in 2..n {
            let (Some(e0), Some(e1), Some(e2)) = (edge(k - 2), edge(k - 1), edge(k)) else {
                continue;
            };
            let d = libm::fabsf(e2.since(at)) / half;
            let v1 = classify(e1.since(e0) / half) == Interval::Violation;
            let v2 = classify(e2.since(e1) / half) == Interval::Violation;
            if v1 && v2 && d < 1.5 && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, e2));
            }
        }
        if let Some((_, e)) = best {
            return e;
        }
        self.recent_edges
            .iter()
            .flatten()
            .map(|e| (libm::fabsf(e.since(at)) / half, *e))
            .filter(|(d, _)| *d < self.params.sync_snap)
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal))
            .map_or(at, |(_, e)| e)
    }

    /// Fraction of each half cell that is integrated.
    fn window_fraction(&self) -> f32 {
        1.0 - self.params.rx_window_trim - self.params.rx_window_outer_trim
    }

    /// Weighted sum of the filtered signal over relative time `[a, b]`
    /// (sample `n` covers `[n - 0.5, n + 0.5]`).
    fn integrate(&self, base: u64, a: f32, b: f32) -> f32 {
        let mask = self.history.len() - 1;
        let oldest = self
            .samples_pushed
            .saturating_sub(self.history.len() as u64);
        let lo = libm::floorf(a + 0.5) as i64;
        let hi = libm::floorf(b + 0.5) as i64;
        let mut sum = 0.0;
        for n in lo..=hi {
            let w = (n as f32 + 0.5).min(b) - (n as f32 - 0.5).max(a);
            let idx = base as i64 + n;
            if w <= 0.0 || idx < oldest as i64 || idx >= self.samples_pushed as i64 {
                continue;
            }
            sum += w * self.history[idx as usize & mask];
        }
        sum
    }

    fn decide_bit(&mut self, events: &mut Vec<RxEvent>) {
        let Some(body) = &self.body else { return };
        let inner = self.params.rx_window_trim * body.half;
        let outer = self.params.rx_window_outer_trim * body.half;
        let g = 2 * body.bit;
        let (t0, t1, t2) = (body.grid(g), body.grid(g + 1), body.grid(g + 2));
        // Every cell starts with a transition, so the half cells either side
        // of this cell are inverted second looks at its first and second
        // half: averaging them in halves the noise of both estimates.
        let s0 = self.integrate(body.base, body.grid(g - 1) + inner, t0 - outer);
        let s1 = self.integrate(body.base, t0 + outer, t1 - inner);
        let s2 = self.integrate(body.base, t1 + inner, t2 - outer);
        let s3 = self.integrate(body.base, t2 + outer, body.grid(g + 3) - inner);
        let (s1, s2) = (0.5 * (s1 - s0), 0.5 * (s2 - s3));
        let one = libm::fabsf(s1 - s2);
        let zero = libm::fabsf(s1 + s2);
        let energy = libm::fabsf(s1) + libm::fabsf(s2);
        let dropout = energy < self.params.dropout_ratio * body.bit_energy;
        let margin = if dropout {
            0.0
        } else {
            libm::fabsf(one - zero) / (one + zero).max(1e-12)
        };

        let body = self.body.as_mut().expect("body");
        if !dropout {
            body.bit_energy += (energy - body.bit_energy) / 8.0;
        }
        body.bit += 1;
        body.acc = (body.acc << 1) | (one > zero) as u8;
        body.acc_bits += 1;
        body.nibble_margin = body.nibble_margin.min(margin);
        body.nibble_erased |= dropout;
        if body.acc_bits < 4 {
            return;
        }
        let nibble = body.acc & 0x0F;
        let margin = body.nibble_margin;
        let erased = body.nibble_erased || margin < self.params.erase_margin;
        body.acc = 0;
        body.acc_bits = 0;
        body.nibble_margin = 1.0;
        body.nibble_erased = false;
        body.nibbles += 1;
        body.margin_sum += margin;
        if erased {
            body.erasures += 1;
            // Without FEC an erasure is just a likely bit error: the CRC
            // decides, and the parallel sync search loses nothing meanwhile.
            if self.max_erasures > 0 && body.erasures > self.max_erasures {
                // More gaps than the code can fill: not (or no longer) a packet.
                self.body = None;
                events.push(RxEvent::SyncLost);
                return;
            }
        }
        let confidence_db = margin_db(margin);
        let done = body.nibbles >= self.expected_body_nibbles;
        if done {
            let mean = body.margin_sum / body.nibbles as f32;
            let on_grid = body.edges_on_grid as f32 / body.edges.max(1) as f32;
            // Where the next burst's sync can first be (`post_body_guard`):
            // the end of a burst (last body bits, postamble, tail, fade into
            // silence) can look like a sync to the correlator.
            let p = &self.params;
            let units = (2 * p.postamble_bits + 2 * p.gap_bits + 2 * p.preamble_bits + 7) as f32
                + p.edge_frac
                - 2.0;
            let t = body.grid(2 * body.bit) + units * body.half;
            let whole = libm::floorf(t);
            let guard = (
                Pos {
                    index: body.base + whole as u64,
                    frac: t - whole,
                },
                body.sync_rho,
            );
            self.body = None;
            if mean < self.params.min_body_margin || on_grid < self.params.min_on_grid {
                // Noise-like body: a false lock (noise, MFSK tones). Dropping
                // it keeps the CRC / FEC from ever seeing it.
                events.push(RxEvent::SyncLost);
                return;
            }
            self.last_confidence_db = margin_db(mean);
            if self.params.post_body_guard {
                self.guard = Some(guard);
            }
            // Body complete: `Locked` for a while (see `link_state`).
            self.last_body_end = Some(self.samples_pushed);
        }
        events.push(RxEvent::BodyNibble {
            result: DemodResult {
                nibble,
                power: self.env * self.env,
                confidence_db,
                purity: margin,
                erased,
            },
            end_sample: self.samples_pushed,
        });
    }
}
