//! Experimental BMC (LTC-style) phy behind the unchanged modem API: sync,
//! message types, channel impairments, noise rejection, fail-safe and dedup.
//! Uses the simulator in `tests/support`. Measurements (A/B tables) live in
//! `phy_ab_measurements.rs`.

mod support;

use cricket_core::modem::{
    decode_samples, encode_to_samples, CricketModemRx, CricketModemTx, ModemOutput,
};
use cricket_core::phy::{Modulation, PhyConfig};
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage};
use support::{decode_chunked, ChannelSim, XorShift64};

const CHUNK_SIZES: [usize; 5] = [64, 128, 240, 512, 4096];

fn note_on_for(i: usize) -> TransportMessage {
    TransportMessage::NoteOn {
        channel: 0,
        note: (i % 128) as u8,
        velocity: (1 + (i * 7) % 127) as u8,
    }
}

fn ltc_profiles() -> Vec<(&'static str, PhyConfig)> {
    vec![
        ("ltc2400", PhyConfig::ltc_with_bit_rate(2400)),
        ("ltc4800", PhyConfig::ltc()),
        ("ltc9600", PhyConfig::ltc_with_bit_rate(9600)),
        ("ltc14400", PhyConfig::from_name("ltc14400")),
    ]
}

/// Single NoteOns through `impair` with random offsets and chunk sizes;
/// returns the number of exact decodes.
fn trials(
    config: &PhyConfig,
    n: usize,
    seed: u64,
    impair: impl Fn(ChannelSim) -> ChannelSim,
) -> usize {
    let mut rng = XorShift64::new(seed);
    let mut ok = 0;
    for trial in 0..n {
        let offset = rng.below(400);
        let chunk = CHUNK_SIZES[rng.below(CHUNK_SIZES.len())];
        let messages = [note_on_for(trial)];
        let clean = encode_to_samples(config.clone(), &messages);
        let sim = impair(ChannelSim::new(config.clone(), rng.next_u64()).offset(offset));
        if decode_chunked(config.clone(), &sim.apply(&clean), chunk) == messages {
            ok += 1;
        }
    }
    ok
}

#[test]
fn profile_selection() {
    assert!(matches!(PhyConfig::robust().modulation, Modulation::Mfsk));
    assert!(matches!(
        PhyConfig::robust_repeat().modulation,
        Modulation::Mfsk
    ));
    assert!(matches!(PhyConfig::fast().modulation, Modulation::Mfsk));
    assert!(matches!(
        PhyConfig::from_name("robust").modulation,
        Modulation::Mfsk
    ));
    for name in ["ltc", "LTC", "ltc4800"] {
        match PhyConfig::from_name(name).modulation {
            Modulation::Bmc(p) => assert_eq!(p.bit_rate, 4800),
            Modulation::Mfsk => panic!("{name} must select BMC"),
        }
    }
    // Burst for a max-size body at 4800 bit/s: 8 + 3 + 64 + 1 + 0.5 bits.
    let ms = PhyConfig::ltc().estimated_first_copy_ms();
    assert!((ms - 15.9).abs() < 0.1, "{ms}");
}

#[test]
fn all_message_types_roundtrip() {
    let messages = vec![
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
            MscCue::from_tenths(120).unwrap(),
            Some(3),
        )),
        TransportMessage::Heartbeat,
        TransportMessage::Panic,
    ];
    for (name, config) in ltc_profiles() {
        let samples = encode_to_samples(config.clone(), &messages);
        assert_eq!(decode_samples(config, &samples), messages, "{name}");
    }
}

