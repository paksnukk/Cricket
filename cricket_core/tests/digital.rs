//! The experimental bit-transparent `digital` profile
//! (`docs/proposals/digital-transparent.md`): round trips, the fail-safe
//! table (paths that alter samples must never produce wrong MIDI), false
//! accepts on foreign audio, and coexistence with the other profiles.
//!
//! Reports (tables in the proposal):
//!
//! ```sh
//! cargo test --release -p cricket_core --test digital -- --ignored --nocapture
//! ```
//!
//! `DIGITAL_SECONDS` sets the audio per false-accept signal (default 60).

use cricket_core::modem::{
    refresh_spacing_samples, repetition_samples, CricketModemRx, CricketModemTx, ModemOutput,
};
use cricket_core::pathcheck::{self, PathImpairment, Rng, Verdict};
use cricket_core::phy::digital::{self, DigitalRx, CODE_SCALE, STEP, SYNC, TOLERANCE};
use cricket_core::phy::{Modulation, PhyConfig};
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage, TxScheduler};
use std::time::Instant;

const RATE: usize = 48_000;

fn random_message(rng: &mut Rng) -> TransportMessage {
    let ch = rng.range(0, 15) as u8;
    let d1 = rng.range(0, 127) as u8;
    let d2 = rng.range(1, 127) as u8;
    match rng.range(0, 9) {
        0 | 1 => TransportMessage::NoteOn {
            channel: ch,
            note: d1,
            velocity: d2,
        },
        2 | 3 => TransportMessage::NoteOff {
            channel: ch,
            note: d1,
            velocity: d2,
        },
        4 => TransportMessage::ControlChange {
            channel: ch,
            controller: d1,
            value: d2,
        },
        5 => TransportMessage::PitchBend {
            channel: ch,
            value: rng.range(0, 16_383) as u16,
        },
        6 => TransportMessage::ProgramChange {
            channel: ch,
            program: d1,
        },
        7 => TransportMessage::Msc(MscMessage::cue(
            MscCommand::Go,
            MscCue::from_tenths(rng.range(0, 9999) as u16).unwrap(),
            Some(rng.range(1, 15) as u8),
        )),
        8 => TransportMessage::Heartbeat,
        _ => TransportMessage::Panic,
    }
}

/// `n` random messages, each sent with the profile's K copies, separated by
/// 0–40 samples of silence (frames also follow each other directly).
fn traffic(config: &PhyConfig, n: usize, seed: u64) -> (Vec<TransportMessage>, Vec<f32>) {
    let mut rng = Rng::new(seed);
    let mut tx = CricketModemTx::new(config.clone());
    let mut audio = vec![0.0f32; 100];
    let mut sent = Vec::with_capacity(n);
    for _ in 0..n {
        let m = random_message(&mut rng);
        audio.extend(tx.encode_message(&m));
        let gap = rng.range(-20, 40).max(0) as usize;
        audio.extend(std::iter::repeat_n(0.0, gap));
        sent.push(m);
    }
    audio.extend(std::iter::repeat_n(0.0, 100));
    (sent, audio)
}

/// Decode `audio` in chunks of `chunk` samples.
fn decode(config: &PhyConfig, audio: &[f32], chunk: usize) -> Vec<TransportMessage> {
    let mut rx = CricketModemRx::new(config.clone());
    let mut out = Vec::new();
    for c in audio.chunks(chunk) {
        for o in rx.push_samples(c) {
            if let ModemOutput::Message(m) = o {
                out.push(m);
            }
        }
    }
    out
}

/// (delivered in order, false) — a delivered message that is not the next
/// matching sent message is false.
fn score(sent: &[TransportMessage], got: &[TransportMessage]) -> (usize, usize) {
    let mut at = 0usize;
    let (mut ok, mut bad) = (0, 0);
    for m in got {
        match sent[at..].iter().position(|s| s == m) {
            Some(i) => {
                ok += 1;
                at += i + 1;
            }
            None => bad += 1,
        }
    }
    (ok, bad)
}

