//! Level-independent detection (plan task A3): the receiver must decode
//! attenuated signals down to -30 dB below the nominal transmit level (peak
//! -12 dBFS, amplitude 0.25),
//! while noise, near-silence and interfering tones never produce messages.
//! Uses the simulator in `tests/support`.

mod support;

use cricket_core::modem::encode_to_samples;
use cricket_core::phy::PhyConfig;
use cricket_core::transport::TransportMessage;
use std::f32::consts::PI;
use support::{decode_chunked, ChannelSim, XorShift64};

const CHUNK_SIZES: [usize; 5] = [64, 128, 240, 512, 4096];

fn note_on_for(i: usize) -> TransportMessage {
    TransportMessage::NoteOn {
        channel: 0,
        note: (i % 128) as u8,
        velocity: (1 + (i * 7) % 127) as u8,
    }
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Decodes `trials` single messages at `gain_db` (optional SNR relative to the
/// attenuated signal) with random offsets and chunk sizes. Returns
/// (exact decodes, first failure descriptions).
fn level_trials(
    config: &PhyConfig,
    gain_db: f32,
    snr_db: Option<f32>,
    trials: usize,
    seed: u64,
) -> (usize, Vec<String>) {
    let period = config.samples_per_symbol_period();
    let mut rng = XorShift64::new(seed);
    let mut ok = 0usize;
    let mut failures = Vec::new();
    for trial in 0..trials {
        let offset = rng.below(4 * period);
        let chunk = CHUNK_SIZES[rng.below(CHUNK_SIZES.len())];
        let messages = [note_on_for(trial)];
        let clean = encode_to_samples(config.clone(), &messages);
        let mut sim = ChannelSim::new(config.clone(), rng.next_u64())
            .offset(offset)
            .gain(db_to_gain(gain_db));
        if let Some(snr) = snr_db {
            sim = sim.snr_db(snr);
        }
        let decoded = decode_chunked(config.clone(), &sim.apply(&clean), chunk);
        if decoded == messages {
            ok += 1;
        } else if failures.len() < 5 {
            failures.push(format!(
                "trial {trial} offset {offset} chunk {chunk}: got {decoded:?}"
            ));
        }
    }
    (ok, failures)
}

fn level_sweep(name: &str, config: PhyConfig) {
    const TRIALS: usize = 50;
    let mut summary = Vec::new();
    let mut failures = Vec::new();
    for step in 0..=10 {
        let gain_db = -30.0 + 3.0 * step as f32;
        let (ok, f) = level_trials(&config, gain_db, None, TRIALS, 0xA3_0000 + step as u64);
        summary.push(format!("{gain_db:+.0} dB: {ok}/{TRIALS}"));
        if ok != TRIALS {
            failures.extend(f.into_iter().map(|s| format!("[{gain_db:+.0} dB] {s}")));
        }
    }
    eprintln!("[{name}] clean level sweep: {}", summary.join(", "));
    assert!(
        failures.is_empty(),
        "[{name}] clean level sweep must decode 100%: {}\nfirst failures:\n{}",
        summary.join(", "),
        failures.join("\n")
    );
}

fn low_level_with_noise(name: &str, config: PhyConfig) {
    const TRIALS: usize = 200;
    let (ok, failures) = level_trials(&config, -30.0, Some(20.0), TRIALS, 0xA3_F00D);
    eprintln!("[{name}] -30 dB @ SNR 20 dB: {ok}/{TRIALS}");
    assert!(
        ok * 100 >= TRIALS * 99,
        "[{name}] -30 dB @ SNR 20 dB: {ok}/{TRIALS} exact decodes (< 99%); failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn level_sweep_robust() {
    level_sweep("robust", PhyConfig::robust());
}

#[test]
fn level_sweep_robust_repeat() {
    level_sweep("robust_repeat", PhyConfig::robust_repeat());
}

#[test]
fn level_sweep_fast() {
    level_sweep("fast", PhyConfig::fast());
}

#[test]
fn minus_30db_snr20_robust() {
    low_level_with_noise("robust", PhyConfig::robust());
}

#[test]
fn minus_30db_snr20_robust_repeat() {
    low_level_with_noise("robust_repeat", PhyConfig::robust_repeat());
}

#[test]
fn minus_30db_snr20_fast() {
    low_level_with_noise("fast", PhyConfig::fast());
}

// ---------------------------------------------------------------- false alarms

const FALSE_ALARM_SECONDS: usize = 60;
const NOISE_SIGMAS: [f64; 4] = [0.001, 0.05, 0.3, 1.0];

fn white_noise(seconds: usize, sigma: f64, sample_rate: u32, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    (0..seconds * sample_rate as usize)
        .map(|_| (rng.gaussian() * sigma) as f32)
        .collect()
}

/// A random sinusoid (phase-continuous, random amplitude 0.05..0.8) whose
/// frequency changes every 5 ms, plus white noise (sigma 0.01). With
/// `grid = Some(config)` each frequency is snapped to that profile's tone
/// table (worst case: every segment is a valid MFSK symbol); otherwise it is
/// uniform in 2..6 kHz.
fn tone_soup(seconds: usize, sample_rate: u32, grid: Option<&PhyConfig>, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let total = seconds * sample_rate as usize;
    let segment = sample_rate as usize * 5 / 1000;
    let mut out = Vec::with_capacity(total);
    let mut phase = 0.0f32;
    while out.len() < total {
        let freq = match grid {
            Some(cfg) => cfg.tone_hz(rng.below(cfg.n_tones()) as u8),
            None => 2000.0 + 4000.0 * rng.next_f64() as f32,
        };
        let amp = 0.05 + 0.75 * rng.next_f64() as f32;
        let inc = 2.0 * PI * freq / sample_rate as f32;
        for _ in 0..segment.min(total - out.len()) {
            out.push(amp * phase.sin() + (rng.gaussian() * 0.01) as f32);
            phase = (phase + inc) % (2.0 * PI);
        }
    }
    out
}

fn assert_no_messages(name: &str, what: &str, config: &PhyConfig, audio: &[f32]) {
    let decoded = decode_chunked(config.clone(), audio, 512);
    assert!(
        decoded.is_empty(),
        "[{name}] {what}: {} spurious messages: {:?}",
        decoded.len(),
        decoded.iter().take(10).collect::<Vec<_>>()
    );
}

fn noise_false_alarm(name: &str, config: PhyConfig) {
    let sr = config.sample_rate_hz;
    for (i, &sigma) in NOISE_SIGMAS.iter().enumerate() {
        let audio = white_noise(FALSE_ALARM_SECONDS, sigma, sr, 0xA3_0A00 + i as u64);
        assert_no_messages(name, &format!("white noise sigma={sigma}"), &config, &audio);
    }
    let audio = white_noise(5, 1e-5, sr, 0xA3_0B00);
    assert_no_messages(name, "near-silence sigma=1e-5", &config, &audio);
}

fn soup_false_alarm(name: &str, config: PhyConfig) {
    let sr = config.sample_rate_hz;
    let audio = tone_soup(FALSE_ALARM_SECONDS, sr, None, 0xA3_0C00);
    assert_no_messages(name, "tone soup 2-6 kHz", &config, &audio);
    let audio = tone_soup(FALSE_ALARM_SECONDS / 2, sr, Some(&config), 0xA3_0D00);
    assert_no_messages(name, "on-grid tone soup", &config, &audio);
}

#[test]
fn noise_false_alarm_robust() {
    noise_false_alarm("robust", PhyConfig::robust());
}

#[test]
fn noise_false_alarm_robust_repeat() {
    noise_false_alarm("robust_repeat", PhyConfig::robust_repeat());
}

#[test]
fn noise_false_alarm_fast() {
    noise_false_alarm("fast", PhyConfig::fast());
}

#[test]
fn tone_soup_false_alarm_robust() {
    soup_false_alarm("robust", PhyConfig::robust());
}

#[test]
fn tone_soup_false_alarm_robust_repeat() {
    soup_false_alarm("robust_repeat", PhyConfig::robust_repeat());
}

#[test]
fn tone_soup_false_alarm_fast() {
    soup_false_alarm("fast", PhyConfig::fast());
}
