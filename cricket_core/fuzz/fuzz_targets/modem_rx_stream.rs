//! The whole receiver (`CricketModemRx`: phy, body decode, duplicate
//! suppression, fail-safe) on an arbitrary sample stream in arbitrary
//! chunks, for every catalog profile and the `+digital` detector: never
//! panics, counters stay consistent, and every delivered message is one
//! the transmitter could have sent (its payload round-trips).
#![no_main]

use arbitrary::Arbitrary;
use cricket_core::modem::{CricketModemRx, ModemOutput};
use cricket_core::phy::PROFILES;
use cricket_core::transport::TransportMessage;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    profile: u8,
    digital_detect: bool,
    samples: Vec<u32>,
    chunks: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let info = &PROFILES[usize::from(input.profile) % PROFILES.len()];
    let config = (info.build)().with_digital_detect(input.digital_detect);
    let mut rx = CricketModemRx::new(config);
    let samples: Vec<f32> = input.samples.iter().map(|&b| f32::from_bits(b)).collect();
    let mut chunks = input.chunks.iter().cycle();
    let mut pushed = 0usize;
    let mut at = 0;
    while at < samples.len() {
        let n = usize::from(*chunks.next().unwrap_or(&64)).max(1);
        let chunk = &samples[at..(at + n).min(samples.len())];
        at += chunk.len();
        pushed += chunk.len();
        let mut saw_stats = false;
        for out in rx.push_samples(chunk) {
            match out {
                ModemOutput::Message(msg) => {
                    let payload = msg.to_payload();
                    let again = TransportMessage::from_type_payload(msg.msg_type(), &payload)
                        .expect("a delivered message has a canonical payload");
                    assert_eq!(again, msg);
                }
                ModemOutput::Midi(bytes) => assert!(!bytes.is_empty()),
                ModemOutput::Stats(s) => {
                    saw_stats = true;
                    assert!(s.wrong_key_suspects <= s.crc_fail);
                    assert!(!s.last_confidence_db.is_nan());
                }
                _ => {}
            }
        }
        assert!(saw_stats, "every push ends with Stats");
        assert_eq!(rx.samples_processed(), pushed as u64);
    }
    rx.reset();
    let _ = rx.stats();
});
