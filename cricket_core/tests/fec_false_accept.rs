//! False-accept analysis for the FEC profiles (`ltc_fec`, `ltc2400_fec`,
//! `ltc9600_fec`, `ltc14400_fec`, `robust`): how often non-Cricket audio turns into a
//! message. See `docs/proposals/fec.md` §9 for the analysis and target.
//!
//! A false message needs (1) a false lock whose body survives the phy gates
//! ("false body"), (2) a pair of RS codeword decodings ("candidate", the
//! pre-CRC event: both RS decoders report success), and (3) a candidate that
//! passes every packet check. The report measures (1) and (2) directly and
//! the structural checks (version, type, pad, payload) per candidate; the
//! CRC-16 factor (2^-16) is then applied analytically, so a false-message
//! rate of one per decades can be estimated from minutes of audio.
//!
//! ```sh
//! cargo test --release -p cricket_core --test fec_false_accept -- --ignored --nocapture
//! ```
//!
//! Environment: `FALSE_SECONDS` (audio per signal, default 120),
//! `FALSE_PROFILES` (comma-separated: ltc_fec, ltc2400_fec, ltc9600_fec,
//! ltc14400_fec, robust), `FALSE_RANDOM_BODIES` (simulated random bodies per row,
//! default 200000). Results are recorded in `docs/proposals/fec.md` §9.

mod support;

use cricket_core::link::{
    crc16_ccitt, nibbles_to_bytes, BodySymbol, FecCodec, PROTOCOL_VERSION, SEQ_MODULUS,
};
use cricket_core::modem::{encode_to_samples, CricketModemTx};
use cricket_core::phy::{PhyConfig, PhyRx, RxEvent};
use cricket_core::transport::{MessageType, TransportMessage};
use support::{ChannelSim, XorShift64};

const RATE: usize = 48_000;
/// CRC-16 pass probability for a random candidate.
const CRC_FACTOR: f64 = 1.0 / 65_536.0;

// ---------------------------------------------------------------------------
// Signals (48 kHz mono)

fn white(seconds: f32, sigma: f64, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    (0..(seconds * RATE as f32) as usize)
        .map(|_| (rng.gaussian() * sigma) as f32)
        .collect()
}

/// Back-to-back traffic of `config` itself with link key `key` (spec §4.1:
/// a neighbouring link, or the far end configured differently), SNR 20 dB.
fn other_key_traffic(config: &PhyConfig, key: u16, seconds: f32, seed: u64) -> Vec<f32> {
    let keyed = config.clone().with_link_key(key);
    let n = (seconds * RATE as f32) as usize;
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while out.len() < n {
        let msgs: Vec<_> = (0..20)
            .map(|k| TransportMessage::NoteOn {
                channel: ((i + k) % 16) as u8,
                note: ((i + k) % 128) as u8,
                velocity: 1 + ((i + k) * 7 % 127) as u8,
            })
            .collect();
        i += 20;
        let block = encode_to_samples(keyed.clone(), &msgs);
        out.extend(
            ChannelSim::new(keyed.clone(), seed ^ i as u64)
                .snr_db(20.0)
                .apply(&block),
        );
    }
    out.truncate(n);
    out
}

/// Back-to-back MFSK NoteOns of `profiles` (equal shares) at SNR 20 dB.
fn mfsk_traffic(seconds: f32, seed: u64, profiles: &[PhyConfig]) -> Vec<f32> {
    let n = (seconds * RATE as f32) as usize;
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while out.len() < n {
        let m = &profiles[(out.len() * profiles.len() / n.max(1)).min(profiles.len() - 1)];
        let mut tx = CricketModemTx::new(m.clone());
        let mut block = Vec::new();
        while block.len() < RATE {
            block.extend(tx.encode_message(&TransportMessage::NoteOn {
                channel: 0,
                note: (i % 128) as u8,
                velocity: 1 + (i * 7 % 127) as u8,
            }));
            i += 1;
        }
        out.extend(
            ChannelSim::new(m.clone(), seed ^ i as u64)
                .snr_db(20.0)
                .apply(&block),
        );
    }
    out.truncate(n);
    out
}

