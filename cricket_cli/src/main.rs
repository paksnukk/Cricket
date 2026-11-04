use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use cricket_core::pedal::{
    BankMode, PedalCommand, PedalConfig, PedalRx, PedalTx, PresetMap, PresetState,
};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::{MscCommand, MscCue, MscMessage, MscTarget, TransportMessage};
use cricket_core::wav::{read_wav_f32, write_wav_f32};
use cricket_engine::measure_midi_loopback;
use cricket_engine::{
    audio, format_message, midi, AlarmKind, AudioIn, AudioOut, DecoderConfig, DecoderHandle,
    DeviceSelector, EncoderConfig, EncoderHandle, EngineEvent, MidiIn, MidiOut,
};
use std::sync::mpsc::Receiver;

#[derive(Parser)]
#[command(author, version, about = "Cricket MIDI-over-audio modem CLI", long_about = None)]
struct Cli {
    /// PHY profile (both ends must use the same one): robust (default;
    /// Reed–Solomon coded body, sent once), robust_repeat (uncoded body sent
    /// twice; for lines with dropouts of ~50 ms or more), fast (clean
    /// dedicated line), or the LTC-style biphase-mark profiles for a
    /// dedicated clean line without EQ: ltc (4800 bit/s), ltc2400, ltc4800,
    /// ltc9600, and with Reed–Solomon body FEC ltc_fec, ltc2400_fec,
    /// ltc9600_fec
    #[arg(long, global = true, default_value = "robust")]
    profile: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the real-time encoder (MIDI -> Audio)
    ///
    /// Devices are chosen by index or by exact name (see `list-devices`);
    /// a name that is not present is an error, never another device.
    Encode {
        /// Input MIDI device index
        #[arg(short, long)]
        midi_in: Option<usize>,

        /// Input MIDI port by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "midi_in")]
        midi_in_name: Option<String>,

        /// Output Audio device index (default device if neither is given)
        #[arg(short, long)]
        audio_out: Option<usize>,

        /// Output Audio device by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "audio_out")]
        audio_out_name: Option<String>,

        /// Simulate a C-major scale if no MIDI device is used
        #[arg(long, default_value_t = false)]
        simulate: bool,
    },
    /// Start the real-time decoder (Audio -> MIDI)
    ///
    /// Devices are chosen by index or by exact name (see `list-devices`);
    /// a name that is not present is an error, never another device.
    Decode {
        /// Input Audio device index (default device if neither is given)
        #[arg(short, long)]
        audio_in: Option<usize>,

        /// Input Audio device by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "audio_in")]
        audio_in_name: Option<String>,

        /// Output MIDI device index
        #[arg(short, long)]
        midi_out: Option<usize>,

        /// Output MIDI port by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "midi_out")]
        midi_out_name: Option<String>,

        /// MSC device ID put on MSC output (0–127, decimal or 0x hex;
        /// 0x7F = all-call)
        #[arg(long, value_name = "ID", value_parser = parse_7bit, default_value = "0x7F")]
        msc_device_id: u8,

        /// MSC command format put on MSC output (0x01 Lighting (General),
        /// 0x10 Sound (General), 0x7F All-types, …)
        #[arg(long, value_name = "FORMAT", value_parser = parse_7bit, default_value = "0x01")]
        msc_format: u8,
    },
    /// List available Audio and MIDI devices
    ListDevices,
    /// Synthetic / WAV loopback test for the Cricket modem
    Test {
        /// Optional path to write encoded WAV
        #[arg(long)]
        wav_out: Option<String>,
        /// Optional path to decode from WAV instead of synthetic encode
        #[arg(long)]
        wav_in: Option<String>,
    },
    /// Encode→decode loopback with latency measurement (in-process)
    Loopback {
        /// Profiles to measure, comma-separated (e.g.
        /// `robust,robust_repeat,fast,ltc_fec`)
        #[arg(long, value_delimiter = ',', default_value = "fast,robust")]
        profiles: Vec<String>,
        /// NoteOns per profile
        #[arg(long, default_value_t = LOOPBACK_NOTES)]
        notes: usize,
    },
    /// Pedal commands (experimental, not AML): write a WAV with bank/program
    /// commands in 20–21.5 kHz, optionally mixed into a 48 kHz recording, to
    /// play through a real rig (docs/proposals/pedal-commands.md)
    PedalWav {
        /// Commands in order, comma-separated: bank-up, bank-down,
        /// program-up, program-down
        #[arg(long, value_delimiter = ',', required = true)]
        commands: Vec<String>,
        /// Output WAV (48 kHz mono, 32-bit float)
        #[arg(long)]
        out: String,
        /// Mix the commands into this 48 kHz recording (first channel)
        #[arg(long)]
        mix_into: Option<String>,
        /// Time from one command start to the next, ms
        #[arg(long, default_value_t = 1000)]
        every_ms: u32,
        /// Peak level of the command bursts, dBFS
        #[arg(long, default_value_t = -30.0, allow_hyphen_values = true)]
        level_dbfs: f32,
        /// Signal variant: standard (16 ms), three_symbols (12 ms),
        /// short_symbols (12 ms); both ends must match
        #[arg(long, default_value = "standard")]
        variant: String,
    },
    /// Pedal commands (experimental, not AML): find commands in a 48 kHz WAV
    /// recording and print the MIDI a pedal-side decoder would send
    PedalScan {
        /// Input WAV (48 kHz; first channel)
        #[arg(long = "in", value_name = "WAV")]
        input: String,
        /// Signal variant, as for pedal-wav
        #[arg(long, default_value = "standard")]
        variant: String,
        /// Symbols per command that may show no tone (dropouts); 0 or 1
        #[arg(long, default_value_t = 0)]
        max_misses: usize,
        /// MIDI channel 1–16
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
        channel: u8,
        /// Banks on the pedal
        #[arg(long, default_value_t = 32)]
        banks: u8,
        /// Presets per bank
        #[arg(long, default_value_t = 4)]
        programs_per_bank: u8,
        /// Send Bank Select (CC 0) = bank and Program Change = slot, instead
        /// of Program Change = bank × programs_per_bank + slot
        #[arg(long)]
        bank_select: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let profile = cli.profile.as_str();

    match &cli.command {
        Commands::Encode {
            midi_in,
            midi_in_name,
            audio_out,
            audio_out_name,
            simulate,
        } => {
            run_encoder(
                selector(*midi_in, midi_in_name),
                selector(*audio_out, audio_out_name),
                *simulate,
                profile,
            )?;
        }
        Commands::Decode {
            audio_in,
            audio_in_name,
            midi_out,
            midi_out_name,
            msc_device_id,
            msc_format,
        } => {
            run_decoder(
                selector(*audio_in, audio_in_name),
                selector(*midi_out, midi_out_name),
                MscTarget {
                    device_id: *msc_device_id,
                    command_format: *msc_format,
                },
                profile,
            )?;
        }
        Commands::ListDevices => {
            list_all_devices()?;
        }
        Commands::Test { wav_out, wav_in } => {
            run_test_mode(wav_out.as_deref(), wav_in.as_deref(), profile)?;
        }
        Commands::Loopback { profiles, notes } => {
            run_loopback_metrics(profiles, *notes)?;
        }
        Commands::PedalWav {
            commands,
            out,
            mix_into,
            every_ms,
            level_dbfs,
            variant,
        } => {
            run_pedal_wav(
                commands,
                out,
                mix_into.as_deref(),
                *every_ms,
                *level_dbfs,
                variant,
            )?;
        }
        Commands::PedalScan {
            input,
            variant,
            max_misses,
            channel,
            banks,
            programs_per_bank,
            bank_select,
        } => {
            let map = PresetMap {
                channel: channel - 1,
                banks: *banks,
                programs_per_bank: *programs_per_bank,
                mode: if *bank_select {
                    BankMode::BankSelect
                } else {
                    BankMode::Flat
                },
            };
            run_pedal_scan(input, variant, *max_misses, map)?;
        }
    }

    Ok(())
}

/// A 7-bit value, decimal or `0x` hex.
fn parse_7bit(s: &str) -> Result<u8, String> {
    let v = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => s.parse::<u8>(),
    }
    .map_err(|e| format!("{}: {}", s, e))?;
    if v > 0x7F {
        return Err(format!("{} is above 0x7F", s));
    }
    Ok(v)
}