#[test]
fn profile_selection_and_airtime() {
    let c = PhyConfig::from_name("digital").unwrap();
    assert!(matches!(c.modulation, Modulation::Digital(_)));
    assert_eq!(c.repetition_k, 2);
    assert!((c.estimated_first_copy_ms() - 21.0 / 48.0).abs() < 1e-4);
    assert_eq!(cricket_core::modem::repetition_samples(&c), 21);
    assert_eq!(cricket_core::modem::dedup_window_samples(&c), 52);
    let auto = PhyConfig::from_name("LTC9600+digital").unwrap();
    assert!(auto.digital_detect);
    assert!(matches!(auto.modulation, Modulation::Bmc(ref p) if p.bit_rate == 9600));
    assert!(!PhyConfig::from_name("ltc9600").unwrap().digital_detect);

    // One NoteOn: 2 x 21 samples, nothing else.
    let mut tx = CricketModemTx::new(c);
    let audio = tx.encode_message(&TransportMessage::NoteOn {
        channel: 0,
        note: 60,
        velocity: 100,
    });
    assert_eq!(audio.len(), 42);
    let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert_eq!(peak, digital::PEAK);
    let heartbeat = tx.encode_heartbeat();
    assert_eq!(heartbeat.len(), 30);
}

#[test]
fn roundtrip_all_kinds_chunks_offsets_polarity() {
    let config = PhyConfig::digital();
    let (sent, audio) = traffic(&config, 3000, 7);
    for chunk in [1, 7, 64, 512, 4096] {
        assert_eq!(decode(&config, &audio, chunk), sent, "chunk {chunk}");
    }
    let inverted: Vec<f32> = audio.iter().map(|x| -x).collect();
    assert_eq!(decode(&config, &inverted, 333), sent);
    // Other sample rates: the layout does not depend on the rate.
    for rate in [44_100, 96_000] {
        let c = PhyConfig::digital().with_sample_rate(rate);
        let (sent, audio) = traffic(&c, 200, 9);
        assert_eq!(decode(&c, &audio, 100), sent, "{rate} Hz");
    }
}

#[test]
fn second_copy_is_suppressed_and_bridges_a_hit_first_copy() {
    let config = PhyConfig::digital();
    let mut tx = CricketModemTx::new(config.clone());
    let msg = TransportMessage::NoteOn {
        channel: 3,
        note: 64,
        velocity: 1,
    };
    let audio = tx.encode_message(&msg);
    assert_eq!(decode(&config, &audio, 5), vec![msg.clone()]);
    for k in 0..21 {
        let mut hit = audio.clone();
        hit[k] = 0.3;
        assert_eq!(decode(&config, &hit, 64), vec![msg.clone()], "sample {k}");
    }
}

/// A note struck and released through the scheduler the way the encoder
/// drives it (polled every 1 ms while idle), with refreshes spaced
/// `spacing` samples apart. Returns the audio and where the user NoteOff
/// starts.
fn release_stream(spacing: u64) -> (Vec<f32>, usize) {
    let config = PhyConfig::digital();
    let msg = repetition_samples(&config) * config.repetition_k as u64;
    let mut s = TxScheduler::new(48_000, 24_000, msg)
        .with_refresh_schedule(cricket_core::modem::refresh_schedule(&config))
        .with_refresh_spacing(spacing);
    let mut tx = CricketModemTx::new(config);
    let note = |on: bool| {
        let (channel, note, velocity) = (3, 64, 90);
        if on {
            TransportMessage::NoteOn {
                channel,
                note,
                velocity,
            }
        } else {
            TransportMessage::NoteOff {
                channel,
                note,
                velocity,
            }
        }
    };
    let mut audio = vec![0.0f32; 480];
    let mut script = vec![(480usize, note(true)), (5_280, note(false))];
    let mut off_at = None;
    while audio.len() < RATE {
        let now = audio.len();
        while script.first().is_some_and(|(at, _)| *at <= now) {
            s.push(script.remove(0).1, now as u64);
        }
        match s.poll(now as u64) {
            Some(m) => {
                if matches!(m, TransportMessage::NoteOff { .. }) && off_at.is_none() {
                    off_at = Some(now);
                }
                audio.extend(if m == TransportMessage::Heartbeat {
                    tx.encode_heartbeat()
                } else {
                    tx.encode_message(&m)
                });
            }
            None => audio.extend([0.0; 48]),
        }
    }
    (audio, off_at.expect("NoteOff sent"))
}