/// Back-to-back BMC traffic of `config` (for the MFSK receiver), SNR 20 dB.
fn bmc_traffic(config: &PhyConfig, seconds: f32, seed: u64) -> Vec<f32> {
    let n = (seconds * RATE as f32) as usize;
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while out.len() < n {
        let msgs: Vec<_> = (0..50)
            .map(|k| TransportMessage::NoteOn {
                channel: 0,
                note: ((i + k) % 128) as u8,
                velocity: 1 + ((i + k) * 7 % 127) as u8,
            })
            .collect();
        i += 50;
        let block = encode_to_samples(config.clone(), &msgs);
        out.extend(
            ChannelSim::new(config.clone(), seed ^ i as u64)
                .snr_db(20.0)
                .apply(&block),
        );
    }
    out.truncate(n);
    out
}

/// Clicks (1–4 sample spikes, random sign and level) on a quiet noise floor.
fn clicks(seconds: f32, per_second: f64, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    let n = (seconds * RATE as f32) as usize;
    let mut out: Vec<f32> = (0..n).map(|_| (rng.gaussian() * 0.001) as f32).collect();
    let p = per_second / RATE as f64;
    let mut i = 0;
    while i < n {
        if rng.next_f64() < p {
            let amp =
                (0.1 + 0.8 * rng.next_f64()) as f32 * if rng.below(2) == 0 { 1.0 } else { -1.0 };
            let width = 1 + rng.below(4);
            for s in out.iter_mut().skip(i).take(width) {
                *s += amp;
            }
        }
        i += 1;
    }
    out
}

/// Music-like program material (`support::music`).
fn music(seconds: f32, seed: u64) -> Vec<f32> {
    support::music(RATE, seconds, seed)
}

/// Speech-like program material (`support::speech`).
fn speech(seconds: f32, seed: u64) -> Vec<f32> {
    support::speech(RATE, seconds, seed)
}

/// SMPTE/EBU LTC timecode (80-bit frames, biphase mark) at `fps`.
fn smpte_ltc(seconds: f32, fps: u32) -> Vec<f32> {
    let bit_samples = RATE as f32 / (80.0 * fps as f32);
    let n = (seconds * RATE as f32) as usize;
    let mut out = Vec::with_capacity(n);
    let mut level = 0.5f32;
    let mut pos = 0.0f32;
    let mut frame = 0u32;
    let mut lp = 0.0f32;
    while out.len() < n {
        let f = frame % fps;
        let s = (frame / fps) % 60;
        let m = (frame / fps / 60) % 60;
        let mut bits = [0u8; 80];
        let mut put = |at: usize, width: usize, v: u32| {
            for b in 0..width {
                bits[at + b] = ((v >> b) & 1) as u8;
            }
        };
        put(0, 4, f % 10);
        put(8, 2, f / 10);
        put(16, 4, s % 10);
        put(24, 3, s / 10);
        put(32, 4, m % 10);
        put(40, 3, m / 10);
        put(4, 4, frame.wrapping_mul(2_654_435_761) >> 28);
        put(64, 16, 0xBFFC); // sync word 0011 1111 1111 1101, LSB first
        for &bit in &bits {
            for half in 0..2 {
                if half == 0 || bit == 1 {
                    level = -level;
                }
                pos += bit_samples / 2.0;
                while (out.len() as f32) < pos {
                    // ~25 µs rise time.
                    lp += 0.6 * (level - lp);
                    out.push(lp);
                }
            }
        }
        frame += 1;
    }
    out.truncate(n);
    out
}

