//! Experimental LTC (BMC) profiles with Reed–Solomon body FEC (`ltc_fec`,
//! `ltc2400_fec`, `ltc9600_fec`, `ltc14400_fec`) and the soft-decision BMC receiver they
//! rely on: loopback, sync sweep, erasure repair, false-lock recovery and
//! noise rejection. Measurements live in `ltc_fec_ab.rs`.

mod support;

use cricket_core::link::FecMode;
use cricket_core::modem::{
    encode_to_samples, repetition_samples, CricketModemRx, CricketModemTx, ModemOutput,
};
use cricket_core::phy::{Modulation, PhyConfig};
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage};
use support::{decode_chunked, ChannelSim, XorShift64};

fn fec_profiles() -> Vec<(&'static str, PhyConfig)> {
    vec![
        ("ltc2400_fec", PhyConfig::from_name("ltc2400_fec")),
        ("ltc_fec", PhyConfig::ltc_fec()),
        ("ltc9600_fec", PhyConfig::from_name("ltc9600_fec")),
        ("ltc14400_fec", PhyConfig::from_name("ltc14400_fec")),
    ]
}

fn all_profiles() -> Vec<(&'static str, PhyConfig)> {
    let mut v = vec![
        ("ltc2400", PhyConfig::ltc_with_bit_rate(2400)),
        ("ltc", PhyConfig::ltc()),
        ("ltc9600", PhyConfig::ltc_with_bit_rate(9600)),
        ("ltc14400", PhyConfig::from_name("ltc14400")),
    ];
    v.extend(fec_profiles());
    v
}

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

fn half_cell(config: &PhyConfig) -> f32 {
    match &config.modulation {
        Modulation::Bmc(p) => p.half_cell_samples(config.sample_rate_hz),
        _ => panic!("BMC profile expected"),
    }
}

#[test]
fn profile_selection() {
    let fec = PhyConfig::ltc_fec();
    assert_eq!(fec.fec, FecMode::Rs16 { parity: 4 });
    assert_eq!(fec.fec.body_nibbles(), 22);
    assert_eq!(fec.repetition_k, 1);
    for (name, rate) in [
        ("ltc_fec", 4800),
        ("LTC-FEC", 4800),
        ("ltc4800_fec", 4800),
        ("ltc2400_fec", 2400),
        ("ltc9600_fec", 9600),
        ("ltc14400_fec", 14400),
    ] {
        let c = PhyConfig::from_name(name);
        assert_eq!(c.fec, FecMode::Rs16 { parity: 4 }, "{name}");
        match c.modulation {
            Modulation::Bmc(p) => assert_eq!(p.bit_rate, rate, "{name}"),
            _ => panic!("{name} must select BMC"),
        }
    }
    // The uncoded LTC profiles are unchanged.
    assert_eq!(PhyConfig::ltc().fec, FecMode::None);
    // Burst at 4800 bit/s: 8 + 3 + 88 + 1 + 0.5 bits = 20.9 ms (ltc: 15.9).
    assert!((fec.estimated_first_copy_ms() - 20.9).abs() < 0.05);
    assert!(repetition_samples(&fec) > repetition_samples(&PhyConfig::ltc()));
}

#[test]
fn loopback_all_message_types() {
    for (name, config) in fec_profiles() {
        let messages = all_types();
        let samples = encode_to_samples(config.clone(), &messages);
        for chunk in [64, 240, 4096] {
            assert_eq!(
                decode_chunked(config.clone(), &samples, chunk),
                messages,
                "{name} chunk {chunk}"
            );
        }
    }
}

