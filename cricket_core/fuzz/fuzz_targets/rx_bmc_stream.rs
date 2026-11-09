//! The BMC (LTC) receiver on an arbitrary sample stream in arbitrary
//! chunks, at every catalog bit rate: never panics (NaN and infinities
//! included) and never reports a non-finite confidence (review A1: one
//! non-finite sample used to poison the IIR state for good).
#![no_main]

use arbitrary::Arbitrary;
use cricket_core::phy::bmc::BmcRx;
use cricket_core::phy::{PhyConfig, RxEvent};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    profile: u8,
    samples: Vec<u32>,
    chunks: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let config = match input.profile % 6 {
        0 => PhyConfig::ltc(),
        1 => PhyConfig::ltc_fec(),
        2 => PhyConfig::ltc_with_bit_rate(2400),
        3 => PhyConfig::ltc_fec_with_bit_rate(9600),
        4 => PhyConfig::ltc_fast_with_bit_rate(14400),
        _ => PhyConfig::ltc_fast_fec_with_bit_rate(14400),
    };
    let body_nibbles = config.body_nibbles();
    let mut rx = BmcRx::new(config);
    rx.set_expected_body_nibbles(body_nibbles);
    let samples: Vec<f32> = input.samples.iter().map(|&b| f32::from_bits(b)).collect();
    let mut chunks = input.chunks.iter().cycle();
    let mut pushed = 0usize;
    let mut at = 0;
    while at < samples.len() {
        let n = usize::from(*chunks.next().unwrap_or(&64)).max(1);
        let chunk = &samples[at..(at + n).min(samples.len())];
        at += chunk.len();
        pushed += chunk.len();
        for ev in rx.push_samples(chunk) {
            if let RxEvent::BodyNibble { result, .. } = ev {
                assert!(result.nibble < 16);
                assert!(!result.confidence_db.is_nan(), "NaN nibble confidence");
            }
        }
        assert_eq!(rx.samples_pushed(), pushed as u64);
        assert!(rx.last_confidence_db().is_finite(), "non-finite confidence");
    }
    let _ = rx.link_state();
    rx.reset();
});
