//! Link identity (spec §4.1): two Cricket links in one multicore or Dante
//! domain leak into each other, and a receiver is level-independent down
//! to −60 dBFS, so without a link key it delivers the neighbour's packets.
//! With different keys every one of them fails the CRC; with the same key
//! (or no key) they are delivered, which is the fault being fixed.
//!
//! Link A sends notes with silence between them; link B sends its own
//! notes in A's gaps, mixed in at −40 dB. A's receiver must deliver all of
//! A's messages and none of B's.

mod support;

use cricket_core::link::{parse_link_key, DEFAULT_LINK_KEY};
use cricket_core::modem::{flush_samples, CricketModemRx, CricketModemTx, ModemOutput};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::TransportMessage;
use support::ChannelSim;

/// Crosstalk level of link B into A's receiver.
const CROSSTALK_DB: f32 = -40.0;
const NOTES: usize = 12;

fn note(channel: u8, note: u8) -> TransportMessage {
    TransportMessage::NoteOn {
        channel,
        note,
        velocity: 100,
    }
}

/// Audio of `messages`, each followed by `gap` samples of silence, with the
/// first message starting after `offset` samples.
fn stream(
    config: &PhyConfig,
    messages: &[TransportMessage],
    offset: usize,
    gap: usize,
) -> Vec<f32> {
    let mut tx = CricketModemTx::new(config.clone());
    let mut out = vec![0.0f32; offset];
    for m in messages {
        out.extend(tx.encode_message(m));
        out.resize(out.len() + gap, 0.0);
    }
    out
}

struct Outcome {
    own: usize,
    cross: usize,
    other: Vec<TransportMessage>,
    wrong_key_suspects: u64,
}

/// Decode A's receiver (key `key_a`) on A's stream plus B's (key `key_b`)
/// at `CROSSTALK_DB`.
fn run(config: &PhyConfig, key_a: u16, key_b: u16, seed: u64) -> Outcome {
    let a_msgs: Vec<_> = (0..NOTES).map(|i| note(0, 60 + i as u8)).collect();
    let b_msgs: Vec<_> = (0..NOTES).map(|i| note(5, 40 + i as u8)).collect();
    let cfg_a = config.clone().with_link_key(key_a);
    let cfg_b = config.clone().with_link_key(key_b);
    let msg_len = CricketModemTx::new(cfg_a.clone())
        .encode_message(&TransportMessage::Heartbeat)
        .len();
    // Each link sends one message per period; B's lands in A's gap.
    let period = 4 * msg_len;
    let a = stream(&cfg_a, &a_msgs, 0, period - msg_len);
    let mut b = stream(&cfg_b, &b_msgs, 2 * msg_len, period - msg_len);
    b.resize(a.len(), 0.0);
    let gain = 10f32.powf(CROSSTALK_DB / 20.0);
    let mixed: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y * gain).collect();
    // A quiet line: the noise floor is well below the crosstalk.
    let audio = ChannelSim::new(cfg_a.clone(), seed)
        .snr_db(55.0)
        .apply(&mixed);

    let mut rx = CricketModemRx::new(cfg_a.clone());
    let mut delivered = Vec::new();
    let flush = vec![0.0f32; flush_samples(&cfg_a)];
    for chunk in audio.chunks(512).chain(flush.chunks(512)) {
        for out in rx.push_samples(chunk) {
            if let ModemOutput::Message(m) = out {
                delivered.push(m);
            }
        }
    }
    let own = delivered.iter().filter(|m| a_msgs.contains(m)).count();
    let cross = delivered.iter().filter(|m| b_msgs.contains(m)).count();
    let other: Vec<_> = delivered
        .iter()
        .filter(|m| !a_msgs.contains(m) && !b_msgs.contains(m))
        .cloned()
        .collect();
    Outcome {
        own,
        cross,
        other,
        wrong_key_suspects: rx.stats().wrong_key_suspects,
    }
}