/// Whether the note of [`release_stream`] is released after `mute_ms` of
/// silence from its NoteOff on (a dropout, e.g. lost network packets).
fn released_after_dropout(spacing: u64, mute_ms: usize) -> bool {
    let (mut audio, off_at) = release_stream(spacing);
    audio[off_at..off_at + mute_ms * RATE / 1000].fill(0.0);
    let got = decode(&PhyConfig::digital(), &audio, 256);
    assert!(matches!(got.first(), Some(TransportMessage::NoteOn { .. })));
    got.iter()
        .any(|m| matches!(m, TransportMessage::NoteOff { note: 64, .. }))
}

/// A release is sent 7 times (the NoteOff and 6 refreshes, K = 2 copies
/// each). Back to back (the consecutive schedule of spec §5.5 alone) that
/// is 6 ms on air, all lost in one 10 ms dropout; spaced 20 ms apart (the
/// digital profile's encoder setting) it survives a 100 ms dropout.
#[test]
fn spaced_refresh_survives_a_dropout() {
    let spacing = refresh_spacing_samples(&PhyConfig::digital(), 48_000);
    assert_eq!(spacing, 960);
    assert!(released_after_dropout(0, 0));
    assert!(!released_after_dropout(0, 10));
    for mute_ms in [0, 10, 50, 100] {
        assert!(released_after_dropout(spacing, mute_ms), "{mute_ms} ms");
    }
    assert!(!released_after_dropout(spacing, 130));
}

/// Conditions of the fail-safe table and what the receiver must do.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    /// Every message delivered.
    All,
    /// At least this fraction delivered.
    AtLeast(f64),
    /// Anything (but never a false message).
    Some,
    /// Nothing delivered.
    None,
}

fn expectation(name: &str) -> (Expect, &'static [Verdict]) {
    use Verdict::*;
    match name {
        "bit-exact" | "delay-1" | "polarity" => (Expect::All, &[BitExact]),
        "16bit-truncate" | "16bit-tpdf" | "float-2^23-1" | "int16-32767" | "noise-120dBFS"
        | "dc+1lsb16" | "gain+0.1dB" | "gain-0.1dB" => (Expect::All, &[DigitalOk]),
        "noise-100dBFS" => (Expect::AtLeast(0.999), &[DigitalOk, Marginal]),
        // Mild processing that keeps (almost) every frame sample within
        // tolerance: the bits arrive unchanged, so what is delivered is right.
        "gain-0.2dB+16bit-tpdf" | "eq+1dB@1kHz" => (Expect::Some, &[Marginal, Unusable]),
        _ => (Expect::None, &[Unusable]),
    }
}

fn fail_safe_row(name: &str, path: &[PathImpairment], n: usize) -> (usize, usize, usize, Verdict) {
    let config = PhyConfig::digital();
    let (sent, audio) = traffic(&config, n, 11);
    let got = decode(&config, &pathcheck::simulate(path, &audio), 256);
    let (ok, bad) = score(&sent, &got);
    let report = pathcheck::analyze(
        &pathcheck::simulate(path, &pathcheck::pattern_samples(30)),
        48_000,
    );
    let _ = name;
    (sent.len(), ok, bad, report.verdict)
}

