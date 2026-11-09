#![forbid(unsafe_code)]

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use cricket_core::pathcheck::{self, PathImpairment, PathReport, Verdict};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::{MscCommand, MscCue, MscMessage, MscTarget, TransportMessage};
use cricket_core::wav::{read_wav_f32, write_wav_f32};
use cricket_engine::{
    audio, format_message, measure_chord_loopback, measure_midi_loopback_with, measure_throughput,
    midi, AlarmKind, AudioIn, AudioOptions, AudioOut, DecoderConfig, DecoderHandle, DeviceSelector,
    EncoderConfig, EncoderHandle, EngineEvent, MidiIn, MidiOut, Pacing, SampleFormatPref,
};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "cricket", author, version, about = "Cricket MIDI-over-audio modem CLI", long_about = None)]
struct Cli {
    /// PHY profile (both ends must use the same one): robust (default;
    /// Reed–Solomon coded body, sent once), robust_repeat (uncoded body sent
    /// twice; for lines with dropouts of ~50 ms or more), fast (clean
    /// dedicated line), or the LTC-style biphase-mark profiles for a
    /// dedicated clean line without EQ: ltc (4800 bit/s), ltc2400, ltc4800,
    /// ltc9600, and with Reed–Solomon body FEC ltc_fec, ltc2400_fec,
    /// ltc9600_fec; for digital lines only (Dante, MADI, AES; >= 48 kHz)
    /// ltc14400 and ltc14400_fec; or (experimental) digital, packets in the
    /// sample values of a bit-transparent digital path (check it with
    /// `cricket pathcheck` first). A `+digital` suffix (e.g.
    /// ltc9600+digital) makes a decoder also accept digital-profile frames
    #[arg(long, global = true, default_value = "robust")]
    profile: String,

    /// Audio device buffer size in frames, e.g. 64–512 (default: the host's;
    /// ALSA uses 100 ms). Smaller is faster, too small drops out
    #[arg(long, global = true, value_name = "FRAMES")]
    buffer_frames: Option<u32>,

    /// Audio sample format: auto (device default; the digital profile
    /// prefers i32, then f32, then i16), f32, i16 or i32
    #[arg(long, global = true, value_name = "FORMAT", default_value = "auto")]
    sample_format: SampleFormatPref,

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
        /// `robust,robust_repeat,fast,ltc_fec,digital`)
        #[arg(long, value_delimiter = ',', default_value = "fast,robust")]
        profiles: Vec<String>,
        /// NoteOns per profile
        #[arg(long, default_value_t = LOOPBACK_NOTES)]
        notes: usize,
        /// Also strike this many chords (all notes at once) and report the
        /// latency of their first and last note
        #[arg(long, default_value_t = 0)]
        chords: usize,
        /// Notes per chord
        #[arg(long, default_value_t = 4)]
        chord_size: usize,
        /// Also send this many Program Changes at once and report messages/s
        #[arg(long, default_value_t = 0)]
        throughput: usize,
        /// Deliver audio to the decoder as soon as it is written instead of
        /// at the sample rate: the engine's own overhead, without airtime
        #[arg(long, default_value_t = false)]
        zero_airtime: bool,
    },
    /// Is an audio path bit-transparent? (for the `digital` profile)
    ///
    /// Run `pathcheck send` on the sending machine and `pathcheck receive`
    /// on the receiving one (or `pathcheck loop` when both ends are on one
    /// machine). The receiver reports whether every sample arrived
    /// unchanged and, if not, what the path did (gain, word length, dither,
    /// sample-rate conversion, polarity, offset, noise). The test signal
    /// peaks at -48 dBFS. `pathcheck sim` runs simulated paths in-process.
    Pathcheck {
        #[command(subcommand)]
        action: PathcheckAction,
    },
}

