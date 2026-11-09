//! Resync hardening (v1.1 plan A7): recovery from false preamble locks and
//! symbol-boundary alignment accuracy.

mod support;

use cricket_core::modem::{CricketModemTx, MAX_BODY_NIBBLES};
use cricket_core::phy::{ModemRx, PhyConfig, RxEvent};
use cricket_core::transport::TransportMessage;
use std::f32::consts::PI;
use support::decode_chunked;

fn profiles() -> Vec<(&'static str, PhyConfig)> {
    vec![
        ("robust_repeat", PhyConfig::robust_repeat()),
        ("fast", PhyConfig::fast()),
    ]
}

/// Steady tone at `config.tone_hz(nibble)` and the transmit amplitude.
fn tone(config: &PhyConfig, nibble: u8, len: usize, phase: f32) -> Vec<f32> {
    let w = 2.0 * PI * config.tone_hz(nibble) / config.sample_rate_hz as f32;
    (0..len)
        .map(|i| config.amplitude * (w * i as f32 + phase).sin())
        .collect()
}

/// Lock position error in samples: end of the first body symbol period as
/// reported by the receiver minus its true position, or `None` if the
/// receiver never locked.
fn first_body_error(config: &PhyConfig, offset: usize, chunk: usize) -> Option<i64> {
    let msg = TransportMessage::NoteOn {
        channel: 0,
        note: (offset % 128) as u8,
        velocity: 100,
    };
    let mut audio = vec![0.0; offset];
    audio.extend(CricketModemTx::new(config.clone()).encode_message_with_seq(&msg, 1));
    audio.extend(vec![0.0; 4096]);

    let mut rx = ModemRx::new(config.clone());
    rx.set_expected_body_nibbles(MAX_BODY_NIBBLES);
    let period = config.samples_per_symbol_period();
    let truth = (offset + (config.preamble.len() + 1) * period) as i64;
    for c in audio.chunks(chunk) {
        for ev in rx.push_samples(c) {
            if let RxEvent::BodyNibble { end_sample, .. } = ev {
                return Some(end_sample as i64 - truth);
            }
        }
    }
    None
}

/// Residual symbol-boundary error after preamble fine alignment on a clean
/// signal, over every start offset within two symbol periods. The receiver's
/// tone-band input filter delays the signal by a constant few samples (its
/// group delay), which every window sees alike; after removing that median
/// offset the residual must be at most hop / 8 (Robust 9, Fast 5 samples)
/// and independent of chunking.
#[test]
fn fine_alignment_residual_error() {
    let mut failures = Vec::new();
    for (name, config) in profiles() {
        let period = config.samples_per_symbol_period();
        let bound = config.hop_samples().div_ceil(8) as i64;
        for (chunk, step) in [(4096usize, 1usize), (64, 5)] {
            let mut errs = Vec::new();
            for offset in (0..2 * period).step_by(step) {
                let err = first_body_error(&config, offset, chunk)
                    .unwrap_or_else(|| panic!("[{name}] offset {offset}: no lock"));
                errs.push((offset, err));
            }
            let mut sorted: Vec<i64> = errs.iter().map(|e| e.1).collect();
            sorted.sort_unstable();
            let delay = sorted[sorted.len() / 2];
            for e in errs.iter_mut() {
                e.1 -= delay;
            }
            let max = errs.iter().map(|e| e.1.abs()).max().unwrap();
            let mean = errs.iter().map(|e| e.1.abs() as f64).sum::<f64>() / errs.len() as f64;
            let min_signed = errs.iter().map(|e| e.1).min().unwrap();
            let max_signed = errs.iter().map(|e| e.1).max().unwrap();
            println!(
                "[{name}] chunk {chunk}: filter delay {delay}; |err| max {max} mean {mean:.2} \
                 samples (signed {min_signed}..{max_signed}, bound {bound}, hop {})",
                config.hop_samples()
            );
            let worst: Vec<_> = errs.iter().filter(|e| e.1.abs() > bound).take(8).collect();
            if !worst.is_empty() {
                failures.push(format!(
                    "[{name}] chunk {chunk}: residual alignment error max {max} > {bound} \
                     samples; (offset, err): {worst:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A burst of the preamble[0] tone directly in front of a packet makes the
/// receiver lock on the burst; the preamble check then fails part-way into
/// the real packet's preamble. The receiver must rewind and still find the
/// real packet (single repetition, so nothing else can save it).
#[test]
fn false_preamble_lock_before_packet_recovers() {
    let mut failures = Vec::new();
    let mut total = 0usize;
    for (name, mut config) in profiles() {
        config.repetition_k = 1;
        let period = config.samples_per_symbol_period();
        let msg = TransportMessage::NoteOn {
            channel: 0,
            note: 64,
            velocity: 99,
        };
        let packet = CricketModemTx::new(config.clone()).encode_message_with_seq(&msg, 3);
        for burst_len in (period / 4..=2 * period).step_by(period / 8) {
            for phase in [0.0f32, 1.3, 2.9] {
                for (offset, chunk) in [(0usize, 4096usize), (37, 64), (period / 2, 240)] {
                    total += 1;
                    let mut audio = vec![0.0; offset + 500];
                    audio.extend(tone(&config, config.preamble[0], burst_len, phase));
                    audio.extend_from_slice(&packet);
                    let got = decode_chunked(config.clone(), &audio, chunk);
                    if got != [msg.clone()] {
                        failures.push(format!(
                            "[{name}] burst {burst_len} phase {phase} offset {offset} \
                             chunk {chunk}: got {got:?}"
                        ));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}/{} cases lost the packet after a false lock:\n{}",
        failures.len(),
        total,
        failures.join("\n")
    );
}

/// A complete false preamble (e.g. a truncated packet, or noise that happens
/// to pass the preamble check) whose body window overlaps a real packet: the
/// false lock collects a full body made of its own few nibbles plus the real
/// packet's preamble and body, which then fails to decode. The receiver must
/// rewind past the false trigger and re-scan the buffered audio so the real
/// packet (single repetition) is still decoded, for every overlap.
#[test]
fn failed_body_decode_rewinds_to_packet_inside_it() {
    let mut failures = Vec::new();
    let mut total = 0usize;
    let mut configs = profiles();
    configs.push(("robust", PhyConfig::robust()));
    for (name, mut config) in configs {
        config.repetition_k = 1;
        let period = config.samples_per_symbol_period();
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
        // With more of its body kept, the spurious packet itself decodes (the
        // tail is padding, or FEC repairs it) and legitimately owns the
        // overlap; that is not a false lock.
        for kept in 0..body - 6 {
            for (lead, chunk) in [(0usize, 4096usize), (37, 64), (period / 2, 240)] {
                total += 1;
                // Spurious preamble + `kept` body symbols, then the real packet.
                let cut = (config.preamble.len() + kept) * period;
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
        "{}/{} cases lost the packet inside a failed body:\n{}",
        failures.len(),
        total,
        failures.join("\n")
    );
}
