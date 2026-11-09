//! HF (instrument-audio) profiles (`PhyConfig::hf_robust`, spec §6.6,
//! `docs/proposals/hf-instrument.md`): MIDI embedded on an instrument's own
//! signal at 19.0–21.5 kHz and extracted by a box that passes the audio
//! through. Regression checks run with `cargo test`; the measurement
//! report (delivery vs tone level over each instrument source, PSU spurs,
//! hiss floors, converter and codec models, gate / compressor, drift, false
//! messages, cross-decode, receiver CPU, the IMD and audibility budget and
//! the tone-plan survival table) is an ignored test:
//!
//! ```sh
//! cargo test --release -p cricket_core --test hf_profiles -- --ignored --nocapture
//! ```
//!
//! Environment: `HF_TRIALS` (trials per cell, default 400), `HF_FALSE_SECONDS`
//! (audio per false-message signal, default 60), `HF_SECTIONS`
//! (comma-separated subset of `plans,band,training,heldout,false,cpu,imd`).

mod support;

use cricket_core::link::{
    crc16_ccitt, nibbles_to_bytes, BodySymbol, FecCodec, PROTOCOL_VERSION, SEQ_MODULUS,
};
use cricket_core::modem::{
    encode_to_samples, repetition_samples, CricketModemRx, CricketModemTx, ModemOutput,
};
use cricket_core::phy::{
    BandPass, PhyConfig, PhyRx, RxEvent, HF_BASE_96K_HZ, HF_BASE_HZ, HF_RX_BAND_HZ,
    HF_RX_BAND_ORDER, HF_TX_LEVEL_DBFS,
};
use cricket_core::transport::{MessageType, MscCommand, MscCue, MscMessage, TransportMessage};
use std::time::Instant;
use support::{
    apply_converter, imd_residual_db, instrument, music, speech, ChannelSim, Converter, Dynamics,
    Instrument, XorShift64,
};

const RATE: u32 = 48_000;
/// Long-term RMS of every instrument source: a typical instrument level,
/// with peaks around −6 dBFS.
const PROGRAM_DBFS: f32 = -18.0;
/// CRC-16 pass probability for a random candidate (false-accept method of
/// `docs/proposals/fec.md` §9).
const CRC_FACTOR: f64 = 1.0 / 65_536.0;

// ---------------------------------------------------------------------------
// Cells

/// One measurement cell: the tone level and the chain between embedder and
/// box. The instrument sources sit at `PROGRAM_DBFS`.
#[derive(Clone, Copy)]
struct Cell {
    label: &'static str,
    /// Transmit level of the tones, dBFS peak.
    tone_dbfs: f32,
    chain: fn(ChannelSim) -> ChannelSim,
}

const fn cell(label: &'static str, tone_dbfs: f32, chain: fn(ChannelSim) -> ChannelSim) -> Cell {
    Cell {
        label,
        tone_dbfs,
        chain,
    }
}

fn guitar(s: ChannelSim) -> ChannelSim {
    s.instrument(Instrument::Guitar, PROGRAM_DBFS)
}
fn bass(s: ChannelSim) -> ChannelSim {
    s.instrument(Instrument::Bass, PROGRAM_DBFS)
}
fn synth(s: ChannelSim) -> ChannelSim {
    s.instrument(Instrument::Synth, PROGRAM_DBFS)
}
fn keys(s: ChannelSim) -> ChannelSim {
    s.instrument(Instrument::Keys, PROGRAM_DBFS)
}
fn cymbals(s: ChannelSim) -> ChannelSim {
    s.instrument(Instrument::Cymbals, PROGRAM_DBFS)
}

