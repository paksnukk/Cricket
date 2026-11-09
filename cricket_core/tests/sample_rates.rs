//! The MFSK profiles at the common sample rates: every message type round
//! trips at 44.1, 48 and 96 kHz through chunked streaming (the LTC and
//! digital profiles have the same check in `ltc_fast.rs` and `digital.rs`).

mod support;

use cricket_core::modem::encode_to_samples;
use cricket_core::phy::PhyConfig;
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage};
use support::decode_chunked;

fn all_types() -> Vec<TransportMessage> {
    vec![
        TransportMessage::NoteOn {
            channel: 3,
            note: 60,
            velocity: 100,
        },
        TransportMessage::NoteOff {
            channel: 3,
            note: 60,
            velocity: 64,
        },
        TransportMessage::ControlChange {
            channel: 0,
            controller: 7,
            value: 127,
        },
        TransportMessage::PitchBend {
            channel: 15,
            value: 0x3FFF,
        },
        TransportMessage::ProgramChange {
            channel: 9,
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
fn mfsk_profiles_round_trip_at_common_sample_rates() {
    let mut failures = Vec::new();
    for (name, config) in [
        ("robust", PhyConfig::robust()),
        ("robust_repeat", PhyConfig::robust_repeat()),
        ("fast", PhyConfig::fast()),
    ] {
        for rate in [44_100, 48_000, 96_000] {
            let config = config.clone().with_sample_rate(rate);
            let messages = all_types();
            let samples = encode_to_samples(config.clone(), &messages);
            for chunk in [64, 240, 4096] {
                let got = decode_chunked(config.clone(), &samples, chunk);
                if got != messages {
                    failures.push(format!("[{name} @ {rate} chunk {chunk}] got {got:?}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
