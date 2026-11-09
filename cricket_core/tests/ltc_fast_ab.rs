//! A/B measurements for the fast LTC (BMC) profiles on clean digital paths
//! (Dante / MADI / AES): bit rates above 9600 bit/s, converter-like
//! anti-alias filters, short overhead. Prints Markdown tables; apart from
//! "no false messages" it asserts nothing. Run with
//!
//! ```sh
//! cargo test --release -p cricket_core --test ltc_fast_ab -- --ignored --nocapture
//! ```
//!
//! Environment: `FAST_AB_TRIALS` (default 400 per cell), `FAST_AB_PROFILES`
//! (comma-separated keys, see [`profile`]), `FAST_AB_SET` (`snr`, `filter`,
//! `impair`, `all`, or `quick`). Results are recorded in
//! `docs/proposals/ltc-bmc.md` §10.

mod support;

use cricket_core::link::FecMode;
use cricket_core::modem::{
    encode_to_samples, repetition_samples, CricketModemRx, CricketModemTx, ModemOutput,
};
use cricket_core::phy::{Modulation, PhyConfig};
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage};
use std::f64::consts::PI;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use support::{ChannelSim, XorShift64};

// ---------------------------------------------------------------------------
// Profiles

/// Profile by key: `name[@sr_khz][/param=value...]`.
///
/// `name` is a profile name [`PhyConfig::from_name`] knows, or
/// `ltc<rate>[_fec[<parity>]]` for any bit rate (with the original LTC
/// parameters; `/fast=1` switches to `BmcParams::fast`). `@96` runs it at
/// 96 kHz. Parameters override the BMC parameters in order (wire: `edge`,
/// `pre`, `post`, `gap`; receiver: see the match below). `pre` also sets
/// the sync template to the whole preamble unless `sync_pre` follows.
fn profile(key: &str) -> PhyConfig {
    let mut parts = key.split('/');
    let head = parts.next().expect("profile key");
    let (name, sr) = match head.split_once('@') {
        Some((n, k)) => (n, Some(k.parse::<u32>().expect("sample rate kHz") * 1000)),
        None => (head, None),
    };
    let known = PhyConfig::from_name(name);
    let mut config = if let Some(known) =
        known.filter(|k| matches!(k.modulation, Modulation::Bmc(_)) || !name.starts_with("ltc"))
    {
        known
    } else {
        let (rate, parity) = match name.split_once("_fec") {
            Some((r, p)) => (r, p.parse::<u8>().unwrap_or(4)),
            None => (name, 0),
        };
        let rate = match rate.trim_start_matches("ltc") {
            "" => 4800,
            r => r.parse().expect("bit rate"),
        };
        PhyConfig::ltc_with_bit_rate(rate).with_fec(if parity == 0 {
            FecMode::None
        } else {
            FecMode::Rs16 { parity }
        })
    };
    if let Some(sr) = sr {
        config = config.with_sample_rate(sr);
    }
    let Modulation::Bmc(p) = &mut config.modulation else {
        return config;
    };
    let mut sync_pre_set = false;
    for kv in parts.filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').expect("name=value");
        let v: f32 = v.parse().expect("number");
        match k {
            // The fast-profile parameter set (put it first).
            "fast" => *p = cricket_core::phy::BmcParams::fast(p.bit_rate),
            "edge" => p.edge_frac = v,
            "pre" => {
                p.preamble_bits = v as usize;
                if !sync_pre_set {
                    p.sync_preamble_half_cells = 2 * p.preamble_bits;
                }
            }
            "post" => p.postamble_bits = v as usize,
            "gap" => p.gap_bits = v as usize,
            "lp" => p.rx_lowpass_ratio = v,
            "hp" => p.rx_highpass_ratio = v,
            "hyst" => p.hysteresis = v,
            "trim" => p.rx_window_trim = v,
            "otrim" => p.rx_window_outer_trim = v,
            "kp" => p.pll_phase_gain = v,
            "ki" => p.pll_rate_gain = v,
            "window" => p.pll_window = v,
            "taper" => p.pll_taper = v,
            "dropout" => p.dropout_ratio = v,
            "delay" => p.decision_delay = v,
            "erase" => p.erase_margin = v,
            "body_margin" => p.min_body_margin = v,
            "on_grid" => p.min_on_grid = v,
            "sync_th" => p.sync_threshold = v,
            "sync_pre" => {
                p.sync_preamble_half_cells = v as usize;
                sync_pre_set = true;
            }
            "snap" => p.sync_snap = v,
            "resync" => p.resync_margin = v,
            "certain" => p.resync_certain = v,
            "max_rate" => p.max_rate_error = v,
            "env" => p.envelope_release_bits = v,
            "frac" => p.sync_fractional = v != 0.0,
            "guard" => p.post_body_guard = v != 0.0,
            _ => panic!("unknown parameter {k}"),
        }
    }
    config
}

// ---------------------------------------------------------------------------
// Converter / codec filter models (applied to the transmitted audio)

/// Modified Bessel function I0 (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < 1e-14 * sum {
            break;
        }
    }
    sum
}

