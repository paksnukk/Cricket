//! Receiver CPU load on preamble-like input: the profile's preamble tones
//! repeated back to back at the symbol rate, which makes every search
//! window near a preamble start a lock candidate. The receiver must not
//! retry a lock attempt that aligns to a position it has already rejected,
//! and the work per sample must stay within a small multiple of the
//! silence case.
//!
//! Numbers (run in release):
//!
//! ```sh
//! cargo test --release -p cricket_core --test search_load -- --ignored --nocapture
//! ```

use cricket_core::modem::CricketModemRx;
use cricket_core::phy::{Acquisition, ModemTx, PhyConfig};
use std::time::Instant;

fn profiles() -> Vec<(&'static str, PhyConfig)> {
    vec![
        ("robust", PhyConfig::robust()),
        ("robust_repeat", PhyConfig::robust_repeat()),
        ("fast", PhyConfig::fast()),
    ]
}

/// `seconds` of the preamble tones repeated at the symbol rate.
fn preamble_tones(config: &PhyConfig, seconds: usize) -> Vec<f32> {
    let tx = ModemTx::new(config.clone());
    let period = config.samples_per_symbol_period();
    let n = config.sample_rate_hz as usize * seconds / period + 1;
    let symbols: Vec<u8> = (0..n)
        .map(|i| config.preamble[i % config.preamble.len()])
        .collect();
    tx.synthesize_nibbles(&symbols)
}

struct Load {
    ns_per_sample: f64,
    /// Bodies that completed and failed to decode.
    crc_fail: u64,
}

fn measure(config: &PhyConfig, audio: &[f32]) -> Load {
    let mut rx = CricketModemRx::new(config.clone());
    let start = Instant::now();
    for chunk in audio.chunks(256) {
        rx.push_samples(chunk);
    }
    let elapsed = start.elapsed();
    Load {
        ns_per_sample: elapsed.as_nanos() as f64 / audio.len() as f64,
        crc_fail: rx.stats().crc_fail,
    }
}

/// A failed body is not read again from the same alignment: the number of
/// failed bodies on `seconds` of preamble tones is at most one per preamble
/// occurrence with first-symbol acquisition (Fast), at most two with joint
/// acquisition (a second attempt a quarter symbol off, from the
/// half-overlap window; see `ModemRx::is_failed_alignment`). Before the
/// receiver remembered failed alignments it was four to six.
#[test]
fn preamble_tones_fail_each_body_at_most_twice() {
    let seconds = 2;
    for (name, config) in profiles() {
        let audio = preamble_tones(&config, seconds);
        let period = config.samples_per_symbol_period();
        let occurrences = audio.len() / (period * config.preamble.len()) + 1;
        let per_occurrence = match config.acquisition {
            Acquisition::FirstSymbol => 1,
            Acquisition::Joint { .. } => 2,
        };
        let load = measure(&config, &audio);
        println!(
            "[{name}] {seconds} s of preamble tones: {} failed bodies, {} preamble occurrences",
            load.crc_fail, occurrences
        );
        assert!(
            load.crc_fail as usize <= per_occurrence * occurrences,
            "[{name}] {} failed bodies for {occurrences} preamble occurrences",
            load.crc_fail
        );
        assert!(load.crc_fail > 0, "[{name}] the tones never locked");
    }
}

/// CPU per sample on 5 s of preamble tones vs 5 s of silence (release).
#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn search_load_report() {
    let seconds = 5;
    println!("\n| profile | silence ns/sample | preamble tones ns/sample | failed bodies/s |");
    println!("|---|---|---|---|");
    for (name, config) in profiles() {
        let silence = vec![0.0f32; config.sample_rate_hz as usize * seconds];
        let quiet = measure(&config, &silence);
        let tones = preamble_tones(&config, seconds);
        let load = measure(&config, &tones);
        println!(
            "| {name} | {:.1} | {:.1} | {:.0} |",
            quiet.ns_per_sample,
            load.ns_per_sample,
            load.crc_fail as f64 / seconds as f64,
        );
    }
}