/// Training cells: the thresholds and the receiver band were chosen on
/// these. `HELD_OUT` is reported once per change and never tuned on.
const TRAINING: &[Cell] = &[
    cell("clean line, no instrument", -30.0, |s| s),
    cell("guitar, tones -20 dBFS", -20.0, guitar),
    cell("guitar, tones -30 dBFS", -30.0, guitar),
    cell("guitar, tones -40 dBFS", -40.0, guitar),
    cell("guitar, tones -50 dBFS", -50.0, guitar),
    cell("bass, tones -30 dBFS", -30.0, bass),
    cell("bass, tones -40 dBFS", -40.0, bass),
    cell("bass, tones -50 dBFS", -50.0, bass),
    cell("bright synth, tones -20 dBFS", -20.0, synth),
    cell("bright synth, tones -30 dBFS", -30.0, synth),
    cell("bright synth, tones -40 dBFS", -40.0, synth),
    cell("keys, tones -30 dBFS", -30.0, keys),
    cell("keys, tones -40 dBFS", -40.0, keys),
    cell("keys, tones -50 dBFS", -50.0, keys),
    cell("cymbals, tones -20 dBFS", -20.0, cymbals),
    cell("cymbals, tones -30 dBFS", -30.0, cymbals),
    cell("cymbals, tones -40 dBFS", -40.0, cymbals),
    cell("guitar + hiss -85 dBFS, tones -30", -30.0, |s| {
        guitar(s).hiss(-85.0, false)
    }),
    cell("guitar + hiss -85 dBFS, tones -40", -40.0, |s| {
        guitar(s).hiss(-85.0, false)
    }),
    cell("guitar + hiss -85 dBFS, tones -50", -50.0, |s| {
        guitar(s).hiss(-85.0, false)
    }),
    cell("guitar + hiss -95 dBFS, tones -50", -50.0, |s| {
        guitar(s).hiss(-95.0, false)
    }),
    cell(
        "guitar + spur 20.0 kHz (slot 6) -40 dBFS, tones -30",
        -30.0,
        |s| guitar(s).spur(20_000.0, -40.0),
    ),
    cell(
        "guitar + spur 20.0 kHz (slot 6) -50 dBFS, tones -30",
        -30.0,
        |s| guitar(s).spur(20_000.0, -50.0),
    ),
    cell(
        "guitar + spur 20.0 kHz (slot 6) -50 dBFS, tones -40",
        -40.0,
        |s| guitar(s).spur(20_000.0, -50.0),
    ),
    cell(
        "guitar + PSU spurs 20.0/-50, 22.3/-40, 23.7/-40, tones -30",
        -30.0,
        |s| {
            guitar(s)
                .spur(20_000.0, -50.0)
                .spur(22_300.0, -40.0)
                .spur(23_700.0, -40.0)
        },
    ),
    cell("guitar, sharp 48 kHz converter, tones -30", -30.0, |s| {
        guitar(s).converter(Converter::Sharp48)
    }),
    cell("guitar, slow roll-off converter, tones -30", -30.0, |s| {
        guitar(s).converter(Converter::Slow48)
    }),
    cell(
        "guitar, 44.1 kHz hop (half-band filters), tones -30",
        -30.0,
        |s| guitar(s).converter(Converter::Hop441),
    ),
    cell("guitar, 44.1 kHz sharp SRC, tones -30", -30.0, |s| {
        guitar(s).converter(Converter::Src441)
    }),
    cell("bright-pickup guitar, tones -30 dBFS", -30.0, |s| {
        s.instrument(Instrument::GuitarBright, PROGRAM_DBFS)
    }),
    cell("bright-pickup guitar, tones -40 dBFS", -40.0, |s| {
        s.instrument(Instrument::GuitarBright, PROGRAM_DBFS)
    }),
    cell("guitar, 19 kHz 8th-order codec LP, tones -30", -30.0, |s| {
        guitar(s).converter(Converter::Codec19Lp8)
    }),
    cell("guitar, 19 kHz codec brick wall, tones -30", -30.0, |s| {
        guitar(s).converter(Converter::Codec19Wall)
    }),
    cell("guitar, gate -40 dBFS 5/50/100 ms, tones -30", -30.0, |s| {
        guitar(s).gate(-40.0, 5.0, 50.0, 100.0)
    }),
    cell("guitar, gate -40 dBFS 5/50/100 ms, tones -50", -50.0, |s| {
        guitar(s).gate(-40.0, 5.0, 50.0, 100.0)
    }),
    cell(
        "guitar, gate -25 dBFS 5/50/100 ms (tuner mute), tones -30",
        -30.0,
        |s| guitar(s).gate(-25.0, 5.0, 50.0, 100.0),
    ),
    cell(
        "guitar, comp -30 dBFS 4:1 5/100 ms, tones -30",
        -30.0,
        |s| guitar(s).compressor(Dynamics::compressor(-30.0, 4.0, 5.0, 100.0)),
    ),
    cell("guitar, limiter -20 dBFS, tones -30", -30.0, |s| {
        guitar(s).limiter(Dynamics::limiter(-20.0, 50.0))
    }),
    cell("guitar, drift +0.1 %, tones -30", -30.0, |s| {
        guitar(s).drift_ppm_sinc(1_000.0)
    }),
    cell("guitar, drift +0.3 %, tones -30", -30.0, |s| {
        guitar(s).drift_ppm_sinc(3_000.0)
    }),
    cell("guitar, drift +1 %, tones -30", -30.0, |s| {
        guitar(s).drift_ppm_sinc(10_000.0)
    }),
    cell("guitar, drift -1 %, tones -30", -30.0, |s| {
        guitar(s).drift_ppm_sinc(-10_000.0)
    }),
    cell("guitar, 10 ms dropout, tones -30", -30.0, |s| {
        guitar(s).dropout_samples(480)
    }),
    cell("guitar, 30 ms dropout, tones -30", -30.0, |s| {
        guitar(s).dropout_samples(1_440)
    }),
    cell("guitar, burst 3 x 1 ms 10/s, tones -30", -30.0, |s| {
        guitar(s).burst_loss(1.0, 3, 10.0)
    }),
    cell(
        "guitar, box input -20 dB + hiss -95, tones -30",
        -30.0,
        |s| guitar(s).gain(0.1).hiss(-95.0, false),
    ),
];

/// Held-out cells: other levels, other combinations, other seeds.
const HELD_OUT: &[Cell] = &[
    cell("guitar, tones -25 dBFS", -25.0, guitar),
    cell("guitar, tones -35 dBFS", -35.0, guitar),
    cell("guitar, tones -45 dBFS", -45.0, guitar),
    cell("bass, tones -35 dBFS", -35.0, bass),
    cell("bass, tones -45 dBFS", -45.0, bass),
    cell("bright synth, tones -25 dBFS", -25.0, synth),
    cell("bright synth, tones -35 dBFS", -35.0, synth),
    cell("keys, tones -35 dBFS", -35.0, keys),
    cell("keys, tones -45 dBFS", -45.0, keys),
    cell("cymbals, tones -25 dBFS", -25.0, cymbals),
    cell("cymbals, tones -35 dBFS", -35.0, cymbals),
    cell("guitar + pink hiss -90 dBFS, tones -40", -40.0, |s| {
        guitar(s).hiss(-90.0, true)
    }),
    cell("keys + hiss -85 dBFS, tones -45", -45.0, |s| {
        keys(s).hiss(-85.0, false)
    }),
    cell(
        "guitar + spur 20.0 kHz (slot 6) -45 dBFS, tones -30",
        -30.0,
        |s| guitar(s).spur(20_000.0, -45.0),
    ),
    cell(
        "guitar + spur 20.08 kHz (between slots) -40 dBFS, tones -30",
        -30.0,
        |s| guitar(s).spur(20_083.0, -40.0),
    ),
    cell(
        "bass + spur 21.5 kHz (slot 15) -50 dBFS, tones -35",
        -35.0,
        |s| bass(s).spur(21_500.0, -50.0),
    ),
    cell(
        "guitar, sharp converter + hiss -90, tones -40",
        -40.0,
        |s| guitar(s).converter(Converter::Sharp48).hiss(-90.0, false),
    ),
    cell("guitar, slow converter + hiss -85, tones -40", -40.0, |s| {
        guitar(s).converter(Converter::Slow48).hiss(-85.0, false)
    }),
    cell(
        "keys, 44.1 kHz hop (half-band filters), tones -25",
        -25.0,
        |s| keys(s).converter(Converter::Hop441),
    ),
    cell("keys, 44.1 kHz sharp SRC, tones -25", -25.0, |s| {
        keys(s).converter(Converter::Src441)
    }),
    cell("bright-pickup guitar, tones -35 dBFS", -35.0, |s| {
        s.instrument(Instrument::GuitarBright, PROGRAM_DBFS)
    }),
    cell(
        "guitar, gate -35 dBFS 10/30/200 ms, tones -35",
        -35.0,
        |s| guitar(s).gate(-35.0, 10.0, 30.0, 200.0),
    ),
    cell("guitar, comp -25 dBFS 8:1 2/50 ms, tones -35", -35.0, |s| {
        guitar(s).compressor(Dynamics::compressor(-25.0, 8.0, 2.0, 50.0))
    }),
    cell("guitar, drift +0.5 %, tones -30", -30.0, |s| {
        guitar(s).drift_ppm_sinc(5_000.0)
    }),
    cell("guitar, drift -0.2 %, tones -35", -35.0, |s| {
        guitar(s).drift_ppm_sinc(-2_000.0)
    }),
    cell("guitar, 20 ms dropout, tones -35", -35.0, |s| {
        guitar(s).dropout_samples(960)
    }),
    cell(
        "synth + hiss -85 + sharp converter, tones -30",
        -30.0,
        |s| synth(s).converter(Converter::Sharp48).hiss(-85.0, false),
    ),
    cell("cymbals + spur 20.0 kHz -50, tones -30", -30.0, |s| {
        cymbals(s).spur(20_000.0, -50.0)
    }),
];

