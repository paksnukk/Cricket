//! The digital (bit-transparent) receiver on an arbitrary sample stream:
//! never panics (NaN and infinities included), every reported frame has a
//! known type and a consistent length, and the counters add up.
#![no_main]

use arbitrary::Arbitrary;
use cricket_core::phy::digital::{body_nibbles_for_type, DigitalRx};
use cricket_core::phy::{PhyConfig, RxEvent};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    samples: Vec<u32>,
    chunks: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let mut rx = DigitalRx::new(PhyConfig::digital());
    let samples: Vec<f32> = input.samples.iter().map(|&b| f32::from_bits(b)).collect();
    let mut chunks = input.chunks.iter().cycle();
    let mut pushed = 0usize;
    let mut frames = 0u64;
    let mut at = 0;
    while at < samples.len() {
        let n = usize::from(*chunks.next().unwrap_or(&64)).max(1);
        let chunk = &samples[at..(at + n).min(samples.len())];
        at += chunk.len();
        pushed += chunk.len();
        for ev in rx.push_samples(chunk) {
            if let RxEvent::Frame(f) = ev {
                frames += 1;
                let packet = f.packet();
                assert!(!packet.is_empty() && packet.len() <= 7);
                let nibbles = body_nibbles_for_type(packet[0] & 0x0F).expect("known type");
                assert_eq!(nibbles, 2 * packet.len());
                assert!(f.end_sample <= pushed as u64);
                assert!(f.max_residual >= 0);
            }
        }
        assert_eq!(rx.samples_pushed(), pushed as u64);
    }
    let c = rx.counters();
    assert_eq!(c.frames, frames);
    assert!(c.syncs >= c.frames);
    let _ = rx.link_state();
    assert!(rx.last_confidence_db().is_finite());
    rx.reset();
});