/// Linear-phase Kaiser-windowed sinc low-pass: flat to `pass_hz`, at least
/// `atten_db` down from `stop_hz`. Odd length; delay (len - 1) / 2.
fn kaiser_lowpass(pass_hz: f64, stop_hz: f64, atten_db: f64, sr: f64) -> Vec<f64> {
    let dw = 2.0 * PI * (stop_hz - pass_hz) / sr;
    let mut n = ((atten_db - 7.95) / (2.285 * dw)).ceil() as usize + 1;
    if n.is_multiple_of(2) {
        n += 1;
    }
    let beta = if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    };
    let fc = (pass_hz + stop_hz) / 2.0 / sr; // cycles per sample
    let m = (n - 1) as f64 / 2.0;
    let i0b = bessel_i0(beta);
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - m;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * PI * fc * x).sin() / (PI * x)
            };
            let r = x / m;
            sinc * bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b
        })
        .collect();
    let dc: f64 = h.iter().sum();
    for v in &mut h {
        *v /= dc;
    }
    h
}

/// Zero-delay FIR (the linear-phase delay is removed: converter latency is
/// not codec latency).
fn fir_zero_delay(h: &[f64], x: &[f32]) -> Vec<f32> {
    let d = (h.len() - 1) / 2;
    (0..x.len())
        .map(|n| {
            let mut acc = 0.0;
            for (k, &hk) in h.iter().enumerate() {
                let i = n as isize + d as isize - k as isize;
                if i >= 0 && (i as usize) < x.len() {
                    acc += hk * x[i as usize] as f64;
                }
            }
            acc as f32
        })
        .collect()
}

/// Delay by `frac` (0..1) samples: Kaiser-windowed sinc, 64 taps, flat
/// (+-0.05 dB) to ~0.45 fs. An analog leg between two converters on a
/// common clock shifts the transitions by an arbitrary fraction of a sample;
/// the filter conditions draw it at random so they cover every phase.
fn fractional_delay(x: &[f32], frac: f64) -> Vec<f32> {
    const HALF: isize = 32;
    let beta = 8.0;
    let i0b = bessel_i0(beta);
    let h: Vec<f64> = (-HALF + 1..=HALF)
        .map(|k| {
            let t = k as f64 - frac;
            let sinc = if t.abs() < 1e-12 {
                1.0
            } else {
                (PI * t).sin() / (PI * t)
            };
            let r = t / HALF as f64;
            sinc * bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b
        })
        .collect();
    let dc: f64 = h.iter().sum();
    (0..x.len() as isize)
        .map(|n| {
            let mut acc = 0.0;
            for (j, &hk) in h.iter().enumerate() {
                let i = n - (j as isize - HALF + 1);
                if i >= 0 && (i as usize) < x.len() {
                    acc += hk * x[i as usize] as f64;
                }
            }
            (acc / dc) as f32
        })
        .collect()
}

/// Band-limited resampling by `1 + ppm * 1e-6` (positive = the receiver
/// reads faster, as `support::resample_drift`), starting `frac` of a sample
/// in: a sample-rate converter or a wrong sample rate with a proper
/// (windowed-sinc, 32-tap) interpolator. The simulator's own drift uses
/// linear interpolation, which also low-passes the signal by up to 4.6 dB at
/// 0.3 fs, varying along the burst.
fn sinc_resample(x: &[f32], ppm: f64, frac: f64) -> Vec<f32> {
    const HALF: usize = 16;
    const PHASES: usize = 512;
    let beta = 7.0;
    let i0b = bessel_i0(beta);
    let ratio = 1.0 + ppm * 1e-6;
    let fc = 0.97 * ratio.recip().min(1.0);
    // Kernel table over [-HALF, HALF] at 1/PHASES steps.
    let table: Vec<f64> = (0..=2 * HALF * PHASES)
        .map(|i| {
            let t = i as f64 / PHASES as f64 - HALF as f64;
            let sinc = if t.abs() < 1e-12 {
                1.0
            } else {
                (PI * fc * t).sin() / (PI * fc * t)
            };
            let r = t / HALF as f64;
            fc * sinc * bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b
        })
        .collect();
    let kernel = |t: f64| -> f64 {
        let u = (t + HALF as f64) * PHASES as f64;
        let i = u.floor() as usize;
        if i + 1 >= table.len() {
            return 0.0;
        }
        let f = u - i as f64;
        table[i] + f * (table[i + 1] - table[i])
    };
    let mut out = Vec::with_capacity((x.len() as f64 / ratio) as usize + 1);
    let mut i = 0usize;
    loop {
        let pos = (i as f64 + frac) * ratio;
        if pos > (x.len() - 1) as f64 {
            break;
        }
        let k0 = pos.floor() as isize;
        let mut acc = 0.0;
        for k in k0 - HALF as isize + 1..=k0 + HALF as isize {
            if k >= 0 && (k as usize) < x.len() {
                acc += x[k as usize] as f64 * kernel(pos - k as f64);
            }
        }
        out.push(acc as f32);
        i += 1;
    }
    out
}

/// Second-order section (b0, b1, b2, a1, a2), f64, direct form I.
#[derive(Debug, Clone, Copy)]
struct Sos([f64; 5]);

impl Sos {
    /// Low-pass section from an analog pole pair (natural frequency `w0`
    /// rad/s, quality `q`), bilinear transform at `sr` (prewarping is done
    /// by the caller on `w0`).
    fn from_analog(w0: f64, q: f64, sr: f64) -> Self {
        let k = 2.0 * sr;
        let a0 = k * k + w0 / q * k + w0 * w0;
        let g = w0 * w0 / a0;
        Sos([
            g,
            2.0 * g,
            g,
            (2.0 * w0 * w0 - 2.0 * k * k) / a0,
            (k * k - w0 / q * k + w0 * w0) / a0,
        ])
    }

    fn run(&self, x: &mut [f32]) {
        let [b0, b1, b2, a1, a2] = self.0;
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for s in x.iter_mut() {
            let x0 = *s as f64;
            let y = b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
            x2 = x1;
            x1 = x0;
            y2 = y1;
            y1 = y;
            *s = y as f32;
        }
    }
}

