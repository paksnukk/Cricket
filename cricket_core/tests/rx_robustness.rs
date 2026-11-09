//! Receiver robustness: non-finite input samples, hard reset, the
//! `crc_fail` counter and the offline helpers' flush.

mod support;

use cricket_core::link::{bytes_to_nibbles, interleave};
use cricket_core::modem::{
    decode_samples, encode_to_samples, CricketModemRx, CricketModemTx, ModemOutput,
    MAX_BODY_NIBBLES,
};
use cricket_core::phy::{LinkState, PhyConfig, PhyTx, LOCK_HOLD_MS};
use cricket_core::transport::TransportMessage;

fn all_profiles() -> Vec<(&'static str, PhyConfig)> {
    [
        "robust",
        "robust_repeat",
        "fast",
        "ltc",
        "ltc_fec",
        "ltc9600",
        "ltc9600_fec",
        "ltc14400",
        "ltc14400_fec",
        "digital",
    ]
    .into_iter()
    .map(|name| (name, PhyConfig::from_name(name).expect("known profile")))
    .collect()
}

fn notes(first: u8, n: usize) -> Vec<TransportMessage> {
    (0..n)
        .map(|i| TransportMessage::NoteOn {
            channel: 0,
            note: first + i as u8,
            velocity: 100,
        })
        .collect()
}

/// Push `samples` in chunks of 256 and collect the decoded messages.
fn push_chunked(rx: &mut CricketModemRx, samples: &[f32]) -> Vec<TransportMessage> {
    let mut out = Vec::new();
    for chunk in samples.chunks(256) {
        for o in rx.push_samples(chunk) {
            if let ModemOutput::Message(m) = o {
                out.push(m);
            }
        }
    }
    out
}