const TRAINING_SEED: u64 = 0x4F00_0000;
const HELD_OUT_SEED: u64 = 0x4F1D_0000;

struct Outcome {
    ok: usize,
    trials: usize,
    false_msgs: usize,
    /// Mean message start → delivery (ms) over delivered trials.
    mean_latency_ms: f64,
}

impl Outcome {
    fn pct(&self) -> f64 {
        100.0 * self.ok as f64 / self.trials.max(1) as f64
    }
}

fn random_message(rng: &mut XorShift64, t: usize) -> TransportMessage {
    match t % 4 {
        0 | 1 => TransportMessage::NoteOn {
            channel: rng.below(16) as u8,
            note: rng.below(128) as u8,
            velocity: 1 + rng.below(127) as u8,
        },
        2 => TransportMessage::ProgramChange {
            channel: rng.below(16) as u8,
            program: rng.below(128) as u8,
        },
        _ => {
            let list = rng.below(16) as u8;
            let cue = rng.below(128) as u16;
            TransportMessage::Msc(MscMessage::cue(
                MscCommand::Go,
                MscCue::from_tenths(cue * 10 + u16::from(list % 10)).unwrap(),
                (list > 0).then_some(list),
            ))
        }
    }
}

/// `trials` single-message transmissions over the cell's chain: a random
/// Note On / Program Change / MSC GO at a random position inside the
/// instrument signal, decoded in 64-sample chunks.
fn run_cell(config: &PhyConfig, c: &Cell, trials: usize, seed: u64) -> Outcome {
    let config = config.clone().with_tx_level_dbfs(c.tone_dbfs);
    let mut rng = XorShift64::new(seed);
    let rate = config.sample_rate_hz;
    let period = config.samples_per_symbol_period();
    let mut ok = 0;
    let mut false_msgs = 0;
    let mut lat = Vec::new();
    for t in 0..trials {
        let msg = random_message(&mut rng, t);
        let offset = rate as usize / 10 + rng.below(4 * period);
        let tx = CricketModemTx::new(config.clone());
        let mut clean = vec![0.0f32; offset];
        clean.extend(tx.encode_message_with_seq(&msg, rng.below(4) as u8));
        clean.extend(std::iter::repeat_n(0.0, rate as usize / 4));
        let sim_seed = rng.next_u64();
        let sim = (c.chain)(ChannelSim::new(config.clone(), sim_seed));
        let audio = sim.apply(&clean);
        let mut rx = CricketModemRx::new(config.clone());
        let mut delivered_at = None;
        for chunk in audio.chunks(64) {
            for out in rx.push_samples(chunk) {
                if let ModemOutput::Message(m) = out {
                    if m == msg && delivered_at.is_none() {
                        delivered_at = Some(rx.samples_processed());
                    } else if m != msg {
                        false_msgs += 1;
                    }
                }
            }
        }
        if let Some(at) = delivered_at {
            ok += 1;
            let ratio = 1.0 + sim.drift_ppm * 1e-6;
            lat.push((at as f64 - offset as f64 / ratio) / rate as f64 * 1000.0);
        }
    }
    Outcome {
        ok,
        trials,
        false_msgs,
        mean_latency_ms: if lat.is_empty() {
            f64::NAN
        } else {
            lat.iter().sum::<f64>() / lat.len() as f64
        },
    }
}

/// `results[profile][cell]`, one thread per cell.
fn run_table(
    profiles: &[(&str, PhyConfig)],
    cells: &[Cell],
    trials: usize,
    seed_base: u64,
) -> Vec<Vec<Outcome>> {
    std::thread::scope(|s| {
        let handles: Vec<Vec<_>> = profiles
            .iter()
            .map(|(_, c)| {
                cells
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| {
                        s.spawn(move || run_cell(c, cell, trials, seed_base + i as u64))
                    })
                    .collect()
            })
            .collect();
        handles
            .into_iter()
            .map(|hs| hs.into_iter().map(|h| h.join().unwrap()).collect())
            .collect()
    })
}