/// Butterworth low-pass of even `order` (-3 dB at `fc`).
fn butterworth(order: usize, fc: f64, sr: f64) -> Vec<Sos> {
    let wc = 2.0 * sr * (PI * fc / sr).tan();
    (1..=order / 2)
        .map(|k| {
            let q = 1.0 / (2.0 * ((2 * k - 1) as f64 * PI / (2 * order) as f64).sin());
            Sos::from_analog(wc, q, sr)
        })
        .collect()
}

/// Chebyshev type I low-pass of even `order`, `ripple_db` passband ripple up
/// to `fp` (minimum phase, the steep IIR a "low-latency" converter filter
/// resembles). Unity gain at DC.
fn chebyshev1(order: usize, ripple_db: f64, fp: f64, sr: f64) -> Vec<Sos> {
    let wp = 2.0 * sr * (PI * fp / sr).tan();
    let eps = (10f64.powf(ripple_db / 10.0) - 1.0).sqrt();
    let v = (1.0 / eps).asinh() / order as f64;
    let mut secs: Vec<Sos> = (1..=order / 2)
        .map(|k| {
            let th = (2 * k - 1) as f64 * PI / (2 * order) as f64;
            let re = -v.sinh() * th.sin();
            let im = v.cosh() * th.cos();
            let w0 = (re * re + im * im).sqrt() * wp;
            let q = (re * re + im * im).sqrt() / (2.0 * -re);
            Sos::from_analog(w0, q, sr)
        })
        .collect();
    // Even order: DC gain of the prototype is 1/sqrt(1+eps^2); sections are
    // unity at DC, so scale to the passband peak instead (0 dB max).
    let g = 1.0 / (1.0 + eps * eps).sqrt();
    secs[0].0[0] *= g;
    secs[0].0[1] *= g;
    secs[0].0[2] *= g;
    secs
}

/// 10th-order elliptic low-pass, 0.01 dB ripple, 100 dB stop band, edge at
/// 0.452 fs (21.7 kHz at 48 kHz, 43.4 kHz at 96 kHz): a sharp minimum-phase
/// converter filter ("short delay" / "low latency" DAC and ADC modes).
/// scipy: `ellip(10, 0.01, 100, 0.452 * fs, fs=fs, output='sos')`.
#[rustfmt::skip]
const ELLIP_0452: [[f64; 5]; 5] = [
    [3.919911340326e-01, 7.834001970572e-01, 3.919911340326e-01, 1.096641473477e+00, 3.258742271990e-01],
    [1.0, 1.988270679049e+00, 1.0, 1.479127174755e+00, 6.341149484341e-01],
    [1.0, 1.974228080338e+00, 1.0, 1.720081830307e+00, 8.292188080405e-01],
    [1.0, 1.962739466274e+00, 1.0, 1.835495032330e+00, 9.245916145680e-01],
    [1.0, 1.956685784954e+00, 1.0, 1.896166755680e+00, 9.783660422326e-01],
];

/// The same design with its edge at 21.7 kHz on a 96 kHz path (a 48 kHz
/// device in a 96 kHz chain).
#[rustfmt::skip]
const ELLIP_96K_21K7: [[f64; 5]; 5] = [
    [3.043693260864e-03, 5.827214220973e-03, 3.043693260864e-03, -8.427159404872e-01, 2.120005997158e-01],
    [1.0, 1.410264944223e+00, 1.0, -6.727365597436e-01, 3.708546811880e-01],
    [1.0, 8.958085009859e-01, 1.0, -4.557537059013e-01, 5.837913263101e-01],
    [1.0, 5.758521846822e-01, 1.0, -2.912378767698e-01, 7.722444062279e-01],
    [1.0, 4.335429744001e-01, 1.0, -2.121363453096e-01, 9.265936548831e-01],
];

/// Path filter models.
#[derive(Debug, Clone, Copy)]
enum Filter {
    /// `n` cascaded sharp minimum-phase converter filters ([`ELLIP_0452`];
    /// on a 96 kHz path with `hop48`, [`ELLIP_96K_21K7`]).
    AaEllip { n: usize, hop48: bool },
    /// `n` cascaded linear-phase converter filters (DA, AD, SRC): flat to
    /// `pass` Hz, >= 100 dB down from `stop` Hz.
    AaLin { pass: f64, stop: f64, n: usize },
    /// Minimum-phase converter filter: 8th-order Chebyshev I, 0.1 dB ripple
    /// to `pass` Hz.
    AaMin { pass: f64 },
    /// Butterworth low-pass of `order` at `fc` Hz.
    Bw { order: usize, fc: f64 },
}

impl Filter {
    fn label(&self) -> String {
        match *self {
            Filter::AaLin { pass, stop, n } => format!(
                "AD/DA linear-phase x{n} (flat {:.1} k, stop {:.1} k)",
                pass / 1000.0,
                stop / 1000.0
            ),
            Filter::AaMin { pass } => {
                format!("AD/DA min-phase (Cheby 8, flat {:.1} k)", pass / 1000.0)
            }
            Filter::AaEllip { n, hop48 } => format!(
                "AD/DA min-phase x{n} (ellip 10, flat {})",
                if hop48 { "21.7 k" } else { "0.452 fs" }
            ),
            Filter::Bw { order, fc } => format!("{order}-pole LP {:.1} kHz", fc / 1000.0),
        }
    }

