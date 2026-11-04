//! Pedal commands (experimental, `cricket_core::pedal`): delivery, latency,
//! false triggers and audible leakage with guitar, bass and keys on the same
//! signal. See `docs/proposals/pedal-commands.md`.
//!
//! The regression checks run with `cargo test`. The full report:
//!
//! ```sh
//! cargo test --release -p cricket_core --test pedal -- --ignored --nocapture
//! ```
//!
//! Environment: `PEDAL_SECONDS` (audio per delivery row and seed, default
//! 30), `PEDAL_SEEDS` (default 2), `PEDAL_FALSE_MINUTES` (audio per
//! false-trigger row, default 10), `PEDAL_MIN_PURITY` (overrides the purity
//! gate of every variant, for tuning).

mod support;

use cricket_core::modem::CricketModemTx;
use cricket_core::pedal::{
    Butterworth, PedalCommand, PedalConfig, PedalRx, PedalTx, PresetMap, PresetState,
};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::TransportMessage;
use std::f64::consts::PI;
use std::time::Instant;
use support::{Biquad, XorShift64};

const RATE: usize = 48_000;
/// Level of every synthetic music signal.
const MUSIC_RMS_DBFS: f32 = -18.0;

fn db(x: f32) -> f32 {
    10f32.powf(x / 20.0)
}

fn rms(x: &[f32]) -> f32 {
    let e: f64 = x.iter().map(|&v| v as f64 * v as f64).sum();
    (e / x.len().max(1) as f64).sqrt() as f32
}

fn normalize(x: &mut [f32], rms_dbfs: f32) {
    let r = rms(x);
    if r > 0.0 {
        let g = db(rms_dbfs) / r;
        x.iter_mut().for_each(|v| *v *= g);
    }
}

fn midi_hz(note: u8) -> f32 {
    440.0 * 2f32.powf((note as f32 - 69.0) / 12.0)
}

// ---------------------------------------------------------------------------
// Synthetic music (48 kHz mono)

/// Karplus–Strong string: a white-noise burst (a bright pick, energy up to
/// 24 kHz) that the averaging loop darkens over time.
fn pluck(out: &mut [f32], start: usize, freq: f32, amp: f32, decay: f32, rng: &mut XorShift64) {
    if start >= out.len() {
        return;
    }
    let period = ((RATE as f32 / freq).round() as usize).max(2);
    let mut buf: Vec<f32> = (0..period)
        .map(|_| rng.next_f64() as f32 * 2.0 - 1.0)
        .collect();
    let mut i = 0;
    for o in out[start..].iter_mut().take(2 * RATE) {
        let a = buf[i];
        let b = buf[(i + 1) % period];
        *o += amp * a;
        buf[i] = decay * 0.5 * (a + b);
        i = (i + 1) % period;
    }
}

/// Open chords from low E (82 Hz) up.
const CHORDS: [&[u8]; 6] = [
    &[40, 47, 52, 56, 59, 64],
    &[45, 52, 57, 61, 64],
    &[50, 57, 62, 66],
    &[43, 47, 50, 55, 59, 67],
    &[48, 52, 55, 60, 64],
    &[40, 47, 52, 55, 59, 64],
];

/// Strummed chords and lead lines, unfiltered (as bright as a pick on an
/// active pickup).
fn guitar(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let mut out = vec![0.0f32; n];
    let mut t = 0;
    while t < n {
        if rng.below(4) == 0 {
            for _ in 0..2 + rng.below(5) {
                let note = 60 + rng.below(29) as u8;
                pluck(&mut out, t, midi_hz(note), 0.8, 0.997, &mut rng);
                t += RATE / 8 + rng.below(RATE / 8);
            }
        } else {
            let chord = CHORDS[rng.below(CHORDS.len())];
            let up = rng.below(2) == 1;
            for k in 0..chord.len() {
                let note = if up {
                    chord[chord.len() - 1 - k]
                } else {
                    chord[k]
                };
                pluck(
                    &mut out,
                    t + k * RATE / 100,
                    midi_hz(note),
                    0.5,
                    0.996,
                    &mut rng,
                );
            }
            t += RATE / 4 + rng.below(RATE / 2);
        }
    }
    out
}

