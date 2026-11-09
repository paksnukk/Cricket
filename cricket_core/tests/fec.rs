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
use support::{decode_chunked, ChannelSim, Dynamics, Program, XorShift64};

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
    /// Any other channel: label and the impairments to apply.
    Custom(&'static str, fn(ChannelSim) -> ChannelSim),
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
            Impairment::Custom(label, _) => label.into(),
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
            Impairment::Custom(_, f) => f(base),
        }
    }
}

/// Dropout of `ms` at SNR 20 dB (for `Custom` cells; the `Dropout` variant
/// also reports where the dropout fell).
fn burst(s: ChannelSim, block_ms: f32, blocks: usize, per_second: f32) -> ChannelSim {
    s.burst_loss(block_ms, blocks, per_second).snr_db(20.0)
}

/// The MFSK training cells: the §4/§10 channels plus the impairments added
/// for the receiver sprint (hum, program bleed, pink noise, dynamics, bursty
/// packet loss, converters, codecs, band limits, sample-rate error).
/// Thresholds are tuned on these; `HELD_OUT` is for the final report only.
const TRAINING: &[Impairment] = &[
    Impairment::Clean,
    Impairment::Snr(2.0),
    Impairment::Snr(0.0),
    Impairment::Snr(-2.0),
    Impairment::Snr(-3.0),
    Impairment::Snr(-4.0),
    Impairment::Snr(-6.0),
    Impairment::SymErr(0.02),
    Impairment::SymErr(0.05),
    Impairment::Dropout(10.0),
    Impairment::Dropout(30.0),
    Impairment::Dropout(60.0),
    Impairment::Custom("hum 50 Hz @ 0 dB, SNR 20", |s| {
        s.hum(50.0, 0.0).snr_db(20.0)
    }),
    Impairment::Custom("hum 100 Hz @ +6 dB, SNR 20", |s| {
        s.hum(100.0, 6.0).snr_db(20.0)
    }),
    Impairment::Custom("speech bleed -20 dB, SNR 20", |s| {
        s.program(Program::Speech, -20.0).snr_db(20.0)
    }),
    Impairment::Custom("speech bleed -30 dB, SNR 20", |s| {
        s.program(Program::Speech, -30.0).snr_db(20.0)
    }),
    Impairment::Custom("music bleed -20 dB, SNR 20", |s| {
        s.program(Program::Music, -20.0).snr_db(20.0)
    }),
    Impairment::Custom("music bleed -30 dB, SNR 20", |s| {
        s.program(Program::Music, -30.0).snr_db(20.0)
    }),
    Impairment::Custom("pink SNR -3 dB", |s| s.pink_snr_db(-3.0)),
    Impairment::Custom("pink SNR -6 dB", |s| s.pink_snr_db(-6.0)),
    Impairment::Custom("pink SNR -9 dB", |s| s.pink_snr_db(-9.0)),
    Impairment::Custom("gate -40 dBFS 5/50/100 ms, SNR 20", |s| {
        s.gate(-40.0, 5.0, 50.0, 100.0).snr_db(20.0)
    }),
    Impairment::Custom("gate -30 dBFS 20/20/50 ms, SNR 20", |s| {
        s.gate(-30.0, 20.0, 20.0, 50.0).snr_db(20.0)
    }),
    Impairment::Custom("comp -30 dBFS 4:1 5/100 ms, SNR 20", |s| {
        s.compressor(Dynamics::compressor(-30.0, 4.0, 5.0, 100.0))
            .snr_db(20.0)
    }),
    Impairment::Custom("limiter -20 dBFS, SNR 20", |s| {
        s.limiter(Dynamics::limiter(-20.0, 50.0)).snr_db(20.0)
    }),
    Impairment::Custom("burst 3 x 1 ms, 10/s, SNR 20", |s| burst(s, 1.0, 3, 10.0)),
    Impairment::Custom("burst 2 x 5 ms, 5/s, SNR 20", |s| burst(s, 5.0, 2, 5.0)),
    Impairment::Custom("3 steep converters, SNR 6", |s| {
        s.converters(3, false).snr_db(6.0)
    }),
    Impairment::Custom("2 slow-rolloff converters, SNR 6", |s| {
        s.converters(2, true).snr_db(6.0)
    }),
    Impairment::Custom("16 kHz codec, SNR 6", |s| s.codec_16k().snr_db(6.0)),
    Impairment::Custom("2-pole LP 3.4 kHz, SNR 6", |s| {
        s.lowpass2_hz(3_400.0).snr_db(6.0)
    }),
    Impairment::Custom("2-pole LP 5 kHz, SNR 6", |s| {
        s.lowpass2_hz(5_000.0).snr_db(6.0)
    }),
    Impairment::Custom("2-pole LP 6 kHz, SNR 6", |s| {
        s.lowpass2_hz(6_000.0).snr_db(6.0)
    }),
    Impairment::DriftSnr(1000.0, 6.0),
    Impairment::DriftSnr(5000.0, 6.0),
    Impairment::DriftSnr(10000.0, 6.0),
    Impairment::DriftSnr(-10000.0, 6.0),
    Impairment::DriftSnr(20000.0, 6.0),
];