    fn apply(&self, x: &[f32], sr: f64) -> Vec<f32> {
        match *self {
            Filter::AaLin { pass, stop, n } => {
                let h = kaiser_lowpass(pass, stop, 100.0, sr);
                let mut y = x.to_vec();
                for _ in 0..n {
                    y = fir_zero_delay(&h, &y);
                }
                y
            }
            Filter::AaMin { pass } => {
                let mut y = x.to_vec();
                for s in chebyshev1(8, 0.1, pass, sr) {
                    s.run(&mut y);
                }
                y
            }
            Filter::AaEllip { n, hop48 } => {
                let coeffs = if hop48 {
                    assert_eq!(sr, 96_000.0, "48 kHz hop model is for 96 kHz paths");
                    ELLIP_96K_21K7
                } else {
                    ELLIP_0452
                };
                let mut y = x.to_vec();
                for _ in 0..n {
                    for c in coeffs {
                        Sos(c).run(&mut y);
                    }
                }
                y
            }
            Filter::Bw { order, fc } => {
                let mut y = x.to_vec();
                for s in butterworth(order, fc, sr) {
                    s.run(&mut y);
                }
                y
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Channel conditions

#[derive(Debug, Clone, Copy)]
enum Imp {
    Clean,
    Snr(f32),
    /// Path filter, then AWGN at the SNR.
    Filt(Filter, f32),
    /// Path filter + band-limited SRC (ppm) + SNR: a Dante-like path with an
    /// analog leg and SRC.
    FiltDrift(Filter, f64, f32),
    /// Rate error (ppm) through a band-limited SRC ([`sinc_resample`]) at an
    /// SNR.
    Src(f64, f32),
    /// Drift ppm at an SNR, with the simulator's linear-interpolating
    /// resampler (as the existing LTC tables).
    Drift(f64, f32),
    /// Polarity inverted at an SNR.
    Inverted(f32),
    /// Gain dB at an SNR.
    Level(f32, f32),
    /// Dropout ms at an SNR (inside the message).
    Dropout(f32, f32),
    /// Clicks per second, peak dB re signal peak, at an SNR.
    Clicks(f32, f32, f32),
}

impl Imp {
    fn label(&self) -> String {
        match *self {
            Imp::Clean => "clean".into(),
            Imp::Snr(s) => format!("SNR {s} dB"),
            Imp::Filt(f, s) => format!("{} + SNR {s}", f.label()),
            Imp::FiltDrift(f, p, s) => {
                format!("{} + SRC {:+}% + SNR {s}", f.label(), p / 10_000.0)
            }
            Imp::Src(p, s) => format!("SRC {:+}% + SNR {s}", p / 10_000.0),
            Imp::Drift(p, s) => format!("drift {:+}% (linear interp.) + SNR {s}", p / 10_000.0),
            Imp::Inverted(s) => format!("inverted + SNR {s}"),
            Imp::Level(g, s) => format!("level {g} dB + SNR {s}"),
            Imp::Dropout(ms, s) => format!("dropout {ms} ms + SNR {s}"),
            Imp::Clicks(r, l, s) => format!("clicks {r}/s @ {l:+} dB + SNR {s}"),
        }
    }

    /// The transmitted audio through the path filter. Paths with an analog
    /// leg or SRC (filters, drift) first shift it by `frac` of a sample (a
    /// random phase); a purely digital path (the other conditions) is
    /// sample-synchronous and keeps the transmitter's phase.
    fn path(&self, x: &[f32], sr: f64, frac: f64) -> Vec<f32> {
        match *self {
            Imp::Filt(f, _) => f.apply(&fractional_delay(x, frac), sr),
            Imp::FiltDrift(f, ppm, _) => sinc_resample(&f.apply(x, sr), ppm, frac),
            Imp::Src(ppm, _) => sinc_resample(x, ppm, frac),
            Imp::Drift(..) => fractional_delay(x, frac),
            _ => x.to_vec(),
        }
    }

    fn sim(&self, config: &PhyConfig, seed: u64, offset: usize) -> ChannelSim {
        let base = ChannelSim::new(config.clone(), seed).offset(offset);
        let ms = |ms: f32| (ms * config.sample_rate_hz as f32 / 1000.0) as usize;
        match *self {
            Imp::Clean => base,
            Imp::Snr(s) | Imp::Filt(_, s) => base.snr_db(s),
            Imp::FiltDrift(_, _, s) | Imp::Src(_, s) => base.snr_db(s),
            Imp::Drift(p, s) => base.drift_ppm(p).snr_db(s),
            Imp::Inverted(s) => base.invert().snr_db(s),
            Imp::Level(g, s) => base.gain(10f32.powf(g / 20.0)).snr_db(s),
            Imp::Dropout(d, s) => base.dropout_samples(ms(d)).snr_db(s),
            Imp::Clicks(r, l, s) => base.clicks(r, l).snr_db(s),
        }
    }
}

fn snr_set() -> Vec<Imp> {
    [30.0, 25.0, 20.0, 18.0, 16.0, 14.0, 12.0, 10.0, 8.0, 6.0]
        .into_iter()
        .map(Imp::Snr)
        .collect()
}

/// Sharp linear-phase converter / SRC filters: flat to 20 kHz, >= 100 dB
/// down from 23 kHz (on a 96 kHz path: a 48 kHz hop).
const fn aa_lin(n: usize) -> Filter {
    Filter::AaLin {
        pass: 20_000.0,
        stop: 23_000.0,
        n,
    }
}

const fn bw(order: usize, fc: f64) -> Filter {
    Filter::Bw { order, fc }
}

/// Converter / codec band limits of a 48 kHz path.
fn filter_set(snr: f32) -> Vec<Imp> {
    [
        aa_lin(2),
        aa_lin(4),
        Filter::AaEllip { n: 1, hop48: false },
        Filter::AaEllip { n: 2, hop48: false },
        Filter::AaMin { pass: 20_000.0 },
        bw(4, 18_000.0),
        bw(4, 16_000.0),
        bw(2, 16_000.0),
        bw(2, 12_000.0),
    ]
    .into_iter()
    .map(|f| Imp::Filt(f, snr))
    .collect()
}

/// Band limits of a 96 kHz path: native converters, then a 48 kHz device
/// or SRC in the chain, then soft low-passes.
fn filter96_set(snr: f32) -> Vec<Imp> {
    [
        Filter::AaLin {
            pass: 40_000.0,
            stop: 46_000.0,
            n: 2,
        },
        Filter::AaEllip { n: 2, hop48: false },
        aa_lin(2),
        Filter::AaEllip { n: 1, hop48: true },
        Filter::AaEllip { n: 2, hop48: true },
        bw(4, 32_000.0),
        bw(4, 24_000.0),
        bw(4, 18_000.0),
        bw(4, 16_000.0),
    ]
    .into_iter()
    .map(|f| Imp::Filt(f, snr))
    .collect()
}

fn impair_set() -> Vec<Imp> {
    vec![
        Imp::Clean,
        Imp::Src(1_000.0, 20.0),
        Imp::Src(-1_000.0, 20.0),
        Imp::Src(10_000.0, 20.0),
        Imp::Src(-10_000.0, 20.0),
        Imp::Drift(10_000.0, 20.0),
        Imp::Drift(-10_000.0, 20.0),
        Imp::FiltDrift(aa_lin(2), 1_000.0, 20.0),
        Imp::FiltDrift(aa_lin(2), -10_000.0, 20.0),
        Imp::Inverted(20.0),
        Imp::Level(-30.0, 20.0),
        Imp::Level(-50.0, 30.0),
        Imp::Dropout(1.0, 30.0),
        Imp::Dropout(3.0, 30.0),
        Imp::Clicks(20.0, 0.0, 30.0),
        Imp::Clicks(200.0, 6.0, 30.0),
    ]
}

/// Hard cases for receiver tuning.
fn tune_set() -> Vec<Imp> {
    vec![
        Imp::Snr(10.0),
        Imp::Filt(Filter::AaEllip { n: 2, hop48: false }, 20.0),
        Imp::Filt(Filter::AaMin { pass: 20_000.0 }, 20.0),
        Imp::Filt(bw(4, 16_000.0), 20.0),
        Imp::Filt(bw(2, 12_000.0), 20.0),
        Imp::Drift(10_000.0, 14.0),
        Imp::Drift(-10_000.0, 14.0),
    ]
}

fn quick_set() -> Vec<Imp> {
    let mut v = vec![Imp::Clean, Imp::Snr(20.0), Imp::Snr(14.0), Imp::Snr(10.0)];
    v.extend(filter_set(20.0));
    v.extend([
        Imp::Drift(10_000.0, 20.0),
        Imp::Drift(-10_000.0, 20.0),
        Imp::Dropout(1.0, 30.0),
    ]);
    v
}

// ---------------------------------------------------------------------------
// Trials

#[derive(Debug, Clone, Copy, Default)]
struct Cell {
    ok: usize,
    trials: usize,
    latency_sum_ms: f64,
    false_msgs: usize,
}

impl Cell {
    fn add(&mut self, (lat, false_msgs): (Option<f64>, usize)) {
        if let Some(l) = lat {
            self.ok += 1;
            self.latency_sum_ms += l;
        }
        self.false_msgs += false_msgs;
    }

    fn pct(&self) -> f64 {
        100.0 * self.ok as f64 / self.trials.max(1) as f64
    }

    fn mean_latency(&self) -> f64 {
        self.latency_sum_ms / self.ok.max(1) as f64
    }
}

fn message(rng: &mut XorShift64, t: usize) -> TransportMessage {
    if t % 4 == 3 {
        let list = rng.below(16) as u8;
        let cue = rng.below(128) as u16;
        TransportMessage::Msc(MscMessage::cue(
            MscCommand::Go,
            MscCue::from_tenths(cue * 10 + u16::from(list % 10)).unwrap(),
            (list > 0).then_some(list),
        ))
    } else {
        TransportMessage::NoteOn {
            channel: 0,
            note: rng.below(128) as u8,
            velocity: 1 + rng.below(127) as u8,
        }
    }
}

/// `trials` single messages through `imp` (random message, start offset and
/// sequence number; 64-sample chunks).
fn run_cell(config: &PhyConfig, imp: Imp, trials: usize, seed: u64) -> Cell {
    let mut rng = XorShift64::new(seed);
    let sr = config.sample_rate_hz as f64;
    let mut cell = Cell {
        trials,
        ..Cell::default()
    };
    for t in 0..trials {
        let msg = message(&mut rng, t);
        let offset = (200 + rng.below(400)) * config.sample_rate_hz as usize / 48_000;
        let tx = CricketModemTx::new(config.clone());
        let mut clean = tx.encode_message_with_seq(&msg, rng.below(4) as u8);
        let msg_len = clean.len();
        clean.extend(std::iter::repeat_n(0.0, config.sample_rate_hz as usize / 8));
        clean = imp.path(&clean, sr, rng.next_f64());
        let mut sim = imp.sim(config, rng.next_u64(), offset);
        let audio = if let Imp::Dropout(..) = imp {
            // Dropout inside the message, not in the trailing silence.
            let mut audio = sim.apply(&clean[..msg_len]);
            sim.dropout_samples = 0;
            audio.extend(sim.clone().offset(0).apply(&clean[msg_len..]));
            audio
        } else {
            sim.apply(&clean)
        };
        cell.add(decode(config, &audio, &msg, offset, sr));
    }
    cell
}

fn decode(
    config: &PhyConfig,
    audio: &[f32],
    msg: &TransportMessage,
    offset: usize,
    sr: f64,
) -> (Option<f64>, usize) {
    let mut rx = CricketModemRx::new(config.clone());
    let mut delivered_at = None;
    let mut false_msgs = 0;
    let chunk = 64 * config.sample_rate_hz as usize / 48_000;
    for c in audio.chunks(chunk) {
        for out in rx.push_samples(c) {
            if let ModemOutput::Message(m) = out {
                if m == *msg && delivered_at.is_none() {
                    delivered_at = Some(rx.samples_processed());
                } else if m != *msg {
                    false_msgs += 1;
                }
            }
        }
    }
    (
        delivered_at.map(|at| (at as f64 - offset as f64) / sr * 1000.0),
        false_msgs,
    )
}

fn env_list(name: &str, default: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn table(profiles: &[(String, PhyConfig)], imps: &[Imp], trials: usize) -> usize {
    let jobs: Vec<(usize, usize)> = (0..profiles.len())
        .flat_map(|p| (0..imps.len()).map(move |i| (p, i)))
        .collect();
    let next = AtomicUsize::new(0);
    let results = Mutex::new(vec![vec![Cell::default(); imps.len()]; profiles.len()]);
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().map_or(4, |n| n.get()) {
            s.spawn(|| loop {
                let j = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(p, i)) = jobs.get(j) else { break };
                // Same seed for every profile: same messages, offsets, noise.
                let cell = run_cell(&profiles[p].1, imps[i], trials, 0xFA57 + i as u64);
                results.lock().unwrap()[p][i] = cell;
            });
        }
    });
    let results = results.into_inner().unwrap();
    print!("| channel |");
    for (name, _) in profiles {
        print!(" {name} |");
    }
    println!();
    println!("|---|{}", "---|".repeat(profiles.len()));
    let mut false_total = 0;
    for (i, imp) in imps.iter().enumerate() {
        print!("| {} |", imp.label());
        for r in &results {
            let c = &r[i];
            false_total += c.false_msgs;
            print!(" {:.1} ({:.1}) |", c.pct(), c.mean_latency());
        }
        println!();
    }
    false_total
}

fn airtime(profiles: &[(String, PhyConfig)]) {
    println!(
        "\n| profile | sample rate | half cell (samples) | body nibbles | burst ms | airtime ms |"
    );
    println!("|---|---|---|---|---|---|");
    for (name, c) in profiles {
        let Modulation::Bmc(p) = &c.modulation else {
            continue;
        };
        let rep = repetition_samples(c) as f64 * 1000.0 / c.sample_rate_hz as f64;
        println!(
            "| {name} | {} | {:.3} | {} | {:.2} | {rep:.2} |",
            c.sample_rate_hz,
            p.half_cell_samples(c.sample_rate_hz),
            c.fec.body_nibbles(),
            c.estimated_first_copy_ms()
        );
    }
}

#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn ltc_fast_ab_table() {
    let trials = env_usize("FAST_AB_TRIALS", 400);
    let keys = env_list(
        "FAST_AB_PROFILES",
        "ltc9600,ltc12000,ltc14400,ltc16000,ltc9600_fec,ltc14400_fec",
    );
    let set = std::env::var("FAST_AB_SET").unwrap_or_else(|_| "all".into());
    let snr: f32 = std::env::var("FAST_AB_FILTER_SNR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    let mut imps = Vec::new();
    match set.as_str() {
        "quick" => imps.extend(quick_set()),
        "tune" => imps.extend(tune_set()),
        "snr" => imps.extend(snr_set()),
        "filter" => imps.extend(filter_set(snr)),
        "filter96" => imps.extend(filter96_set(snr)),
        "all96" => {
            imps.extend(snr_set());
            imps.extend(filter96_set(snr));
            imps.extend(impair_set());
        }
        "impair" => imps.extend(impair_set()),
        _ => {
            imps.extend(snr_set());
            imps.extend(filter_set(snr));
            imps.extend(impair_set());
        }
    }
    let profiles: Vec<(String, PhyConfig)> = keys.iter().map(|k| (k.clone(), profile(k))).collect();
    airtime(&profiles);
    println!("\n### Delivery % (mean start-to-delivery ms), {trials} trials per cell\n");
    let false_total = table(&profiles, &imps, trials);
    println!("\nfalse messages (all cells): {false_total}");
    assert_eq!(false_total, 0);
}

// ---------------------------------------------------------------------------
// Phy-level diagnostics: sync rate, gate drops and raw bit errors

#[derive(Debug, Clone, Copy, Default)]
struct PhyCell {
    trials: usize,
    /// A body of the expected length was delivered (passed the gates).
    bodies: usize,
    /// ... with every nibble right.
    exact: usize,
    bit_errors: usize,
    margin_sum: f64,
    nibbles: usize,
    /// Trials with at least one sync lock.
    locked: usize,
    /// Bodies completed but dropped by the false-lock gates.
    gated: usize,
}

fn run_phy_cell(config: &PhyConfig, imp: Imp, trials: usize, seed: u64) -> PhyCell {
    use cricket_core::phy::RxEvent;
    let mut rng = XorShift64::new(seed);
    let sr = config.sample_rate_hz as f64;
    let n_body = config.fec.body_nibbles();
    let tx = config.modulation.phy().tx(config);
    let mut cell = PhyCell {
        trials,
        ..PhyCell::default()
    };
    for _ in 0..trials {
        let body: Vec<u8> = (0..n_body).map(|_| rng.below(16) as u8).collect();
        let offset = (200 + rng.below(400)) * config.sample_rate_hz as usize / 48_000;
        let mut clean = tx.synthesize_framed(&body);
        clean.extend(std::iter::repeat_n(
            0.0,
            config.sample_rate_hz as usize / 50,
        ));
        clean = imp.path(&clean, sr, rng.next_f64());
        let audio = imp.sim(config, rng.next_u64(), offset).apply(&clean);
        let mut rx = config.modulation.phy().rx(config);
        rx.set_expected_body_nibbles(n_body);
        let mut got: Vec<(u8, f32)> = Vec::new();
        let mut best: Option<Vec<(u8, f32)>> = None;
        let mut locked = false;
        for c in audio.chunks(64) {
            for ev in rx.push_samples(c) {
                match ev {
                    RxEvent::PreambleLocked { .. } => {
                        locked = true;
                        got.clear();
                    }
                    RxEvent::SyncLost => {
                        cell.gated += (got.len() + 1 == n_body) as usize;
                        got.clear();
                    }
                    RxEvent::BodyNibble { result, .. } => {
                        got.push((result.nibble, result.purity));
                        if got.len() == n_body {
                            best = Some(std::mem::take(&mut got));
                        }
                    }
                    _ => {}
                }
            }
        }
        cell.locked += locked as usize;
        if let Some(b) = best {
            cell.bodies += 1;
            let errs: usize = b
                .iter()
                .zip(&body)
                .map(|((g, _), &w)| (g ^ w).count_ones() as usize)
                .sum();
            cell.exact += (errs == 0) as usize;
            cell.bit_errors += errs;
            cell.margin_sum += b.iter().map(|(_, m)| *m as f64).sum::<f64>();
            cell.nibbles += b.len();
        }
    }
    cell
}

/// Per condition: bodies delivered %, exact bodies %, bit error rate in
/// delivered bodies, mean nibble margin.
#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn ltc_fast_phy_diag() {
    let trials = env_usize("FAST_AB_TRIALS", 200);
    let keys = env_list("FAST_AB_PROFILES", "ltc9600,ltc14400");
    let set = std::env::var("FAST_AB_SET").unwrap_or_else(|_| "quick".into());
    let imps = match set.as_str() {
        "snr" => snr_set(),
        "filter" => filter_set(20.0),
        "impair" => impair_set(),
        "tune" => tune_set(),
        "filter96" => filter96_set(20.0),
        _ => quick_set(),
    };
    let profiles: Vec<(String, PhyConfig)> = keys.iter().map(|k| (k.clone(), profile(k))).collect();
    let jobs: Vec<(usize, usize)> = (0..profiles.len())
        .flat_map(|p| (0..imps.len()).map(move |i| (p, i)))
        .collect();
    let next = AtomicUsize::new(0);
    let results = Mutex::new(vec![vec![PhyCell::default(); imps.len()]; profiles.len()]);
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().map_or(4, |n| n.get()) {
            s.spawn(|| loop {
                let j = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(p, i)) = jobs.get(j) else { break };
                let cell = run_phy_cell(&profiles[p].1, imps[i], trials, 0xD1A6 + i as u64);
                results.lock().unwrap()[p][i] = cell;
            });
        }
    });
    let results = results.into_inner().unwrap();
    println!(
        "\nlocked % / gated % / bodies % / exact % / BER x1e3 / mean margin, {trials} trials per cell\n"
    );
    print!("| channel |");
    for (name, _) in &profiles {
        print!(" {name} |");
    }
    println!();
    println!("|---|{}", "---|".repeat(profiles.len()));
    for (i, imp) in imps.iter().enumerate() {
        print!("| {} |", imp.label());
        for r in &results {
            let c = &r[i];
            let bits = (c.nibbles * 4).max(1) as f64;
            print!(
                " {:.0} / {:.0} / {:.0} / {:.0} / {:.1} / {:.2} |",
                100.0 * c.locked as f64 / c.trials as f64,
                100.0 * c.gated as f64 / c.trials as f64,
                100.0 * c.bodies as f64 / c.trials as f64,
                100.0 * c.exact as f64 / c.trials as f64,
                1e3 * c.bit_errors as f64 / bits,
                c.margin_sum / c.nibbles.max(1) as f64
            );
        }
        println!();
    }
}

