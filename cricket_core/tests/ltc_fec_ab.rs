//! A/B measurements for the experimental LTC (BMC) profiles with and without
//! Reed–Solomon body FEC. Prints Markdown tables; not an assertion test
//! (apart from "no false messages"). Run with
//!
//! ```sh
//! cargo test --release -p cricket_core --test ltc_fec_ab -- --ignored --nocapture
//! ```
//!
//! Environment: `LTC_AB_TRIALS` (default 400 per cell), `LTC_AB_PROFILES`
//! (comma-separated keys of [`profile`]), `LTC_AB_SET` (`snr`, `impair` or
//! `all`). Results are recorded in `docs/proposals/ltc-bmc.md`.

mod support;

use cricket_core::link::FecMode;
use cricket_core::modem::{repetition_samples, CricketModemRx, CricketModemTx, ModemOutput};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use support::{ChannelSim, XorShift64};

/// Profiles by key: `ltc<rate>[_fec<parity>]` (`ltc` = 4800).
fn profile(key: &str) -> PhyConfig {
    let (rate, parity) = match key.split_once("_fec") {
        Some((r, p)) => (r, p.parse::<u8>().unwrap_or(4)),
        None => (key, 0),
    };
    let rate = match rate.trim_start_matches("ltc") {
        "" => 4800,
        r => r.parse().expect("bit rate"),
    };
    let mut config = PhyConfig::ltc_with_bit_rate(rate).with_fec(if parity == 0 {
        FecMode::None
    } else {
        FecMode::Rs16 { parity }
    });
    // Receiver parameter overrides for tuning: LTC_AB_PARAMS="name=value,...".
    if let (Ok(spec), cricket_core::phy::Modulation::Bmc(p)) =
        (std::env::var("LTC_AB_PARAMS"), &mut config.modulation)
    {
        for kv in spec.split(',').filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').expect("name=value");
            let v: f32 = v.parse().expect("number");
            match k {
                "hysteresis" => p.hysteresis = v,
                "lowpass" => p.rx_lowpass_ratio = v,
                "trim" => p.rx_window_trim = v,
                "outer_trim" => p.rx_window_outer_trim = v,
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
                "sync_pre" => p.sync_preamble_half_cells = v as usize,
                "snap" => p.sync_snap = v,
                "resync" => p.resync_margin = v,
                "certain" => p.resync_certain = v,
                "max_rate" => p.max_rate_error = v,
                _ => panic!("unknown parameter {k}"),
            }
        }
    }
    config
}

#[derive(Debug, Clone, Copy)]
enum Imp {
    Clean,
    Snr(f32),
    /// Clicks per second, click peak dB re signal peak; at SNR 20 dB.
    Clicks(f32, f32),
    /// Dropout ms, at SNR 20 dB.
    Dropout(f32),
    /// Peaking EQ gain dB @ 3 kHz Q1, at SNR 20 dB.
    Eq3k(f32),
    /// Peaking EQ gain dB @ 6 kHz Q1, at SNR 20 dB.
    Eq6k(f32),
    /// 2-pole low-pass Hz, at SNR 20 dB.
    Lp2(f32),
    /// 1-pole high-pass Hz, at SNR 20 dB.
    Hp(f32),
    /// Drift ppm, at an SNR.
    Drift(f64, f32),
    /// Polarity inverted, at an SNR.
    Inverted(f32),
    /// Gain dB, at an SNR.
    Level(f32, f32),
}

impl Imp {
    fn label(&self) -> String {
        match *self {
            Imp::Clean => "clean".into(),
            Imp::Snr(s) => format!("SNR {s} dB"),
            Imp::Clicks(r, l) => format!("clicks {r}/s @ {l:+} dB"),
            Imp::Dropout(ms) => format!("dropout {ms} ms"),
            Imp::Eq3k(g) => format!("EQ {g:+} dB @ 3 kHz"),
            Imp::Eq6k(g) => format!("EQ {g:+} dB @ 6 kHz"),
            Imp::Lp2(f) => format!("2-pole LP {f} Hz"),
            Imp::Hp(f) => format!("HP {f} Hz"),
            Imp::Drift(p, s) => format!("drift {:+}% + SNR {s}", p / 10_000.0),
            Imp::Inverted(s) => format!("inverted + SNR {s}"),
            Imp::Level(g, s) => format!("level {g} dB + SNR {s}"),
        }
    }