/// Held-out cells: other parameter values, other combinations and other
/// seeds (`HELD_OUT_SEED`). Never used for tuning; reported once per
/// change in the CHANGELOG / proposal.
const HELD_OUT: &[Impairment] = &[
    Impairment::Snr(-1.0),
    Impairment::Snr(-2.5),
    Impairment::Snr(-3.5),
    Impairment::Snr(-5.0),
    Impairment::SymErr(0.03),
    Impairment::Dropout(20.0),
    Impairment::Dropout(45.0),
    Impairment::Custom("hum 60 Hz @ +3 dB, SNR 15", |s| {
        s.hum(60.0, 3.0).snr_db(15.0)
    }),
    Impairment::Custom("hum 150 Hz @ 0 dB, SNR 10", |s| {
        s.hum(150.0, 0.0).snr_db(10.0)
    }),
    Impairment::Custom("speech bleed -25 dB, SNR 15", |s| {
        s.program(Program::Speech, -25.0).snr_db(15.0)
    }),
    Impairment::Custom("music bleed -25 dB, SNR 15", |s| {
        s.program(Program::Music, -25.0).snr_db(15.0)
    }),
    Impairment::Custom("speech -40 dB + pink SNR -6", |s| {
        s.program(Program::Speech, -40.0).pink_snr_db(-6.0)
    }),
    Impairment::Custom("pink SNR -4.5 dB", |s| s.pink_snr_db(-4.5)),
    Impairment::Custom("pink SNR -7.5 dB", |s| s.pink_snr_db(-7.5)),
    Impairment::Custom("gate -35 dBFS 10/30/200 ms, SNR 15", |s| {
        s.gate(-35.0, 10.0, 30.0, 200.0).snr_db(15.0)
    }),
    Impairment::Custom("comp -25 dBFS 8:1 2/50 ms, SNR 15", |s| {
        s.compressor(Dynamics::compressor(-25.0, 8.0, 2.0, 50.0))
            .snr_db(15.0)
    }),
    Impairment::Custom("limiter -16 dBFS, SNR 15", |s| {
        s.limiter(Dynamics::limiter(-16.0, 100.0)).snr_db(15.0)
    }),
    Impairment::Custom("burst 5 x 1 ms, 8/s, SNR 20", |s| burst(s, 1.0, 5, 8.0)),
    Impairment::Custom("burst 1 x 5 ms, 10/s, SNR 20", |s| burst(s, 5.0, 1, 10.0)),
    Impairment::Custom("4 steep converters, SNR 3", |s| {
        s.converters(4, false).snr_db(3.0)
    }),
    Impairment::Custom("slow converter + 16 kHz codec, SNR 3", |s| {
        s.converters(1, true).codec_16k().snr_db(3.0)
    }),
    Impairment::Custom("2-pole LP 4 kHz, SNR 3", |s| {
        s.lowpass2_hz(4_000.0).snr_db(3.0)
    }),
    Impairment::Custom("2-pole LP 5.5 kHz, SNR 3", |s| {
        s.lowpass2_hz(5_500.0).snr_db(3.0)
    }),
    Impairment::Custom("1-pole LP 3 kHz, SNR 6", |s| {
        s.lowpass_hz(3_000.0).snr_db(6.0)
    }),
    Impairment::Custom("HP 300 Hz + EQ -12 dB @ 3 kHz, SNR 6", |s| {
        s.highpass_hz(300.0)
            .peaking_eq(3_000.0, -12.0, 1.0)
            .snr_db(6.0)
    }),
    Impairment::DriftSnr(7000.0, 3.0),
    Impairment::DriftSnr(-15000.0, 6.0),
    Impairment::Custom("speech -20 dB + drift 5000 ppm, SNR 10", |s| {
        s.program(Program::Speech, -20.0)
            .drift_ppm(5000.0)
            .snr_db(10.0)
    }),
];

/// Seed bases: the training tables use `0xFEC0` / `0xACC0` / `TRAINING_SEED`
/// + cell index, the held-out table `HELD_OUT_SEED` + cell index.
const TRAINING_SEED: u64 = 0x7A10;
const HELD_OUT_SEED: u64 = 0x4E1D_0000;

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