/// Electric bass from E1 (41 Hz), darkened like a bass pickup.
fn bass(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let mut out = vec![0.0f32; n];
    let mut t = 0;
    while t < n {
        let note = 28 + rng.below(25) as u8;
        pluck(&mut out, t, midi_hz(note), 1.0, 0.995, &mut rng);
        t += RATE / 4 + rng.below(RATE / 4);
    }
    Biquad::lowpass(2_000.0, RATE as u32).run(&mut out);
    out
}

/// Polysynth chords of naive (aliasing) sawtooths with vibrato: harmonics
/// and their aliases cover the whole band up to 24 kHz and sweep with the
/// vibrato. A worst case for keys (an analog sawtooth with an open filter
/// has similar energy above 20 kHz); [`Music::Band`] darkens it.
fn keys(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let mut out = vec![0.0f32; n];
    let mut t = 0;
    while t < n {
        let dur = RATE / 2 + rng.below(3 * RATE / 2);
        let root = 48 + rng.below(24) as u8;
        let len = dur.min(n - t);
        for note in [root, root + 4, root + 7, root + 11] {
            let f0 = midi_hz(note) as f64;
            let mut phase = rng.next_f64();
            for k in 0..len {
                let env = (k as f32 / (0.01 * RATE as f32)).min(1.0)
                    * ((dur - k) as f32 / (0.1 * RATE as f32)).min(1.0);
                let lfo = (2.0 * PI * 5.5 * (t + k) as f64 / RATE as f64).sin();
                phase += f0 * (1.0 + 0.004 * lfo) / RATE as f64;
                phase -= phase.floor();
                out[t + k] += 0.25 * env * (2.0 * phase as f32 - 1.0);
            }
        }
        t += dur;
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Music {
    None,
    /// Guitar (through a pickup-like 6 kHz low-pass), bass and keys (the
    /// synth through a 9 kHz low-pass, like an electric piano, organ or pad).
    Band,
    /// Guitar alone, unfiltered.
    BrightGuitar,
    /// Bright guitar through a digital fuzz without oversampling (tanh,
    /// drive 30): aliased harmonics everywhere, 20–24 kHz included.
    FuzzGuitar,
    /// The bright aliasing synth alone, unfiltered.
    Synth,
}

fn music(kind: Music, n: usize, seed: u64) -> Vec<f32> {
    let mut out = match kind {
        Music::None => return vec![0.0; n],
        Music::Band => {
            let mut g = guitar(n, seed);
            let lp = Biquad::lowpass(6_000.0, RATE as u32);
            lp.run(&mut g);
            lp.run(&mut g);
            normalize(&mut g, -20.0);
            let mut b = bass(n, seed ^ 1);
            normalize(&mut b, -22.0);
            let mut k = keys(n, seed ^ 2);
            Butterworth::lowpass(16, 9_000.0, RATE as u32).run(&mut k);
            normalize(&mut k, -24.0);
            g.iter()
                .zip(&b)
                .zip(&k)
                .map(|((g, b), k)| g + b + k)
                .collect()
        }
        Music::BrightGuitar => guitar(n, seed),
        Music::FuzzGuitar => {
            let mut g = guitar(n, seed);
            normalize(&mut g, -18.0);
            g.iter().map(|&v| (30.0 * v).tanh() / 30.0).collect()
        }
        Music::Synth => keys(n, seed),
    };
    normalize(&mut out, MUSIC_RMS_DBFS);
    out
}

/// Receiver clock offset of `ppm` (positive = RX reads faster), by
/// Hann-windowed sinc interpolation (64 taps). The channel simulator's linear
/// interpolation would attenuate 20 kHz by up to 12 dB.
fn resample_sinc(input: &[f32], ppm: f64) -> Vec<f32> {
    const HALF: i64 = 32;
    let ratio = 1.0 + ppm * 1e-6;
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
                    (PI * t).sin() / (PI * t)
                };
                let w = 0.5 + 0.5 * (PI * t / HALF as f64).cos();
                acc += input[j as usize] as f64 * sinc * w;
            }
            acc as f32
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Delivery

#[derive(Debug, Clone)]
struct Scenario {
    name: &'static str,
    music: Music,
    /// Control tone RMS relative to the music RMS, dB (no music: the
    /// configured amplitude).
    ctrl_db: f32,
    /// White noise over the full band, relative to the control tone RMS.
    snr_db: Option<f32>,
    /// Clicks: (per second, peak in dB relative to the control peak).
    clicks: Option<(f32, f32)>,
    /// One muted stretch of this length inside every command.
    dropout_ms: f32,
    drift_ppm: f64,
    /// Converter band limit: Butterworth low-pass (order, cutoff).
    lowpass: Option<(usize, f32)>,
    /// Digital fuzz between encoder and decoder: tanh drive on everything.
    fuzz: Option<f32>,
    /// Overall level change, dB.
    level_db: f32,
}

impl Scenario {
    fn new(name: &'static str, music: Music) -> Self {
        Self {
            name,
            music,
            ctrl_db: -30.0,
            snr_db: None,
            clicks: None,
            dropout_ms: 0.0,
            drift_ppm: 0.0,
            lowpass: None,
            fuzz: None,
            level_db: 0.0,
        }
    }
}

#[derive(Debug, Default)]
struct Outcome {
    sent: usize,
    delivered: usize,
    /// A different command recognized for a sent one.
    wrong: usize,
    /// Detections with no command sent near them (or a second detection of
    /// one command).
    false_hits: usize,
    latencies_ms: Vec<f32>,
}

impl Outcome {
    fn add(&mut self, o: Outcome) {
        self.sent += o.sent;
        self.delivered += o.delivered;
        self.wrong += o.wrong;
        self.false_hits += o.false_hits;
        self.latencies_ms.extend(o.latencies_ms);
    }

    fn delivery_pct(&self) -> f64 {
        100.0 * self.delivered as f64 / self.sent.max(1) as f64
    }

    fn mean_ms(&self) -> f32 {
        self.latencies_ms.iter().sum::<f32>() / self.latencies_ms.len().max(1) as f32
    }

    fn p95_ms(&self) -> f32 {
        let mut v = self.latencies_ms.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v.get((v.len() * 95 / 100).min(v.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    }
}

/// Music plus random commands (spaced 50–250 ms apart) through the channel
/// of `sc`, decoded and matched against what was sent.
fn run(config: &PedalConfig, sc: &Scenario, seconds: f32, seed: u64) -> Outcome {
    let n = (seconds * RATE as f32) as usize;
    let mut rng = XorShift64::new(seed ^ 0xC0FF_EE00);
    let mut x = music(sc.music, n, seed);
    let ctrl_peak = if sc.music == Music::None {
        config.amplitude
    } else {
        db(MUSIC_RMS_DBFS + sc.ctrl_db) * 2f32.sqrt()
    };
    let g = ctrl_peak / config.amplitude;
    let tx = PedalTx::new(config.clone());

    let mut sent: Vec<(usize, PedalCommand)> = Vec::new();
    let mut t = RATE / 10;
    while t + config.total_samples() < n {
        let cmd = PedalCommand::ALL[rng.below(4)];
        for (k, v) in tx.synthesize(cmd).iter().enumerate() {
            x[t + k] += g * v;
        }
        if sc.dropout_ms > 0.0 {
            let d = (sc.dropout_ms * RATE as f32 / 1000.0) as usize;
            let s = t + rng.below(config.command_samples() - d);
            x[s..s + d].fill(0.0);
        }
        sent.push((t, cmd));
        t += config.total_samples() + RATE / 20 + rng.below(RATE / 5);
    }

    if let Some(drive) = sc.fuzz {
        x.iter_mut().for_each(|v| *v = (drive * *v).tanh() / drive);
    }
    if let Some((order, cutoff)) = sc.lowpass {
        Butterworth::lowpass(order, cutoff, RATE as u32).run(&mut x);
    }
    let level = db(sc.level_db);
    if sc.level_db != 0.0 {
        x.iter_mut().for_each(|v| *v *= level);
    }
    if sc.drift_ppm != 0.0 {
        x = resample_sinc(&x, sc.drift_ppm);
    }
    if let Some((per_second, rel_db)) = sc.clicks {
        let peak = ctrl_peak * level * db(rel_db);
        let p = per_second as f64 / RATE as f64;
        let mut i = 0;
        while i < x.len() {
            if rng.next_f64() < p {
                let sign = if rng.below(2) == 0 { 1.0 } else { -1.0 };
                let end = (i + 1 + rng.below(4)).min(x.len());
                x[i..end].iter_mut().for_each(|v| *v += sign * peak);
                i = end;
            }
            i += 1;
        }
    }
    if let Some(snr) = sc.snr_db {
        let sigma = (ctrl_peak * level / 2f32.sqrt() / db(snr)) as f64;
        x.iter_mut()
            .for_each(|v| *v += (rng.gaussian() * sigma) as f32);
    }

    let mut rx = PedalRx::new(config.clone());
    let detections = rx.push_samples(&x);

    let ratio = 1.0 + sc.drift_ppm * 1e-6;
    let expected: Vec<f64> = sent.iter().map(|&(s, _)| s as f64 / ratio).collect();
    let span = (config.command_samples() + 2 * config.symbol_samples) as f64;
    let mut matched = vec![false; sent.len()];
    let mut out = Outcome {
        sent: sent.len(),
        ..Outcome::default()
    };
    for d in detections {
        let at = d.at_sample as f64;
        let k = expected.partition_point(|&s| s <= at);
        match k.checked_sub(1).filter(|&k| at - expected[k] <= span) {
            Some(k) if !matched[k] => {
                matched[k] = true;
                if d.command == sent[k].1 {
                    out.delivered += 1;
                    out.latencies_ms
                        .push(((at - expected[k]) * 1000.0 / RATE as f64) as f32);
                } else {
                    out.wrong += 1;
                }
            }
            _ => out.false_hits += 1,
        }
    }
    out
}

fn delivery_scenarios() -> Vec<Scenario> {
    use Music::*;
    let s = Scenario::new;
    vec![
        s("clean, no music", None),
        Scenario {
            ctrl_db: -20.0,
            ..s("band, control -20 dB", Band)
        },
        s("band, control -30 dB", Band),
        Scenario {
            ctrl_db: -40.0,
            ..s("band, control -40 dB", Band)
        },
        Scenario {
            ctrl_db: -50.0,
            ..s("band, control -50 dB", Band)
        },
        Scenario {
            ctrl_db: -60.0,
            ..s("band, control -60 dB", Band)
        },
        Scenario {
            snr_db: Some(10.0),
            ..s("band -30 + noise, SNR 10 dB", Band)
        },
        Scenario {
            snr_db: Some(3.0),
            ..s("band -30 + noise, SNR 3 dB", Band)
        },
        Scenario {
            snr_db: Some(0.0),
            ..s("band -30 + noise, SNR 0 dB", Band)
        },
        Scenario {
            snr_db: Some(-3.0),
            ..s("band -30 + noise, SNR -3 dB", Band)
        },
        Scenario {
            clicks: Some((100.0, 20.0)),
            ..s("band -30 + clicks 100/s, +20 dB", Band)
        },
        Scenario {
            dropout_ms: 1.0,
            ..s("band -30, 1 ms dropout per command", Band)
        },
        Scenario {
            dropout_ms: 3.0,
            ..s("band -30, 3 ms dropout per command", Band)
        },
        Scenario {
            drift_ppm: 500.0,
            ..s("band -30, clock drift +500 ppm", Band)
        },
        Scenario {
            lowpass: Some((8, 21_000.0)),
            ..s("band -30, converter low-pass 21 kHz", Band)
        },
        Scenario {
            level_db: -40.0,
            snr_db: Some(20.0),
            ..s("band -30, level -40 dB, SNR 20 dB", Band)
        },
        s("bright guitar, control -30 dB", BrightGuitar),
        Scenario {
            ctrl_db: -10.0,
            ..s("bright synth, control -10 dB", Synth)
        },
        Scenario {
            ctrl_db: -20.0,
            ..s("bright synth, control -20 dB", Synth)
        },
        s("bright synth, control -30 dB", Synth),
        s("fuzz guitar (aliasing), control -30 dB", FuzzGuitar),
        Scenario {
            fuzz: Some(10.0),
            ..s("guitar -30, fuzz before the decoder", BrightGuitar)
        },
    ]
}

// ---------------------------------------------------------------------------
// False triggers

/// Back-to-back AML NoteOns of `config` (20 dB SNR).
fn aml_traffic(config: PhyConfig, n: usize, seed: u64) -> Vec<f32> {
    let mut tx = CricketModemTx::new(config.clone());
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while out.len() < n {
        out.extend(tx.encode_message(&TransportMessage::NoteOn {
            channel: (i % 16) as u8,
            note: (i * 37 % 128) as u8,
            velocity: 1 + (i * 7 % 127) as u8,
        }));
        i += 1;
    }
    out.truncate(n);
    let mut rng = XorShift64::new(seed);
    let sigma = (config.amplitude / 2f32.sqrt() / db(20.0)) as f64;
    out.iter_mut()
        .for_each(|v| *v += (rng.gaussian() * sigma) as f32);
    out
}

/// A whistle sweeping 19.0–22.5 kHz and back, 2 s per cycle.
fn sweep(n: usize) -> Vec<f32> {
    let mut phase = 0.0f64;
    (0..n)
        .map(|i| {
            let t = (i % (2 * RATE)) as f64 / RATE as f64;
            let frac = if t < 1.0 { t } else { 2.0 - t };
            phase += 2.0 * PI * (19_000.0 + 3_500.0 * frac) / RATE as f64;
            0.1 * phase.sin() as f32
        })
        .collect()
}

fn with_noise_floor(mut x: Vec<f32>, dbfs: f32, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let sigma = db(dbfs) as f64;
    x.iter_mut()
        .for_each(|v| *v += (rng.gaussian() * sigma) as f32);
    x
}

fn false_signals(n: usize) -> Vec<(&'static str, Vec<f32>)> {
    let loud_band = {
        let mut b = music(Music::Band, n, 11);
        normalize(&mut b, -12.0);
        b
    };
    let clicky = {
        let mut b = loud_band.clone();
        let mut rng = XorShift64::new(5);
        let p = 200.0 / RATE as f64;
        for i in 0..n {
            if rng.next_f64() < p {
                let sign = if rng.below(2) == 0 { 1.0 } else { -1.0 };
                for v in b[i..(i + 1 + rng.below(4)).min(n)].iter_mut() {
                    *v += sign;
                }
            }
        }
        b
    };
    let high_band_robust = PhyConfig {
        tone_base_hz: 20_000.0,
        ..PhyConfig::robust()
    };
    vec![
        (
            "band, loud (-12 dBFS RMS)",
            with_noise_floor(loud_band, -100.0, 1),
        ),
        (
            "bright guitar",
            with_noise_floor(music(Music::BrightGuitar, n, 12), -100.0, 2),
        ),
        (
            "fuzz guitar (aliasing)",
            with_noise_floor(music(Music::FuzzGuitar, n, 13), -100.0, 3),
        ),
        (
            "bright synth (aliasing saw, vibrato)",
            with_noise_floor(music(Music::Synth, n, 14), -100.0, 4),
        ),
        ("band + clicks 200/s at 0 dBFS", clicky),
        (
            "white noise -30 dBFS",
            with_noise_floor(vec![0.0; n], -30.0, 6),
        ),
        (
            "whistle sweeping 19-22.5 kHz",
            with_noise_floor(sweep(n), -100.0, 7),
        ),
        (
            "AML Robust traffic (2.5-4 kHz)",
            aml_traffic(PhyConfig::robust(), n, 8),
        ),
        (
            "AML Robust moved to 20-21.5 kHz",
            aml_traffic(high_band_robust, n, 9),
        ),
    ]
}

struct FalseRow {
    windows: u64,
    gated: u64,
    detections: u64,
    hours: f64,
    /// Independence model: hops per hour x sum over codewords of the
    /// probability that its windows match (each expected tone at its gate
    /// rate, a tone never seen counting as seen once; up to `max_misses`
    /// windows showing no tone instead). Meaningless for structured signals
    /// such as a sweep or MFSK traffic, whose windows are not independent.
    model_per_hour: f64,
}

fn false_triggers(config: &PedalConfig, signal: &[f32]) -> FalseRow {
    let mut rx = PedalRx::new(config.clone());
    let detections = rx.push_samples(signal).len() as u64;
    let st = rx.stats();
    let w = st.windows.max(1) as f64;
    let p = |t: u8| st.gated_per_tone[t as usize].max(1) as f64 / w;
    let p_none = 1.0 - st.gated_windows as f64 / w;
    let l = config.codeword_len();
    let per_hop: f64 = config
        .codewords
        .iter()
        .map(|c| {
            (0u32..1 << l)
                .filter(|missed| missed.count_ones() as usize <= config.max_misses)
                .map(|missed| {
                    c.iter()
                        .enumerate()
                        .map(|(j, &t)| if missed & 1 << j != 0 { p_none } else { p(t) })
                        .product::<f64>()
                })
                .sum::<f64>()
        })
        .sum();
    let hops_per_hour = 3600.0 * RATE as f64 / config.hop_samples as f64;
    FalseRow {
        windows: st.windows,
        gated: st.gated_windows,
        detections,
        hours: signal.len() as f64 / RATE as f64 / 3600.0,
        model_per_hour: per_hop * hops_per_hour,
    }
}

fn rate_text(per_hour: f64) -> String {
    if per_hour >= 1.0 {
        format!("{per_hour:.1} / h")
    } else if per_hour * 24.0 * 365.0 >= 1.0 {
        format!("1 per {:.0} h", 1.0 / per_hour)
    } else {
        format!("1 per {:.1e} years", 1.0 / (per_hour * 24.0 * 365.0))
    }
}

// ---------------------------------------------------------------------------
// Leakage

/// Fraction of the energy of `x` below each cutoff, dB (exact DFT).
fn energy_below_db(x: &[f32], cutoffs: &[f32]) -> Vec<f32> {
    let m = x.len();
    let (cos, sin): (Vec<f64>, Vec<f64>) = (0..m)
        .map(|i| {
            let w = 2.0 * PI * i as f64 / m as f64;
            (w.cos(), w.sin())
        })
        .unzip();
    let power: Vec<f64> = (0..=m / 2)
        .map(|k| {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, &v) in x.iter().enumerate() {
                let idx = k * i % m;
                re += v as f64 * cos[idx];
                im -= v as f64 * sin[idx];
            }
            let both = if k == 0 || 2 * k == m { 1.0 } else { 2.0 };
            both * (re * re + im * im)
        })
        .collect();
    let total: f64 = power.iter().sum();
    cutoffs
        .iter()
        .map(|&f| {
            let below: f64 = power
                .iter()
                .enumerate()
                .filter(|(k, _)| (*k as f64 * RATE as f64 / m as f64) < f as f64)
                .map(|(_, p)| p)
                .sum();
            10.0 * (below / total).max(1e-30).log10() as f32
        })
        .collect()
}

fn all_commands(config: &PedalConfig) -> Vec<f32> {
    let tx = PedalTx::new(config.clone());
    PedalCommand::ALL
        .iter()
        .flat_map(|&c| tx.synthesize(c))
        .collect()
}

fn energy_db(x: &[f32]) -> f32 {
    let e: f64 = x.iter().map(|&v| v as f64 * v as f64).sum();
    10.0 * e.max(1e-30).log10() as f32
}

/// Energy of `x` below 16 kHz (16th-order Butterworth, >100 dB down at
/// 20 kHz), dB.
fn audible_energy_db(x: &[f32]) -> f32 {
    let mut y = x.to_vec();
    Butterworth::lowpass(16, 16_000.0, RATE as u32).run(&mut y);
    energy_db(&y)
}

/// Commands repeating back to back for `n` samples, at `peak`.
fn command_track(config: &PedalConfig, n: usize, peak: f32) -> Vec<f32> {
    let g = peak / config.amplitude;
    let one: Vec<f32> = all_commands(config).iter().map(|v| v * g).collect();
    one.iter().copied().cycle().take(n).collect()
}

// ---------------------------------------------------------------------------
// Regression checks (cargo test)

fn assert_perfect(config: &PedalConfig, sc: &Scenario, seconds: f32, seed: u64) {
    let o = run(config, sc, seconds, seed);
    assert!(o.sent > 10, "{}: only {} commands", sc.name, o.sent);
    assert_eq!(
        (o.delivered, o.wrong, o.false_hits),
        (o.sent, 0, 0),
        "{}: {o:?}",
        sc.name
    );
}

#[test]
fn commands_survive_band_music() {
    let config = PedalConfig::standard();
    assert_perfect(&config, &Scenario::new("band -30", Music::Band), 10.0, 1);
    let quiet = Scenario {
        ctrl_db: -50.0,
        ..Scenario::new("band -50", Music::Band)
    };
    assert_perfect(&config, &quiet, 5.0, 2);
}

#[test]
fn shorter_variants_survive_band_music() {
    for config in [PedalConfig::three_symbols(), PedalConfig::short_symbols()] {
        assert_perfect(&config, &Scenario::new("band -30", Music::Band), 5.0, 3);
    }
}

#[test]
fn commands_survive_level_and_drift() {
    let config = PedalConfig::standard();
    let sc = Scenario {
        level_db: -40.0,
        snr_db: Some(20.0),
        drift_ppm: 500.0,
        ..Scenario::new("band -30, -40 dB, drift", Music::Band)
    };
    assert_perfect(&config, &sc, 5.0, 4);
}

#[test]
fn music_clicks_and_noise_give_no_commands() {
    let n = 10 * RATE;
    for (name, signal) in false_signals(n) {
        if name.starts_with("AML") || name.starts_with("whistle") {
            continue; // measured in the report
        }
        let mut rx = PedalRx::new(PedalConfig::standard());
        let got = rx.push_samples(&signal);
        assert!(got.is_empty(), "{name}: {got:?}");
    }
}

#[test]
fn tx_leaks_little_below_19_khz() {
    for config in [
        PedalConfig::standard(),
        PedalConfig::three_symbols(),
        PedalConfig::short_symbols(),
    ] {
        let tx = PedalTx::new(config.clone());
        for cmd in PedalCommand::ALL {
            let below = energy_below_db(&tx.synthesize(cmd), &[19_000.0, 16_000.0]);
            assert!(below[0] < -30.0, "{cmd:?}: {below:?} dB below 19/16 kHz");
            assert!(below[1] < -65.0, "{cmd:?}: {below:?} dB below 19/16 kHz");
        }
    }
}

#[test]
fn detected_commands_drive_the_preset_state() {
    let config = PedalConfig::standard();
    let tx = PedalTx::new(config.clone());
    let mut rng = XorShift64::new(9);
    let cmds: Vec<PedalCommand> = (0..40).map(|_| PedalCommand::ALL[rng.below(4)]).collect();
    let mut x = music(Music::Band, cmds.len() * config.total_samples() + RATE, 9);
    let g = db(MUSIC_RMS_DBFS - 30.0) * 2f32.sqrt() / config.amplitude;
    for (i, &c) in cmds.iter().enumerate() {
        // Back to back: one symbol of silence between commands.
        let start = RATE / 2 + i * config.total_samples();
        for (k, v) in tx.synthesize(c).iter().enumerate() {
            x[start + k] += g * v;
        }
    }
    let mut rx = PedalRx::new(config);
    let mut via_audio = PresetState::new(PresetMap::default()).unwrap();
    let mut midi = Vec::new();
    for d in rx.push_samples(&x) {
        midi.extend(via_audio.apply(d.command));
    }
    let mut direct = PresetState::new(PresetMap::default()).unwrap();
    for &c in &cmds {
        direct.apply(c);
    }
    assert_eq!(via_audio, direct);
    assert_eq!(midi.len(), cmds.len());
}

// ---------------------------------------------------------------------------
// Report

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "report: cargo test --release -p cricket_core --test pedal -- --ignored --nocapture"]
fn pedal_report() {
    let seconds: f32 = env_or("PEDAL_SECONDS", 30.0);
    let seeds: u64 = env_or("PEDAL_SEEDS", 2);
    let false_minutes: f32 = env_or("PEDAL_FALSE_MINUTES", 10.0);
    let purity: Option<f32> = std::env::var("PEDAL_MIN_PURITY")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut variants = [
        ("standard (4 x 4 ms)", PedalConfig::standard()),
        ("three_symbols (3 x 4 ms)", PedalConfig::three_symbols()),
        ("short_symbols (4 x 3 ms)", PedalConfig::short_symbols()),
        (
            "standard, 1 miss allowed",
            PedalConfig {
                max_misses: 1,
                ..PedalConfig::standard()
            },
        ),
    ];
    if let Some(p) = purity {
        println!("\nPurity gate overridden: {p}");
        for (_, c) in variants.iter_mut() {
            c.min_purity = p;
        }
    }

    println!("\n## Delivery ({seconds} s x {seeds} seeds per row; latency = command start to detection)\n");
    print!("| channel |");
    for (name, _) in &variants {
        print!(" {name} |");
    }
    println!();
    println!("|---|{}", "---|".repeat(variants.len()));
    for sc in delivery_scenarios() {
        print!("| {} |", sc.name);
        for (_, config) in &variants {
            let mut total = Outcome::default();
            for seed in 0..seeds {
                total.add(run(config, &sc, seconds, 100 + seed));
            }
            let mut extra = String::new();
            if total.wrong > 0 {
                extra.push_str(&format!(", **{} wrong**", total.wrong));
            }
            if total.false_hits > 0 {
                extra.push_str(&format!(", **{} false**", total.false_hits));
            }
            if total.latencies_ms.is_empty() {
                print!(" {:.1} %{extra} |", total.delivery_pct());
            } else {
                print!(
                    " {:.1} % ({:.1} / {:.1} ms{extra}) |",
                    total.delivery_pct(),
                    total.mean_ms(),
                    total.p95_ms()
                );
            }
        }
        println!();
    }
    println!("\nCells: delivered % (mean / p95 latency, wrong and false commands if any).");

    let n = (false_minutes * 60.0 * RATE as f32) as usize;
    println!("\n## False triggers ({false_minutes} min per signal)\n");
    println!("| signal | variant | windows through the gate | commands | model |");
    println!("|---|---|---|---|---|");
    for (name, signal) in false_signals(n) {
        for (vname, config) in &variants {
            let r = false_triggers(config, &signal);
            println!(
                "| {name} | {vname} | {:.3} % | {} ({}) | {} |",
                100.0 * r.gated as f64 / r.windows.max(1) as f64,
                r.detections,
                if r.detections > 0 {
                    rate_text(r.detections as f64 / r.hours)
                } else {
                    "0".to_string()
                },
                rate_text(r.model_per_hour),
            );
        }
    }

    println!("\n## Leakage into the audible band\n");
    println!("| variant | energy below 19 kHz | below 18 kHz | below 16 kHz | below 12 kHz |");
    println!("|---|---|---|---|---|");
    for (vname, config) in &variants {
        let b = energy_below_db(
            &all_commands(config),
            &[19_000.0, 18_000.0, 16_000.0, 12_000.0],
        );
        println!(
            "| {vname} | {:.1} dB | {:.1} dB | {:.1} dB | {:.1} dB |",
            b[0], b[1], b[2], b[3]
        );
    }
    let config = PedalConfig::standard();
    let cmds = all_commands(&config);
    for order in [8, 16] {
        let mut y = cmds.clone();
        Butterworth::lowpass(order, 18_000.0, RATE as u32).run(&mut y);
        println!(
            "\nStrip filter (Butterworth low-pass, order {order}, 18 kHz) on the commands: {:.1} dB",
            energy_db(&y) - energy_db(&cmds)
        );
    }

    println!("\nDigital fuzz (tanh, drive 10, no oversampling) on band music + commands. Audible");
    println!("artifact = energy below 16 kHz of fuzz(music + commands) - fuzz(music), relative to");
    println!("fuzz(music) below 16 kHz:\n");
    println!(
        "| control level | no strip filter | strip filter (16th order, 18 kHz) before the fuzz |"
    );
    println!("|---|---|---|");
    let len = 5 * RATE;
    let m = music(Music::Band, len, 21);
    let fuzz = |x: &[f32]| -> Vec<f32> { x.iter().map(|&v| (10.0 * v).tanh() / 10.0).collect() };
    let strip = |x: &[f32]| -> Vec<f32> {
        let mut y = x.to_vec();
        Butterworth::lowpass(16, 18_000.0, RATE as u32).run(&mut y);
        y
    };
    for ctrl_db in [-20.0f32, -30.0, -40.0] {
        let c = command_track(&config, len, db(MUSIC_RMS_DBFS + ctrl_db) * 2f32.sqrt());
        let mix: Vec<f32> = m.iter().zip(&c).map(|(a, b)| a + b).collect();
        let artifact = |pre: &dyn Fn(&[f32]) -> Vec<f32>| {
            let y0 = fuzz(&pre(&m));
            let y1 = fuzz(&pre(&mix));
            let diff: Vec<f32> = y1.iter().zip(&y0).map(|(a, b)| a - b).collect();
            audible_energy_db(&diff) - audible_energy_db(&y0)
        };
        println!(
            "| {ctrl_db} dB | {:.1} dB | {:.1} dB |",
            artifact(&|x: &[f32]| x.to_vec()),
            artifact(&strip)
        );
    }

    let signal = music(Music::Band, 20 * RATE, 31);
    for (vname, config) in &variants {
        let mut rx = PedalRx::new(config.clone());
        let start = Instant::now();
        let got = rx.push_samples(&signal);
        let ns = start.elapsed().as_nanos() as f64 / signal.len() as f64;
        assert!(got.is_empty());
        println!("\nRX CPU, {vname}: {ns:.1} ns per sample (host, release)");
    }
}
