//! Fast LTC (BMC) profiles for clean digital lines (`ltc14400`,
//! `ltc14400_fec`; docs/proposals/ltc-bmc.md §10): wire parameters, the
//! original LTC profiles unchanged, loopback at 44.1 / 48 / 96 kHz, sync
//! sweep, back-to-back chords, band-limited and resampled paths, dropout
//! repair and isolation from the other LTC rates. Measurements (A/B
//! tables) live in `ltc_fast_ab.rs`.

mod support;

use cricket_core::link::FecMode;
use cricket_core::modem::{encode_to_samples, CricketModemTx};
use cricket_core::phy::{BmcParams, Modulation, PhyConfig};
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage};
use support::{decode_chunked, ChannelSim, XorShift64};

fn fast_profiles() -> Vec<(&'static str, PhyConfig)> {
    vec![
        ("ltc14400", PhyConfig::from_name("ltc14400")),
        ("ltc14400_fec", PhyConfig::from_name("ltc14400_fec")),
    ]
}

fn bmc(config: &PhyConfig) -> &BmcParams {
    match &config.modulation {
        Modulation::Bmc(p) => p,
        Modulation::Mfsk | Modulation::Digital => panic!("BMC profile expected"),
    }
}

fn note_on(note: u8, velocity: u8) -> TransportMessage {
    TransportMessage::NoteOn {
        channel: 0,
        note,
        velocity,
    }
}