/// A single NaN or infinite sample (a glitching driver, a division by zero
/// upstream) must not poison the receiver: after it and a second of
/// silence, packets decode as before.
#[test]
fn non_finite_sample_does_not_poison_receiver() {
    const N: usize = 5;
    let mut failures = Vec::new();
    for (name, config) in all_profiles() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let rate = config.sample_rate_hz as usize;
            let before = encode_to_samples(config.clone(), &notes(40, N));
            let after = encode_to_samples(config.clone(), &notes(60, N));
            let mut rx = CricketModemRx::new(config.clone());
            let got_before = push_chunked(&mut rx, &before);
            let mut glitch = vec![0.0f32; 100];
            glitch.push(bad);
            glitch.extend(vec![0.0; rate]);
            push_chunked(&mut rx, &glitch);
            let got_after = push_chunked(&mut rx, &after);
            push_chunked(&mut rx, &vec![0.0; rate / 4]);
            if got_before.len() != N || got_after.len() != N {
                failures.push(format!(
                    "[{name}] {bad}: {}/{N} before, {}/{N} after the glitch",
                    got_before.len(),
                    got_after.len()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A body whose type nibble is reserved is a failed body: it counts in
/// `crc_fail` like every other body that held no valid packet.
#[test]
fn reserved_type_nibble_counts_as_crc_fail() {
    for (name, config) in [
        ("fast", PhyConfig::fast()),
        ("robust_repeat", PhyConfig::robust_repeat()),
        ("ltc", PhyConfig::ltc()),
    ] {
        // Version 1, type 0xF (reserved), SEQ 0, any payload and CRC.
        let bytes = [0x1Fu8, 0x00, 0x90, 0x3C, 0x40, 0x12, 0x34];
        let mut nibbles = bytes_to_nibbles(&bytes);
        nibbles.resize(MAX_BODY_NIBBLES, 0);
        let body = interleave(&nibbles, config.interleave_depth);
        let mut audio = vec![0.0f32; 500];
        audio.extend(PhyTx::new(config.clone()).synthesize_framed(&body));
        audio.extend(vec![0.0; config.sample_rate_hz as usize / 4]);
        let mut rx = CricketModemRx::new(config.clone());
        let got = push_chunked(&mut rx, &audio);
        assert!(got.is_empty(), "[{name}] decoded {got:?}");
        let stats = rx.stats();
        assert_eq!(stats.crc_pass, 0, "[{name}]");
        // Before: 0. Every completed body that fails counts; the MFSK
        // receiver may read the same audio a second time from a slightly
        // different alignment after the first rejection (at most twice).
        assert!(
            (1..=2).contains(&stats.crc_fail),
            "[{name}] crc_fail {stats:?}"
        );
    }
}

/// `reset` is a hard reset: a packet cut off by it leaves nothing behind, a
/// fresh stream decodes, duplicates of pre-reset packets are not suppressed,
/// and link-dead does not fire for traffic received before the reset.
#[test]
fn reset_is_a_hard_reset() {
    for (name, config) in all_profiles() {
        let rate = config.sample_rate_hz as usize;
        let msg = TransportMessage::NoteOn {
            channel: 0,
            note: 64,
            velocity: 100,
        };
        let packet = CricketModemTx::new(config.clone()).encode_message_with_seq(&msg, 1);
        let mut rx = CricketModemRx::new(config.clone());

        // A complete packet arms link-dead and enters the duplicate history.
        let got = push_chunked(&mut rx, &packet);
        assert_eq!(got, vec![msg.clone()], "[{name}] first packet");
        assert!(rx.is_link_armed(), "[{name}] armed");
        // Half of a second packet, then the reset.
        push_chunked(&mut rx, &packet[..packet.len() / 2]);
        rx.reset();
        assert!(!rx.is_link_armed(), "[{name}] disarmed by reset");
        let stats = rx.stats();
        assert_eq!(stats.last_seq, None, "[{name}] last_seq cleared");

        // The same packet bytes again: not a duplicate after the reset.
        let mut after = vec![0.0f32; 200];
        after.extend_from_slice(&packet);
        after.extend(vec![0.0; rate / 10]);
        let mut outputs = Vec::new();
        for chunk in after.chunks(256) {
            outputs.extend(rx.push_samples(chunk));
        }
        let got: Vec<_> = outputs
            .iter()
            .filter_map(|o| match o {
                ModemOutput::Message(m) => Some(m.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(got, vec![msg.clone()], "[{name}] after reset");

        // Reset again, then silence past link-dead: nothing fires.
        rx.reset();
        let mut outputs = Vec::new();
        let silence = vec![0.0f32; rate * (config.link_dead_ms as usize + 500) / 1000];
        for chunk in silence.chunks(1024) {
            outputs.extend(rx.push_samples(chunk));
        }
        assert!(
            !outputs.iter().any(|o| matches!(o, ModemOutput::LinkDead)),
            "[{name}] link-dead fired for pre-reset traffic"
        );
        assert!(
            !outputs.iter().any(|o| matches!(o, ModemOutput::Midi(_))),
            "[{name}] fail-safe burst after reset"
        );
    }
}

/// `LinkState` follows one rule on every phy: `Locked` from a decoded
/// packet until a second without one, then `Search`, then `Dead` at
/// link-dead time (on the profiles whose link-dead time is not longer than
/// the hold, `digital` and the LTC profiles from 9600 bit/s, `Dead` comes
/// first); heartbeats at the profile's interval keep a live link `Locked`.
#[test]
fn link_state_holds_for_a_second_after_a_packet() {
    for (name, config) in all_profiles() {
        let rate = config.sample_rate_hz as usize;
        let hold = rate * LOCK_HOLD_MS as usize / 1000;
        let link_dead = rate * config.link_dead_ms as usize / 1000;
        let mut rx = CricketModemRx::new(config.clone());
        assert_eq!(rx.stats().link_state, LinkState::Search, "[{name}] fresh");
        let packet = CricketModemTx::new(config.clone()).encode_message(&notes(60, 1)[0]);
        let got = push_chunked(&mut rx, &packet);
        assert_eq!(got.len(), 1, "[{name}]");
        assert_eq!(
            rx.stats().link_state,
            LinkState::Locked,
            "[{name}] after packet"
        );
        // Silence in 1 ms steps until the lock is released: between half
        // the hold and the hold after the packet's audio ends (its body
        // ended a little before that), long before link-dead.
        let step = rate / 1000;
        let mut elapsed = 0;
        while rx.stats().link_state == LinkState::Locked && elapsed <= hold {
            push_chunked(&mut rx, &vec![0.0; step]);
            elapsed += step;
        }
        if link_dead > hold + 2 * step {
            assert_eq!(
                rx.stats().link_state,
                LinkState::Search,
                "[{name}] after hold"
            );
            assert!(
                elapsed > hold / 2 && elapsed <= hold + step,
                "[{name}] lock released {} ms after the packet",
                elapsed * 1000 / rate
            );
            assert!(rx.is_link_armed(), "[{name}] link-dead still pending");
        } else {
            // Link-dead is due at or before the end of the hold.
            assert!(
                matches!(
                    rx.stats().link_state,
                    LinkState::Search | LinkState::Dead
                ),
                "[{name}] after hold"
            );
        }
        // Link-dead then reports Dead.
        push_chunked(&mut rx, &vec![0.0; link_dead]);
        assert_eq!(rx.stats().link_state, LinkState::Dead, "[{name}] dead");
        // A heartbeat every interval keeps the link Locked.
        let mut rx = CricketModemRx::new(config.clone());
        let mut tx = CricketModemTx::new(config.clone());
        let interval = rate * config.heartbeat_interval_ms as usize / 1000;
        for _ in 0..4 {
            let hb = tx.encode_heartbeat();
            push_chunked(&mut rx, &hb);
            push_chunked(&mut rx, &vec![0.0; interval - hb.len().min(interval)]);
            assert_eq!(
                rx.stats().link_state,
                LinkState::Locked,
                "[{name}] idle link"
            );
        }
    }
}

/// `decode_samples` must deliver the last message of a stream whatever the
/// profile and sample rate: its trailing flush covers the receiver's
/// decision delay even when the input ends with the last sample of the
/// last burst.
#[test]
fn decode_samples_delivers_last_message() {
    let mut failures = Vec::new();
    for (name, config) in all_profiles() {
        for rate in [44_100, 48_000, 96_000] {
            let config = config.clone().with_sample_rate(rate);
            let messages = notes(60, 3);
            let samples = encode_to_samples(config.clone(), &messages);
            // Cut every trailing silent sample: the flush alone must finish
            // the last message.
            let end = samples
                .iter()
                .rposition(|&s| s != 0.0)
                .map_or(samples.len(), |i| i + 1);
            let got = decode_samples(config.clone(), &samples[..end]);
            if got != messages {
                failures.push(format!("[{name} @ {rate}] got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