/// The fail-safe requirement: whatever the path does, no wrong message.
/// Paths that keep every symbol within tolerance deliver everything.
#[test]
fn fail_safe_table() {
    for (name, path) in PathImpairment::presets() {
        let (n, ok, bad, verdict) = fail_safe_row(name, &path, 1500);
        assert_eq!(bad, 0, "{name}: false messages");
        let (expect, verdicts) = expectation(name);
        match expect {
            Expect::All => assert_eq!(ok, n, "{name}"),
            Expect::AtLeast(f) => assert!(ok as f64 >= f * n as f64, "{name}: {ok}/{n}"),
            Expect::Some => {}
            Expect::None => assert_eq!(ok, 0, "{name}"),
        }
        assert!(verdicts.contains(&verdict), "{name}: pathcheck {verdict:?}");
    }
}

#[test]
fn pathcheck_findings() {
    let pattern = pathcheck::pattern_samples(30);
    let run =
        |path: &[PathImpairment]| pathcheck::analyze(&pathcheck::simulate(path, &pattern), 48_000);
    let r = run(&[PathImpairment::Polarity]);
    assert!(r.inverted && r.verdict == Verdict::BitExact);
    let r = run(&[PathImpairment::Gain(-0.1)]);
    assert!((r.gain_db + 0.1).abs() < 0.002, "{}", r.gain_db);
    let r = run(&[PathImpairment::WordLength {
        bits: 16,
        mode: pathcheck::Requantize::Truncate,
    }]);
    assert_eq!(r.low_zero_bits, 8);
    assert_eq!(r.requantize, Some((16, pathcheck::Requantize::Truncate)));
    let r = run(&[PathImpairment::Resample(1.0 + 10e-6)]);
    let ppm = r.drift_ppm.expect("drift detected");
    assert!((ppm - 10.0).abs() < 2.0, "{ppm}");
    let r = run(&[PathImpairment::DcOffset(256)]);
    assert!((r.dc_codes - 256.0).abs() < 1.0);
    // Nothing but silence or unrelated audio: no signal.
    let r = pathcheck::analyze(&vec![0.0; 48_000], 48_000);
    assert_eq!(r.verdict, Verdict::NoSignal);
    let noise: Vec<f32> = {
        let mut rng = Rng::new(5);
        (0..48_000).map(|_| (0.1 * rng.gaussian()) as f32).collect()
    };
    assert_eq!(
        pathcheck::analyze(&noise, 48_000).verdict,
        Verdict::NoSignal
    );
}

// ---------------------------------------------------------------------------
// Foreign audio

fn white(n: usize, sigma_codes: f64, seed: u64, word_bits: u32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let q = (1u64 << (24 - word_bits)) as f64;
    (0..n)
        .map(|_| {
            let c = (sigma_codes * rng.gaussian() / q).round() * q;
            (c.clamp(-8_388_608.0, 8_388_607.0) / CODE_SCALE as f64) as f32
        })
        .collect()
}