#[test]
fn sync_offset_chunk_sweep() {
    let config = PhyConfig::ltc_fec();
    let mut failures = Vec::new();
    for offset in 0..97 {
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

/// A 1 ms dropout anywhere in the body is filled in by the code (and
/// reported in the stats); the uncoded profile loses those packets.
#[test]
fn dropout_in_body_is_repaired() {
    let msg = TransportMessage::Msc(MscMessage::cue(
        MscCommand::Load,
        MscCue::from_tenths(990).unwrap(),
        Some(3),
    ));
    let run = |config: &PhyConfig| -> (usize, usize, u64) {
        let half = half_cell(config);
        let tx = CricketModemTx::new(config.clone());
        let clean = tx.encode_message_with_seq(&msg, 1);
        let body_start = (22.0 * half) as usize;
        let body_len = (8.0 * config.fec.body_nibbles() as f32 * half) as usize;
        let drop = config.sample_rate_hz as usize / 1000;
        let (mut ok, mut n, mut corrected) = (0, 0, 0);
        for start in (body_start..body_start + body_len - drop).step_by(drop / 3) {
            let mut audio = vec![0.0; 300];
            audio.extend_from_slice(&clean);
            audio[300 + start..300 + start + drop].fill(0.0);
            audio.extend(vec![0.0; 2000]);
            let mut rx = CricketModemRx::new(config.clone());
            n += 1;
            for out in rx.push_samples(&audio) {
                match out {
                    ModemOutput::Message(m) if m == msg => ok += 1,
                    ModemOutput::Stats(s) => corrected = corrected.max(s.fec_corrected),
                    _ => {}
                }
            }
        }
        (ok, n, corrected)
    };
    let (ok, n, corrected) = run(&PhyConfig::ltc_fec());
    assert_eq!(ok, n, "ltc_fec repaired {ok}/{n}");
    assert!(corrected >= 1);
    let (ok, n, _) = run(&PhyConfig::ltc());
    assert!(ok < n / 2, "ltc without FEC decoded {ok}/{n}");
}

/// A truncated packet (preamble, sync and a few body nibbles) right before a
/// real one: the false lock reads into the real packet, but the real sync
/// replaces it, so the packet is decoded for every overlap (the BMC
/// counterpart of the MFSK rewind after a failed body).
#[test]
fn false_lock_does_not_hide_packet() {
    let mut failures = Vec::new();
    let mut total = 0;
    for (name, config) in all_profiles() {
        let half = half_cell(&config);
        let body = config.fec.body_nibbles();
        let tx = CricketModemTx::new(config.clone());
        let spurious = tx.encode_message_with_seq(
            &TransportMessage::NoteOn {
                channel: 0,
                note: 21,
                velocity: 7,
            },
            2,
        );
        let msg = TransportMessage::NoteOn {
            channel: 0,
            note: 64,
            velocity: 99,
        };
        let packet = tx.encode_message_with_seq(&msg, 3);
        // With more of its body kept the spurious packet decodes itself
        // (padding / FEC) and legitimately owns the overlap.
        for kept in 0..body - 6 {
            for (lead, chunk) in [(0usize, 4096usize), (37, 64), (3, 240)] {
                total += 1;
                let cut = ((22 + 8 * kept) as f32 * half) as usize;
                let mut audio = vec![0.0; lead + 500];
                audio.extend_from_slice(&spurious[..cut]);
                audio.extend_from_slice(&packet);
                let got = decode_chunked(config.clone(), &audio, chunk);
                if got != [msg.clone()] {
                    failures.push(format!(
                        "[{name}] kept {kept} lead {lead} chunk {chunk}: got {got:?}"
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}/{total} cases lost the packet:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// White noise and MFSK traffic never decode on the FEC profiles (the
/// erasure-tolerant decoder still needs CRC-16 + version/type/pad checks).
#[test]
fn noise_and_mfsk_produce_no_messages() {
    let mut rng = XorShift64::new(0x17C);
    let noise: Vec<f32> = (0..48_000 * 5)
        .map(|_| (rng.gaussian() * 0.2) as f32)
        .collect();
    let notes: Vec<_> = (0..12)
        .map(|i| TransportMessage::NoteOn {
            channel: 0,
            note: i * 9,
            velocity: 1 + i * 5,
        })
        .collect();
    let mut mfsk = encode_to_samples(PhyConfig::fast(), &notes);
    mfsk.extend(encode_to_samples(PhyConfig::robust(), &notes[..4]));
    for (name, config) in fec_profiles() {
        for audio in [&noise, &mfsk] {
            let got = decode_chunked(config.clone(), audio, 512);
            assert!(got.is_empty(), "{name}: {got:?}");
        }
        // And the other way round: MFSK receivers ignore the BMC audio.
        let bmc = encode_to_samples(config.clone(), &notes);
        for m in [PhyConfig::fast(), PhyConfig::robust()] {
            assert!(decode_chunked(m, &bmc, 512).is_empty(), "{name}");
        }
    }
}