fn profiles() -> Vec<(&'static str, PhyConfig)> {
    vec![
        ("robust", PhyConfig::robust()),
        ("robust_repeat", PhyConfig::robust_repeat()),
        ("fast", PhyConfig::fast()),
        ("ltc_fec", PhyConfig::ltc_fec()),
        ("ltc9600", PhyConfig::ltc_with_bit_rate(9600)),
        ("digital", PhyConfig::digital()),
    ]
}

/// Without a key (both links unkeyed) the neighbour's packets at −40 dB
/// are delivered: the fault. With different keys none is, A's own
/// delivery is unchanged, and the receiver counts the neighbour's packets
/// as wrong-key suspects.
#[test]
fn different_keys_stop_crosstalk_deliveries() {
    for (name, config) in profiles() {
        let unkeyed = run(&config, DEFAULT_LINK_KEY, DEFAULT_LINK_KEY, 1);
        assert_eq!(unkeyed.own, NOTES, "{name}: own delivery (unkeyed)");
        assert!(unkeyed.other.is_empty(), "{name}: {:?}", unkeyed.other);
        if !matches!(config.modulation, cricket_core::phy::Modulation::Digital(_)) {
            assert!(
                unkeyed.cross > 0,
                "{name}: the crosstalk at {CROSSTALK_DB} dB was not even decoded unkeyed"
            );
        }

        let keyed = run(&config, 0x1234, parse_link_key("stage right"), 1);
        assert_eq!(keyed.own, NOTES, "{name}: own delivery (keyed)");
        assert_eq!(
            keyed.cross, 0,
            "{name}: crosstalk delivered with another key"
        );
        assert!(keyed.other.is_empty(), "{name}: {:?}", keyed.other);
        // Every neighbour packet that got through unkeyed is a wrong-key
        // suspect. The count is per body, like `crc_fail`: each of the K
        // copies counts, and a body may count twice when the phy re-locks
        // on it after the rejection.
        let copies = config.repetition_k.max(1) as u64;
        assert!(
            keyed.wrong_key_suspects >= unkeyed.cross as u64
                && keyed.wrong_key_suspects <= 4 * copies * unkeyed.cross as u64 + 2,
            "{name}: {} suspects for {} neighbour packets",
            keyed.wrong_key_suspects,
            unkeyed.cross
        );
        assert_eq!(unkeyed.wrong_key_suspects, 0, "{name}");

        // The same key on both: delivered as before (the operator's choice).
        let same = run(&config, 0x1234, 0x1234, 1);
        assert_eq!((same.own, same.cross), (NOTES, unkeyed.cross), "{name}");
        println!(
            "{name}: unkeyed {} of {} neighbour packets delivered, keyed 0 ({} suspects)",
            unkeyed.cross, NOTES, keyed.wrong_key_suspects
        );
    }
}

/// The two ends configured differently (one keyed, one not): nothing is
/// delivered and every packet counts as a wrong-key suspect, so the
/// mismatch is visible in the status.
#[test]
fn mismatched_ends_deliver_nothing_and_say_why() {
    let config = PhyConfig::robust();
    let keyed = config.clone().with_link_key(0xBEEF);
    let msgs: Vec<_> = (0..4).map(|i| note(0, 60 + i)).collect();
    let audio = stream(&keyed, &msgs, 0, 2_000);
    let mut rx = CricketModemRx::new(config.clone());
    let mut delivered = 0;
    let flush = vec![0.0f32; flush_samples(&config)];
    for chunk in audio.chunks(512).chain(flush.chunks(512)) {
        delivered += rx
            .push_samples(chunk)
            .iter()
            .filter(|o| matches!(o, ModemOutput::Message(_)))
            .count();
    }
    let stats = rx.stats();
    assert_eq!(delivered, 0);
    assert_eq!(stats.crc_pass, 0);
    // At least one suspect per packet (a packet counts again each time the
    // phy re-locks on it after the rejection), and nothing else failed.
    assert!(
        (4..=16).contains(&stats.wrong_key_suspects),
        "{} suspects",
        stats.wrong_key_suspects
    );
    assert_eq!(stats.crc_fail, stats.wrong_key_suspects);
    assert!(
        !rx.is_link_armed(),
        "another link's packets do not arm link-dead"
    );
}
