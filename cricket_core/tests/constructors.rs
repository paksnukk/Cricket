//! Constructors never panic on out-of-range parameters: the BMC bit rate
//! and the RS16 parity are clamped to documented ranges, and `Rs16::new`
//! reports bad parameters instead of asserting.

mod support;

use cricket_core::link::{FecMode, Rs16, Rs16Error, MAX_PARITY};
use cricket_core::modem::{encode_to_samples, CricketModemRx, CricketModemTx, ModemOutput};
use cricket_core::phy::bmc::BIT_RATE_RANGE;
use cricket_core::phy::{BmcParams, Modulation, PhyConfig};
use cricket_core::transport::TransportMessage;
use support::decode_chunked;

fn note() -> TransportMessage {
    TransportMessage::NoteOn {
        channel: 0,
        note: 60,
        velocity: 100,
    }
}

fn bit_rate(config: &PhyConfig) -> u32 {
    match &config.modulation {
        Modulation::Bmc(p) => p.bit_rate,
        _ => panic!("BMC profile expected"),
    }
}

/// A bit rate of 0 (or anything absurd) is clamped, the profile stays
/// consistent with it, and a receiver built from it runs on input.
#[test]
fn bmc_bit_rate_is_clamped_without_panic() {
    assert_eq!(BIT_RATE_RANGE, 300..=100_000);
    assert_eq!(BmcParams::new(0).bit_rate, 300);
    assert_eq!(BmcParams::new(u32::MAX).bit_rate, 100_000);
    assert_eq!(BmcParams::fast(0).bit_rate, 300);
    assert_eq!(BmcParams::new(4800).bit_rate, 4800);

    for raw in [0u32, 1, 299, 100_001, u32::MAX] {
        for config in [
            PhyConfig::ltc_with_bit_rate(raw),
            PhyConfig::ltc_fec_with_bit_rate(raw),
            PhyConfig::ltc_fast_with_bit_rate(raw),
            PhyConfig::ltc_fast_fec_with_bit_rate(raw),
        ] {
            let rate = bit_rate(&config);
            assert!(BIT_RATE_RANGE.contains(&rate), "{raw} -> {rate}");
            // The symbol fields follow the clamped rate (4 bits per nibble).
            let want_ms = 4000.0 / rate as f32;
            assert!(
                (config.symbol_duration_ms - want_ms).abs() < 1e-3,
                "{raw}: symbol {} ms, want {want_ms}",
                config.symbol_duration_ms
            );
            assert!(config.estimated_first_copy_ms().is_finite());
            // TX and RX are constructed and run without panicking.
            let audio = CricketModemTx::new(config.clone()).encode_message(&note());
            assert!(!audio.is_empty() && audio.iter().all(|s| s.is_finite()));
            let mut rx = CricketModemRx::new(config.clone());
            let mut stats = None;
            for chunk in audio.chunks(256).chain(vec![0.0f32; 4096].chunks(256)) {
                for out in rx.push_samples(chunk) {
                    if let ModemOutput::Stats(s) = out {
                        stats = Some(s);
                    }
                }
            }
            assert!(stats.is_some(), "{raw}: no stats");
        }
    }
}

/// At the clamped minimum the BMC profile is slow but still works end to end.
#[test]
fn bmc_minimum_bit_rate_round_trips() {
    let config = PhyConfig::ltc_with_bit_rate(0);
    assert_eq!(bit_rate(&config), 300);
    let samples = encode_to_samples(config.clone(), &[note()]);
    assert_eq!(decode_chunked(config, &samples, 1024), vec![note()]);
}

/// `Rs16::new` validates instead of asserting.
#[test]
fn rs16_new_reports_bad_params() {
    assert!(Rs16::new(7, 4).is_ok());
    assert!(Rs16::new(7, 8).is_ok());
    assert_eq!(Rs16::new(7, 0).err(), Some(Rs16Error::BadParams));
    assert_eq!(Rs16::new(0, 4).err(), Some(Rs16Error::BadParams));
    assert_eq!(Rs16::new(7, 9).err(), Some(Rs16Error::BadParams));
    assert_eq!(Rs16::new(15, 1).err(), Some(Rs16Error::BadParams));
}

/// An out-of-range `Rs16` parity is clamped to 1..=8 consistently for the
/// body size, the erasure limit and the codec, so a transmitter and a
/// receiver built from the same `FecMode` still talk to each other.
#[test]
fn fec_parity_is_clamped_without_panic() {
    assert_eq!(MAX_PARITY, 8);
    for (parity, want) in [(0u8, 1usize), (1, 1), (4, 4), (8, 8), (9, 8), (255, 8)] {
        let mode = FecMode::Rs16 { parity };
        assert_eq!(mode.parity(), want, "parity {parity}");
        assert_eq!(mode.body_nibbles(), 2 * (7 + want), "parity {parity}");
        assert_eq!(mode.max_body_erasures(), 2 * want, "parity {parity}");
        for base in [PhyConfig::robust(), PhyConfig::ltc()] {
            let config = PhyConfig { fec: mode, ..base };
            // Both constructors used to reach `Rs16::new`'s assertion.
            let mut tx = CricketModemTx::new(config.clone());
            let audio = tx.encode_message(&note());
            let got = decode_chunked(config, &audio, 256);
            assert_eq!(got, vec![note()], "parity {parity}");
        }
    }
    assert_eq!(FecMode::None.parity(), 0);
}