/// Music-like: harmonic voices with envelopes, drum-ish noise hits, a pink
/// bed, scaled to `rms_dbfs`, with quiet passages and fades into a
/// dithered noise floor; quantized to `word_bits`.
fn music(n: usize, rms_dbfs: f64, seed: u64, word_bits: u32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0f64; n];
    for voice in 0..3 {
        let mut t = rng.range(0, 12_000) as usize;
        while t < n {
            let dur = 4800 + rng.range(0, 24_000) as usize;
            let note = 36 + rng.range(0, 60) + voice * 5;
            let f0 = 440.0 * 2f64.powf((note - 69) as f64 / 12.0);
            let harmonics = 1 + rng.range(0, 11);
            let level = 0.05 + 0.15 * rng.uniform();
            for k in 0..dur.min(n - t) {
                let env = (k as f64 / 480.0).min(1.0) * (-(k as f64) / dur as f64 * 2.0).exp();
                let ph = 2.0 * std::f64::consts::PI * f0 * k as f64 / RATE as f64;
                let v: f64 = (1..=harmonics)
                    .map(|h| (ph * h as f64).sin() / h as f64)
                    .sum();
                out[t + k] += level * env * v;
            }
            t += dur + rng.range(0, 6000) as usize;
        }
    }
    let (mut b0, mut b1, mut b2) = (0.0, 0.0, 0.0);
    let mut hit = 0usize;
    let mut hit_level = 0.0;
    for (i, s) in out.iter_mut().enumerate() {
        if i >= hit {
            hit = i + 6000 + rng.range(0, 24_000) as usize;
            hit_level = 0.1 + 0.3 * rng.uniform();
        }
        let w = rng.gaussian();
        b0 = 0.99765 * b0 + w * 0.0990460;
        b1 = 0.96300 * b1 + w * 0.2965164;
        b2 = 0.57000 * b2 + w * 1.0526913;
        let pink = (b0 + b1 + b2 + w * 0.1848) * 0.25;
        let since = (i + 24_000).saturating_sub(hit) as f64;
        let drum = if since < 9600.0 {
            hit_level * (-since / 2400.0).exp() * w
        } else {
            0.0
        };
        *s += 0.02 * pink + drum;
    }
    let rms = (out.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
    let g = 10f64.powf(rms_dbfs / 20.0) / rms;
    // Every 2 s: a 0.5 s fade from full level to -100 dB and back.
    let q = (1u64 << (24 - word_bits)) as f64;
    out.iter()
        .enumerate()
        .map(|(i, v)| {
            let ph = (i % 96_000) as f64 / 24_000.0;
            let fade = if ph < 1.0 {
                10f64.powf(-5.0 * (1.0 - (ph - 0.5).abs() * 2.0))
            } else {
                1.0
            };
            let c = v * g * fade * CODE_SCALE as f64 + q * (rng.uniform() + rng.uniform() - 1.0);
            ((c / q).round() * q).clamp(-8_388_608.0, 8_388_607.0) as f32 / CODE_SCALE
        })
        .collect()
}

/// Back-to-back traffic of another profile.
fn other_traffic(config: &PhyConfig, n: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut tx = CricketModemTx::new(config.clone());
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend(tx.encode_message(&random_message(&mut rng)));
    }
    out.truncate(n);
    out
}

#[derive(Default, Debug)]
struct Foreign {
    syncs: u64,
    frames: u64,
    messages: u64,
}

fn foreign(config: &PhyConfig, audio: &[f32]) -> Foreign {
    let mut phy = DigitalRx::new(config.clone());
    for &x in audio {
        phy.push(x);
    }
    let mut rx = CricketModemRx::new(config.clone());
    let mut messages = 0;
    for c in audio.chunks(512) {
        messages += rx
            .push_samples(c)
            .iter()
            .filter(|o| matches!(o, ModemOutput::Message(_)))
            .count() as u64;
    }
    let c = phy.counters();
    Foreign {
        syncs: c.syncs,
        frames: c.frames,
        messages,
    }
}

#[test]
fn foreign_audio_never_becomes_a_message() {
    let config = PhyConfig::digital();
    let n = 5 * RATE;
    // White noise through the worst-case level range, 24- and 16-bit.
    for (i, db) in [-90.0, -70.0, -60.0, -58.0, -50.0, -20.0, 0.0]
        .iter()
        .enumerate()
    {
        let sigma = CODE_SCALE as f64 * 10f64.powf(db / 20.0);
        for bits in [24, 16] {
            let r = foreign(&config, &white(n, sigma, i as u64, bits));
            assert_eq!(r.messages, 0, "{db} dBFS {bits} bit");
        }
    }
    for db in [-60.0, -20.0] {
        let r = foreign(&config, &music(n, db, 3, 24));
        assert_eq!((r.syncs, r.messages), (0, 0), "music {db}");
    }
    // Other profiles' traffic never shows a digital sync.
    for name in [
        "robust",
        "fast",
        "ltc",
        "ltc9600_fec",
        "ltc14400",
        "ltc14400_fec",
    ] {
        let r = foreign(
            &config,
            &other_traffic(&PhyConfig::from_name(name).unwrap(), n, 4),
        );
        assert_eq!(r.syncs, 0, "{name}");
        assert_eq!(r.messages, 0, "{name}");
    }
}