#[derive(Subcommand)]
enum PathcheckAction {
    /// Play the test pattern (until Ctrl-C, or for --seconds)
    Send {
        /// Output audio device index (default device if neither is given)
        #[arg(short, long)]
        audio_out: Option<usize>,
        /// Output audio device by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "audio_out")]
        audio_out_name: Option<String>,
        /// Stop after this many seconds
        #[arg(long)]
        seconds: Option<f64>,
        /// Write the pattern to a 32-bit float WAV file instead of playing it
        #[arg(long, value_name = "FILE")]
        wav_out: Option<String>,
    },
    /// Record the far end of the path and analyse it
    Receive {
        /// Input audio device index (default device if neither is given)
        #[arg(short, long)]
        audio_in: Option<usize>,
        /// Input audio device by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "audio_in")]
        audio_in_name: Option<String>,
        /// Seconds to record
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
        /// Analyse this WAV recording instead of recording a device
        #[arg(long, value_name = "FILE")]
        wav_in: Option<String>,
    },
    /// Send and receive at once on this machine (output -> path -> input)
    Loop {
        #[arg(short, long)]
        audio_out: Option<usize>,
        #[arg(long, value_name = "NAME", conflicts_with = "audio_out")]
        audio_out_name: Option<String>,
        #[arg(short = 'i', long)]
        audio_in: Option<usize>,
        #[arg(long, value_name = "NAME", conflicts_with = "audio_in")]
        audio_in_name: Option<String>,
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
    },
    /// Simulated paths, in-process (no hardware): `all` for the table, or
    /// one of the names it lists for the full report
    Sim {
        #[arg(default_value = "all")]
        path: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let profile = cli.profile.as_str();
    let audio = AudioOptions {
        buffer_frames: cli.buffer_frames,
        sample_format: cli.sample_format,
        digital: false,
    };

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
                &audio,
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
                &audio,
            )?;
        }
        Commands::ListDevices => {
            list_all_devices()?;
        }
        Commands::Test { wav_out, wav_in } => {
            run_test_mode(wav_out.as_deref(), wav_in.as_deref(), profile)?;
        }
        Commands::Loopback {
            profiles,
            notes,
            chords,
            chord_size,
            throughput,
            zero_airtime,
        } => {
            let pacing = if *zero_airtime {
                Pacing::Instant
            } else {
                Pacing::RealTime
            };
            run_loopback_metrics(profiles, *notes, *chords, *chord_size, *throughput, pacing)?;
        }
        Commands::Pathcheck { action } => run_pathcheck(action, &audio)?,
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

fn run_loopback_metrics(
    profiles: &[String],
    notes: usize,
    chords: usize,
    chord_size: usize,
    throughput: usize,
    pacing: Pacing,
) -> Result<()> {
    println!(
        "Cricket engine loopback latency ({} MIDI in → MIDI out)...",
        match pacing {
            Pacing::RealTime => "paced",
            Pacing::Instant => "zero-airtime",
        }
    );
    // Sub-millisecond results need more digits.
    let ms = |v: f64| {
        if v < 10.0 {
            format!("{:.2}", v)
        } else {
            format!("{:.1}", v)
        }
    };
    for profile in profiles {
        let s = measure_midi_loopback_with(profile, notes, pacing)?;
        if s.n < notes {
            return Err(anyhow!(
                "{}: only {} of {} NoteOns delivered",
                profile,
                s.n,
                notes
            ));
        }
        println!(
            "{}: n={} mean={} ms p95={} ms jitter={} ms",
            profile,
            s.n,
            ms(s.mean_ms),
            ms(s.p95_ms),
            ms(s.jitter_ms)
        );
        if chords > 0 {
            let c = measure_chord_loopback(profile, chords, chord_size)?;
            println!(
                "{}: {}-note chords n={} first note mean={} ms, last note mean={} ms p95={} ms",
                profile,
                chord_size,
                c.last.n,
                ms(c.first.mean_ms),
                ms(c.last.mean_ms),
                ms(c.last.p95_ms)
            );
        }
        if throughput > 0 {
            let rate = measure_throughput(profile, throughput)?;
            println!(
                "{}: throughput {:.0} messages/s ({} Program Changes)",
                profile, rate, throughput
            );
        }
    }
    Ok(())
}

fn print_report(report: &PathReport) {
    for line in report.lines() {
        println!("  {}", line);
    }
}

fn stream_line(what: &str, info: &cricket_engine::pathcheck::StreamInfo) -> String {
    format!(
        "{} {} Hz, {} ch{}",
        what,
        info.0,
        info.1,
        if info.2.is_empty() {
            String::new()
        } else {
            format!(", {}", info.2)
        }
    )
}

/// Exit status 1 when the digital profile would not work.
fn finish_pathcheck(report: &PathReport) -> Result<()> {
    if matches!(report.verdict, Verdict::NoSignal | Verdict::Unusable) {
        std::process::exit(1);
    }
    Ok(())
}

