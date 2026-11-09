//! Body FEC (spec §4.6) on the Robust profile (`PhyConfig::robust`,
//! docs/proposals/fec.md): loopback, sync sweep, and an A/B comparison
//! against K = 2 repetition (`PhyConfig::robust_repeat`) on the channel
//! simulator.
//!
//! The full A/B table is an ignored test (it takes a while in debug builds):
//!
//! ```sh
//! cargo test --release -p cricket_core --test fec -- --ignored --nocapture
//! ```

mod support;

use cricket_core::link::FecMode;
use cricket_core::modem::{
    encode_to_samples, repetition_samples, CricketModemRx, CricketModemTx, ModemOutput,
};
use cricket_core::phy::{Acquisition, PhyConfig};
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage};
use support::{decode_chunked, ChannelSim, XorShift64};

fn all_types() -> Vec<TransportMessage> {
    vec![
        TransportMessage::NoteOn {
            channel: 0,
            note: 60,
            velocity: 100,
        },
        TransportMessage::NoteOff {
            channel: 0,
            note: 60,
            velocity: 64,
        },
        TransportMessage::ProgramChange {
            channel: 0,
            program: 42,
        },
        TransportMessage::Msc(MscMessage::cue(
            MscCommand::Go,
            MscCue::from_tenths(1270).unwrap(),
            Some(15),
        )),
        TransportMessage::Heartbeat,
        TransportMessage::Panic,
    ]
}

fn fec_parity(parity: u8, k: usize) -> PhyConfig {
    PhyConfig {
        repetition_k: k,
        fec: FecMode::Rs16 { parity },
        ..PhyConfig::robust()
    }
}

#[test]
fn profile_parameters() {
    assert_eq!(PhyConfig::robust_repeat().fec, FecMode::None);
    assert_eq!(PhyConfig::robust_repeat().repetition_k, 2);
    assert_eq!(PhyConfig::fast().fec, FecMode::None);
    assert_eq!(PhyConfig::default().fec, FecMode::Rs16 { parity: 4 });
    assert_eq!(repetition_samples(&PhyConfig::robust_repeat()), 21 * 312);
    let fec = PhyConfig::robust();
    assert_eq!(fec.repetition_k, 1);
    assert_eq!(fec.fec.body_nibbles(), 22);
    assert_eq!(repetition_samples(&fec), 27 * 312);
    assert!((fec.estimated_first_copy_ms() - 26.0 * 6.5).abs() < 1e-3);
    // Names: `robust_fec` is a deprecated alias of `robust`, `analog-safe`
    // its user-facing name; unknown names are rejected.
    for name in [
        "robust",
        "ROBUST",
        "robust_fec",
        "robust-fec",
        "analog-safe",
        "analog_safe",
    ] {
        let c = PhyConfig::from_name(name).unwrap();
        assert_eq!(c.fec, fec.fec, "{name}");
        assert_eq!(c.repetition_k, 1, "{name}");
        assert_eq!(c.acquisition, fec.acquisition, "{name}");
    }
    assert!(PhyConfig::from_name("nonsense").is_none());
    for name in ["robust_repeat", "robust-repeat", "analog-safe-repeat"] {
        let c = PhyConfig::from_name(name).unwrap();
        assert_eq!(c.fec, FecMode::None, "{name}");
        assert_eq!(c.repetition_k, 2, "{name}");
    }
    assert_eq!(
        PhyConfig::robust_repeat().acquisition,
        Acquisition::Joint { misses: 1 }
    );
    assert_eq!(PhyConfig::fast().acquisition, Acquisition::FirstSymbol);
    assert_eq!(fec.acquisition, Acquisition::Joint { misses: 2 });
}