/// Device from an index flag or a name flag (clap rejects both at once);
/// `Default` when neither is given.
fn selector(index: Option<usize>, name: &Option<String>) -> DeviceSelector {
    match (index, name) {
        (_, Some(n)) => DeviceSelector::Name(n.clone()),
        (i, None) => DeviceSelector::from_index(i),
    }
}

fn list_all_devices() -> Result<()> {
    println!("Host: {}", audio::host_name());
    println!("--- Input Devices ---");
    for (index, name) in audio::list_input_devices()?.iter().enumerate() {
        println!("{}: {}", index, name);
    }
    println!("--- Output Devices ---");
    for (index, name) in audio::list_output_devices()?.iter().enumerate() {
        println!("{}: {}", index, name);
    }
    println!("--- MIDI Input Devices ---");
    for (i, name) in midi::list_inputs()?.iter().enumerate() {
        println!("{}: {}", i, name);
    }
    println!("--- MIDI Output Devices ---");
    for (i, name) in midi::list_outputs()?.iter().enumerate() {
        println!("{}: {}", i, name);
    }
    Ok(())
}

fn pedal_config(variant: &str) -> Result<PedalConfig> {
    PedalConfig::from_name(variant).ok_or_else(|| {
        anyhow!(
            "unknown variant {:?} (standard, three_symbols, short_symbols)",
            variant
        )
    })
}