fn signals(config: &PhyConfig, seconds: f32) -> Vec<(&'static str, Vec<f32>)> {
    let bmc = matches!(config.modulation, cricket_core::phy::Modulation::Bmc(_));
    let third = seconds / 3.0;
    let mut noise = white(third, 0.003, 1);
    noise.extend(white(third, 0.05, 2));
    noise.extend(white(third, 0.5, 3));
    let mut click = clicks(seconds / 2.0, 100.0, 4);
    click.extend(clicks(seconds / 2.0, 1000.0, 5));
    let mut ltc = smpte_ltc(seconds / 2.0, 30);
    ltc.extend(smpte_ltc(seconds / 2.0, 25));
    let mut v = vec![
        ("white noise", noise),
        ("clicks", click),
        ("music", music(seconds, 6)),
        ("speech", speech(seconds, 7)),
        ("SMPTE LTC", ltc),
        // A neighbouring link of the same profile with another link key.
        (OTHER_KEY, other_key_traffic(config, 0x1234, seconds, 12)),
    ];
    if bmc {
        let mfsk = [
            PhyConfig::robust_repeat(),
            PhyConfig::fast(),
            PhyConfig::robust(),
        ];
        v.insert(0, ("MFSK traffic", mfsk_traffic(seconds, 8, &mfsk)));
        // Misconfiguration: an uncoded LTC transmitter at the same bit rate.
        let uncoded = PhyConfig {
            fec: cricket_core::link::FecMode::None,
            ..config.clone()
        };
        v.push(("uncoded LTC traffic", bmc_traffic(&uncoded, seconds, 10)));
    } else {
        v.insert(
            0,
            (
                "BMC traffic",
                bmc_traffic(&PhyConfig::ltc_fec(), seconds, 9),
            ),
        );
        // Misconfiguration: uncoded MFSK transmitters (robust_repeat, fast).
        let uncoded_mfsk = [PhyConfig::robust_repeat(), PhyConfig::fast()];
        v.push((
            "uncoded MFSK traffic",
            mfsk_traffic(seconds, 11, &uncoded_mfsk),
        ));
    }
    v
}

// ---------------------------------------------------------------------------
// Candidate checks (mirroring `CricketModemRx`)

/// Packet checks a candidate data block passes, cheapest first.
#[derive(Debug, Default, Clone, Copy)]
struct Checks {
    /// Version nibble and type nibble valid.
    header: bool,
    /// ... and the SEQ reserved bits and the bytes after the packet are zero.
    pad: bool,
    /// ... and the payload is canonical (spec §5.1).
    payload: bool,
    /// CRC-16 matches.
    crc: bool,
}

fn checks(nibbles: &[u8]) -> Checks {
    let bytes = nibbles_to_bytes(nibbles);
    let mut c = Checks::default();
    let Some(t) = MessageType::from_u8(bytes[0] & 0x0F) else {
        return c;
    };
    if bytes[0] >> 4 != PROTOCOL_VERSION {
        return c;
    }
    c.header = true;
    let needed = 2 + t.payload_len() + 2;
    c.pad = bytes[1] < SEQ_MODULUS && bytes[needed..].iter().all(|&b| b == 0);
    c.payload = c.pad && t.payload_in_range(&bytes[2..2 + t.payload_len()]);
    let crc = crc16_ccitt(&bytes[..needed - 2]);
    c.crc = bytes[needed - 2] == (crc >> 8) as u8 && bytes[needed - 1] == crc as u8;
    c
}

#[derive(Debug, Default, Clone, Copy)]
struct FalseStats {
    seconds: f64,
    /// Bodies handed to the FEC decoder (passed every phy gate).
    bodies: u64,
    /// Candidate packets (pairs of successful RS decodes) checked.
    candidates: u64,
    header_pass: u64,
    /// Structurally valid apart from the CRC and the payload check.
    struct_lenient: u64,
    /// ... with canonical payloads.
    struct_strict: u64,
    /// Bodies the decoder actually accepted (false messages).
    false_msgs: u64,
}

impl FalseStats {
    fn per_min(&self, x: u64) -> f64 {
        x as f64 * 60.0 / self.seconds
    }

    /// Expected years of this audio per false message, conservatively: the
    /// larger of the observed structural passes and `analytic` (structural
    /// pass probability of a uniformly random candidate) times the
    /// `candidates`, times the CRC-16 factor.
    fn years_per_false(&self, structural: u64, candidates: u64, analytic: f64) -> f64 {
        let per_sec =
            (structural as f64).max(candidates as f64 * analytic) / self.seconds * CRC_FACTOR;
        1.0 / per_sec / (3600.0 * 8766.0)
    }
}

/// A neighbouring link's own packets (spec §4.1): structurally valid, and
/// rejected by the CRC deterministically, not with probability 2^-16, so
/// they are left out of the estimate; only the other candidates count.
const OTHER_KEY: &str = "other-key traffic";

