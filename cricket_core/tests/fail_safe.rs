//! Fail-safe (link-dead, Panic) and duplicate-suppression tests.
//! See docs/aml-v1-spec.md §4.4 and §5.2.

mod support;

use cricket_core::link::{bytes_to_nibbles, encode_packet_bytes, interleave, Packet};
use cricket_core::modem::{CricketModemRx, CricketModemTx, ModemOutput, MAX_BODY_NIBBLES};
use cricket_core::phy::{LinkState, ModemTx, PhyConfig};
use cricket_core::transport::TransportMessage;
use support::ChannelSim;

const SUSTAIN_OFF: [u8; 3] = [0xB0, 0x40, 0x00];
const ALL_NOTES_OFF: [u8; 3] = [0xB0, 0x7B, 0x00];
const CHUNK_SIZES: [usize; 3] = [64, 240, 4096];

/// One output together with the stream range `[start, end)` of the
/// `push_samples` call that produced it.
struct Ev {
    start: usize,
    end: usize,
    out: ModemOutput,
}

fn feed(rx: &mut CricketModemRx, stream: &[f32], chunk: usize, base: usize) -> Vec<Ev> {
    let mut evs = Vec::new();
    let mut pos = base;
    for c in stream.chunks(chunk) {
        let start = pos;
        pos += c.len();
        for out in rx.push_samples(c) {
            evs.push(Ev {
                start,
                end: pos,
                out,
            });
        }
    }
    evs
}

fn silence(config: &PhyConfig, ms: usize) -> Vec<f32> {
    vec![0.0; config.sample_rate_hz as usize * ms / 1000]
}

fn note_on(note: u8) -> TransportMessage {
    TransportMessage::NoteOn {
        channel: 0,
        note,
        velocity: 100,
    }
}