fn run_pedal_wav(
    commands: &[String],
    out: &str,
    mix_into: Option<&str>,
    every_ms: u32,
    level_dbfs: f32,
    variant: &str,
) -> Result<()> {
    let config = pedal_config(variant)?;
    let rate = config.sample_rate_hz;
    let cmds = commands
        .iter()
        .map(|c| PedalCommand::from_name(c).ok_or_else(|| anyhow!("unknown command {:?}", c)))
        .collect::<Result<Vec<_>>>()?;
    let every = (every_ms as u64 * rate as u64 / 1000) as usize;
    if every < config.total_samples() {
        return Err(anyhow!(
            "--every-ms must be at least {:.0} ms",
            config.total_samples() as f32 * 1000.0 / rate as f32
        ));
    }
    let mut samples = match mix_into {
        Some(path) => {
            let (r, s) = read_wav_f32(path).map_err(|e| anyhow!(e))?;
            if r != rate {
                return Err(anyhow!(
                    "{} is {} Hz; pedal commands need {} Hz",
                    path,
                    r,
                    rate
                ));
            }
            s
        }
        None => Vec::new(),
    };
    let first = rate as usize / 2;
    let needed = first + (cmds.len().max(1) - 1) * every + config.total_samples() + first;
    if samples.len() < needed {
        samples.resize(needed, 0.0);
    }
    let gain = 10f32.powf(level_dbfs / 20.0) / config.amplitude;
    let tx = PedalTx::new(config);
    for (i, &c) in cmds.iter().enumerate() {
        let start = first + i * every;
        for (k, v) in tx.synthesize(c).iter().enumerate() {
            samples[start + k] += gain * v;
        }
        println!("{:8.3} s  {}", start as f32 / rate as f32, c.name());
    }
    write_wav_f32(out, rate, &samples).map_err(|e| anyhow!(e))?;
    println!(
        "Wrote {} commands ({} samples @ {} Hz) to {}",
        cmds.len(),
        samples.len(),
        rate,
        out
    );
    Ok(())
}

fn run_pedal_scan(input: &str, variant: &str, max_misses: usize, map: PresetMap) -> Result<()> {
    let config = PedalConfig {
        max_misses,
        ..pedal_config(variant)?
    };
    config.validate().map_err(|e| anyhow!(e))?;
    let (rate, samples) = read_wav_f32(input).map_err(|e| anyhow!(e))?;
    if rate != config.sample_rate_hz {
        return Err(anyhow!(
            "{} is {} Hz; pedal commands need {} Hz (at 44.1 kHz there is no room above 20 kHz)",
            input,
            rate,
            config.sample_rate_hz
        ));
    }
    let mut state = PresetState::new(map).map_err(|e| anyhow!(e))?;
    let mut rx = PedalRx::new(config);
    let detections = rx.push_samples(&samples);
    for d in &detections {
        let midi: Vec<String> = state
            .apply(d.command)
            .iter()
            .map(|m| {
                let (kind, payload) = format_message(m);
                format!("{} {}", kind, payload)
            })
            .collect();
        println!(
            "{:8.3} s  {:<12}  preset {:3} (bank {}, slot {})  -> {}",
            d.at_sample as f64 / rate as f64,
            d.command.name(),
            state.index(),
            state.bank(),
            state.slot(),
            midi.join(", ")
        );
    }
    let st = rx.stats();
    println!(
        "{} commands in {:.1} s; {:.2} % of symbol windows showed a tone",
        detections.len(),
        samples.len() as f64 / rate as f64,
        100.0 * st.gated_windows as f64 / st.windows.max(1) as f64
    );
    Ok(())
}