fn print_table(profiles: &[(&str, PhyConfig)], cells: &[Cell], results: &[Vec<Outcome>]) -> usize {
    print!("| {:<58} |", "chain (instrument at −18 dBFS RMS)");
    for (name, _) in profiles {
        print!(" {name:>22} |");
    }
    println!();
    print!("|{:-<60}|", "");
    for _ in profiles {
        print!("{:->24}|", "");
    }
    println!();
    let mut false_total = 0;
    for (i, c) in cells.iter().enumerate() {
        print!("| {:<58} |", c.label);
        for r in results {
            let o = &r[i];
            false_total += o.false_msgs;
            print!(" {:>8.1} % ({:>5.0} ms) |", o.pct(), o.mean_latency_ms);
        }
        println!();
    }
    false_total
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn sections() -> Vec<String> {
    std::env::var("HF_SECTIONS")
        .unwrap_or_else(|_| "plans,band,training,heldout,false,cpu,imd".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Tone-plan survival: per-tone attenuation through each converter model

struct Plan {
    name: &'static str,
    rate: u32,
    base_hz: f32,
    step_hz: f32,
    tones: usize,
    symbol_ms: f32,
}

const PLANS: &[Plan] = &[
    Plan {
        name: "hf_robust: 16 × 166⅔ Hz, 6 ms (19.0–21.5 kHz)",
        rate: 48_000,
        base_hz: HF_BASE_HZ,
        step_hz: 1000.0 / 6.0,
        tones: 16,
        symbol_ms: 6.0,
    },
    Plan {
        name: "8 × 333⅓ Hz, 3 ms, 3 bits (19.0–21.33 kHz)",
        rate: 48_000,
        base_hz: HF_BASE_HZ,
        step_hz: 1000.0 / 3.0,
        tones: 8,
        symbol_ms: 3.0,
    },
    Plan {
        name: "16 × 250 Hz, 4 ms (19.0–22.75 kHz)",
        rate: 48_000,
        base_hz: HF_BASE_HZ,
        step_hz: 250.0,
        tones: 16,
        symbol_ms: 4.0,
    },
    Plan {
        name: "16 × 333⅓ Hz, 3 ms (19.0–24.0 kHz)",
        rate: 48_000,
        base_hz: HF_BASE_HZ,
        step_hz: 1000.0 / 3.0,
        tones: 16,
        symbol_ms: 3.0,
    },
    Plan {
        name: "8 × 125 Hz, 8 ms, 3 bits (19.0–19.875 kHz)",
        rate: 48_000,
        base_hz: HF_BASE_HZ,
        step_hz: 125.0,
        tones: 8,
        symbol_ms: 8.0,
    },
    Plan {
        name: "hf_fast_96k: 16 × 333⅓ Hz, 3 ms (24.0–29.0 kHz)",
        rate: 96_000,
        base_hz: HF_BASE_96K_HZ,
        step_hz: 1000.0 / 3.0,
        tones: 16,
        symbol_ms: 3.0,
    },
];

/// Attenuation (dB) of a steady tone at `hz` through `model`.
fn tone_gain_db(hz: f32, model: Converter, rate: u32) -> f32 {
    let n = rate as usize / 10;
    let mut x: Vec<f32> = (0..n)
        .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin())
        .collect();
    apply_converter(&mut x, model, rate);
    let tail = &x[n / 2..];
    let e: f64 = tail.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / tail.len() as f64;
    10.0 * (e / 0.5).max(1e-20).log10() as f32
}

/// Tones of a plan that a model keeps within 6 dB of the plan's strongest
/// tone (the receiver is level-independent, so a tilt of the whole band
/// costs nothing; a tone 6 dB under its neighbours is as good as lost
/// against a hiss floor or a spur).
fn surviving_tones(plan: &Plan, model: Converter) -> usize {
    let gains: Vec<f32> = (0..plan.tones)
        .map(|k| tone_gain_db(plan.base_hz + k as f32 * plan.step_hz, model, plan.rate))
        .collect();
    let best = gains.iter().fold(f32::NEG_INFINITY, |m, &g| m.max(g));
    gains
        .iter()
        .filter(|&&g| g > best - 6.0 && g > -40.0)
        .count()
}

fn print_plan_table() {
    println!("\n### Tone plans vs converter models (tones kept within 6 dB / tones in the plan; airtime per coded message)\n");
    println!("| plan | airtime | sharp 48 kHz | slow 48 kHz | 44.1 kHz hop (half-band) | 44.1 kHz SRC (sharp) | 19 kHz LP (8th) | 19 kHz codec wall | 96→48 kHz hop | ±1 % clock = bins |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for plan in PLANS {
        let symbols = 4 + 88usize.div_ceil(plan.tones.trailing_zeros() as usize);
        let airtime = symbols as f32 * plan.symbol_ms + plan.symbol_ms;
        let models = [
            Converter::Sharp48,
            Converter::Slow48,
            Converter::Hop441,
            Converter::Src441,
            Converter::Codec19Lp8,
            Converter::Codec19Wall,
        ];
        let mut row = format!("| {} | {:.0} ms |", plan.name, airtime);
        for m in models {
            if plan.rate == 96_000 {
                row.push_str(" n/a |");
            } else {
                row.push_str(&format!(" {}/{} |", surviving_tones(plan, m), plan.tones));
            }
        }
        if plan.rate == 96_000 {
            row.push_str(&format!(
                " {}/{} |",
                surviving_tones(plan, Converter::Hop96To48),
                plan.tones
            ));
        } else {
            row.push_str(" — |");
        }
        let top = plan.base_hz + (plan.tones - 1) as f32 * plan.step_hz;
        row.push_str(&format!(" {:.1} |", 0.01 * top / plan.step_hz));
        println!("{row}");
    }
}

// ---------------------------------------------------------------------------
// Receiver band: what each instrument leaves in the band

/// RMS (dBFS) of `x` after the HF receiver band filter, and after a plain
/// second-order 0.7 × / 1.3 × band as the baseband profiles use.
fn in_band_dbfs(x: &[f32]) -> (f32, f32) {
    let rms_db = |y: &[f32]| {
        let e: f64 = y.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / y.len() as f64;
        10.0 * e.max(1e-20).log10() as f32
    };
    let mut steep = BandPass::with_order(
        HF_RX_BAND_HZ.0,
        HF_RX_BAND_HZ.1,
        HF_RX_BAND_ORDER as usize,
        RATE,
    );
    let a: Vec<f32> = x.iter().map(|&v| steep.process(v)).collect();
    let mut plain = BandPass::new(0.7 * HF_BASE_HZ, 1.3 * 21_500.0, RATE);
    let b: Vec<f32> = x.iter().map(|&v| plain.process(v)).collect();
    (rms_db(&a), rms_db(&b))
}

fn print_band_table() {
    println!("\n### Instrument sources at −18 dBFS RMS: what reaches the receiver's purity gate\n");
    println!("| source | peak dBFS | RMS in 18–22.5 kHz (8th-order band, the HF receiver) | RMS after a 2nd-order 13.3–28 kHz band | tone RMS at −30 dBFS peak |");
    println!("|---|---|---|---|---|");
    let n = RATE as usize * 10;
    for (name, kind) in [
        ("guitar (passive pickup)", Instrument::Guitar),
        (
            "guitar (bright active / piezo pickup)",
            Instrument::GuitarBright,
        ),
        ("bass", Instrument::Bass),
        ("bright synth", Instrument::Synth),
        ("keys", Instrument::Keys),
        ("cymbals", Instrument::Cymbals),
    ] {
        let x = instrument(kind, RATE, n, PROGRAM_DBFS, 7);
        let peak = x.iter().fold(0f32, |m, &v| m.max(v.abs()));
        let (steep, plain) = in_band_dbfs(&x);
        println!(
            "| {name} | {:.1} | {steep:.1} dBFS | {plain:.1} dBFS | {:.1} dBFS |",
            20.0 * peak.log10(),
            HF_TX_LEVEL_DBFS - 3.0
        );
    }
}

// ---------------------------------------------------------------------------
// False messages (methodology of tests/fec_false_accept.rs)

#[derive(Debug, Default, Clone, Copy)]
struct Checks {
    header: bool,
    pad: bool,
    payload: bool,
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
    let _ = crc16_ccitt(&bytes[..needed - 2]);
    c
}

#[derive(Debug, Default, Clone, Copy)]
struct FalseStats {
    seconds: f64,
    locks: u64,
    bodies: u64,
    candidates: u64,
    struct_strict: u64,
    false_msgs: u64,
}

impl FalseStats {
    /// Years of this audio per false message, conservatively: the larger
    /// of the observed structural passes and the analytic structural pass
    /// probability of a random candidate (2.3 × 10⁻⁵, `fec.md` §9) times
    /// the candidates, times the CRC-16 factor.
    fn years_per_false(&self) -> f64 {
        const ANALYTIC_STRICT: f64 = 2.3e-5;
        let per_sec = (self.struct_strict as f64).max(self.candidates as f64 * ANALYTIC_STRICT)
            / self.seconds
            * CRC_FACTOR;
        1.0 / per_sec / (3600.0 * 8766.0)
    }
}

/// The receive flow of `CricketModemRx` with the candidate checks counted.
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
                    st.locks += 1;
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
                        st.struct_strict += checks(nibbles).payload as u64;
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

/// Back-to-back traffic of `config` (NoteOns), SNR 20 dB on the tones.
fn traffic(config: &PhyConfig, seconds: f32, seed: u64) -> Vec<f32> {
    let n = (seconds * config.sample_rate_hz as f32) as usize;
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

/// `n` samples of back-to-back packets of `config` with no noise added
/// (for the leakage and intermodulation budget).
fn clean_traffic(config: &PhyConfig, n: usize) -> Vec<f32> {
    let mut tx = CricketModemTx::new(config.clone());
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while out.len() < n {
        out.extend(tx.encode_message(&TransportMessage::NoteOn {
            channel: (i % 16) as u8,
            note: (i * 37 % 128) as u8,
            velocity: 1 + (i * 7 % 127) as u8,
        }));
        i += 1;
    }
    out.truncate(n);
    out
}

fn white(seconds: f32, sigma: f64, seed: u64) -> Vec<f32> {
    let mut rng = XorShift64::new(seed);
    (0..(seconds * RATE as f32) as usize)
        .map(|_| (rng.gaussian() * sigma) as f32)
        .collect()
}

/// Loud instrument signals (−12 dBFS RMS), noise, speech and music, other
/// profiles' traffic and HF traffic with another link key: nothing the HF
/// receiver should deliver.
fn false_signals(seconds: f32) -> Vec<(&'static str, Vec<f32>)> {
    let n = (seconds * RATE as f32) as usize;
    let loud = -12.0;
    let mut v = vec![
        (
            "guitar (−12 dBFS)",
            instrument(Instrument::Guitar, RATE, n, loud, 21),
        ),
        (
            "bass (−12 dBFS)",
            instrument(Instrument::Bass, RATE, n, loud, 22),
        ),
        (
            "bright synth (−12 dBFS)",
            instrument(Instrument::Synth, RATE, n, loud, 23),
        ),
        (
            "keys (−12 dBFS)",
            instrument(Instrument::Keys, RATE, n, loud, 24),
        ),
        (
            "cymbals (−12 dBFS)",
            instrument(Instrument::Cymbals, RATE, n, loud, 25),
        ),
        ("white noise −30 dBFS", white(seconds, 0.0316, 26)),
        ("speech", speech(RATE as usize, seconds, 27)),
        ("music", music(RATE as usize, seconds, 28)),
        (
            "MFSK Robust traffic (2.5–5 kHz)",
            traffic(&PhyConfig::robust(), seconds, 29),
        ),
        (
            "MFSK Fast traffic (2.5–7.5 kHz)",
            traffic(&PhyConfig::fast(), seconds, 30),
        ),
        (
            "LTC (clean-line) traffic",
            traffic(&PhyConfig::ltc_fec(), seconds, 31),
        ),
        (
            "ltc14400_fec traffic",
            traffic(&PhyConfig::ltc_fast_fec_with_bit_rate(14_400), seconds, 32),
        ),
    ];
    for (_, x) in v.iter_mut() {
        x.truncate(n);
    }
    v.push((
        OTHER_KEY,
        traffic(&PhyConfig::hf_robust().with_link_key(0x1234), seconds, 33),
    ));
    v
}

const OTHER_KEY: &str = "HF traffic, other link key";

fn print_false_table(seconds: f32) {
    let hf = PhyConfig::hf_robust();
    println!("\n### False messages into the `instrument` receiver ({seconds:.0} s per signal; target ≥ 10 years per false message)\n");
    println!("| signal | phy locks / min | bodies / min | RS candidates / min | structural passes | false messages | years per false (CRC factor applied) |");
    println!("|---|---|---|---|---|---|---|");
    let signals = false_signals(seconds);
    let rows: Vec<(&str, FalseStats)> = std::thread::scope(|s| {
        let handles: Vec<_> = signals
            .iter()
            .map(|(name, audio)| {
                let hf = hf.clone();
                s.spawn(move || (*name, run_false(&hf, audio)))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut total_false = 0;
    for (name, st) in rows {
        total_false += st.false_msgs;
        let per_min = |x: u64| x as f64 * 60.0 / st.seconds;
        let years = if name == OTHER_KEY {
            "deterministic CRC reject".to_string()
        } else {
            format!("{:.1e}", st.years_per_false())
        };
        println!(
            "| {name} | {:.1} | {:.1} | {:.1} | {} | {} | {years} |",
            per_min(st.locks),
            per_min(st.bodies),
            per_min(st.candidates),
            st.struct_strict,
            st.false_msgs
        );
    }
    println!("\nfalse messages in all: {total_false}");

    // Cross-decode: the baseband receivers on HF traffic.
    println!("\n### Cross-decode: HF traffic into the baseband receivers ({seconds:.0} s)\n");
    println!("| receiver | phy locks | bodies | false messages |");
    println!("|---|---|---|---|");
    let hf_traffic = traffic(&hf, seconds, 40);
    for (name, config) in [
        ("analog-safe (robust)", PhyConfig::robust()),
        ("analog-safe-repeat", PhyConfig::robust_repeat()),
        ("analog-fast", PhyConfig::fast()),
        ("analog-safe-fast", PhyConfig::robust_fast()),
    ] {
        let (locks, bodies, msgs) = cross_decode(&config, &hf_traffic);
        println!("| {name} | {locks} | {bodies} | {msgs} |");
    }
}

/// Phy locks, completed bodies and delivered messages of `config`'s
/// receiver on `audio`.
fn cross_decode(config: &PhyConfig, audio: &[f32]) -> (u64, u64, usize) {
    let mut rx = CricketModemRx::new(config.clone());
    let mut msgs = 0;
    for chunk in audio.chunks(512) {
        for out in rx.push_samples(chunk) {
            if let ModemOutput::Message(_) = out {
                msgs += 1;
            }
        }
    }
    let st = rx.stats();
    (st.crc_pass + st.crc_fail, st.crc_pass + st.crc_fail, msgs)
}

// ---------------------------------------------------------------------------
// CPU

/// Receiver CPU time per input sample (ns) over 10 s of `audio`.
fn ns_per_sample(config: &PhyConfig, audio: &[f32]) -> f64 {
    let mut rx = CricketModemRx::new(config.clone());
    let t = Instant::now();
    let mut n = 0usize;
    for chunk in audio.chunks(256) {
        n += rx.push_samples(chunk).len();
    }
    std::hint::black_box(n);
    t.elapsed().as_nanos() as f64 / audio.len() as f64
}

fn print_cpu_table() {
    println!("\n### Receiver CPU (host, release build, ns per input sample)\n");
    println!("| profile | idle on guitar (no packets) | busy (back-to-back packets on guitar) | band filter sections |");
    println!("|---|---|---|---|");
    let n = RATE as usize * 10;
    for (name, config) in [
        ("analog-safe (robust)", PhyConfig::robust()),
        ("instrument (hf_robust)", PhyConfig::hf_robust()),
    ] {
        let idle = instrument(Instrument::Guitar, RATE, n, PROGRAM_DBFS, 50);
        let mut busy = traffic(&config, 10.0, 51);
        for (b, g) in busy.iter_mut().zip(&idle) {
            *b += g;
        }
        let sections = config
            .rx_band_hz
            .map(|(lo, hi)| {
                BandPass::with_order(lo, hi, config.rx_band_order as usize, RATE).sections()
            })
            .unwrap_or(0);
        println!(
            "| {name} | {:.1} | {:.1} | {sections} |",
            ns_per_sample(&config, &idle),
            ns_per_sample(&config, &busy)
        );
    }
}

// ---------------------------------------------------------------------------
// IMD and audibility budget

fn print_imd_table() {
    println!("\n### Intermodulation budget: tones + guitar (−18 dBFS RMS) into a tanh drive; residue below 17 kHz relative to the driven guitar\n");
    println!("| tone level (dBFS peak) | drive 3 (clean boost) | drive 10 (overdrive) | drive 30 (fuzz) |");
    println!("|---|---|---|---|");
    let n = RATE as usize * 5;
    let g = instrument(Instrument::Guitar, RATE, n, PROGRAM_DBFS, 60);
    let tones = clean_traffic(&PhyConfig::hf_robust().with_tx_level_dbfs(0.0), n);
    for level in [-20.0f32, -30.0, -40.0, -50.0] {
        let k = 10f32.powf(level / 20.0);
        let t: Vec<f32> = tones.iter().map(|&v| v * k).collect();
        let mut row = format!("| {level:.0} |");
        for drive in [3.0f32, 10.0, 30.0] {
            row.push_str(&format!(
                " {:.1} dB |",
                imd_residual_db(&g, &t, drive, RATE)
            ));
        }
        println!("{row}");
    }
    // Leakage of the tones themselves into the audible band (symbol ramps).
    let total: f64 = tones.iter().map(|&v| v as f64 * v as f64).sum();
    let below = support::audible_energy_db(&tones, RATE);
    println!(
        "\nEnergy of the HF signal itself below 17 kHz (symbol ramps, no nonlinearity): {:.1} dB of its total.",
        below - 10.0 * total.log10() as f32
    );
}

// ---------------------------------------------------------------------------
// Regression checks (cargo test)

fn assert_cell(config: &PhyConfig, c: &Cell, trials: usize, seed: u64, min_pct: f64) {
    let o = run_cell(config, c, trials, seed);
    assert_eq!(o.false_msgs, 0, "{}: false messages", c.label);
    assert!(
        o.pct() >= min_pct,
        "{}: {}/{} delivered",
        c.label,
        o.ok,
        o.trials
    );
}

/// The HF profile delivers over every instrument source at the default
/// level (−30 dBFS), and over a guitar at −40 dBFS, through a sharp 48 kHz
/// converter and with a PSU spur in a tone slot.
#[test]
fn instrument_profiles_deliver_over_instruments() {
    for hf in [PhyConfig::hf_robust(), PhyConfig::hf_fast()] {
        instrument_cells_deliver(&hf);
    }
}

fn instrument_cells_deliver(hf: &PhyConfig) {
    for (i, c) in [
        cell("guitar, tones -30", -30.0, guitar),
        cell("guitar, tones -40", -40.0, guitar),
        cell("bass, tones -30", -30.0, bass),
        cell("keys, tones -30", -30.0, keys),
        cell("cymbals, tones -20", -20.0, cymbals),
        cell("bright synth, tones -30", -30.0, synth),
        cell(
            "guitar + sharp converter + hiss -85, tones -30",
            -30.0,
            |s| guitar(s).converter(Converter::Sharp48).hiss(-85.0, false),
        ),
        cell("guitar + spur 20.0 kHz -50, tones -30", -30.0, |s| {
            guitar(s).spur(20_000.0, -50.0)
        }),
    ]
    .iter()
    .enumerate()
    {
        assert_cell(hf, c, 12, 0x5EED_0000 + i as u64, 100.0);
    }
}

/// Every message type round-trips on both HF profiles through chunked
/// streaming, and the 8-tone plan of `hf_fast` packs 22 nibbles into 30
/// symbols of 3 bits (34 symbols with the preamble: 102 ms on air).
#[test]
fn hf_profiles_round_trip_all_message_types() {
    let messages = vec![
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
    ];
    let fast = PhyConfig::hf_fast();
    assert_eq!(fast.bits_per_symbol(), 3);
    assert_eq!(fast.body_symbols(), 30);
    assert_eq!(fast.n_tones, 8);
    assert!(fast.preamble.iter().all(|&p| p < 8));
    assert!((fast.estimated_first_copy_ms() - 102.0).abs() < 1e-3);
    assert_eq!(repetition_samples(&fast), 35 * 144);
    assert!((fast.tone_hz(7) - 21_333.3).abs() < 0.1);
    assert_eq!(PhyConfig::hf_robust().bits_per_symbol(), 4);
    assert_eq!(PhyConfig::hf_robust().body_symbols(), 22);
    for (name, config) in [
        ("hf_robust", PhyConfig::hf_robust()),
        ("hf_fast", fast),
        ("hf_fast_96k", PhyConfig::hf_fast_96k()),
    ] {
        let samples = encode_to_samples(config.clone(), &messages);
        for chunk in [64usize, 240, 4096] {
            let got = support::decode_chunked(config.clone(), &samples, chunk);
            assert_eq!(got, messages, "{name} chunk {chunk}");
        }
    }
}

/// What kills the band: a 19 kHz codec wall delivers nothing (and nothing
/// false), so the line requirement is real. A sharp 44.1 kHz conversion
/// removes the top tones; on an otherwise empty band the code still fills
/// them in, with a hiss floor it does not (see the report), so it delivers
/// nothing false either way and is not relied on.
#[test]
fn a_codec_kills_the_band() {
    let hf = PhyConfig::hf_robust();
    let wall = cell("guitar, 19 kHz codec wall", -20.0, |s| {
        guitar(s).converter(Converter::Codec19Wall)
    });
    let o = run_cell(&hf, &wall, 8, 0x5EED_1000);
    assert_eq!((o.ok, o.false_msgs), (0, 0), "{}", wall.label);
    let src = cell("guitar, 44.1 kHz sharp SRC + hiss", -30.0, |s| {
        guitar(s).converter(Converter::Src441).hiss(-85.0, false)
    });
    let o = run_cell(&hf, &src, 8, 0x5EED_1001);
    assert_eq!(o.false_msgs, 0, "{}", src.label);
}

/// The 96 kHz variant works at 96 kHz and is removed entirely by a 48 kHz
/// device in the path.
#[test]
fn hf_96k_variant_needs_96k_end_to_end() {
    let hf96 = PhyConfig::hf_fast_96k();
    let keys96 = |s: ChannelSim| s.instrument(Instrument::Keys, PROGRAM_DBFS);
    assert_cell(
        &hf96,
        &cell("keys @96k, tones -30", -30.0, keys96),
        10,
        0x96_0000,
        100.0,
    );
    let o = run_cell(
        &hf96,
        &cell("keys @96k through a 48 kHz device", -30.0, |s| {
            s.instrument(Instrument::Keys, PROGRAM_DBFS)
                .converter(Converter::Hop96To48)
        }),
        6,
        0x96_0001,
    );
    assert_eq!((o.ok, o.false_msgs), (0, 0));
}

/// Nothing false from loud instruments, noise, music or other profiles'
/// traffic in 10 s each, and the baseband receivers hear nothing of HF
/// traffic.
#[test]
fn instrument_receiver_delivers_nothing_false() {
    let hf = PhyConfig::hf_robust();
    for (name, audio) in false_signals(10.0) {
        let st = run_false(&hf, &audio);
        assert_eq!(st.false_msgs, 0, "{name}: {st:?}");
    }
    let hf_traffic = traffic(&hf, 10.0, 41);
    for config in [
        PhyConfig::robust(),
        PhyConfig::fast(),
        PhyConfig::robust_repeat(),
    ] {
        let (_, _, msgs) = cross_decode(&config, &hf_traffic);
        assert_eq!(msgs, 0);
    }
}

/// The level budget documented in the proposal: the HF signal keeps
/// ≥ 55 dB of its energy above 17 kHz (−58 dB measured with the 1 ms
/// raised-cosine ramps, against −40 dB with Robust's linear 0.3 ms ramps).
/// Through a nonlinear stage the tones do become audible: a fuzz (drive
/// 30) turns −30 dBFS tones into a residue only ~17 dB under the guitar
/// (a clean boost, drive 3: ~38 dB), and every 10 dB less tone level buys
/// 10–20 dB. This is why the box taps the signal *before* any drive and
/// offers a stripped output for drive rigs; the test pins the numbers so
/// the proposal cannot drift from them.
#[test]
fn imd_and_leakage_budget() {
    let n = RATE as usize * 2;
    let g = instrument(Instrument::Guitar, RATE, n, PROGRAM_DBFS, 60);
    let mut tones = clean_traffic(&PhyConfig::hf_robust(), n);
    tones.truncate(n);
    let total: f64 = tones.iter().map(|&v| v as f64 * v as f64).sum();
    let leak = support::audible_energy_db(&tones, RATE) - 10.0 * total.log10() as f32;
    let mut linear = PhyConfig::hf_robust();
    linear.raised_cosine_ramps = false;
    linear.attack_ms = 0.3;
    linear.release_ms = 0.3;
    let lin = clean_traffic(&linear, n);
    let lin_total: f64 = lin.iter().map(|&v| v as f64 * v as f64).sum();
    let lin_leak = support::audible_energy_db(&lin, RATE) - 10.0 * lin_total.log10() as f32;
    println!(
        "leakage below 17 kHz: raised-cosine 1 ms {leak:.1} dB, linear 0.3 ms {lin_leak:.1} dB"
    );
    assert!(leak < -55.0, "leakage below 17 kHz: {leak} dB");
    assert!(leak < lin_leak - 15.0);
    let fuzz = imd_residual_db(&g, &tones, 30.0, RATE);
    let boost = imd_residual_db(&g, &tones, 3.0, RATE);
    let quieter: Vec<f32> = tones.iter().map(|v| v * 0.1).collect();
    let fuzz_40 = imd_residual_db(&g, &quieter, 30.0, RATE);
    println!("IMD residue: fuzz {fuzz:.1} dB, clean boost {boost:.1} dB, fuzz at -40 dBFS {fuzz_40:.1} dB");
    assert!((-22.0..=-12.0).contains(&fuzz), "fuzz residue {fuzz} dB");
    assert!(boost < -33.0, "clean-boost residue {boost} dB");
    // 10 dB less tone level buys at least 10 dB (t·p² products) and up to
    // 20 dB (t²·p) of residue.
    assert!(fuzz_40 < fuzz - 8.0, "level scaling: {fuzz} vs {fuzz_40}");
}

/// The plan survival table's hard facts: every tone of `hf_robust` passes
/// a sharp 48 kHz converter within 6 dB; the 44.1 kHz hop loses most of
/// them; the 96 kHz plan loses all of them through a 48 kHz device.
#[test]
fn tone_plan_survival() {
    assert_eq!(surviving_tones(&PLANS[0], Converter::Sharp48), 16);
    assert_eq!(surviving_tones(&PLANS[0], Converter::Hop441), 16);
    // The slow model tilts the band by 12 dB (−14 dB at 19 kHz, −26.5 dB
    // at 21.5 kHz); the receiver does not mind a tilt, as the delivery
    // table shows, but the top tones are 6 dB under the bottom ones.
    assert!(surviving_tones(&PLANS[0], Converter::Slow48) >= 8);
    assert!(surviving_tones(&PLANS[0], Converter::Src441) <= 11);
    assert!(surviving_tones(&PLANS[0], Converter::Codec19Wall) <= 3);
    assert!(surviving_tones(&PLANS[3], Converter::Sharp48) < 16);
    // 24.0 kHz sits at the 48 kHz device's −6 dB point and 26.2 kHz is
    // −60 dB: at most the bottom quarter of the plan comes through.
    assert!(surviving_tones(&PLANS[5], Converter::Hop96To48) <= 4);
}

// ---------------------------------------------------------------------------
// Report

#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn hf_report() {
    let trials = env_usize("HF_TRIALS", 400);
    let false_seconds = env_usize("HF_FALSE_SECONDS", 60) as f32;
    let want = sections();
    let on = |s: &str| want.iter().any(|w| w == s);
    let profiles: Vec<(&str, PhyConfig)> = vec![
        ("instrument (hf_robust)", PhyConfig::hf_robust()),
        ("instrument-fast (hf_fast)", PhyConfig::hf_fast()),
        ("hf_robust, 2nd-order band", {
            let mut c = PhyConfig::hf_robust();
            c.rx_band_order = 2;
            c.rx_band_hz = Some((0.7 * HF_BASE_HZ, 1.3 * 21_500.0));
            c
        }),
    ];
    println!("\n## HF profile report ({trials} trials per cell)\n");
    if on("plans") {
        print_plan_table();
    }
    if on("band") {
        print_band_table();
    }
    if on("training") {
        println!("\n### Training cells (seed base {TRAINING_SEED:#x})\n");
        let results = run_table(&profiles, TRAINING, trials, TRAINING_SEED);
        let f = print_table(&profiles, TRAINING, &results);
        println!("\nfalse messages (all cells): {f}");
    }
    if on("heldout") {
        println!("\n### Held-out cells (seed base {HELD_OUT_SEED:#x})\n");
        let results = run_table(&profiles, HELD_OUT, trials, HELD_OUT_SEED);
        let f = print_table(&profiles, HELD_OUT, &results);
        println!("\nfalse messages (all cells): {f}");
    }
    if on("false") {
        print_false_table(false_seconds);
    }
    if on("cpu") {
        print_cpu_table();
    }
    if on("imd") {
        print_imd_table();
    }
    let hf = PhyConfig::hf_robust();
    println!(
        "\nAirtime per message: {:.1} ms ({} symbols of {:.1} ms + one period of silence).",
        repetition_samples(&hf) as f32 * 1000.0 / RATE as f32,
        hf.preamble.len() + hf.fec.body_nibbles(),
        hf.symbol_duration_ms + hf.guard_gap_ms
    );
}