fn run_pathcheck(action: &PathcheckAction, options: &AudioOptions) -> Result<()> {
    use cricket_engine::pathcheck as engine_pc;
    match action {
        PathcheckAction::Send {
            audio_out,
            audio_out_name,
            seconds,
            wav_out,
        } => {
            if let Some(path) = wav_out {
                let secs = seconds.unwrap_or(10.0);
                let periods = (secs * 48_000.0 / pathcheck::PERIOD as f64).ceil() as usize;
                write_wav_f32(path, 48_000, &pathcheck::pattern_samples(periods))
                    .map_err(|e| anyhow!(e))?;
                println!(
                    "Wrote {:.1} s of the path-check pattern (48 kHz, 32-bit float) to {}",
                    periods as f64 * pathcheck::PERIOD as f64 / 48_000.0,
                    path
                );
                return Ok(());
            }
            let stop = AtomicBool::new(false);
            engine_pc::play_pattern(
                AudioOut::Device(selector(*audio_out, audio_out_name)),
                options,
                seconds.map(Duration::from_secs_f64),
                &stop,
                |info| {
                    println!(
                        "Cricket path check: playing the test pattern ({}; peak -48 dBFS, \
                         mono on every channel). Ctrl-C to stop.",
                        stream_line("output", info)
                    );
                },
            )?;
        }
        PathcheckAction::Receive {
            audio_in,
            audio_in_name,
            seconds,
            wav_in,
        } => {
            let report = if let Some(path) = wav_in {
                let (rate, samples) = read_wav_f32(path).map_err(|e| anyhow!(e))?;
                println!(
                    "Cricket path check: analysing {} ({} samples at {} Hz, channel 1)",
                    path,
                    samples.len(),
                    rate
                );
                pathcheck::analyze(&samples, rate)
            } else {
                println!("Cricket path check: recording {:.1} s ...", seconds);
                let (samples, info) = engine_pc::capture(
                    AudioIn::Device(selector(*audio_in, audio_in_name)),
                    options,
                    Duration::from_secs_f64(*seconds),
                )?;
                println!("  {} (channel 1)", stream_line("input", &info));
                pathcheck::analyze(&samples, info.0)
            };
            print_report(&report);
            finish_pathcheck(&report)?;
        }
        PathcheckAction::Loop {
            audio_out,
            audio_out_name,
            audio_in,
            audio_in_name,
            seconds,
        } => {
            println!(
                "Cricket path check: loop through this machine, {:.1} s ...",
                seconds
            );
            let (samples, out_info, in_info) = engine_pc::loop_check(
                AudioOut::Device(selector(*audio_out, audio_out_name)),
                AudioIn::Device(selector(*audio_in, audio_in_name)),
                options,
                Duration::from_secs_f64(*seconds),
            )?;
            println!("  {}", stream_line("output", &out_info));
            println!("  {} (channel 1)", stream_line("input", &in_info));
            let report = pathcheck::analyze(&samples, in_info.0);
            print_report(&report);
            finish_pathcheck(&report)?;
        }
        PathcheckAction::Sim { path } => run_pathcheck_sim(path)?,
    }
    Ok(())
}

fn run_pathcheck_sim(name: &str) -> Result<()> {
    let presets = PathImpairment::presets();
    let pattern = pathcheck::pattern_samples(30);
    if name != "all" {
        let Some((n, path)) = presets.iter().find(|(n, _)| *n == name) else {
            let names: Vec<&str> = presets.iter().map(|(n, _)| *n).collect();
            return Err(anyhow!(
                "unknown path {:?}; one of: all, {}",
                name,
                names.join(", ")
            ));
        };
        println!("Cricket path check, simulated path `{}` ({:?}):", n, path);
        let report = pathcheck::analyze(&pathcheck::simulate(path, &pattern), 48_000);
        print_report(&report);
        return Ok(());
    }
    println!("Cricket path check, simulated paths (3 s of pattern each):");
    println!(
        "{:<24} {:<10} {:>10} {:>12} {:>8}",
        "path", "verdict", "bit-exact", "symbols ok", "probes"
    );
    for (n, path) in &presets {
        let r = pathcheck::analyze(&pathcheck::simulate(path, &pattern), 48_000);
        println!(
            "{:<24} {:<10} {:>9.2}% {:>11.4}% {:>4}/{:<3}",
            n,
            format!("{:?}", r.verdict),
            100.0 * r.exact as f64 / r.compared.max(1) as f64,
            100.0 * r.symbols_ok as f64 / r.symbols.max(1) as f64,
            r.probes_ok,
            r.probes_expected
        );
    }
    println!("Details: cricket pathcheck sim <path>");
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
    audio: &AudioOptions,
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
        audio: audio.clone(),
    })?;
    print_events(dec.events(), "Decoder")
}

fn run_encoder(
    midi_in: DeviceSelector,
    audio_out: DeviceSelector,
    simulate: bool,
    profile: &str,
    audio: &AudioOptions,
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
        audio: audio.clone(),
    })?;
    print_events(enc.events(), "Encoder")
}