/// Joint acquisition: any one or two muted preamble symbols (including the
/// first) still lock; three do not, and nothing false is decoded then.
#[test]
fn joint_acquisition_tolerates_two_missing_preamble_symbols() {
    let config = PhyConfig::robust();
    let period = config.samples_per_symbol_period();
    let msg = TransportMessage::Msc(MscMessage::cue(
        MscCommand::Go,
        MscCue::from_tenths(770).unwrap(),
        Some(2),
    ));
    let packet = CricketModemTx::new(config.clone()).encode_message_with_seq(&msg, 1);
    let muted_sets: [&[usize]; 9] = [
        &[0],
        &[1],
        &[2],
        &[3],
        &[0, 1],
        &[0, 3],
        &[1, 2],
        &[2, 3],
        &[0, 1, 2],
    ];
    let mut failures = Vec::new();
    for muted in muted_sets {
        let mut audio = packet.clone();
        for &i in muted {
            audio[i * period..(i + 1) * period].fill(0.0);
        }
        for (offset, chunk) in [(0usize, 4096usize), (101, 64), (period / 2, 240)] {
            let rx = ChannelSim::new(config.clone(), 3)
                .offset(offset)
                .snr_db(20.0)
                .apply(&audio);
            let got = decode_chunked(config.clone(), &rx, chunk);
            let expect = if muted.len() <= 2 {
                vec![msg.clone()]
            } else {
                vec![]
            };
            if got != expect {
                failures.push(format!("muted {muted:?} offset {offset}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Robust repeat's joint acquisition (1 miss): a copy with any one muted
/// preamble symbol, including the first, still decodes; with two it does
/// not (checked on a single copy, K = 1).
#[test]
fn robust_repeat_joint_acquisition_tolerates_one_missing_preamble_symbol() {
    let config = PhyConfig {
        repetition_k: 1,
        ..PhyConfig::robust_repeat()
    };
    let period = config.samples_per_symbol_period();
    let msg = TransportMessage::NoteOn {
        channel: 3,
        note: 64,
        velocity: 90,
    };
    let packet = CricketModemTx::new(config.clone()).encode_message_with_seq(&msg, 2);
    let muted_sets: [&[usize]; 7] = [&[0], &[1], &[2], &[3], &[0, 1], &[1, 3], &[2, 3]];
    let mut failures = Vec::new();
    for muted in muted_sets {
        let mut audio = packet.clone();
        for &i in muted {
            audio[i * period..(i + 1) * period].fill(0.0);
        }
        for (offset, chunk) in [(0usize, 4096usize), (101, 64), (period / 2, 240)] {
            let rx = ChannelSim::new(config.clone(), 5)
                .offset(offset)
                .snr_db(20.0)
                .apply(&audio);
            let got = decode_chunked(config.clone(), &rx, chunk);
            let expect = if muted.len() <= 1 {
                vec![msg.clone()]
            } else {
                vec![]
            };
            if got != expect {
                failures.push(format!("muted {muted:?} offset {offset}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn loopback_all_message_types() {
    for config in [PhyConfig::robust(), fec_parity(2, 1), fec_parity(4, 2)] {
        let messages = all_types();
        let samples = encode_to_samples(config.clone(), &messages);
        for chunk in [64, 240, 4096] {
            assert_eq!(decode_chunked(config.clone(), &samples, chunk), messages);
        }
    }
}

#[test]
fn sync_offset_sweep() {
    let config = PhyConfig::robust();
    let period = config.samples_per_symbol_period();
    let mut failures = Vec::new();
    for offset in (0..2 * period).step_by(5) {
        let messages = [TransportMessage::NoteOn {
            channel: 0,
            note: (offset % 128) as u8,
            velocity: 1 + (offset % 127) as u8,
        }];
        let clean = encode_to_samples(config.clone(), &messages);
        let rx = ChannelSim::new(config.clone(), 0)
            .offset(offset)
            .apply(&clean);
        let chunk = [64, 128, 240, 512, 4096][offset % 5];
        if decode_chunked(config.clone(), &rx, chunk) != messages {
            failures.push(offset);
        }
    }
    assert!(failures.is_empty(), "failed offsets: {failures:?}");
}

/// Wrong-tone symbols are fixed by the code and counted in the stats.
#[test]
fn corrects_wrong_symbols_and_reports_them() {
    let config = PhyConfig::robust();
    let msg = TransportMessage::Msc(MscMessage::cue(
        MscCommand::Load,
        MscCue::from_tenths(990).unwrap(),
        Some(3),
    ));
    let tx = CricketModemTx::new(config.clone());
    let audio = tx.encode_message_with_seq(&msg, 1);
    // Overwrite body symbols 0..4 (2 per codeword) with a different tone.
    let period = config.samples_per_symbol_period();
    let pre = config.preamble.len();
    let phy = cricket_core::phy::ModemTx::new(config.clone());
    let mut audio = audio;
    for i in 0..4 {
        let at = (pre + i) * period;
        let wrong = phy.synthesize_nibbles(&[((i * 3 + 1) % 16) as u8]);
        audio[at..at + period].copy_from_slice(&wrong[..period]);
    }
    audio.extend(std::iter::repeat_n(0.0, 8 * period));
    let mut rx = CricketModemRx::new(config);
    let mut got = Vec::new();
    let mut corrected = 0;
    for out in rx.push_samples(&audio) {
        match out {
            ModemOutput::Message(m) => got.push(m),
            ModemOutput::Stats(s) => corrected = s.fec_corrected,
            _ => {}
        }
    }
    assert_eq!(got, vec![msg]);
    assert!(corrected >= 1, "fec_corrected = {corrected}");
}

/// Erasure-tolerant body + tolerant preamble must not turn noise or random
/// tone streams into messages (CRC-16 + version/type/pad checks).
#[test]
fn noise_and_random_tones_produce_no_messages() {
    let config = PhyConfig::robust();
    let rate = config.sample_rate_hz as usize;
    let mut rng = XorShift64::new(0x0BAD);
    // 10 s of white noise.
    let noise: Vec<f32> = (0..10 * rate)
        .map(|_| (rng.gaussian() * 0.3) as f32)
        .collect();
    // 10 s of random MFSK symbols with preambles sprinkled in.
    let phy = cricket_core::phy::ModemTx::new(config.clone());
    let mut nibbles = Vec::new();
    while nibbles.len() * config.samples_per_symbol_period() < 10 * rate {
        if rng.below(8) == 0 {
            nibbles.extend_from_slice(&config.preamble);
        }
        nibbles.push(rng.below(16) as u8);
    }
    let tones = phy.synthesize_nibbles(&nibbles);
    for audio in [noise, tones] {
        let got = decode_chunked(config.clone(), &audio, 512);
        assert!(got.is_empty(), "false messages: {got:?}");
    }
}

/// Same trials through both profiles: single-copy FEC must deliver at least
/// as often as two uncoded copies under random wrong-tone symbols.
#[test]
fn fec_k1_at_least_as_robust_as_repetition_k2() {
    let trials = 120;
    let robust = run_cell(
        &PhyConfig::robust_repeat(),
        Impairment::SymErr(0.04),
        trials,
        77,
    );
    let fec = run_cell(&PhyConfig::robust(), Impairment::SymErr(0.04), trials, 77);
    assert!(
        fec.ok >= robust.ok,
        "robust_repeat (K2) {}/{trials}, robust (FEC K1) {}/{trials}",
        robust.ok,
        fec.ok
    );
}

// ---------------------------------------------------------------------------
// A/B measurement

#[derive(Debug, Clone, Copy)]
enum Impairment {
    Clean,
    Snr(f32),
    SymErr(f64),
    /// Dropout length in ms, at SNR 20 dB.
    Dropout(f32),
    /// Gain in dB, at an SNR (dB).
    GainSnr(f32, f32),
    /// One-pole low-pass cutoff (Hz), at an SNR (dB).
    LowpassSnr(f32, f32),
    /// Clock drift (ppm), at an SNR (dB).
    DriftSnr(f64, f32),
}

impl Impairment {
    fn label(&self) -> String {
        match *self {
            Impairment::Clean => "clean".into(),
            Impairment::Snr(s) => format!("AWGN SNR {s} dB"),
            Impairment::SymErr(p) => format!("wrong tone p={:.0}%", p * 100.0),
            Impairment::Dropout(ms) => format!("dropout {ms} ms"),
            Impairment::GainSnr(g, s) => format!("gain {g} dB, SNR {s}"),
            Impairment::LowpassSnr(f, s) => format!("LP {f} Hz, SNR {s}"),
            Impairment::DriftSnr(p, s) => format!("drift {p} ppm, SNR {s}"),
        }
    }

    fn sim(&self, config: &PhyConfig, seed: u64, offset: usize) -> ChannelSim {
        let base = ChannelSim::new(config.clone(), seed).offset(offset);
        let ms = |ms: f32| (ms * config.sample_rate_hz as f32 / 1000.0) as usize;
        match *self {
            Impairment::Clean => base,
            Impairment::Snr(s) => base.snr_db(s),
            Impairment::SymErr(p) => base.symbol_error_prob(p).snr_db(20.0),
            Impairment::Dropout(d) => base.dropout_samples(ms(d)).snr_db(20.0),
            Impairment::GainSnr(g, s) => base.gain(10f32.powf(g / 20.0)).snr_db(s),
            Impairment::LowpassSnr(f, s) => base.lowpass_hz(f).snr_db(s),
            Impairment::DriftSnr(p, s) => base.drift_ppm(p).snr_db(s),
        }
    }
}

struct Cell {
    ok: usize,
    trials: usize,
    /// Mean message start → delivery time over delivered trials (ms).
    mean_latency_ms: f64,
    false_msgs: usize,
    /// Dropout cells only: lost trials whose dropout overlapped the first
    /// copy's preamble, and lost trials whose dropout started inside the
    /// first copy's body.
    lost_preamble_hit: usize,
    lost_body_only: usize,
}

/// `trials` single-message transmissions (random NoteOn / MSC GO, random
/// start offset, 64-sample chunks so delivery time has ~1.3 ms resolution).
fn run_cell(config: &PhyConfig, imp: Impairment, trials: usize, seed: u64) -> Cell {
    let mut rng = XorShift64::new(seed);
    let rate = config.sample_rate_hz as f64;
    let mut ok = 0;
    let mut false_msgs = 0;
    let mut lost_preamble_hit = 0;
    let mut lost_body_only = 0;
    let mut lat = Vec::new();
    let period = config.samples_per_symbol_period();
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
        let offset = 200 + rng.below(4 * config.samples_per_symbol_period());
        let tx = CricketModemTx::new(config.clone());
        let mut clean = tx.encode_message_with_seq(&msg, rng.below(4) as u8);
        clean.extend(std::iter::repeat_n(0.0, config.sample_rate_hz as usize / 4));
        let sim_seed = rng.next_u64();
        let audio = imp.sim(config, sim_seed, offset).apply(&clean);

        let mut rx = CricketModemRx::new(config.clone());
        let mut delivered_at = None;
        for chunk in audio.chunks(64) {
            for out in rx.push_samples(chunk) {
                if let ModemOutput::Message(m) = out {
                    if m == msg && delivered_at.is_none() {
                        delivered_at = Some(rx.samples_processed());
                    } else if m != msg {
                        false_msgs += 1;
                    }
                }
            }
        }
        if let Some(at) = delivered_at {
            ok += 1;
            lat.push((at as f64 - offset as f64) / rate * 1000.0);
        } else if let Impairment::Dropout(ms) = imp {
            // Same draw as `ChannelSim::apply` (first RNG use for a dropout
            // without symbol corruption).
            let d = (ms * config.sample_rate_hz as f32 / 1000.0) as usize;
            let start = XorShift64::new(sim_seed).below(clean.len() - d);
            if start < config.preamble.len() * period {
                lost_preamble_hit += 1;
            } else if (start as u64) < repetition_samples(config) {
                lost_body_only += 1;
            }
        }
    }
    Cell {
        ok,
        trials,
        lost_preamble_hit,
        lost_body_only,
        mean_latency_ms: if lat.is_empty() {
            f64::NAN
        } else {
            lat.iter().sum::<f64>() / lat.len() as f64
        },
        false_msgs,
    }
}

#[test]
#[ignore = "long-running A/B table; run with --release -- --ignored --nocapture"]
fn fec_ab_table() {
    let trials: usize = std::env::var("FEC_AB_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let profiles: Vec<(&str, PhyConfig)> = vec![
        ("robust_repeat K2", PhyConfig::robust_repeat()),
        (
            "robust_repeat K1",
            PhyConfig {
                repetition_k: 1,
                ..PhyConfig::robust_repeat()
            },
        ),
        ("robust (p4 K1)", PhyConfig::robust()),
        ("fec p2 K1", fec_parity(2, 1)),
        ("fec p4 K2", fec_parity(4, 2)),
    ];
    let imps = [
        Impairment::Clean,
        Impairment::Snr(6.0),
        Impairment::Snr(0.0),
        Impairment::Snr(-2.0),
        Impairment::Snr(-3.0),
        Impairment::Snr(-4.0),
        Impairment::SymErr(0.01),
        Impairment::SymErr(0.02),
        Impairment::SymErr(0.05),
        Impairment::SymErr(0.10),
        Impairment::Dropout(10.0),
        Impairment::Dropout(30.0),
        Impairment::Dropout(60.0),
        Impairment::GainSnr(-40.0, 0.0),
        Impairment::LowpassSnr(2500.0, 6.0),
        Impairment::DriftSnr(500.0, 0.0),
    ];

    println!("\nAirtime per message (all copies) and first-copy body end:");
    for (name, c) in &profiles {
        let rep = repetition_samples(c) as f64 / c.sample_rate_hz as f64 * 1000.0;
        println!(
            "  {name:<10} airtime {:6.1} ms  first copy {:6.1} ms",
            rep * c.repetition_k as f64,
            c.estimated_first_copy_ms()
        );
    }

    let results: Vec<Vec<Cell>> = std::thread::scope(|s| {
        let handles: Vec<_> = profiles
            .iter()
            .map(|(_, c)| {
                s.spawn(move || {
                    imps.iter()
                        .enumerate()
                        .map(|(i, imp)| run_cell(c, *imp, trials, 0xFEC0 + i as u64))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    println!("\nDelivery rate % (mean start→delivery ms), {trials} trials per cell:");
    print!("| {:<22} |", "channel");
    for (name, _) in &profiles {
        print!(" {name:>15} |");
    }
    println!();
    print!("|{:-<24}|", "");
    for _ in &profiles {
        print!("{:->17}|", "");
    }
    println!();
    let mut false_total = 0;
    for (i, imp) in imps.iter().enumerate() {
        print!("| {:<22} |", imp.label());
        for r in &results {
            let c = &r[i];
            false_total += c.false_msgs;
            let pct = 100.0 * c.ok as f64 / c.trials as f64;
            print!(" {:>5.1} ({:>5.0} ms) |", pct, c.mean_latency_ms);
        }
        println!();
    }
    println!("\nfalse messages (all cells): {false_total}");
    assert_eq!(false_total, 0);
}

// ---------------------------------------------------------------------------
// Acquisition (preamble detection) measurements

/// Profiles of the acquisition tables (`ACQ_PROFILES`, comma-separated
/// `PhyConfig::from_name` names, optionally with an acquisition override:
/// `name@first` = AML v1 first-symbol acquisition, `name@jointN` = joint
/// acquisition tolerating N misses; a `+pre6` suffix on the name extends
/// the preamble to 6 symbols, `A 5 C 3 8 E`). Default: robust_repeat and
/// robust with first-symbol acquisition (`@first`, as before joint
/// acquisition) and as specified, and fast.
fn acq_profiles() -> Vec<(String, PhyConfig)> {
    let names = std::env::var("ACQ_PROFILES").unwrap_or_else(|_| {
        "robust_repeat@first,robust_repeat,robust@first,robust,fast".to_string()
    });
    names
        .split(',')
        .map(|n| {
            let n = n.trim();
            let (base, acq) = n.split_once('@').unwrap_or((n, ""));
            let (base, pre6) = match base.strip_suffix("+pre6") {
                Some(b) => (b, true),
                None => (base, false),
            };
            let mut config =
                PhyConfig::from_name(base).unwrap_or_else(|| panic!("unknown profile {base}"));
            if pre6 {
                config.preamble = vec![0xA, 0x5, 0xC, 0x3, 0x8, 0xE];
            }
            if acq == "first" {
                config.acquisition = Acquisition::FirstSymbol;
            } else if let Some(m) = acq.strip_prefix("joint") {
                config.acquisition = Acquisition::Joint {
                    misses: m.parse().expect("joint<misses>"),
                };
            }
            (n.to_string(), config)
        })
        .collect()
}

/// Delivery under the channels where acquisition matters, plus the dropout
/// loss split (dropout starting in the preamble vs in the body).
///
/// ```sh
/// cargo test --release -p cricket_core --test fec acquisition_ab_table -- --ignored --nocapture
/// ```
#[test]
#[ignore = "long-running A/B table; run with --release -- --ignored --nocapture"]
fn acquisition_ab_table() {
    let trials: usize = std::env::var("FEC_AB_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let profiles = acq_profiles();
    let imps = [
        Impairment::Clean,
        Impairment::Snr(2.0),
        Impairment::Snr(0.0),
        Impairment::Snr(-2.0),
        Impairment::Snr(-3.0),
        Impairment::Snr(-4.0),
        Impairment::SymErr(0.01),
        Impairment::SymErr(0.02),
        Impairment::SymErr(0.05),
        Impairment::SymErr(0.10),
        Impairment::Dropout(10.0),
        Impairment::Dropout(30.0),
        Impairment::Dropout(60.0),
        Impairment::GainSnr(-40.0, 0.0),
        Impairment::LowpassSnr(2500.0, 6.0),
        Impairment::DriftSnr(500.0, 0.0),
    ];
    // One thread per (profile, impairment) cell.
    let results: Vec<Vec<Cell>> = std::thread::scope(|s| {
        let handles: Vec<Vec<_>> = profiles
            .iter()
            .map(|(_, c)| {
                imps.iter()
                    .enumerate()
                    .map(|(i, imp)| s.spawn(move || run_cell(c, *imp, trials, 0xACC0 + i as u64)))
                    .collect()
            })
            .collect();
        handles
            .into_iter()
            .map(|hs| hs.into_iter().map(|h| h.join().unwrap()).collect())
            .collect()
    });

    println!("\nDelivery rate % (mean start→delivery ms), {trials} trials per cell:");
    print!("| {:<22} |", "channel");
    for (name, _) in &profiles {
        print!(" {name:>15} |");
    }
    println!();
    let mut false_total = 0;
    for (i, imp) in imps.iter().enumerate() {
        print!("| {:<22} |", imp.label());
        for r in &results {
            let c = &r[i];
            false_total += c.false_msgs;
            let pct = 100.0 * c.ok as f64 / c.trials as f64;
            print!(" {:>5.1} ({:>5.0} ms) |", pct, c.mean_latency_ms);
        }
        println!();
    }
    println!("\nDropout losses (dropout starts in first-copy preamble / in its body):");
    for (i, imp) in imps.iter().enumerate() {
        if let Impairment::Dropout(_) = imp {
            print!("  {:<16}", imp.label());
            for ((name, _), r) in profiles.iter().zip(&results) {
                let c = &r[i];
                print!("  {name}: {}/{}", c.lost_preamble_hit, c.lost_body_only);
            }
            println!();
        }
    }
    println!("\nfalse messages (all cells): {false_total}");
    assert_eq!(false_total, 0);
}

/// Music-like test audio: harmonic voices (fundamentals 110–880 Hz, 10
/// harmonics, so partials sweep the 2.5–4.1 kHz tone band), a pure-sine
/// "whistle" melody inside the tone band, vibrato, note envelopes and
/// drum-like noise bursts.
fn music_like(rate: usize, seconds: usize, seed: u64) -> Vec<f32> {
    use std::f32::consts::PI;
    let mut rng = XorShift64::new(seed);
    let n = rate * seconds;
    let mut out = vec![0.0f32; n];
    let midi_hz = |m: f32| 440.0 * 2f32.powf((m - 69.0) / 12.0);
    for voice in 0..4 {
        let whistle = voice == 3;
        let mut t = 0usize;
        let mut phase = 0.0f32;
        while t < n {
            let len = rate * (80 + rng.below(420)) / 1000;
            let note = if whistle {
                (88 + rng.below(16)) as f32 // ~2.6–4.1 kHz
            } else {
                (45 + rng.below(36)) as f32 // 110–880 Hz
            };
            let f0 = midi_hz(note);
            let amp = if whistle { 0.15 } else { 0.08 };
            let vib_rate = 4.0 + rng.next_f64() as f32 * 2.0;
            for i in 0..len.min(n - t) {
                let ts = i as f32 / rate as f32;
                let env = (ts / 0.01).min(1.0) * (-ts * 3.0).exp().max(0.3);
                let f = f0 * (1.0 + 0.005 * (2.0 * PI * vib_rate * ts).sin());
                phase += 2.0 * PI * f / rate as f32;
                if phase > 2.0 * PI {
                    phase -= 2.0 * PI;
                }
                let x = if whistle {
                    phase.sin()
                } else {
                    (1..=10).map(|h| (h as f32 * phase).sin() / h as f32).sum()
                };
                out[t + i] += amp * env * x;
            }
            t += len;
        }
    }
    let mut t = 0;
    while t < n {
        let len = (rate / 20).min(n - t);
        for i in 0..len {
            let env = (-(i as f32) / (rate as f32 * 0.01)).exp();
            out[t + i] += 0.2 * env * rng.gaussian() as f32;
        }
        t += rate * (150 + rng.below(350)) / 1000;
    }
    out
}

/// Phy lock attempts that reach the body (`PreambleLocked`) on audio without
/// packets; every completed body is rejected, as the modem would.
fn phy_locks(config: &PhyConfig, audio: &[f32]) -> usize {
    use cricket_core::phy::{ModemRx, RxEvent};
    let mut rx = ModemRx::new(config.clone());
    rx.set_expected_body_nibbles(config.fec.body_nibbles());
    let mut locks = 0;
    for chunk in audio.chunks(256) {
        let mut events = rx.push_samples(chunk);
        loop {
            locks += events
                .iter()
                .filter(|e| matches!(e, RxEvent::PreambleLocked { .. }))
                .count();
            if !rx.awaiting_verdict() {
                break;
            }
            events = rx.reject();
        }
    }
    locks
}

/// False-trigger rates (phy locks per minute) and receiver CPU (host ns per
/// input sample, best of 3) on 60 s each of white noise, music-like audio
/// and random MFSK symbols (no preambles), and on 200 back-to-back packets
/// at SNR 0 dB. None of the packet-free inputs may produce a message.
///
/// ```sh
/// cargo test --release -p cricket_core --test fec false_trigger_report -- --ignored --nocapture
/// ```
#[test]
#[ignore = "long-running report; run with --release -- --ignored --nocapture"]
fn false_trigger_report() {
    let profiles = acq_profiles();
    let rate = 48_000usize;
    let mut rng = XorShift64::new(0x5EED);
    let noise: Vec<f32> = (0..60 * rate)
        .map(|_| (rng.gaussian() * 0.3) as f32)
        .collect();
    let music = music_like(rate, 60, 0x0A11);
    println!(
        "\n| profile | locks/min noise | music | random tones | false msgs | ns/sample noise | music | random tones | packets 0 dB | delivered |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for (name, config) in &profiles {
        // 60 s of random MFSK symbols (no preambles): other packets' bodies.
        let mut rng = XorShift64::new(0x70E5);
        let symbols = 60 * rate / config.samples_per_symbol_period();
        let nibbles: Vec<u8> = (0..symbols).map(|_| rng.below(16) as u8).collect();
        let tones = cricket_core::phy::ModemTx::new(config.clone()).synthesize_nibbles(&nibbles);
        let locks: Vec<usize> = [&noise, &music, &tones]
            .into_iter()
            .map(|a| phy_locks(config, a))
            .collect();
        let mut tx = CricketModemTx::new(config.clone());
        let mut clean = Vec::new();
        let sent = 200u32;
        for i in 0..sent {
            clean.extend(tx.encode_message(&TransportMessage::NoteOn {
                channel: 0,
                note: (i % 128) as u8,
                velocity: 1 + (i % 127) as u8,
            }));
        }
        let packets = ChannelSim::new(config.clone(), 7).snr_db(0.0).apply(&clean);
        let mut false_msgs = 0;
        let mut delivered = 0;
        let mut ns = Vec::new();
        for (i, audio) in [&noise, &music, &tones, &packets].into_iter().enumerate() {
            let mut best = f64::MAX;
            for _ in 0..3 {
                let mut rx = CricketModemRx::new(config.clone());
                let start = std::time::Instant::now();
                let mut msgs = 0;
                for chunk in audio.chunks(256) {
                    for out in rx.push_samples(chunk) {
                        if let ModemOutput::Message(_) = out {
                            msgs += 1;
                        }
                    }
                }
                best = best.min(start.elapsed().as_nanos() as f64 / audio.len() as f64);
                if i < 3 {
                    false_msgs += msgs;
                } else {
                    delivered = msgs;
                }
            }
            ns.push(best);
        }
        println!(
            "| {name} | {} | {} | {} | {false_msgs} | {:.1} | {:.1} | {:.1} | {:.1} | {delivered}/{sent} |",
            locks[0], locks[1], locks[2], ns[0], ns[1], ns[2], ns[3]
        );
        assert_eq!(false_msgs, 0, "{name}: false messages without packets");
    }
}