#[test]
fn sync_offset_chunk_sweep() {
    let config = PhyConfig::ltc();
    let mut failures = Vec::new();
    for &chunk in &CHUNK_SIZES {
        for offset in 0..97 {
            let messages = [
                note_on_for(offset),
                TransportMessage::NoteOff {
                    channel: 0,
                    note: (offset % 128) as u8,
                    velocity: 64,
                },
            ];
            let clean = encode_to_samples(config.clone(), &messages);
            let rx = ChannelSim::new(config.clone(), 0)
                .offset(offset)
                .apply(&clean);
            let got = decode_chunked(config.clone(), &rx, chunk);
            if got != messages {
                failures.push(format!("offset {offset} chunk {chunk}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn other_sample_rates() {
    // Non-integer half cells (44.1 kHz: 4.59 samples at 4800 bit/s).
    for rate in [44_100, 96_000] {
        for (name, config) in ltc_profiles() {
            let config = config.with_sample_rate(rate);
            let messages: Vec<_> = (0..4).map(note_on_for).collect();
            let samples = encode_to_samples(config.clone(), &messages);
            assert_eq!(
                decode_samples(config, &samples),
                messages,
                "{name} @ {rate}"
            );
        }
    }
}

#[test]
fn impairments_4800() {
    type Impair = fn(ChannelSim) -> ChannelSim;
    let cases: [(&str, Impair); 9] = [
        ("SNR 12 dB", |s| s.snr_db(12.0)),
        ("level -50 dB", |s| s.gain(0.00316)),
        ("inverted + SNR 15", |s| s.invert().snr_db(15.0)),
        ("drift +1000 ppm", |s| s.drift_ppm(1000.0)),
        ("drift -1 %", |s| s.drift_ppm(-10_000.0)),
        ("HP 300 Hz", |s| s.highpass_hz(300.0)),
        ("2-pole LP 6 kHz", |s| s.lowpass2_hz(6_000.0)),
        ("soft clip", |s| s.soft_clip(4.0)),
        ("hum 50 Hz -6 dB", |s| s.hum(50.0, -6.0)),
    ];
    let config = PhyConfig::ltc();
    for (i, (name, impair)) in cases.iter().enumerate() {
        let n = 40;
        let ok = trials(&config, n, 0xB3C0 + i as u64, impair);
        assert_eq!(ok, n, "{name}: {ok}/{n}");
    }
}

#[test]
fn noise_and_silence_never_decode() {
    for (name, config) in ltc_profiles() {
        let mut rng = XorShift64::new(0x5EED);
        for sigma in [1e-4f64, 0.05, 0.5] {
            let noise: Vec<f32> = (0..48_000 * 3)
                .map(|_| (rng.gaussian() * sigma) as f32)
                .collect();
            let got = decode_chunked(config.clone(), &noise, 512);
            assert!(got.is_empty(), "{name} sigma {sigma}: {got:?}");
        }
    }
}

#[test]
fn repetitions_are_deduplicated() {
    let mut config = PhyConfig::ltc();
    config.repetition_k = 3;
    let messages: Vec<_> = (0..3).map(note_on_for).collect();
    let samples = encode_to_samples(config.clone(), &messages);
    assert_eq!(decode_samples(config, &samples), messages);
}

#[test]
fn link_dead_fires_after_silence() {
    let config = PhyConfig::ltc();
    let mut tx = CricketModemTx::new(config.clone());
    let mut rx = CricketModemRx::new(config.clone());
    let mut audio = tx.encode_heartbeat();
    let packet_end = audio.len();
    audio.resize(packet_end + 48_000 * 3, 0.0);
    let mut dead_at = None;
    let mut heartbeat = false;
    for (i, chunk) in audio.chunks(256).enumerate() {
        for out in rx.push_samples(chunk) {
            match out {
                ModemOutput::Message(TransportMessage::Heartbeat) => heartbeat = true,
                ModemOutput::LinkDead => dead_at = Some(i * 256),
                _ => {}
            }
        }
    }
    assert!(heartbeat);
    let dead_at = dead_at.expect("link-dead after 2 s of silence");
    let fired = rx.last_link_dead_sample().unwrap() as usize;
    // 2000 ms after the end of the packet (decoded on its terminating edge).
    let expected = packet_end + 96_000;
    assert!(
        fired.abs_diff(expected) < 200,
        "fired {fired} expected ~{expected}"
    );
    assert!(dead_at <= fired);
}

/// Auto-detect feasibility: MFSK audio never decodes on a BMC receiver and
/// vice versa, so a receiver can run both phys in parallel on one input and
/// take whatever passes CRC.
#[test]
fn mfsk_and_bmc_do_not_cross_decode() {
    let messages: Vec<_> = (0..6).map(note_on_for).collect();
    let mfsk = [
        PhyConfig::robust_repeat(),
        PhyConfig::fast(),
        PhyConfig::robust(),
    ];
    let mut bmc_profiles = ltc_profiles();
    bmc_profiles.extend([
        ("ltc2400_fec", PhyConfig::ltc_fec_with_bit_rate(2400)),
        ("ltc_fec", PhyConfig::ltc_fec()),
        ("ltc9600_fec", PhyConfig::ltc_fec_with_bit_rate(9600)),
        ("ltc14400_fec", PhyConfig::from_name("ltc14400_fec")),
    ]);
    for (name, bmc) in bmc_profiles {
        for m in &mfsk {
            let mfsk_audio = encode_to_samples(m.clone(), &messages);
            assert!(
                decode_samples(bmc.clone(), &mfsk_audio).is_empty(),
                "{name}"
            );
            let bmc_audio = encode_to_samples(bmc.clone(), &messages);
            assert!(decode_samples(m.clone(), &bmc_audio).is_empty(), "{name}");
        }
    }

    // One stream carrying both: each receiver gets exactly its own messages.
    for ltc in [PhyConfig::ltc(), PhyConfig::ltc_fec()] {
        let fast = PhyConfig::fast();
        let (a, b, c) = ([note_on_for(1)], [note_on_for(2)], [note_on_for(3)]);
        let mut stream = encode_to_samples(fast.clone(), &a);
        stream.extend(encode_to_samples(ltc.clone(), &b));
        stream.extend(encode_to_samples(fast.clone(), &c));
        assert_eq!(decode_samples(fast, &stream), [a[0].clone(), c[0].clone()]);
        assert_eq!(decode_samples(ltc, &stream), b);
    }
}