fn messages(evs: &[Ev]) -> Vec<TransportMessage> {
    evs.iter()
        .filter_map(|e| match &e.out {
            ModemOutput::Message(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn count_midi(evs: &[Ev], bytes: &[u8]) -> usize {
    evs.iter()
        .filter(|e| matches!(&e.out, ModemOutput::Midi(b) if b.as_slice() == bytes))
        .count()
}

fn link_dead_events(evs: &[Ev]) -> Vec<&Ev> {
    evs.iter()
        .filter(|e| matches!(e.out, ModemOutput::LinkDead))
        .collect()
}

fn link_dead_samples(config: &PhyConfig) -> usize {
    config.link_dead_ms as usize * config.sample_rate_hz as usize / 1000
}

/// Audio for one packet repetition whose CRC is broken but whose preamble and
/// symbol timing are intact (the phy locks and collects a full body).
fn bad_crc_repetition(config: &PhyConfig, seq: u8) -> Vec<f32> {
    let msg = note_on(60);
    let packet = Packet {
        msg_type: msg.msg_type(),
        seq,
        payload: msg.to_payload(),
    };
    let mut bytes = encode_packet_bytes(&packet).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x5A;
    let mut nibbles = bytes_to_nibbles(&bytes);
    nibbles.resize(MAX_BODY_NIBBLES, 0);
    let body = interleave(&nibbles, config.interleave_depth);
    let mut audio = ModemTx::new(config.clone()).synthesize_framed(&body);
    audio.resize(audio.len() + config.samples_per_symbol_period(), 0.0);
    audio
}

#[test]
fn link_dead_fires_once_sample_accurate_and_dead_state_visible() {
    for (name, config) in [
        ("robust", PhyConfig::robust()),
        ("robust_repeat", PhyConfig::robust_repeat()),
        ("fast", PhyConfig::fast()),
    ] {
        let limit = link_dead_samples(&config);
        let mut tx = CricketModemTx::new(config.clone());
        let first = tx.encode_message(&note_on(60));
        let mut stream = first.clone();
        stream.extend(silence(&config, config.link_dead_ms as usize + 500));
        let revive_at = stream.len();
        stream.extend(tx.encode_message(&note_on(61)));
        stream.extend(silence(&config, 300));
        let stream = ChannelSim::new(config.clone(), 1)
            .offset(123)
            .apply(&stream);
        let revive_at = revive_at + 123;

        // Fire position: end of the last (second, for K=2) body + limit.
        let last_body_end = 123 + first.len() - config.samples_per_symbol_period();
        let expected = (last_body_end + limit) as u64;
        let tolerance = config.hop_samples() as u64;

        let mut fire_samples = Vec::new();
        for chunk in CHUNK_SIZES {
            let mut rx = CricketModemRx::new(config.clone());
            let evs = feed(&mut rx, &stream, chunk, 0);
            let label = format!("{name} chunk {chunk}");

            let dead = link_dead_events(&evs);
            assert_eq!(dead.len(), 1, "{label}: link-dead events");
            assert_eq!(count_midi(&evs, &SUSTAIN_OFF), 1, "{label}: CC64=0 count");
            assert_eq!(count_midi(&evs, &ALL_NOTES_OFF), 1, "{label}: CC123 count");
            assert_eq!(messages(&evs), vec![note_on(60), note_on(61)], "{label}");

            // Reported by the chunk containing the exact fire sample.
            let fire = rx.last_link_dead_sample().expect("link-dead sample");
            let (s, e) = (dead[0].start as u64, dead[0].end as u64);
            assert!(
                s <= fire && fire < e,
                "{label}: fire {fire} outside [{s},{e})"
            );
            assert!(
                fire.abs_diff(expected) <= tolerance,
                "{label}: fire {fire}, expected {expected} ± {tolerance}"
            );
            fire_samples.push(fire);

            // Dead from the link-dead output until the next accepted message.
            let dead_idx = evs
                .iter()
                .position(|e| matches!(e.out, ModemOutput::LinkDead))
                .unwrap();
            let revive_idx = evs
                .iter()
                .position(|e| matches!(&e.out, ModemOutput::Message(m) if *m == note_on(61)))
                .unwrap();
            assert!(revive_idx > dead_idx);
            for ev in &evs[dead_idx..revive_idx] {
                if let ModemOutput::Stats(st) = &ev.out {
                    assert_eq!(
                        st.link_state,
                        LinkState::Dead,
                        "{label}: stats at {}",
                        ev.end
                    );
                }
            }
            assert!(evs[revive_idx].end >= revive_at);
            for ev in &evs[revive_idx..] {
                if let ModemOutput::Stats(st) = &ev.out {
                    assert_ne!(st.link_state, LinkState::Dead, "{label}: after revive");
                }
            }
            assert_ne!(rx.stats().link_state, LinkState::Dead, "{label}");
        }
        // The fire sample does not depend on chunking.
        assert!(
            fire_samples.windows(2).all(|w| w[0] == w[1]),
            "{name}: fire samples differ across chunk sizes: {fire_samples:?}"
        );
    }
}

/// The burst releases the notes this receiver turned on (and has not seen
/// released) explicitly, per channel before that channel's CC64 = 0 and
/// CC123, so synths that ignore CC123 release them too.
#[test]
fn burst_releases_the_notes_this_receiver_turned_on() {
    let config = PhyConfig::fast();
    let mut tx = CricketModemTx::new(config.clone());
    let mut stream = Vec::new();
    for m in [
        note_on(60),
        note_on(64),
        TransportMessage::NoteOn {
            channel: 9,
            note: 36,
            velocity: 100,
        },
        TransportMessage::NoteOff {
            channel: 0,
            note: 64,
            velocity: 0,
        },
    ] {
        stream.extend(tx.encode_message(&m));
        stream.extend(silence(&config, 20));
    }
    stream.extend(silence(&config, config.link_dead_ms as usize + 500));
    for chunk in CHUNK_SIZES {
        let mut rx = CricketModemRx::new(config.clone());
        let evs = feed(&mut rx, &stream, chunk, 0);
        assert_eq!(link_dead_events(&evs).len(), 1, "chunk {chunk}");
        let midi: Vec<&[u8]> = evs
            .iter()
            .filter_map(|e| match &e.out {
                ModemOutput::Midi(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        // The four messages' MIDI, then the burst: 32 + 2 Note Offs.
        assert_eq!(midi.len(), 4 + 34, "chunk {chunk}: {midi:?}");
        let burst = &midi[4..];
        assert_eq!(burst[0], [0x80, 60, 0x40], "chunk {chunk}");
        assert_eq!(burst[1], [0xB0, 0x40, 0x00]);
        assert_eq!(burst[2], [0xB0, 0x7B, 0x00]);
        let ch9 = burst.iter().position(|m| m[0] == 0x89).unwrap();
        assert_eq!(burst[ch9], [0x89, 36, 0x40]);
        assert_eq!(burst[ch9 + 1], [0xB9, 0x40, 0x00]);
        assert!(!burst.contains(&&[0x80u8, 64, 0x40][..]), "released note");
        assert_eq!(count_midi(&evs, &ALL_NOTES_OFF), 1);
    }
}

/// With the burst disabled (cue-only links) link-dead is still detected
/// and reported, but nothing goes to MIDI out, on Panic either.
#[test]
fn disabled_burst_reports_link_dead_without_midi() {
    let config = PhyConfig::fast();
    let mut tx = CricketModemTx::new(config.clone());
    let mut stream = tx.encode_message(&note_on(60));
    stream.extend(silence(&config, 50));
    stream.extend(tx.encode_message(&TransportMessage::Panic));
    stream.extend(silence(&config, config.link_dead_ms as usize + 500));
    let mut rx = CricketModemRx::new(config.clone());
    rx.set_fail_safe_burst(false);
    assert!(!rx.fail_safe_burst_enabled());
    let evs = feed(&mut rx, &stream, 512, 0);
    assert_eq!(messages(&evs), vec![note_on(60), TransportMessage::Panic]);
    assert_eq!(link_dead_events(&evs).len(), 1);
    assert_eq!(rx.stats().link_state, LinkState::Dead);
    let midi: Vec<&[u8]> = evs
        .iter()
        .filter_map(|e| match &e.out {
            ModemOutput::Midi(b) => Some(b.as_slice()),
            _ => None,
        })
        .collect();
    assert_eq!(midi, vec![&[0x90u8, 60, 100][..]], "only the note itself");
    assert!(rx.fail_safe_burst().is_empty());
}

#[test]
fn no_link_dead_before_first_packet() {
    let config = PhyConfig::robust_repeat();
    let stream = ChannelSim::new(config.clone(), 7)
        .snr_db(10.0)
        .apply(&silence(&config, 3 * config.link_dead_ms as usize));
    for chunk in CHUNK_SIZES {
        let mut rx = CricketModemRx::new(config.clone());
        let evs = feed(&mut rx, &stream, chunk, 0);
        assert!(link_dead_events(&evs).is_empty(), "chunk {chunk}");
        assert_eq!(count_midi(&evs, &ALL_NOTES_OFF), 0, "chunk {chunk}");
        assert_ne!(rx.stats().link_state, LinkState::Dead, "chunk {chunk}");
    }
}

/// A valid packet, then > link_dead_ms of bad-CRC traffic (the phy keeps
/// locking), then silence: exactly one link-dead burst.
#[test]
fn bad_crc_traffic_then_silence_single_burst() {
    let config = PhyConfig::robust_repeat();
    let mut tx = CricketModemTx::new(config.clone());
    let mut stream = tx.encode_message(&note_on(60));
    let target = stream.len() + link_dead_samples(&config) * 3 / 2;
    let mut seq = 1u8;
    while stream.len() < target {
        stream.extend(bad_crc_repetition(&config, seq));
        stream.extend(silence(&config, 50));
        seq = (seq + 1) % 4;
    }
    stream.extend(silence(&config, 3 * config.link_dead_ms as usize));
    for chunk in CHUNK_SIZES {
        let mut rx = CricketModemRx::new(config.clone());
        let evs = feed(&mut rx, &stream, chunk, 0);
        assert!(
            rx.stats().crc_fail > 5,
            "chunk {chunk}: bad-CRC traffic decoded"
        );
        assert_eq!(
            link_dead_events(&evs).len(),
            1,
            "chunk {chunk}: link-dead events"
        );
        assert_eq!(
            count_midi(&evs, &ALL_NOTES_OFF),
            1,
            "chunk {chunk}: CC123 count"
        );
        assert_eq!(
            count_midi(&evs, &SUSTAIN_OFF),
            1,
            "chunk {chunk}: CC64=0 count"
        );
    }
}

#[test]
fn panic_emits_exactly_one_burst() {
    let config = PhyConfig::robust_repeat();
    let mut tx = CricketModemTx::new(config.clone());
    let mut stream = tx.encode_message(&TransportMessage::Panic);
    // Up to just before the link-dead deadline that the Panic packet armed.
    let quiet_ms = config.link_dead_ms as usize - 100;
    stream.extend(silence(&config, quiet_ms));
    let quiet_end = stream.len();
    stream.extend(silence(&config, config.link_dead_ms as usize));
    for chunk in CHUNK_SIZES {
        let mut rx = CricketModemRx::new(config.clone());
        let evs = feed(&mut rx, &stream, chunk, 0);
        let (before, after): (Vec<&Ev>, Vec<&Ev>) =
            evs.iter().partition(|e| e.end <= quiet_end - chunk);
        assert_eq!(
            messages_ref(&before),
            vec![TransportMessage::Panic],
            "chunk {chunk}"
        );
        assert_eq!(
            count_midi_ref(&before, &ALL_NOTES_OFF),
            1,
            "chunk {chunk}: Panic CC123"
        );
        assert_eq!(
            count_midi_ref(&before, &SUSTAIN_OFF),
            1,
            "chunk {chunk}: Panic CC64=0"
        );
        // ... on every channel (1–16), sustain off before All Notes Off.
        let burst: Vec<&[u8]> = before
            .iter()
            .filter_map(|e| match &e.out {
                ModemOutput::Midi(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(burst.len(), 32, "chunk {chunk}");
        for ch in 0..16u8 {
            assert_eq!(burst[2 * ch as usize], [0xB0 | ch, 0x40, 0x00]);
            assert_eq!(burst[2 * ch as usize + 1], [0xB0 | ch, 0x7B, 0x00]);
        }
        assert!(
            before
                .iter()
                .all(|e| !matches!(e.out, ModemOutput::LinkDead)),
            "chunk {chunk}: link-dead shortly after Panic"
        );
        // The link going silent afterwards is its own (single) link-dead event.
        assert_eq!(
            after
                .iter()
                .filter(|e| matches!(e.out, ModemOutput::LinkDead))
                .count(),
            1,
            "chunk {chunk}: later link-dead"
        );
        assert_eq!(count_midi_ref(&after, &ALL_NOTES_OFF), 1, "chunk {chunk}");
    }
}

fn messages_ref(evs: &[&Ev]) -> Vec<TransportMessage> {
    evs.iter()
        .filter_map(|e| match &e.out {
            ModemOutput::Message(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn count_midi_ref(evs: &[&Ev], bytes: &[u8]) -> usize {
    evs.iter()
        .filter(|e| matches!(&e.out, ModemOutput::Midi(b) if b.as_slice() == bytes))
        .count()
}

fn decode_stream(config: &PhyConfig, stream: &[f32], chunk: usize) -> Vec<TransportMessage> {
    let mut s = stream.to_vec();
    s.extend(silence(config, 300));
    let mut rx = CricketModemRx::new(config.clone());
    messages(&feed(&mut rx, &s, chunk, 0))
}

#[test]
fn dedup_repetitions_delivered_once() {
    let config = PhyConfig::robust_repeat();
    assert_eq!(config.repetition_k, 2);
    let mut tx = CricketModemTx::new(config.clone());
    let msgs = [
        note_on(60),
        TransportMessage::NoteOff {
            channel: 0,
            note: 60,
            velocity: 64,
        },
        note_on(62),
    ];
    let mut stream = Vec::new();
    for m in &msgs {
        stream.extend(tx.encode_message(m));
        stream.extend(silence(&config, 20));
    }
    for chunk in CHUNK_SIZES {
        assert_eq!(
            decode_stream(&config, &stream, chunk),
            msgs.to_vec(),
            "chunk {chunk}"
        );
    }
}

#[test]
fn dedup_tx_restart_delivers_all() {
    let config = PhyConfig::robust_repeat();
    // Scenario 1: seq 0..3, restart, seq 0..3 again (identical messages).
    let first: Vec<TransportMessage> = (0..4).map(|i| note_on(60 + i)).collect();
    // Scenario 2: last packet seen before the restart had seq 0 and the
    // first packet after it is the same message with seq 0.
    let cases: [(&str, Vec<TransportMessage>); 2] =
        [("seq 0..3", first), ("single seq 0", vec![note_on(60)])];
    for (name, msgs) in cases {
        let mut stream = Vec::new();
        for _ in 0..2 {
            let mut tx = CricketModemTx::new(config.clone());
            for m in &msgs {
                stream.extend(tx.encode_message(m));
                stream.extend(silence(&config, 20));
            }
            // Pause longer than the dedup window, shorter than link-dead.
            stream.extend(silence(&config, 600));
        }
        let mut expected = msgs.clone();
        expected.extend(msgs.clone());
        for chunk in CHUNK_SIZES {
            assert_eq!(
                decode_stream(&config, &stream, chunk),
                expected,
                "{name} chunk {chunk}"
            );
        }
    }
}

#[test]
fn dedup_same_seq_different_payload_delivered() {
    let config = PhyConfig::robust_repeat();
    let tx = CricketModemTx::new(config.clone());
    let a = note_on(60);
    let b = TransportMessage::NoteOff {
        channel: 0,
        note: 60,
        velocity: 64,
    };
    let mut stream = tx.encode_message_with_seq(&a, 1);
    stream.extend(tx.encode_message_with_seq(&b, 1));
    for chunk in CHUNK_SIZES {
        assert_eq!(
            decode_stream(&config, &stream, chunk),
            vec![a.clone(), b.clone()],
            "chunk {chunk}"
        );
    }
}

/// A watchdog declares link-dead mid-outage: no burst from the modem, state
/// `Dead`, and resuming samples (silence longer than link-dead) do not fire a
/// second burst for the same outage. The next valid packet re-arms.
#[test]
fn declared_link_dead_suppresses_sample_burst_until_next_packet() {
    let config = PhyConfig::fast();
    let mut tx = CricketModemTx::new(config.clone());
    let mut rx = CricketModemRx::new(config.clone());
    assert!(!rx.is_link_armed());
    assert!(!rx.declare_link_dead(), "not armed before any packet");

    let mut stream = tx.encode_message(&note_on(60));
    stream.extend(silence(&config, 100));
    let evs = feed(&mut rx, &stream, 512, 0);
    assert_eq!(messages(&evs), vec![note_on(60)]);
    assert!(rx.is_link_armed());

    assert!(rx.declare_link_dead());
    assert!(!rx.is_link_armed());
    assert_eq!(rx.stats().link_state, LinkState::Dead);
    assert_eq!(rx.last_link_dead_sample(), Some(stream.len() as u64));
    assert!(!rx.declare_link_dead(), "only once per outage");

    let quiet = silence(&config, 2 * config.link_dead_ms as usize);
    let evs = feed(&mut rx, &quiet, 512, stream.len());
    assert!(link_dead_events(&evs).is_empty());
    assert_eq!(count_midi(&evs, &ALL_NOTES_OFF), 0);
    assert_eq!(rx.stats().link_state, LinkState::Dead);

    // A new packet revives and re-arms; its own outage fires normally.
    let mut stream2 = tx.encode_message(&note_on(61));
    stream2.extend(silence(&config, config.link_dead_ms as usize + 200));
    let evs = feed(&mut rx, &stream2, 512, 0);
    assert_eq!(messages(&evs), vec![note_on(61)]);
    assert_eq!(link_dead_events(&evs).len(), 1);
    assert_eq!(count_midi(&evs, &ALL_NOTES_OFF), 1);
}