/// Digital frames are far below the other receivers' silence floor, and an
/// `+digital` receiver decodes both its own profile and digital frames.
#[test]
fn coexists_with_other_profiles() {
    let (sent, digital_audio) = traffic(&PhyConfig::digital(), 300, 21);
    for name in [
        "robust",
        "fast",
        "ltc",
        "ltc9600_fec",
        "ltc14400",
        "ltc14400_fec",
    ] {
        let c = PhyConfig::from_name(name).unwrap();
        assert!(decode(&c, &digital_audio, 512).is_empty(), "{name}");
        let auto = PhyConfig::from_name(&format!("{name}+digital")).unwrap();
        assert_eq!(decode(&auto, &digital_audio, 512), sent, "{name}+digital");
        let (own, own_audio) = traffic(&c, 6, 22);
        assert_eq!(decode(&auto, &own_audio, 512), own, "{name}+digital own");
    }
}

// ---------------------------------------------------------------------------
// Reports (ignored)

fn phi(x: f64) -> f64 {
    0.5 * (1.0 + libm::erf(x / std::f64::consts::SQRT_2))
}

/// P(a sample of Gaussian noise with `sigma` codes, on a 24- or 16-bit
/// grid, lands within tolerance of symbol `v`).
fn p_symbol(v: f64, sigma: f64, word_bits: u32) -> f64 {
    let q = (1u64 << (24 - word_bits)) as f64;
    let c = v * STEP as f64;
    let k = (TOLERANCE as f64 / q).floor() as i64;
    (-k..=k)
        .map(|j| {
            let x = c + j as f64 * q;
            phi((x + q / 2.0) / sigma) - phi((x - q / 2.0) / sigma)
        })
        .sum()
}

/// Valid packets (spec §4.7: version 1, known type, SEQ < 4, canonical
/// payload, CRC-16) as byte strings.
fn valid_packets() -> Vec<Vec<u8>> {
    use cricket_core::link::crc16_ccitt;
    use cricket_core::transport::MessageType;
    let mut out = Vec::new();
    for ty in [
        MessageType::Midi,
        MessageType::Msc,
        MessageType::Heartbeat,
        MessageType::Panic,
    ] {
        let payloads: Vec<[u8; 3]> = if ty.payload_len() == 0 {
            vec![]
        } else {
            (0..1u32 << 24)
                .map(|v| [(v >> 16) as u8, (v >> 8) as u8, v as u8])
                .filter(|p| ty.payload_in_range(p))
                .collect()
        };
        for seq in 0..4u8 {
            let hdr = 0x10 | ty as u8;
            let mut push = |body: &[u8]| {
                let mut b = body.to_vec();
                let crc = crc16_ccitt(&b);
                b.push((crc >> 8) as u8);
                b.push(crc as u8);
                out.push(b);
            };
            if payloads.is_empty() {
                push(&[hdr, seq]);
            }
            for p in &payloads {
                push(&[hdr, seq, p[0], p[1], p[2]]);
            }
        }
    }
    out
}

/// Analytic false-message rate for white Gaussian noise of `sigma` codes:
/// (P(sync, either polarity), P(END), P(the body is a valid packet)) per
/// sample position.
fn analytic_white(sigma: f64, word_bits: u32, packets: &[Vec<u8>]) -> (f64, f64, f64) {
    let p_sync = 2.0
        * SYNC
            .iter()
            .map(|&v| p_symbol(v as f64, sigma, word_bits))
            .product::<f64>();
    let p_end = p_symbol(digital::END as f64, sigma, word_bits);
    let p_nib: Vec<f64> = (0..16u8)
        .map(|n| p_symbol(digital::nibble_symbol(n) as f64, sigma, word_bits))
        .collect();
    let p_body: f64 = packets
        .iter()
        .map(|b| {
            b.iter()
                .map(|&x| p_nib[(x >> 4) as usize] * p_nib[(x & 15) as usize])
                .product::<f64>()
        })
        .sum();
    (p_sync, p_end, p_body)
}