// ---------------------------------------------------------------------------
// Chords: back-to-back messages

/// `trials` 4-note chords (4 NoteOns back to back, as the engine sends them)
/// through `imp`: (% with all four delivered, mean ms from the chord's first
/// sample to the last note's delivery, false messages).
fn run_chord_cell(config: &PhyConfig, imp: Imp, trials: usize, seed: u64) -> (f64, f64, usize) {
    let mut rng = XorShift64::new(seed);
    let sr = config.sample_rate_hz as f64;
    let (mut ok, mut lat_sum, mut false_msgs) = (0usize, 0.0, 0usize);
    for _ in 0..trials {
        let root = 36 + rng.below(60) as u8;
        let notes: Vec<TransportMessage> = [0u8, 4, 7, 12]
            .iter()
            .map(|&d| TransportMessage::NoteOn {
                channel: 0,
                note: root + d,
                velocity: 1 + rng.below(127) as u8,
            })
            .collect();
        let offset = (200 + rng.below(400)) * config.sample_rate_hz as usize / 48_000;
        let mut tx = CricketModemTx::new(config.clone());
        let mut clean: Vec<f32> = notes.iter().flat_map(|m| tx.encode_message(m)).collect();
        clean.extend(std::iter::repeat_n(0.0, config.sample_rate_hz as usize / 8));
        clean = imp.path(&clean, sr, rng.next_f64());
        let audio = imp.sim(config, rng.next_u64(), offset).apply(&clean);
        let mut rx = CricketModemRx::new(config.clone());
        let mut got = Vec::new();
        let mut last_at = 0;
        let chunk = 64 * config.sample_rate_hz as usize / 48_000;
        for c in audio.chunks(chunk) {
            for out in rx.push_samples(c) {
                if let ModemOutput::Message(m) = out {
                    if notes.contains(&m) && !got.contains(&m) {
                        got.push(m);
                        last_at = rx.samples_processed();
                    } else if !notes.contains(&m) {
                        false_msgs += 1;
                    }
                }
            }
        }
        if got.len() == notes.len() {
            ok += 1;
            lat_sum += (last_at as f64 - offset as f64) / sr * 1000.0;
        }
    }
    (
        100.0 * ok as f64 / trials as f64,
        lat_sum / ok.max(1) as f64,
        false_msgs,
    )
}