/// The modem's receive flow (`CricketModemRx`) with the candidate checks
/// counted. A decoded body is a false message (the audio holds no packet
/// for this profile).
fn run_false(config: &PhyConfig, audio: &[f32]) -> FalseStats {
    let mut phy = PhyRx::new(config.clone());
    let body_len = config.fec.body_nibbles();
    phy.set_expected_body_nibbles(body_len);
    let fec = FecCodec::new(config.fec).expect("FEC profile");
    let mut st = FalseStats {
        seconds: audio.len() as f64 / config.sample_rate_hz as f64,
        ..FalseStats::default()
    };
    let mut body: Vec<BodySymbol> = Vec::new();
    let mut collecting = false;
    for chunk in audio.chunks(512) {
        let mut events: std::collections::VecDeque<RxEvent> = phy.push_samples(chunk).into();
        while let Some(ev) = events.pop_front() {
            match ev {
                RxEvent::PreambleLocked { .. } => {
                    collecting = true;
                    body.clear();
                }
                RxEvent::BodyNibble { result, .. } if collecting => {
                    body.push(BodySymbol {
                        nibble: result.nibble,
                        reliability: result.confidence_db,
                        erased: result.erased,
                    });
                    if body.len() < body_len {
                        continue;
                    }
                    collecting = false;
                    st.bodies += 1;
                    let decoded = fec.decode_body(&body, |nibbles| {
                        st.candidates += 1;
                        let c = checks(nibbles);
                        st.header_pass += c.header as u64;
                        st.struct_lenient += c.pad as u64;
                        st.struct_strict += c.payload as u64;
                        cricket_core::link::fec_candidate_is_valid(nibbles)
                    });
                    let more = if decoded.is_some() {
                        st.false_msgs += 1;
                        phy.confirm()
                    } else {
                        phy.reject()
                    };
                    events.extend(more);
                }
                RxEvent::SyncLost => collecting = false,
                _ => {}
            }
        }
    }
    st
}

const TYPES: [MessageType; 4] = [
    MessageType::Midi,
    MessageType::Msc,
    MessageType::Heartbeat,
    MessageType::Panic,
];

/// Canonical payloads of `t` (spec §5.1), counted over all byte values.
fn canonical_payloads(t: MessageType) -> u64 {
    match t.payload_len() {
        0 => 1,
        3 => (0..1u32 << 24)
            .filter(|v| t.payload_in_range(&v.to_be_bytes()[1..]))
            .count() as u64,
        n => unreachable!("payload length {n}"),
    }
}

/// Probability that a uniformly random 14-nibble data block passes the
/// structural checks: (lenient: header, SEQ and pad only; strict: and a
/// canonical payload). Version 1/16, type 1/16 each, SEQ 2^-6, then per
/// type the pad bits and the canonical fraction of the payload.
fn analytic_struct() -> (f64, f64) {
    let mut lenient = 0.0;
    let mut strict = 0.0;
    for t in TYPES {
        let pad_bits = 8 * (3 - t.payload_len() as i32);
        let p_pad = 2f64.powi(-pad_bits);
        lenient += p_pad;
        let space = 2f64.powi(8 * t.payload_len() as i32);
        strict += p_pad * canonical_payloads(t) as f64 / space;
    }
    let header_seq = 1.0 / (16.0 * 16.0 * 64.0);
    (lenient * header_seq, strict * header_seq)
}