fn run_test_mode(wav_out: Option<&str>, wav_in: Option<&str>, profile: &str) -> Result<()> {
    println!("Running Cricket loopback test (profile={})...", profile);
    let config = PhyConfig::from_name(profile);
    let messages = vec![
        TransportMessage::NoteOn {
            channel: 0,
            note: 60,
            velocity: 100,
        },
        TransportMessage::ControlChange {
            channel: 9,
            controller: 7,
            value: 90,
        },
        TransportMessage::PitchBend {
            channel: 15,
            value: 12_000,
        },
        TransportMessage::NoteOff {
            channel: 0,
            note: 60,
            velocity: 40,
        },
        TransportMessage::ProgramChange {
            channel: 2,
            program: 10,
        },
        TransportMessage::Msc(MscMessage::cue(
            MscCommand::Go,
            MscCue::from_tenths(145).expect("in range"),
            Some(1),
        )),
        TransportMessage::Heartbeat,
    ];

    let samples = if let Some(path) = wav_in {
        let (rate, samples) = read_wav_f32(path).map_err(|e| anyhow!(e))?;
        println!(
            "Loaded {} samples @ {} Hz from {}",
            samples.len(),
            rate,
            path
        );
        samples
    } else {
        let samples = cricket_core::encode_to_samples(config.clone(), &messages);
        if let Some(path) = wav_out {
            write_wav_f32(path, config.sample_rate_hz, &samples).map_err(|e| anyhow!(e))?;
            println!("Wrote {} samples to {}", samples.len(), path);
        }
        samples
    };

    let decoded = cricket_core::decode_samples(config, &samples);
    println!("Decoded {} messages:", decoded.len());
    for m in &decoded {
        let (kind, payload) = format_message(m);
        println!("  {} {}", kind, payload);
    }

    if wav_in.is_none() {
        if decoded != messages {
            return Err(anyhow!("decoded messages differ from the sent ones"));
        }
        println!("Test Passed!");
    }
    Ok(())
}

const LOOPBACK_NOTES: usize = 20;

fn run_loopback_metrics(profiles: &[String], notes: usize) -> Result<()> {
    println!("Cricket engine loopback latency (paced MIDI in → MIDI out)...");
    for profile in profiles {
        let s = measure_midi_loopback(profile, notes)?;
        if s.n < notes {
            return Err(anyhow!(
                "{}: only {} of {} NoteOns delivered",
                profile,
                s.n,
                notes
            ));
        }
        println!(
            "{}: n={} mean={:.1} ms p95={:.1} ms jitter={:.1} ms",
            profile, s.n, s.mean_ms, s.p95_ms, s.jitter_ms
        );
    }
    Ok(())
}

/// Print engine events until the worker stops (it only stops on its own
/// after a device error, so that is reported as a failure).
fn print_events(events: &Receiver<EngineEvent>, what: &str) -> Result<()> {
    for event in events {
        match event {
            EngineEvent::Log(line) => println!("{}", line),
            EngineEvent::Decoded(msg) => {
                let (kind, payload) = format_message(&msg);
                println!("[MSG] {} {}", kind, payload);
            }
            EngineEvent::Alarm {
                kind: AlarmKind::LinkDead,
                ..
            } => println!("[LINK DEAD] All Notes Off"),
            EngineEvent::Alarm { message, .. } => println!("[ALARM] {}", message),
            EngineEvent::AlarmCleared { message, .. } => println!("[ALARM CLEARED] {}", message),
            EngineEvent::Stopped => break,
            EngineEvent::Level(_) | EngineEvent::Stats(_) | EngineEvent::TxStatus { .. } => {}
        }
    }
    Err(anyhow!("{} stopped", what))
}

fn run_decoder(
    audio_in: DeviceSelector,
    midi_out: DeviceSelector,
    msc: MscTarget,
    profile: &str,
) -> Result<()> {
    println!("Initializing Cricket decoder...");
    let midi_out = match midi_out {
        DeviceSelector::Default => MidiOut::None,
        sel => {
            // Fail fast on a bad port, as before the engine.
            println!("MIDI Out: {}", midi::output_port_name(&sel)?);
            MidiOut::Device(sel)
        }
    };
    let dec = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Device(audio_in),
        midi_out,
        profile: profile.into(),
        link_dead_ms: None,
        msc,
    })?;
    print_events(dec.events(), "Decoder")
}

fn run_encoder(
    midi_in: DeviceSelector,
    audio_out: DeviceSelector,
    simulate: bool,
    profile: &str,
) -> Result<()> {
    println!("Initializing Cricket encoder...");
    let midi_in = match midi_in {
        DeviceSelector::Default => MidiIn::None,
        sel => {
            // Fail fast on a bad port, as before the engine.
            println!("MIDI In: {}", midi::input_port_name(&sel)?);
            MidiIn::Device(sel)
        }
    };
    if simulate || matches!(midi_in, MidiIn::None) {
        println!("Simulation mode: C-major scale + heartbeats");
    }
    let enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Device(audio_out),
        midi_in,
        profile: profile.into(),
        simulate,
    })?;
    print_events(enc.events(), "Encoder")
}