#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn ltc_fast_chord_table() {
    let trials = env_usize("FAST_AB_TRIALS", 400);
    let keys = env_list(
        "FAST_AB_PROFILES",
        "ltc9600,ltc9600_fec,ltc14400,ltc14400_fec",
    );
    let imps = [
        Imp::Clean,
        Imp::Snr(20.0),
        Imp::Filt(aa_lin(2), 20.0),
        Imp::Filt(Filter::AaEllip { n: 2, hop48: false }, 20.0),
        Imp::FiltDrift(aa_lin(2), 1_000.0, 20.0),
        Imp::Src(10_000.0, 20.0),
        Imp::Src(-10_000.0, 20.0),
        Imp::Drift(10_000.0, 20.0),
        Imp::Drift(-10_000.0, 20.0),
    ];
    let profiles: Vec<(String, PhyConfig)> = keys.iter().map(|k| (k.clone(), profile(k))).collect();
    println!("\n### 4-note chords: all delivered % (last note ms), {trials} trials per cell\n");
    print!("| channel |");
    for (name, _) in &profiles {
        print!(" {name} |");
    }
    println!();
    println!("|---|{}", "---|".repeat(profiles.len()));
    let mut false_total = 0;
    for (i, imp) in imps.iter().enumerate() {
        let cells: Vec<(f64, f64, usize)> = std::thread::scope(|s| {
            let handles: Vec<_> = profiles
                .iter()
                .map(|(_, c)| s.spawn(move || run_chord_cell(c, *imp, trials, 0xC40D + i as u64)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        print!("| {} |", imp.label());
        for (pct, lat, f) in cells {
            false_total += f;
            print!(" {pct:.1} ({lat:.1}) |");
        }
        println!();
    }
    println!("\nfalse messages (all cells): {false_total}");
    assert_eq!(false_total, 0);
}

// ---------------------------------------------------------------------------
// Receiver CPU

/// Receiver CPU on the host (release, one thread), ns per input sample:
/// best of 5 passes over 10 s of audio, either 20 NoteOns at 30 dB SNR (as
/// `phy_ab_measurements.rs`) or white noise (search only).
#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn ltc_fast_cpu() {
    let keys = env_list(
        "FAST_AB_PROFILES",
        "ltc_fec,ltc9600,ltc9600_fec,ltc14400,ltc14400_fec,ltc14400@96,ltc14400_fec@96,ltc19200@96/fast=1",
    );
    let time = |config: &PhyConfig, audio: &[f32]| -> f64 {
        (0..5)
            .map(|_| {
                let mut rx = CricketModemRx::new(config.clone());
                let t = std::time::Instant::now();
                let mut n = 0usize;
                for chunk in audio.chunks(256) {
                    n += rx.push_samples(chunk).len();
                }
                std::hint::black_box(n);
                t.elapsed().as_nanos() as f64 / audio.len() as f64
            })
            .fold(f64::INFINITY, f64::min)
    };
    println!("\n| profile | ns/sample, traffic | ns/sample, noise | x realtime (traffic) |");
    println!("|---|---|---|---|");
    for k in keys {
        let config = profile(&k);
        let sr = config.sample_rate_hz as usize;
        let msgs: Vec<_> = (0..20u8)
            .map(|i| TransportMessage::NoteOn {
                channel: 0,
                note: 40 + i,
                velocity: 1 + 6 * i,
            })
            .collect();
        let mut traffic = encode_to_samples(config.clone(), &msgs);
        traffic.resize(sr * 10, 0.0);
        let traffic = ChannelSim::new(config.clone(), 3)
            .snr_db(30.0)
            .apply(&traffic);
        let mut rng = XorShift64::new(0xC0DE);
        let noise: Vec<f32> = (0..sr * 10)
            .map(|_| (rng.gaussian() * 0.3) as f32)
            .collect();
        let (t, n) = (time(&config, &traffic), time(&config, &noise));
        println!(
            "| {k} | {t:.1} | {n:.1} | {:.0} |",
            1e9 / (t * config.sample_rate_hz as f64)
        );
    }
}
