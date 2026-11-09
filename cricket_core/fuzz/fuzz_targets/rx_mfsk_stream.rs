//! The MFSK receiver on an arbitrary sample stream in arbitrary chunks, for
//! every MFSK profile: never panics (NaN and infinities included), the
//! sample count is exact, and the input buffer stays bounded when the
//! caller answers every completed body (as the modem does).
#![no_main]

use arbitrary::Arbitrary;
use cricket_core::modem::repetition_samples;
use cricket_core::phy::{ModemRx, PhyConfig, RxEvent};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    profile: u8,
    /// Raw sample bits: every f32, NaN and ±inf included.
    samples: Vec<u32>,
    /// Chunk sizes (0 = 1), cycled.
    chunks: Vec<u8>,
    /// Verdicts for completed bodies, cycled (bit 0: confirm or reject).
    verdicts: u8,
}

fuzz_target!(|input: Input| {
    let config = match input.profile % 6 {
        0 => PhyConfig::robust(),
        1 => PhyConfig::robust_repeat(),
        2 => PhyConfig::fast(),
        3 => PhyConfig::hf_robust(),
        4 => PhyConfig::hf_fast(),
        _ => PhyConfig::robust_fast(),
    };
    let body_nibbles = config.body_nibbles();
    let bound = 4 * repetition_samples(&config) as usize;
    let mut rx = ModemRx::new(config);
    rx.set_expected_body_nibbles(body_nibbles);
    let samples: Vec<f32> = input.samples.iter().map(|&b| f32::from_bits(b)).collect();
    let mut chunks = input.chunks.iter().cycle();
    let mut pushed = 0usize;
    let mut collected = 0usize;
    let mut verdict = input.verdicts;
    let mut at = 0;
    while at < samples.len() {
        let n = usize::from(*chunks.next().unwrap_or(&64)).max(1);
        let chunk = &samples[at..(at + n).min(samples.len())];
        at += chunk.len();
        pushed += chunk.len();
        let mut events = rx.push_samples(chunk);
        let mut i = 0;
        while i < events.len() {
            match &events[i] {
                RxEvent::PreambleLocked { .. } => collected = 0,
                RxEvent::BodyNibble { .. } => {
                    collected += 1;
                    if collected >= body_nibbles {
                        collected = 0;
                        let more = if verdict & 1 == 1 {
                            rx.confirm()
                        } else {
                            rx.reject()
                        };
                        verdict = verdict.rotate_right(1);
                        events.extend(more);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        assert!(!rx.awaiting_verdict(), "a completed body was answered");
        assert_eq!(rx.samples_pushed(), pushed as u64);
        assert!(
            rx.buffered_samples() <= bound + chunk.len(),
            "buffer {} > bound {} + chunk {}",
            rx.buffered_samples(),
            bound,
            chunk.len()
        );
    }
    let _ = rx.link_state();
    assert!(!rx.last_confidence_db().is_nan() || samples.is_empty());
    rx.reset();
    assert_eq!(rx.buffered_samples(), 0);
});