fn env_list(name: &str, default: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Uniformly random bodies (random nibbles and reliabilities, no erasures or
/// `erasures` gate erasures): the analytic model, simulated.
fn random_bodies(config: &PhyConfig, bodies: usize, erasures: usize, seed: u64) -> FalseStats {
    let fec = FecCodec::new(config.fec).expect("FEC profile");
    let mut rng = XorShift64::new(seed);
    let mut st = FalseStats {
        seconds: 60.0,
        bodies: bodies as u64,
        ..FalseStats::default()
    };
    for _ in 0..bodies {
        let mut body: Vec<BodySymbol> = (0..config.fec.body_nibbles())
            .map(|_| BodySymbol {
                nibble: rng.below(16) as u8,
                reliability: (rng.next_f64() * 20.0) as f32,
                erased: false,
            })
            .collect();
        let mut erased = 0;
        while erased < erasures {
            let i = rng.below(body.len());
            if !body[i].erased {
                body[i].erased = true;
                body[i].reliability = 0.0;
                erased += 1;
            }
        }
        fec.decode_body(&body, |nibbles| {
            st.candidates += 1;
            let c = checks(nibbles);
            st.header_pass += c.header as u64;
            st.struct_lenient += c.pad as u64;
            st.struct_strict += c.payload as u64;
            cricket_core::link::fec_candidate_is_valid(nibbles)
        })
        .is_some()
        .then(|| st.false_msgs += 1);
    }
    st
}

// ---------------------------------------------------------------------------
// Fast guards (the full measurement is `false_accept_report` below)

/// Uniformly random bodies: never a message, and the decoder offers few
/// candidates to the CRC (before the weight bound: 2.35 / 2.0 / 1.45 per
/// body at 0 / 4 / 8 gate erasures; with it: 0.04 / 0.18 / 1.02).
#[test]
fn random_bodies_rarely_reach_the_crc() {
    let config = PhyConfig::ltc_fec();
    for (erasures, max_per_body) in [(0usize, 0.1), (4, 0.35), (8, 1.3)] {
        let st = random_bodies(&config, 5_000, erasures, 0xF00D + erasures as u64);
        let per_body = st.candidates as f64 / st.bodies as f64;
        assert_eq!(st.false_msgs, 0, "{erasures} erasures");
        assert!(
            per_body < max_per_body,
            "{erasures} erasures: {per_body:.3} candidates per body"
        );
    }
}

/// Worst cases measured for the false-accept analysis, a few seconds each:
/// MFSK traffic into `ltc2400_fec`, and transmitters without FEC on the
/// same phy (uncoded LTC into `ltc_fec` and `ltc14400_fec`, uncoded Robust
/// repeat / Fast into `robust`). Before the weight bound and the whitening
/// these reached the CRC with 1.7 / 1.8 / 2.3 candidates per false body.
#[test]
fn worst_case_audio_rarely_reaches_the_crc() {
    let ltc_fec = PhyConfig::ltc_fec();
    let uncoded = PhyConfig {
        fec: cricket_core::link::FecMode::None,
        ..ltc_fec.clone()
    };
    let mfsk = [
        PhyConfig::robust_repeat(),
        PhyConfig::fast(),
        PhyConfig::robust(),
    ];
    let uncoded_mfsk = [PhyConfig::robust_repeat(), PhyConfig::fast()];
    let cases = [
        (
            "MFSK traffic -> ltc2400_fec",
            PhyConfig::from_name("ltc2400_fec").unwrap(),
            mfsk_traffic(6.0, 21, &mfsk),
            0.9,
        ),
        (
            "uncoded LTC -> ltc_fec",
            ltc_fec,
            bmc_traffic(&uncoded, 2.0, 22),
            0.15,
        ),
        (
            "uncoded LTC -> ltc14400_fec",
            PhyConfig::from_name("ltc14400_fec").unwrap(),
            // Fewer false bodies per second (the next burst's sync
            // replaces most false locks), so more audio.
            bmc_traffic(&PhyConfig::from_name("ltc14400").unwrap(), 20.0, 24),
            0.4,
        ),
        (
            "robust_repeat/fast -> robust",
            PhyConfig::robust(),
            mfsk_traffic(6.0, 23, &uncoded_mfsk),
            0.15,
        ),
    ];
    for (name, config, audio, max_per_body) in cases {
        let st = run_false(&config, &audio);
        let per_body = st.candidates as f64 / st.bodies.max(1) as f64;
        assert_eq!(st.false_msgs, 0, "{name}");
        assert!(st.bodies >= 10, "{name}: only {} false bodies", st.bodies);
        assert!(
            per_body < max_per_body,
            "{name}: {per_body:.3} candidates per body ({} bodies)",
            st.bodies
        );
    }
}

/// A neighbouring link of the same profile with another link key (spec
/// §4.1): its clean packets decode to one candidate each, which fails the
/// CRC deterministically; the GMD alternatives rarely reach the CRC.
#[test]
fn other_key_traffic_never_decodes() {
    for (name, config, max_per_body) in [
        ("robust", PhyConfig::robust(), 1.3),
        ("ltc_fec", PhyConfig::ltc_fec(), 1.3),
    ] {
        let audio = other_key_traffic(&config, 0x1234, 6.0, 31);
        let st = run_false(&config, &audio);
        let per_body = st.candidates as f64 / st.bodies.max(1) as f64;
        assert_eq!(st.false_msgs, 0, "{name}");
        assert!(st.bodies >= 20, "{name}: only {} bodies", st.bodies);
        assert!(
            per_body < max_per_body,
            "{name}: {per_body:.3} candidates per body ({} bodies)",
            st.bodies
        );
        // Most bodies are the neighbour's packets: structurally valid.
        assert!(
            st.struct_strict >= st.bodies / 2,
            "{name}: {} of {} bodies structurally valid",
            st.struct_strict,
            st.bodies
        );
    }
}

#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn false_accept_report() {
    let seconds: f32 = std::env::var("FALSE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120.0);
    let keys = env_list(
        "FALSE_PROFILES",
        "ltc_fec,ltc2400_fec,ltc9600_fec,ltc14400_fec,robust",
    );
    let (a_len, a_strict) = analytic_struct();
    for t in TYPES {
        println!("{t:?}: {} canonical payloads", canonical_payloads(t));
    }
    println!(
        "\nAnalytic structural pass per random candidate: lenient {a_len:.3e} ({:.2} bits), strict {a_strict:.3e} ({:.2} bits)",
        -a_len.log2(),
        -a_strict.log2()
    );
    println!(
        "CRC-16 factor 2^-16; per candidate false accept: lenient {:.3e}, strict {:.3e}",
        a_len * CRC_FACTOR,
        a_strict * CRC_FACTOR
    );

    let n_random: usize = std::env::var("FALSE_RANDOM_BODIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    println!("\n### Random bodies (simulated analytic model), {n_random} bodies\n");
    println!("| gate erasures | candidates / body | header pass / cand | struct (lenient) / cand | struct (strict) / cand |");
    println!("|---|---|---|---|---|");
    for e in [0usize, 2, 4, 6, 7, 8] {
        let st = random_bodies(&PhyConfig::ltc_fec(), n_random, e, 0xB0D1 + e as u64);
        let c = st.candidates.max(1) as f64;
        println!(
            "| {e} | {:.2} | {:.4} | {:.2e} | {:.2e} |",
            st.candidates as f64 / st.bodies as f64,
            st.header_pass as f64 / c,
            st.struct_lenient as f64 / c,
            st.struct_strict as f64 / c,
        );
    }

    let rows: Vec<(String, &'static str, FalseStats)> = std::thread::scope(|s| {
        let handles: Vec<_> = keys
            .iter()
            .flat_map(|k| {
                let config = PhyConfig::from_name(k).unwrap();
                signals(&config, seconds)
                    .into_iter()
                    .map(move |(name, audio)| (k.clone(), config.clone(), name, audio))
                    .collect::<Vec<_>>()
            })
            .map(|(k, config, name, audio)| s.spawn(move || (k, name, run_false(&config, &audio))))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    println!("\n### Non-Cricket audio, {seconds} s per signal\n");
    println!("| profile | signal | false bodies / min | candidates / min | header pass / min | struct pass, no payload check (count) | struct pass, canonical payload (count) | false msgs | est. years / false msg, no payload check | est. years / false msg, canonical payload |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    let mut total_false = 0;
    let fmt = |y: f64| {
        if y.is_finite() {
            format!("{y:.3e}")
        } else {
            "∞".to_string()
        }
    };
    for (k, name, st) in &rows {
        total_false += st.false_msgs;
        // The neighbour's packets fail the CRC deterministically: the
        // estimate counts only the candidates that are not one of them.
        let (lenient, strict, random) = if *name == OTHER_KEY {
            (0, 0, st.candidates.saturating_sub(st.struct_strict))
        } else {
            (st.struct_lenient, st.struct_strict, st.candidates)
        };
        println!(
            "| {k} | {name} | {:.1} | {:.1} | {:.2} | {} | {} | {} | {} | {} |",
            st.per_min(st.bodies),
            st.per_min(st.candidates),
            st.per_min(st.header_pass),
            st.struct_lenient,
            st.struct_strict,
            st.false_msgs,
            fmt(st.years_per_false(lenient, random, a_len)),
            fmt(st.years_per_false(strict, random, a_strict)),
        );
    }
    println!("\nEstimates: max(observed structural passes, candidates × analytic structural probability) × 2^-16.");
    println!("Other-key traffic: the structural passes are the neighbour's own packets, which the CRC rejects deterministically (spec §4.1); the estimate uses the remaining candidates only.");
    assert_eq!(total_false, 0);
}