/// Lowest AWGN SNR (dB, 0.5 dB grid in [-12, 6]) at which `config` delivers
/// at least 95 % of `trials` messages: the profile's "cliff". Bisection,
/// assuming delivery is monotonic in SNR; `NAN` if 95 % is not reached at
/// 6 dB. The seed is fixed and the same for every profile.
fn cliff95_db(config: &PhyConfig, trials: usize) -> f32 {
    let ok_at = |snr: f32| {
        let c = run_cell(config, Impairment::Snr(snr), trials, 0xC11F);
        c.ok as f64 >= 0.95 * c.trials as f64
    };
    let (mut lo, mut hi) = (-24i32, 12i32); // half-dB units
    if !ok_at(hi as f32 * 0.5) {
        return f32::NAN;
    }
    // Invariant: hi passes; lo fails (or is the floor).
    if ok_at(lo as f32 * 0.5) {
        return lo as f32 * 0.5;
    }
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if ok_at(mid as f32 * 0.5) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi as f32 * 0.5
}

/// Print a delivery table (`results[profile][cell]`), then the "distance
/// to cliff" rows: each profile's 95 % AWGN cliff and the margin a 0 dB
/// line has to it. Returns the number of false messages over all cells.
fn print_delivery_table(
    profiles: &[(String, PhyConfig)],
    imps: &[Impairment],
    results: &[Vec<Cell>],
    trials: usize,
) -> usize {
    println!("\nDelivery rate % (mean start→delivery ms), {trials} trials per cell:");
    print!("| {:<36} |", "channel");
    for (name, _) in profiles {
        print!(" {name:>17} |");
    }
    println!();
    print!("|{:-<38}|", "");
    for _ in profiles {
        print!("{:->19}|", "");
    }
    println!();
    let mut false_total = 0;
    for (i, imp) in imps.iter().enumerate() {
        print!("| {:<36} |", imp.label());
        for r in results {
            let c = &r[i];
            false_total += c.false_msgs;
            let pct = 100.0 * c.ok as f64 / c.trials as f64;
            print!(" {:>7.1} ({:>5.0} ms) |", pct, c.mean_latency_ms);
        }
        println!();
    }
    let cliffs: Vec<f32> = std::thread::scope(|s| {
        let handles: Vec<_> = profiles
            .iter()
            .map(|(_, c)| s.spawn(move || cliff95_db(c, trials)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    print!("| {:<36} |", "AWGN cliff (95 % delivery), dB");
    for c in &cliffs {
        print!(" {:>17} |", format!("{c:+.1}"));
    }
    println!();
    print!("| {:<36} |", "distance to cliff from 0 dB, dB");
    for c in &cliffs {
        print!(" {:>17} |", format!("{:+.1}", -c));
    }
    println!();
    false_total
}

/// `results[profile][cell]`, one thread per cell.
fn run_table(
    profiles: &[(String, PhyConfig)],
    imps: &[Impairment],
    trials: usize,
    seed_base: u64,
) -> Vec<Vec<Cell>> {
    std::thread::scope(|s| {
        let handles: Vec<Vec<_>> = profiles
            .iter()
            .map(|(_, c)| {
                imps.iter()
                    .enumerate()
                    .map(|(i, imp)| {
                        s.spawn(move || run_cell(c, *imp, trials, seed_base + i as u64))
                    })
                    .collect()
            })
            .collect();
        handles
            .into_iter()
            .map(|hs| hs.into_iter().map(|h| h.join().unwrap()).collect())
            .collect()
    })
}

fn env_trials() -> usize {
    std::env::var("FEC_AB_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400)
}

/// Profiles of the MFSK tables (`MFSK_PROFILES`, same syntax as
/// `ACQ_PROFILES`; default `robust_repeat,robust,fast`).
fn mfsk_profiles() -> Vec<(String, PhyConfig)> {
    parse_profiles(
        &std::env::var("MFSK_PROFILES").unwrap_or_else(|_| "robust_repeat,robust,fast".to_string()),
    )
}

/// The MFSK receiver sprint's training table (`TRAINING` cells).
///
/// ```sh
/// cargo test --release -p cricket_core --test fec mfsk_ab_table -- --ignored --nocapture
/// ```
#[test]
#[ignore = "long-running A/B table; run with --release -- --ignored --nocapture"]
fn mfsk_ab_table() {
    let trials = env_trials();
    let profiles = mfsk_profiles();
    let results = run_table(&profiles, TRAINING, trials, TRAINING_SEED);
    let false_total = print_delivery_table(&profiles, TRAINING, &results, trials);
    println!("\nfalse messages (all cells): {false_total}");
    assert_eq!(false_total, 0);
}

/// The held-out table (`HELD_OUT` cells, `HELD_OUT_SEED`): final reporting
/// only, never for tuning.
///
/// ```sh
/// cargo test --release -p cricket_core --test fec mfsk_held_out_table -- --ignored --nocapture
/// ```
#[test]
#[ignore = "long-running A/B table; run with --release -- --ignored --nocapture"]
fn mfsk_held_out_table() {
    let trials = env_trials();
    let profiles = mfsk_profiles();
    let results = run_table(&profiles, HELD_OUT, trials, HELD_OUT_SEED);
    let false_total = print_delivery_table(&profiles, HELD_OUT, &results, trials);
    println!("\nfalse messages (all cells): {false_total}");
    assert_eq!(false_total, 0);
}

#[test]
#[ignore = "long-running A/B table; run with --release -- --ignored --nocapture"]
fn fec_ab_table() {
    let trials = env_trials();
    let profiles: Vec<(String, PhyConfig)> = vec![
        ("robust_repeat K2".into(), PhyConfig::robust_repeat()),
        (
            "robust_repeat K1".into(),
            PhyConfig {
                repetition_k: 1,
                ..PhyConfig::robust_repeat()
            },
        ),
        ("robust (p4 K1)".into(), PhyConfig::robust()),
        ("fec p2 K1".into(), fec_parity(2, 1)),
        ("fec p4 K2".into(), fec_parity(4, 2)),
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

    let results = run_table(&profiles, &imps, trials, 0xFEC0);
    let false_total = print_delivery_table(&profiles, &imps, &results, trials);
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
    parse_profiles(&std::env::var("ACQ_PROFILES").unwrap_or_else(|_| {
        "robust_repeat@first,robust_repeat,robust@first,robust,fast".to_string()
    }))
}

/// Receiver-threshold overrides for tuning runs (every MFSK profile of a
/// table): `MFSK_PURITY` (min tone purity), `MFSK_CONFIDENCE` (dB),
/// `MFSK_BAND` (`lo,hi` Hz, or `none` for no input band-pass), `MFSK_TRACK`
/// (`0` disables body symbol clock tracking), `MFSK_STEP` / `MFSK_BASE`
/// (tone plan of both ends, see below).
fn apply_rx_overrides(config: &mut PhyConfig) {
    if config.n_tones == 0 {
        return;
    }
    if let Some(p) = std::env::var("MFSK_PURITY")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.min_tone_purity = p;
    }
    if let Some(c) = std::env::var("MFSK_CONFIDENCE")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.min_confidence_db = c;
    }
    if let Ok(t) = std::env::var("MFSK_TRACK") {
        config.rx_track_timing = t != "0";
    }
    // `MFSK_SYMBOL_MS`: symbol duration of both ends (ramps scaled with it).
    if let Some(ms) = std::env::var("MFSK_SYMBOL_MS")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        let scale = ms / config.symbol_duration_ms;
        config.symbol_duration_ms = ms;
        config.attack_ms *= scale;
        config.release_ms *= scale;
    }
    // Tone plan (both ends): `MFSK_STEP=ortho` sets the step to 1 / symbol
    // duration (orthogonal tones), a number sets it in Hz; `MFSK_BASE` the
    // lowest tone in Hz. The receiver band follows the plan.
    let mut plan_changed = false;
    if let Ok(step) = std::env::var("MFSK_STEP") {
        config.tone_step_hz = if step == "ortho" {
            1000.0 / config.symbol_duration_ms
        } else {
            step.parse().expect("MFSK_STEP=ortho|<hz>")
        };
        plan_changed = true;
    }
    if let Some(base) = std::env::var("MFSK_BASE").ok().and_then(|v| v.parse().ok()) {
        config.tone_base_hz = base;
        plan_changed = true;
    }
    if plan_changed && config.rx_band_hz.is_some() {
        config.rx_band_hz = config.default_rx_band_hz();
    }
    if let Ok(b) = std::env::var("MFSK_BAND") {
        config.rx_band_hz = if b == "none" {
            None
        } else {
            let (lo, hi) = b.split_once(',').expect("MFSK_BAND=lo,hi");
            Some((lo.parse().unwrap(), hi.parse().unwrap()))
        };
    }
}

/// See [`acq_profiles`] for the syntax.
fn parse_profiles(names: &str) -> Vec<(String, PhyConfig)> {
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
            apply_rx_overrides(&mut config);
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
    let trials = env_trials();
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
    let results = run_table(&profiles, &imps, trials, 0xACC0);
    let false_total = print_delivery_table(&profiles, &imps, &results, trials);
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