fn all_types() -> Vec<TransportMessage> {
    vec![
        note_on(60, 100),
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

#[test]
fn profile_selection_and_wire_format() {
    for (name, fec) in [
        ("ltc14400", FecMode::None),
        ("LTC14400", FecMode::None),
        ("ltc14400_fec", FecMode::Rs16 { parity: 4 }),
        ("ltc14400-FEC", FecMode::Rs16 { parity: 4 }),
    ] {
        let c = PhyConfig::from_name(name);
        assert_eq!(c.fec, fec, "{name}");
        assert_eq!(c.repetition_k, 1, "{name}");
        let p = bmc(&c);
        assert_eq!(*p, BmcParams::fast(14400), "{name}");
        // Wire: 8 '1' preamble, V V sync, no postamble, 1 cell gap,
        // full-width edges.
        assert_eq!(
            (p.preamble_bits, p.postamble_bits, p.gap_bits, p.edge_frac),
            (8, 0, 1, 1.0),
            "{name}"
        );
    }
    // Burst: 16 + 6 + 128 (176 coded) + 1 half cells at 14400 bit/s.
    let ms = PhyConfig::from_name("ltc14400").estimated_first_copy_ms();
    assert!((ms - 5.24).abs() < 0.01, "{ms}");
    let ms = PhyConfig::from_name("ltc14400_fec").estimated_first_copy_ms();
    assert!((ms - 6.91).abs() < 0.01, "{ms}");

    // The original LTC profiles keep their wire format and receiver.
    for rate in [2400, 4800, 9600] {
        for c in [
            PhyConfig::ltc_with_bit_rate(rate),
            PhyConfig::ltc_fec_with_bit_rate(rate),
        ] {
            let p = bmc(&c);
            assert_eq!(*p, BmcParams::new(rate));
            assert_eq!(
                (p.preamble_bits, p.postamble_bits, p.gap_bits, p.edge_frac),
                (8, 1, 4, 0.5)
            );
            assert!(!p.sync_fractional && !p.post_body_guard);
        }
    }
}

#[test]
fn loopback_all_message_types_and_sample_rates() {
    for rate in [44_100, 48_000, 96_000] {
        for (name, config) in fast_profiles() {
            let config = config.with_sample_rate(rate);
            let messages = all_types();
            let samples = encode_to_samples(config.clone(), &messages);
            for chunk in [64, 240, 4096] {
                assert_eq!(
                    decode_chunked(config.clone(), &samples, chunk),
                    messages,
                    "{name} @ {rate} chunk {chunk}"
                );
            }
        }
    }
}

#[test]
fn sync_offset_chunk_sweep() {
    let mut failures = Vec::new();
    for (name, config) in fast_profiles() {
        for offset in 0..97 {
            let messages = [note_on((offset % 128) as u8, 1 + (offset % 127) as u8)];
            let clean = encode_to_samples(config.clone(), &messages);
            let rx = ChannelSim::new(config.clone(), 0)
                .offset(offset)
                .apply(&clean);
            let chunk = [64, 128, 240, 512, 4096][offset % 5];
            if decode_chunked(config.clone(), &rx, chunk) != messages {
                failures.push(format!("{name} offset {offset}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}

/// 4-note chords sent back to back (one cell of silence between bursts):
/// every note arrives, in order.
#[test]
fn chords_back_to_back() {
    let mut rng = XorShift64::new(0xC40D);
    for (name, config) in fast_profiles() {
        for t in 0..25 {
            let root = 36 + rng.below(60) as u8;
            let chord: Vec<_> = [0u8, 4, 7, 12]
                .iter()
                .map(|&d| note_on(root + d, 1 + rng.below(127) as u8))
                .collect();
            let clean = encode_to_samples(config.clone(), &chord);
            let audio = ChannelSim::new(config.clone(), rng.next_u64())
                .offset(rng.below(400))
                .snr_db(20.0)
                .apply(&clean);
            let got = decode_chunked(config.clone(), &audio, 64);
            assert_eq!(got, chord, "{name} chord {t}");
        }
    }
}

/// Paths a Dante / MADI / AES line can include: soft band limits down to
/// 16 kHz, sample-rate errors of 1 %, polarity, level, at 20 dB SNR.
#[test]
fn band_limited_and_resampled_paths() {
    type Impair = fn(ChannelSim) -> ChannelSim;
    let cases: [(&str, Impair); 7] = [
        ("SNR 14 dB", |s| s.snr_db(14.0)),
        ("2-pole LP 16 kHz + SNR 20", |s| {
            s.lowpass2_hz(16_000.0).snr_db(20.0)
        }),
        ("drift +1 % + SNR 20", |s| {
            s.drift_ppm(10_000.0).snr_db(20.0)
        }),
        ("drift -1 % + SNR 20", |s| {
            s.drift_ppm(-10_000.0).snr_db(20.0)
        }),
        ("drift +1000 ppm + SNR 20", |s| {
            s.drift_ppm(1000.0).snr_db(20.0)
        }),
        ("inverted + SNR 20", |s| s.invert().snr_db(20.0)),
        ("level -50 dB + SNR 30", |s| s.gain(0.00316).snr_db(30.0)),
    ];
    let mut failures = Vec::new();
    for (name, config) in fast_profiles() {
        for (i, (cname, impair)) in cases.iter().enumerate() {
            let mut rng = XorShift64::new(0xFA57 + i as u64);
            let n = 40;
            let mut ok = 0;
            for t in 0..n {
                let msg = [note_on(rng.below(128) as u8, 1 + rng.below(127) as u8)];
                let clean = encode_to_samples(config.clone(), &msg);
                let sim = impair(ChannelSim::new(config.clone(), rng.next_u64()).offset(t * 7));
                let chunk = [64, 240, 512][t % 3];
                if decode_chunked(config.clone(), &sim.apply(&clean), chunk) == msg {
                    ok += 1;
                }
            }
            if ok != n {
                failures.push(format!("{name} {cname}: {ok}/{n}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A 0.5 ms dropout anywhere in the body (7 bits at 14400 bit/s) is filled
/// in by the code; the uncoded profile loses most of those packets.
#[test]
fn dropout_in_body_is_repaired() {
    let msg = TransportMessage::Msc(MscMessage::cue(
        MscCommand::Load,
        MscCue::from_tenths(990).unwrap(),
        Some(3),
    ));
    let run = |config: &PhyConfig| -> (usize, usize) {
        let half = bmc(config).half_cell_samples(config.sample_rate_hz);
        let clean = CricketModemTx::new(config.clone()).encode_message_with_seq(&msg, 1);
        let body_start = (22.0 * half) as usize;
        let body_len = (8.0 * config.fec.body_nibbles() as f32 * half) as usize;
        let drop = config.sample_rate_hz as usize / 2000;
        let (mut ok, mut n) = (0, 0);
        for start in (body_start..body_start + body_len - drop).step_by(drop / 3) {
            let mut audio = vec![0.0; 300];
            audio.extend_from_slice(&clean);
            audio[300 + start..300 + start + drop].fill(0.0);
            n += 1;
            if decode_chunked(config.clone(), &audio, 256) == [msg.clone()] {
                ok += 1;
            }
        }
        (ok, n)
    };
    let (ok, n) = run(&PhyConfig::from_name("ltc14400_fec"));
    assert_eq!(ok, n, "ltc14400_fec repaired {ok}/{n}");
    let (ok, n) = run(&PhyConfig::from_name("ltc14400"));
    assert!(ok < n / 2, "ltc14400 without FEC decoded {ok}/{n}");
}

/// The fast profiles and the original LTC rates do not decode each other's
/// audio (both ends must use the same profile).
#[test]
fn other_ltc_rates_do_not_cross_decode() {
    let messages: Vec<_> = (0..6).map(|i| note_on(10 * i, 1 + 9 * i)).collect();
    let others = [
        ("ltc9600", PhyConfig::from_name("ltc9600")),
        ("ltc9600_fec", PhyConfig::from_name("ltc9600_fec")),
        ("ltc", PhyConfig::ltc()),
        ("ltc_fec", PhyConfig::ltc_fec()),
    ];
    for (name, fast) in fast_profiles() {
        let fast_audio = encode_to_samples(fast.clone(), &messages);
        for (other_name, other) in &others {
            let got = decode_chunked(other.clone(), &fast_audio, 512);
            assert!(got.is_empty(), "{name} audio on {other_name}: {got:?}");
            let other_audio = encode_to_samples(other.clone(), &messages);
            let got = decode_chunked(fast.clone(), &other_audio, 512);
            assert!(got.is_empty(), "{other_name} audio on {name}: {got:?}");
        }
    }
}