    fn sim(&self, config: &PhyConfig, seed: u64, offset: usize) -> ChannelSim {
        let base = ChannelSim::new(config.clone(), seed).offset(offset);
        let ms = |ms: f32| (ms * config.sample_rate_hz as f32 / 1000.0) as usize;
        match *self {
            Imp::Clean => base,
            Imp::Snr(s) => base.snr_db(s),
            Imp::Clicks(r, l) => base.clicks(r, l).snr_db(20.0),
            Imp::Dropout(d) => base.dropout_samples(ms(d)).snr_db(20.0),
            Imp::Eq3k(g) => base.peaking_eq(3_000.0, g, 1.0).snr_db(20.0),
            Imp::Eq6k(g) => base.peaking_eq(6_000.0, g, 1.0).snr_db(20.0),
            Imp::Lp2(f) => base.lowpass2_hz(f).snr_db(20.0),
            Imp::Hp(f) => base.highpass_hz(f).snr_db(20.0),
            Imp::Drift(p, s) => base.drift_ppm(p).snr_db(s),
            Imp::Inverted(s) => base.invert().snr_db(s),
            Imp::Level(g, s) => base.gain(10f32.powf(g / 20.0)).snr_db(s),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Cell {
    ok: usize,
    trials: usize,
    latency_sum_ms: f64,
    false_msgs: usize,
}

/// `trials` single-message transmissions (NoteOn, every 4th an MSC GO;
/// random start offset; 64-sample chunks so delivery time has ~1.3 ms
/// resolution). The dropout lands anywhere in the message.
fn run_cell(config: &PhyConfig, imp: Imp, trials: usize, seed: u64) -> Cell {
    let mut rng = XorShift64::new(seed);
    let rate = config.sample_rate_hz as f64;
    let mut cell = Cell {
        trials,
        ..Cell::default()
    };
    for t in 0..trials {
        let msg = if t % 4 == 3 {
            {
                let list = rng.below(16) as u8;
                let cue = rng.below(128) as u16;
                TransportMessage::Msc(MscMessage::cue(
                    MscCommand::Go,
                    MscCue::from_tenths(cue * 10 + u16::from(list % 10)).unwrap(),
                    (list > 0).then_some(list),
                ))
            }
        } else {
            TransportMessage::NoteOn {
                channel: 0,
                note: rng.below(128) as u8,
                velocity: 1 + rng.below(127) as u8,
            }
        };
        let offset = 200 + rng.below(400);
        let tx = CricketModemTx::new(config.clone());
        let mut clean = tx.encode_message_with_seq(&msg, rng.below(4) as u8);
        let msg_len = clean.len();
        clean.extend(std::iter::repeat_n(0.0, config.sample_rate_hz as usize / 8));
        let mut sim = imp.sim(config, rng.next_u64(), offset);
        if let Imp::Dropout(_) = imp {
            // Dropout inside the message, not in the trailing silence.
            let mut audio = sim.apply(&clean[..msg_len]);
            sim.dropout_samples = 0;
            let tail = sim.clone().offset(0).apply(&clean[msg_len..]);
            audio.extend(tail);
            cell.add(decode(config, &audio, &msg, offset, rate));
            continue;
        }
        let audio = sim.apply(&clean);
        cell.add(decode(config, &audio, &msg, offset, rate));
    }
    cell
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

fn decode(
    config: &PhyConfig,
    audio: &[f32],
    msg: &TransportMessage,
    offset: usize,
    rate: f64,
) -> (Option<f64>, usize) {
    let mut rx = CricketModemRx::new(config.clone());
    let mut delivered_at = None;
    let mut false_msgs = 0;
    for chunk in audio.chunks(64) {
        for out in rx.push_samples(chunk) {
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
        delivered_at.map(|at| (at as f64 - offset as f64) / rate * 1000.0),
        false_msgs,
    )
}

fn snr_set() -> Vec<Imp> {
    [12.0, 10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0, 0.0]
        .into_iter()
        .map(Imp::Snr)
        .collect()
}

fn impair_set() -> Vec<Imp> {
    vec![
        Imp::Clean,
        Imp::Clicks(20.0, 0.0),
        Imp::Clicks(50.0, 6.0),
        Imp::Clicks(200.0, 6.0),
        Imp::Clicks(100.0, 12.0),
        Imp::Dropout(1.0),
        Imp::Dropout(3.0),
        Imp::Dropout(10.0),
        Imp::Eq3k(6.0),
        Imp::Eq3k(12.0),
        Imp::Eq3k(-12.0),
        Imp::Eq6k(-12.0),
        Imp::Lp2(6_000.0),
        Imp::Lp2(4_000.0),
        Imp::Lp2(3_400.0),
        Imp::Hp(300.0),
        Imp::Hp(1_000.0),
        Imp::Drift(10_000.0, 12.0),
        Imp::Drift(-10_000.0, 12.0),
        Imp::Drift(10_000.0, 6.0),
        Imp::Inverted(6.0),
        Imp::Level(-40.0, 8.0),
    ]
}

fn env_list(name: &str, default: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn ltc_fec_ab_table() {
    let trials: usize = std::env::var("LTC_AB_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let keys = env_list(
        "LTC_AB_PROFILES",
        "ltc,ltc_fec2,ltc_fec4,ltc2400,ltc2400_fec4",
    );
    let set = std::env::var("LTC_AB_SET").unwrap_or_else(|_| "all".into());
    let mut imps = Vec::new();
    if set == "snr" || set == "all" {
        imps.extend(snr_set());
    }
    if set == "impair" || set == "all" {
        imps.extend(impair_set());
    }
    if set == "tune" {
        imps.extend([
            Imp::Snr(4.0),
            Imp::Snr(2.0),
            Imp::Snr(0.0),
            Imp::Clicks(200.0, 6.0),
            Imp::Dropout(3.0),
            Imp::Eq3k(-12.0),
            Imp::Lp2(3_400.0),
            Imp::Hp(1_000.0),
            Imp::Drift(10_000.0, 6.0),
            Imp::Drift(-30_000.0, 12.0),
            Imp::Drift(30_000.0, 12.0),
            Imp::Drift(50_000.0, 12.0),
            Imp::Eq6k(-12.0),
        ]);
    }
    let profiles: Vec<(String, PhyConfig)> = keys.iter().map(|k| (k.clone(), profile(k))).collect();

    println!("\n### Airtime (one copy, incl. gap)\n");
    println!("| profile | body nibbles | burst ms | airtime ms |");
    println!("|---|---|---|---|");
    for (name, c) in &profiles {
        let rep = repetition_samples(c) as f64 * 1000.0 / c.sample_rate_hz as f64;
        println!(
            "| {name} | {} | {:.1} | {rep:.1} |",
            c.fec.body_nibbles(),
            c.estimated_first_copy_ms()
        );
    }

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
                let cell = run_cell(&profiles[p].1, imps[i], trials, 0x17C0 + i as u64);
                results.lock().unwrap()[p][i] = cell;
            });
        }
    });
    let results = results.into_inner().unwrap();

    println!("\n### Delivery % (mean start-to-delivery ms), {trials} trials per cell\n");
    print!("| channel |");
    for (name, _) in &profiles {
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
    println!("\nfalse messages (all cells): {false_total}");
    assert_eq!(false_total, 0);
}

/// Messages and failed body decodes (false locks) from `audio`.
fn count_decodes(config: &PhyConfig, audio: &[f32]) -> (usize, u64) {
    let mut rx = CricketModemRx::new(config.clone());
    let mut msgs = 0;
    let mut fails = 0;
    for chunk in audio.chunks(512) {
        for out in rx.push_samples(chunk) {
            match out {
                ModemOutput::Message(_) => msgs += 1,
                ModemOutput::Stats(s) => fails = s.crc_fail,
                _ => {}
            }
        }
    }
    (msgs, fails)
}

/// White noise and MFSK traffic must never decode on any LTC profile.
#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn ltc_false_message_report() {
    let keys = env_list(
        "LTC_AB_PROFILES",
        "ltc,ltc_fec2,ltc_fec4,ltc2400,ltc2400_fec4,ltc9600,ltc9600_fec4",
    );
    println!(
        "\n| profile | false msgs (failed bodies), 60 s white noise | false msgs (failed bodies), 60 s MFSK traffic |"
    );
    println!("|---|---|---|");
    std::thread::scope(|s| {
        let handles: Vec<_> = keys
            .iter()
            .map(|k| {
                s.spawn(move || {
                    let config = profile(k);
                    let mut rng = XorShift64::new(0xFA15E);
                    let (mut noise_false, mut noise_locks) = (0, 0);
                    for sigma in [0.003f64, 0.05, 0.5] {
                        let noise: Vec<f32> = (0..48_000 * 20)
                            .map(|_| (rng.gaussian() * sigma) as f32)
                            .collect();
                        let (m, l) = count_decodes(&config, &noise);
                        noise_false += m;
                        noise_locks += l;
                    }
                    // 60 s of back-to-back MFSK messages (robust_repeat, fast, robust).
                    let (mut mfsk_false, mut mfsk_locks) = (0, 0);
                    for m in [
                        PhyConfig::robust_repeat(),
                        PhyConfig::fast(),
                        PhyConfig::robust(),
                    ] {
                        let mut tx = CricketModemTx::new(m.clone());
                        let mut audio = Vec::new();
                        let mut i = 0usize;
                        while audio.len() < 48_000 * 20 {
                            audio.extend(tx.encode_message(&TransportMessage::NoteOn {
                                channel: 0,
                                note: (i % 128) as u8,
                                velocity: 1 + (i * 7 % 127) as u8,
                            }));
                            i += 1;
                        }
                        let audio = ChannelSim::new(m, i as u64).snr_db(20.0).apply(&audio);
                        let (m, l) = count_decodes(&config, &audio);
                        mfsk_false += m;
                        mfsk_locks += l;
                    }
                    (k.clone(), noise_false, noise_locks, mfsk_false, mfsk_locks)
                })
            })
            .collect();
        for h in handles {
            let (k, n, nl, m, ml) = h.join().unwrap();
            println!("| {k} | {n} ({nl}) | {m} ({ml}) |");
        }
    });
}