/// Sync-prefix statistics: how many sample positions show the first `k`
/// sync symbols (either polarity, within tolerance), k = 1..=6.
fn sync_prefixes(audio: &[f32]) -> [u64; 7] {
    let sym: Vec<Option<i32>> = audio
        .iter()
        .map(|&x| {
            let (v, r) = digital::classify(digital::sample_code(x));
            (r.abs() <= TOLERANCE).then_some(v)
        })
        .collect();
    let mut counts = [0u64; 7];
    for i in 0..sym.len().saturating_sub(SYNC.len()) {
        for sign in [1, -1] {
            let k = (0..SYNC.len())
                .take_while(|&j| sym[i + j] == Some(sign * SYNC[j] as i32))
                .count();
            for c in counts.iter_mut().take(k + 1).skip(1) {
                *c += 1;
            }
        }
    }
    counts
}

#[test]
#[ignore]
fn digital_report() {
    let secs: usize = std::env::var("DIGITAL_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let per10y = RATE as f64 * 3600.0 * 24.0 * 365.25 * 10.0;
    let config = PhyConfig::digital();

    println!("\n## Fail-safe table (1500 random messages per path, K = 2)\n");
    println!("| path | delivered | false | pathcheck |");
    println!("|---|---|---|---|");
    for (name, path) in PathImpairment::presets() {
        let (n, ok, bad, verdict) = fail_safe_row(name, &path, 1500);
        println!("| {name} | {ok} / {n} | {bad} | {verdict:?} |");
    }

    println!("\n## Analytic false-message rate, white Gaussian noise\n");
    let packets = valid_packets();
    println!("valid packets: {}", packets.len());
    println!("| grid | sigma (steps) | level dBFS | P(sync) | syncs / 10 y | P(sync+END) | P(valid body) | years per false message |");
    println!("|---|---|---|---|---|---|---|---|");
    for bits in [24u32, 16] {
        for sigma_steps in [3.0, 5.0, 8.0, 10.0, 12.0, 16.0, 25.0] {
            let sigma = sigma_steps * STEP as f64;
            let (p_sync, p_end, p_body) = analytic_white(sigma, bits, &packets);
            let p_false = p_sync * p_end * p_body;
            println!(
                "| {bits}-bit | {sigma_steps} | {:.1} | {:.2e} | {:.3} | {:.2e} | {:.2e} | {:.2e} |",
                20.0 * (sigma / CODE_SCALE as f64).log10(),
                p_sync,
                p_sync * per10y,
                p_sync * p_end,
                p_body,
                10.0 / (p_false * per10y)
            );
        }
    }

    println!("\n## Foreign audio ({secs} s each)\n");
    println!("| signal | sync prefix k=1 | k=2 | k=3 | k=4 | syncs | frames | messages |");
    println!("|---|---|---|---|---|---|---|---|");
    let n = secs * RATE;
    let mut rows: Vec<(String, Vec<f32>)> = Vec::new();
    for db in [-70.0, -60.0, -58.0, -55.0, -40.0, 0.0] {
        for bits in [24, 16] {
            let sigma = CODE_SCALE as f64 * 10f64.powf(db / 20.0);
            rows.push((
                format!("white {db} dBFS {bits}-bit"),
                white(n, sigma, 77, bits),
            ));
        }
    }
    for db in [-20.0, -40.0, -60.0] {
        rows.push((format!("music {db} dBFS RMS 24-bit"), music(n, db, 8, 24)));
        rows.push((format!("music {db} dBFS RMS 16-bit"), music(n, db, 9, 16)));
    }
    for name in [
        "robust",
        "fast",
        "ltc",
        "ltc9600_fec",
        "ltc14400",
        "ltc14400_fec",
    ] {
        rows.push((
            format!("{name} traffic"),
            other_traffic(&PhyConfig::from_name(name).unwrap(), n, 5),
        ));
    }
    for (label, audio) in &rows {
        let r = foreign(&config, audio);
        let p = sync_prefixes(audio);
        println!(
            "| {label} | {} | {} | {} | {} | {} | {} | {} |",
            p[1], p[2], p[3], p[4], r.syncs, r.frames, r.messages
        );
    }

    println!("\n## Repetition: K = 1 vs K = 2 under glitches (5000 messages)\n");
    println!("| glitch | K=1 delivered | K=2 delivered | false |");
    println!("|---|---|---|---|");
    type Glitch = fn(&mut Vec<f32>, &mut Rng, f64);
    let glitches: [(&str, Glitch); 4] = [
        ("1 corrupted sample / 10 ms", |a, rng, rate| {
            for x in a.iter_mut() {
                if rng.uniform() < rate {
                    *x = (rng.uniform() - 0.5) as f32 * 0.01;
                }
            }
        }),
        ("1 dropped sample (slip) / 10 ms", |a, rng, rate| {
            let mut out = Vec::with_capacity(a.len());
            for &x in a.iter() {
                if rng.uniform() >= rate {
                    out.push(x);
                }
            }
            *a = out;
        }),
        ("1 repeated sample (slip) / 10 ms", |a, rng, rate| {
            let mut out = Vec::with_capacity(a.len());
            for &x in a.iter() {
                out.push(x);
                if rng.uniform() < rate {
                    out.push(x);
                }
            }
            *a = out;
        }),
        ("1 ms muted (packet loss) / 100 ms", |a, rng, rate| {
            let mut i = 0;
            while i < a.len() {
                if rng.uniform() < rate / 10.0 {
                    for s in a.iter_mut().skip(i).take(48) {
                        *s = 0.0;
                    }
                    i += 48;
                }
                i += 1;
            }
        }),
    ];
    for (label, glitch) in glitches {
        let mut cells = Vec::new();
        let mut false_total = 0;
        for k in [1usize, 2] {
            let mut c = PhyConfig::digital();
            c.repetition_k = k;
            let (sent, mut audio) = traffic(&c, 5000, 31);
            glitch(&mut audio, &mut Rng::new(32), 1.0 / 480.0);
            let (ok, bad) = score(&sent, &decode(&c, &audio, 256));
            false_total += bad;
            cells.push(format!("{:.2} %", 100.0 * ok as f64 / sent.len() as f64));
        }
        println!("| {label} | {} | {} | {false_total} |", cells[0], cells[1]);
    }

    println!("\n## CPU (DigitalRx, host)\n");
    let sigma = 10.0 * STEP as f64;
    let inputs: [(&str, Vec<f32>); 3] = [
        ("silence", vec![0.0; 10 * RATE]),
        (
            "white noise at the worst level",
            white(10 * RATE, sigma, 3, 24),
        ),
        (
            "back-to-back digital traffic",
            traffic(&config, 25_000, 4).1,
        ),
    ];
    for (label, audio) in inputs {
        let mut rx = CricketModemRx::new(config.clone());
        let t = Instant::now();
        for c in audio.chunks(256) {
            std::hint::black_box(rx.push_samples(c));
        }
        let modem_ns = t.elapsed().as_nanos() as f64 / audio.len() as f64;
        let mut phy = DigitalRx::new(config.clone());
        let t = Instant::now();
        for &x in &audio {
            std::hint::black_box(phy.push(x));
        }
        let phy_ns = t.elapsed().as_nanos() as f64 / audio.len() as f64;
        println!(
            "{label}: phy {phy_ns:.2} ns/sample, modem (incl. validation) {modem_ns:.2} ns/sample"
        );
    }
    let t = Instant::now();
    let mut tx = CricketModemTx::new(config.clone());
    let mut total = 0usize;
    for i in 0..100_000u32 {
        total += tx
            .encode_message(&TransportMessage::NoteOn {
                channel: 0,
                note: (i % 128) as u8,
                velocity: 100,
            })
            .len();
    }
    println!(
        "TX: {:.0} ns per message ({} samples)",
        t.elapsed().as_nanos() as f64 / 100_000.0,
        total / 100_000
    );
}
