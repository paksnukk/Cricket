#![forbid(unsafe_code)]

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use cricket_core::modem::repetition_samples;
use cricket_core::modem::{CricketModemRx, CricketModemTx, ModemOutput};
use cricket_core::pathcheck::{self, PathReport, Verdict};
use cricket_core::phy::{
    PhyConfig, ProfileInfo, ProfileTier, DEFAULT_PROFILE, DEFAULT_TX_LEVEL_DBFS, HF_TX_LEVEL_DBFS,
    PROFILES,
};
use cricket_core::transport::{MscCommand, MscCue, MscMessage, MscTarget, TransportMessage};
use cricket_core::wav::{read_wav_f32, write_wav_f32};
use cricket_engine::{
    audio, format_message, measure_chord_loopback, measure_midi_loopback_pattern,
    measure_throughput, midi, parse_link_key, rms_to_dbfs, AudioIn, AudioOptions, AudioOut,
    DecoderConfig, DecoderHandle, DeviceSelector, EncoderConfig, EncoderHandle, EngineEvent,
    LinkLight, LinkStats, MidiIn, MidiOut, MscFilter, NotePattern, Pacing, PassthroughConfig,
    PassthroughHandle, SampleFormatPref, TapConfig, TapDecoder, MIN_TX_LEVEL_DBFS,
};
use cricket_sim::PathImpairment;
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LONG_ABOUT: &str = "\
Cricket MIDI-over-audio modem CLI (AML v1).

Both ends must use the same profile (`cricket profiles` explains them). While a decoder runs,
Ctrl-C stops it after sending Sustain off + All Notes Off on all 16 channels to MIDI out, and
typing `panic` + Enter sends that burst at once.

Exit status: 0 ok; 1 error (device not found, worker stopped, test failed, path check NoSignal
or Unusable); 2 usage error; 3 path check verdict Marginal.";

/// Exit status of a path check whose verdict is `Marginal`.
const EXIT_MARGINAL: u8 = 3;

#[derive(Parser)]
#[command(name = "cricket", author, version, about = "Cricket MIDI-over-audio modem CLI", long_about = LONG_ABOUT)]
struct Cli {
    /// Profile, same on both ends: analog-safe (default), analog-safe-repeat,
    /// analog-fast, clean-line, clean-line-fast, digital-line,
    /// digital-bit-exact, instrument; `cricket profiles` lists all, with the
    /// older names. `hf-mix`, `hf-scan` and `passthrough` default to
    /// `instrument`
    #[arg(long, global = true, value_name = "NAME", value_parser = parse_profile)]
    profile: Option<String>,

    /// Audio device buffer size in frames, e.g. 64–512 (default: the host's;
    /// ALSA uses 100 ms). Smaller is faster, too small drops out
    #[arg(long, global = true, value_name = "FRAMES")]
    buffer_frames: Option<u32>,

    /// Audio sample format: auto (device default; digital-bit-exact prefers
    /// i32, then f32, then i16), f32, i16 or i32
    #[arg(long, global = true, value_name = "FORMAT", default_value = "auto")]
    sample_format: SampleFormatPref,

    /// Audio input channel the decoder listens on (1 = first)
    #[arg(long, global = true, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    audio_in_channel: u16,

    /// Audio output channel the encoder sends on (1 = first); the other
    /// channels of the device stay silent
    #[arg(long, global = true, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    audio_out_channel: u16,

    /// Transmit level, peak dBFS (−60 … 0). Default: the profile's own
    /// (−12 dBFS, headroom on the bus; the instrument profiles −30 dBFS,
    /// inaudible above the music). The receiver is level-independent.
    /// Ignored by digital-bit-exact
    #[arg(long, global = true, value_name = "DB", allow_negative_numbers = true, value_parser = parse_tx_level)]
    tx_level_db: Option<f32>,

    /// Link key, the same on both ends: `0x1234` (hex) or a passphrase
    /// (hashed to 16 bits). Packets from a Cricket link with another key
    /// are ignored, so two links in one multicore or Dante domain do not
    /// pick up each other's crosstalk. Default: unkeyed (0xFFFF)
    #[arg(long, global = true, value_name = "KEY", default_value = "")]
    link_key: String,

    /// Machine-readable output for encode / decode: one JSON object per line
    /// (NDJSON) with every engine event, instead of the human log
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the real-time encoder (MIDI -> Audio)
    ///
    /// Devices are chosen by index or by exact name (see `list-devices`);
    /// a name that is not present is an error, never another device. With
    /// no MIDI in, only heartbeats are sent unless --simulate is given.
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

        /// Play a C-major test scale (one note per second). Never on by
        /// itself: a decoder on stage must not get notes nobody played
        #[arg(long, default_value_t = false)]
        simulate: bool,

        /// Carry only MSC addressed to this device ID (0–126, decimal or 0x
        /// hex) or to all-call 0x7F; for rigs with several MSC devices on
        /// the MIDI line. Default: any device ID
        #[arg(long, value_name = "ID", value_parser = parse_7bit)]
        msc_accept_device: Option<u8>,

        /// Carry only MSC in this command format (e.g. 0x10 Sound) or in
        /// all-types 0x7F. Default: any format
        #[arg(long, value_name = "FORMAT", value_parser = parse_7bit)]
        msc_accept_format: Option<u8>,
    },
    /// Start the real-time decoder (Audio -> MIDI)
    ///
    /// Devices are chosen by index or by exact name (see `list-devices`);
    /// a name that is not present is an error, never another device.
    /// Ctrl-C sends Sustain off + All Notes Off on all channels before
    /// stopping; `panic` + Enter sends it at once.
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

        /// Link-dead timeout in ms (default: the profile's, 500–2000);
        /// no valid packet for this long sends the fail-safe burst
        #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u32).range(100..=60_000))]
        link_dead_ms: Option<u32>,

        /// Do not send the fail-safe burst (Note Offs, Sustain off, All
        /// Notes Off) on link-dead, Panic or stop: for cue-only (MSC) links
        /// where a stray All Notes Off is unwanted. Link-dead is still
        /// detected and reported
        #[arg(long, default_value_t = false)]
        no_failsafe_burst: bool,
    },
    /// Pass audio through (in → out) while decoding a copy (the box)
    ///
    /// The instrument's signal goes from the audio input to the audio
    /// output unchanged (bit-exact; the device buffers set the delay) while
    /// a copy is decoded with --profile (default `instrument`) and the MIDI
    /// goes to --midi-out. With --strip the data band (the profile's
    /// receiver band, 18–22.5 kHz for the instrument profiles) is removed
    /// from the output by a linear-phase filter: 1.33 ms more delay on
    /// that output, nothing above 18 kHz left for a drive stage to
    /// intermodulate. Both devices must run at the same rate (48 kHz).
    /// Ctrl-C sends the fail-safe burst to MIDI out before stopping.
    Passthrough {
        /// Input Audio device index (default device if neither is given)
        #[arg(short, long)]
        audio_in: Option<usize>,

        /// Input Audio device by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "audio_in")]
        audio_in_name: Option<String>,

        /// Output Audio device index (default device if neither is given)
        #[arg(short = 'o', long)]
        audio_out: Option<usize>,

        /// Output Audio device by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "audio_out")]
        audio_out_name: Option<String>,

        /// Output MIDI device index (none: decode to the log only)
        #[arg(short, long)]
        midi_out: Option<usize>,

        /// Output MIDI port by exact name
        #[arg(long, value_name = "NAME", conflicts_with = "midi_out")]
        midi_out_name: Option<String>,

        /// Remove the data band from the audio output (the box's "stripped"
        /// output); adds the filter's group delay (1.33 ms at 48 kHz)
        #[arg(long, default_value_t = false)]
        strip: bool,

        /// MSC device ID put on MSC output (0–127, decimal or 0x hex;
        /// 0x7F = all-call)
        #[arg(long, value_name = "ID", value_parser = parse_7bit, default_value = "0x7F")]
        msc_device_id: u8,

        /// MSC command format put on MSC output
        #[arg(long, value_name = "FORMAT", value_parser = parse_7bit, default_value = "0x01")]
        msc_format: u8,

        /// Link-dead timeout in ms (default: the profile's)
        #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u32).range(100..=60_000))]
        link_dead_ms: Option<u32>,

        /// Do not send the fail-safe burst on link-dead, input stall or stop
        #[arg(long, default_value_t = false)]
        no_failsafe_burst: bool,
    },
    /// List available Audio and MIDI devices
    ListDevices,
    /// Explain the profiles: which line each is for, latency, older names
    Profiles,
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
        /// `analog-safe,analog-fast,clean-line,digital-bit-exact`)
        #[arg(long, value_delimiter = ',', default_value = "analog-fast,analog-safe", value_parser = parse_profile)]
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
        /// Overlap the notes (the previous note is released 60 ms after the
        /// next one is struck) instead of hold / release / rest
        #[arg(long, default_value_t = false)]
        legato: bool,
    },
    /// Embed MIDI into a recording for the instrument profiles (no hardware
    /// needed)
    ///
    /// Mixes the tones of --messages into --wav-in (48 kHz; the first
    /// channel of a stereo file is used) and writes --wav-out as 32-bit
    /// float. Play the mixed file through the rig (cable, pedals,
    /// interface), record the far end and run `hf-scan` on the recording.
    /// Profile: `instrument` unless --profile says otherwise; level: the
    /// profile's −30 dBFS unless --tx-level-db says otherwise.
    HfMix {
        /// Input recording (WAV, PCM or float, mono or stereo)
        #[arg(long, value_name = "FILE")]
        wav_in: String,
        /// Output file (32-bit float WAV, mono)
        #[arg(long, value_name = "FILE")]
        wav_out: String,
        /// Messages to embed, comma-separated: `note=<ch>:<note>:<vel>`
        /// (Note On, and a Note Off --hold-ms later), `pc=<ch>:<program>`,
        /// `cc=<ch>:<controller>:<value>`, `go=<cue>[:<list>]` (MSC GO, cue
        /// like 14.5), `heartbeat`, `panic`. Channels 1–16, values 0–127.
        /// Default: a Program Change, a Note On/Off and an MSC GO
        #[arg(long, value_delimiter = ',', value_name = "LIST")]
        messages: Vec<String>,
        /// Time of the first message, ms into the recording
        #[arg(long, default_value_t = 500.0, value_name = "MS")]
        start_ms: f32,
        /// Spacing between messages, ms
        #[arg(long, default_value_t = 1000.0, value_name = "MS")]
        every_ms: f32,
        /// Hold a note this long before its Note Off, ms (0 = no Note Off)
        #[arg(long, default_value_t = 400.0, value_name = "MS")]
        hold_ms: f32,
        /// Repeat the list until the recording ends
        #[arg(long, default_value_t = false)]
        repeat: bool,
    },
    /// Decode a recording with the instrument profiles' receiver
    ///
    /// Lists every message with its time, then the receiver's counters
    /// (packets passed and failed, repaired tones, other-key suspects) and
    /// the recording's level in the tone band, so a real rig can be judged
    /// before hardware exists. Profile: `instrument` unless --profile says
    /// otherwise.
    HfScan {
        /// The recording (WAV, PCM or float, mono or stereo)
        #[arg(long, value_name = "FILE")]
        wav_in: String,
    },
    /// Is an audio path bit-transparent? (for the digital-bit-exact profile)
    ///
    /// Run `pathcheck send` on the sending machine and `pathcheck receive`
    /// on the receiving one (or `pathcheck loop` when both ends are on one
    /// machine). The receiver reports whether every sample arrived
    /// unchanged and, if not, what the path did (gain, word length, dither,
    /// sample-rate conversion, polarity, offset, noise). The test signal
    /// peaks at -48 dBFS. `pathcheck sim` runs simulated paths in-process.
    /// Exit status: 0 BitExact or DigitalOk, 3 Marginal, 1 NoSignal or
    /// Unusable.
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    // The HF tools default to the instrument profile; everything else to
    // the catalog default.
    let hf_tool = matches!(
        cli.command,
        Commands::HfMix { .. } | Commands::HfScan { .. } | Commands::Passthrough { .. }
    );
    let profile_name = cli.profile.clone().unwrap_or_else(|| {
        if hf_tool {
            HF_PROFILE.to_string()
        } else {
            DEFAULT_PROFILE.to_string()
        }
    });
    let profile = profile_name.as_str();
    let link_key = parse_link_key(&cli.link_key);
    let audio = AudioOptions {
        buffer_frames: cli.buffer_frames,
        sample_format: cli.sample_format,
        digital: false,
        input_channel: usize::from(cli.audio_in_channel) - 1,
        output_channel: usize::from(cli.audio_out_channel) - 1,
    };

    match &cli.command {
        Commands::Encode {
            midi_in,
            midi_in_name,
            audio_out,
            audio_out_name,
            simulate,
            msc_accept_device,
            msc_accept_format,
        } => run_encoder(
            selector(*midi_in, midi_in_name),
            selector(*audio_out, audio_out_name),
            *simulate,
            profile,
            cli.tx_level_db,
            &audio,
            cli.json,
            EncoderLink {
                link_key,
                msc_filter: MscFilter {
                    device_id: *msc_accept_device,
                    command_format: *msc_accept_format,
                },
            },
        ),
        Commands::Decode {
            audio_in,
            audio_in_name,
            midi_out,
            midi_out_name,
            msc_device_id,
            msc_format,
            link_dead_ms,
            no_failsafe_burst,
        } => run_decoder(
            selector(*audio_in, audio_in_name),
            selector(*midi_out, midi_out_name),
            MscTarget {
                device_id: *msc_device_id,
                command_format: *msc_format,
            },
            profile,
            &audio,
            cli.json,
            DecoderLink {
                link_key,
                link_dead_ms: *link_dead_ms,
                fail_safe_burst: !*no_failsafe_burst,
            },
        ),
        Commands::Passthrough {
            audio_in,
            audio_in_name,
            audio_out,
            audio_out_name,
            midi_out,
            midi_out_name,
            strip,
            msc_device_id,
            msc_format,
            link_dead_ms,
            no_failsafe_burst,
        } => run_passthrough(
            selector(*audio_in, audio_in_name),
            selector(*audio_out, audio_out_name),
            selector(*midi_out, midi_out_name),
            MscTarget {
                device_id: *msc_device_id,
                command_format: *msc_format,
            },
            profile,
            &audio,
            cli.json,
            DecoderLink {
                link_key,
                link_dead_ms: *link_dead_ms,
                fail_safe_burst: !*no_failsafe_burst,
            },
            *strip,
        ),
        Commands::ListDevices => {
            list_all_devices()?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Profiles => {
            print_profiles();
            Ok(ExitCode::SUCCESS)
        }
        Commands::Test { wav_out, wav_in } => {
            run_test_mode(
                wav_out.as_deref(),
                wav_in.as_deref(),
                profile,
                cli.tx_level_db,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Loopback {
            profiles,
            notes,
            chords,
            chord_size,
            throughput,
            zero_airtime,
            legato,
        } => {
            let pacing = if *zero_airtime {
                Pacing::Instant
            } else {
                Pacing::RealTime
            };
            let pattern = if *legato {
                NotePattern::Legato
            } else {
                NotePattern::Detached
            };
            run_loopback_metrics(
                profiles,
                *notes,
                *chords,
                *chord_size,
                *throughput,
                pacing,
                pattern,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::HfMix {
            wav_in,
            wav_out,
            messages,
            start_ms,
            every_ms,
            hold_ms,
            repeat,
        } => {
            run_hf_mix(
                wav_in,
                wav_out,
                messages,
                HfMixTiming {
                    start_ms: *start_ms,
                    every_ms: *every_ms,
                    hold_ms: *hold_ms,
                    repeat: *repeat,
                },
                profile,
                cli.tx_level_db,
                link_key,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::HfScan { wav_in } => {
            run_hf_scan(wav_in, profile, link_key)?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Pathcheck { action } => run_pathcheck(action, &audio),
    }
}

/// The profile `hf-mix` and `hf-scan` use when `--profile` is not given.
const HF_PROFILE: &str = "instrument";

struct HfMixTiming {
    start_ms: f32,
    every_ms: f32,
    hold_ms: f32,
    repeat: bool,
}

/// One `--messages` item (see `Commands::HfMix`). A note item yields the
/// Note On and, with a hold, its Note Off.
fn parse_hf_message(item: &str) -> Result<(TransportMessage, Option<TransportMessage>)> {
    let item = item.trim();
    let (kind, args) = item.split_once('=').unwrap_or((item, ""));
    let fields: Vec<&str> = if args.is_empty() {
        Vec::new()
    } else {
        args.split(':').collect()
    };
    let num = |i: usize, what: &str, max: u32| -> Result<u8> {
        let s = fields
            .get(i)
            .ok_or_else(|| anyhow!("`{item}`: missing {what}"))?;
        let v: u32 = s
            .parse()
            .map_err(|_| anyhow!("`{item}`: {what} `{s}` is not a number"))?;
        if v > max {
            return Err(anyhow!("`{item}`: {what} {v} is above {max}"));
        }
        Ok(v as u8)
    };
    let channel = |i: usize| -> Result<u8> {
        let ch = num(i, "channel", 16)?;
        if ch == 0 {
            return Err(anyhow!("`{item}`: channels are 1–16"));
        }
        Ok(ch - 1)
    };
    match kind.to_ascii_lowercase().as_str() {
        "note" => {
            let channel = channel(0)?;
            let note = num(1, "note", 127)?;
            let velocity = num(2, "velocity", 127)?.max(1);
            Ok((
                TransportMessage::NoteOn {
                    channel,
                    note,
                    velocity,
                },
                Some(TransportMessage::NoteOff {
                    channel,
                    note,
                    velocity: 64,
                }),
            ))
        }
        "pc" => Ok((
            TransportMessage::ProgramChange {
                channel: channel(0)?,
                program: num(1, "program", 127)?,
            },
            None,
        )),
        "cc" => Ok((
            TransportMessage::ControlChange {
                channel: channel(0)?,
                controller: num(1, "controller", 127)?,
                value: num(2, "value", 127)?,
            },
            None,
        )),
        "go" => {
            let cue_text = fields
                .first()
                .ok_or_else(|| anyhow!("`{item}`: missing cue number"))?;
            let cue: f32 = cue_text
                .parse()
                .map_err(|_| anyhow!("`{item}`: cue `{cue_text}` is not a number"))?;
            let tenths = (cue * 10.0).round();
            if !(0.0..=9999.0).contains(&tenths) {
                return Err(anyhow!("`{item}`: cue must be 0–999.9"));
            }
            let cue = MscCue::from_tenths(tenths as u16)
                .ok_or_else(|| anyhow!("`{item}`: cue not representable"))?;
            let list = match fields.get(1) {
                Some(_) => Some(num(1, "list", 15)?).filter(|&l| l > 0),
                None => None,
            };
            Ok((
                TransportMessage::Msc(MscMessage::cue(MscCommand::Go, cue, list)),
                None,
            ))
        }
        "heartbeat" | "hb" => Ok((TransportMessage::Heartbeat, None)),
        "panic" => Ok((TransportMessage::Panic, None)),
        other => Err(anyhow!(
            "unknown message `{other}` in --messages (note=, pc=, cc=, go=, heartbeat, panic)"
        )),
    }
}

/// The HF profile at the recording's sample rate, or an error when the
/// plan does not fit the rate (44.1 kHz).
fn hf_profile_for(profile: &str, sample_rate: u32) -> Result<PhyConfig> {
    let config = PhyConfig::from_name(profile).ok_or_else(|| anyhow!("unknown profile"))?;
    let config = if config.sample_rate_hz == sample_rate {
        config
    } else {
        config.with_sample_rate(sample_rate)
    };
    if let Some(w) = cricket_engine::hf_rate_warning(&config, sample_rate) {
        return Err(anyhow!(
            "{} (the recording is {sample_rate} Hz; record at 48 kHz)",
            w.trim_start_matches("WARNING: ")
        ));
    }
    Ok(config)
}

#[allow(clippy::too_many_arguments)]
fn run_hf_mix(
    wav_in: &str,
    wav_out: &str,
    messages: &[String],
    timing: HfMixTiming,
    profile: &str,
    tx_level_db: Option<f32>,
    link_key: u16,
) -> Result<()> {
    let (rate, mut samples) = read_wav_f32(wav_in).map_err(|e| anyhow!(e))?;
    let mut config = hf_profile_for(profile, rate)?.with_link_key(link_key);
    if let Some(db) = tx_level_db {
        config = config.with_tx_level_dbfs(db);
    }
    let items: Vec<String> = if messages.is_empty() {
        vec!["pc=1:10".into(), "note=1:60:100".into(), "go=14.5:2".into()]
    } else {
        messages.to_vec()
    };
    let parsed: Vec<_> = items
        .iter()
        .map(|m| parse_hf_message(m))
        .collect::<Result<_>>()?;
    if timing.every_ms <= 0.0 {
        return Err(anyhow!("--every-ms must be positive"));
    }
    let ms = |v: f32| (v / 1000.0 * rate as f32).round().max(0.0) as usize;
    let program_rms = {
        let e: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (e / samples.len().max(1) as f64).sqrt() as f32
    };
    let mut tx = CricketModemTx::new(config.clone());
    let mut placed: Vec<(usize, TransportMessage)> = Vec::new();
    let mut t = ms(timing.start_ms);
    let mut round = 0usize;
    'outer: loop {
        for (on, off) in &parsed {
            if t >= samples.len() {
                break 'outer;
            }
            placed.push((t, on.clone()));
            if let Some(off) = off {
                if timing.hold_ms > 0.0 {
                    placed.push((t + ms(timing.hold_ms), off.clone()));
                }
            }
            t += ms(timing.every_ms);
        }
        round += 1;
        if !timing.repeat || round > 100_000 {
            break;
        }
    }
    placed.sort_by_key(|(at, _)| *at);
    // Messages are sent in order on one line: a message that would overlap
    // the previous one starts when that one ends (as the scheduler would).
    let mut line_free = 0usize;
    let mut peak = 0f32;
    let mut written = 0usize;
    for (at, msg) in &placed {
        let start = (*at).max(line_free);
        let audio = tx.encode_message(msg);
        if start >= samples.len() {
            break;
        }
        for (i, v) in audio.iter().enumerate() {
            if let Some(s) = samples.get_mut(start + i) {
                *s += v;
                peak = peak.max(s.abs());
            }
        }
        line_free = start + audio.len();
        written += 1;
        let (kind, payload) = format_message(msg);
        println!("{:9.3} s  {} {}", start as f32 / rate as f32, kind, payload);
    }
    write_wav_f32(wav_out, rate, &samples).map_err(|e| anyhow!(e))?;
    println!(
        "Mixed {} messages ({} profile, tones {:.1}–{:.1} kHz at {:.1} dBFS peak) into {} s of {} \
         (program {:.1} dBFS RMS) → {}; peak after mixing {:.1} dBFS{}.",
        written,
        PhyConfig::canonical_name(profile).unwrap_or_else(|| profile.to_string()),
        config.tone_hz(0) / 1000.0,
        config.tone_hz(config.n_tones() as u8 - 1) / 1000.0,
        config.tx_level_dbfs(),
        samples.len() / rate as usize,
        wav_in,
        rms_to_dbfs(program_rms),
        wav_out,
        rms_to_dbfs(peak / 2f32.sqrt()) + 3.0,
        if peak >= 1.0 {
            " — CLIPPED: lower the recording or the tones"
        } else {
            ""
        }
    );
    println!("Next: play it through the rig, record the far end at 48 kHz, then `cricket hf-scan --wav-in <recording>`.");
    Ok(())
}

fn run_hf_scan(wav_in: &str, profile: &str, link_key: u16) -> Result<()> {
    let (rate, samples) = read_wav_f32(wav_in).map_err(|e| anyhow!(e))?;
    let config = hf_profile_for(profile, rate)?.with_link_key(link_key);
    println!(
        "Scanning {} ({} s at {} Hz) with the {} profile (tones {:.1}–{:.1} kHz)...",
        wav_in,
        samples.len() / rate.max(1) as usize,
        rate,
        PhyConfig::canonical_name(profile).unwrap_or_else(|| profile.to_string()),
        config.tone_hz(0) / 1000.0,
        config.tone_hz(config.n_tones() as u8 - 1) / 1000.0
    );
    // Level in the receiver's band and overall, for judging the rig.
    let (band_rms, full_rms, peak) = {
        let mut band = config.rx_band_hz().map(|(lo, hi)| {
            cricket_core::phy::BandPass::with_order(lo, hi, config.rx_band_order() as usize, rate)
        });
        let mut e_band = 0f64;
        let mut e_full = 0f64;
        let mut peak = 0f32;
        for &s in &samples {
            let b = band.as_mut().map_or(s, |f| f.process(s));
            e_band += (b as f64) * (b as f64);
            e_full += (s as f64) * (s as f64);
            peak = peak.max(s.abs());
        }
        let n = samples.len().max(1) as f64;
        ((e_band / n).sqrt() as f32, (e_full / n).sqrt() as f32, peak)
    };
    let mut rx = CricketModemRx::new(config.clone());
    let mut count = 0usize;
    let flush = cricket_core::modem::flush_samples(&config);
    let mut stream = samples;
    stream.extend(std::iter::repeat_n(0.0, flush));
    for chunk in stream.chunks(256) {
        for out in rx.push_samples(chunk) {
            if let ModemOutput::Message(m) = out {
                count += 1;
                let (kind, payload) = format_message(&m);
                println!(
                    "{:9.3} s  {} {}",
                    rx.samples_processed() as f32 / rate as f32,
                    kind,
                    payload
                );
            }
        }
    }
    let st = rx.stats();
    println!(
        "{} messages. Packets: {} passed, {} failed{}; {} tones repaired by the code; \
         last confidence {:.1} dB.",
        count,
        st.crc_pass,
        st.crc_fail,
        if st.wrong_key_suspects > 0 {
            format!(" ({} look like another link key)", st.wrong_key_suspects)
        } else {
            String::new()
        },
        st.fec_corrected,
        st.last_confidence_db
    );
    println!(
        "Recording: {:.1} dBFS RMS, peak {:.1} dBFS; in the tone band ({:.1}–{:.1} kHz): {:.1} dBFS RMS \
         (tones at −30 dBFS peak measure about −33 dBFS when the line is on).",
        rms_to_dbfs(full_rms),
        rms_to_dbfs(peak / 2f32.sqrt()) + 3.0,
        config.rx_band_hz().map_or(0.0, |b| b.0 / 1000.0),
        config.rx_band_hz().map_or(0.0, |b| b.1 / 1000.0),
        rms_to_dbfs(band_rms)
    );
    if count == 0 {
        println!("Nothing decoded: check that the recording is 48 kHz end to end (no 44.1 kHz device, codec or wireless link) and that the tones are above the band noise (hf-mix at −30 dBFS, or try −20).");
    }
    Ok(())
}

/// A profile name (user-facing or older), returned in its canonical form.
/// Unknown names list what is accepted.
fn parse_profile(s: &str) -> Result<String, String> {
    PhyConfig::canonical_name(s).ok_or_else(|| {
        let names = |tier: ProfileTier| -> Vec<String> {
            PROFILES
                .iter()
                .filter(|p| p.tier == tier)
                .map(|p| {
                    if p.aliases.is_empty() {
                        p.name.to_string()
                    } else {
                        format!("{} ({})", p.name, p.aliases.join(", "))
                    }
                })
                .collect()
        };
        format!(
            "unknown profile `{}`. Profiles: {}; advanced (no error correction): {}; \
             instrument audio (experimental): {}. \
             Add `+digital` to a decoder profile to also accept digital-bit-exact frames. \
             `cricket profiles` explains them.",
            s,
            names(ProfileTier::Standard).join(", "),
            names(ProfileTier::Advanced).join(", "),
            names(ProfileTier::Instrument).join(", ")
        )
    })
}

/// A transmit level in dBFS within the accepted range.
fn parse_tx_level(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|e| format!("{}: {}", s, e))?;
    if !(MIN_TX_LEVEL_DBFS..=0.0).contains(&v) {
        return Err(format!(
            "{} dBFS is outside {} … 0 (0 = full scale)",
            s, MIN_TX_LEVEL_DBFS
        ));
    }
    Ok(v)
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
    let list = |title: &str, what: &str, names: Result<Vec<String>>| {
        println!("--- {} ---", title);
        match names {
            Ok(names) if names.is_empty() => println!("(none)"),
            Ok(names) => {
                for (index, name) in names.iter().enumerate() {
                    println!("{}: {}", index, name);
                }
            }
            Err(e) => println!("{} unavailable: {}", what, e),
        }
    };
    list("Input Devices", "Audio", audio::list_input_devices());
    list("Output Devices", "Audio", audio::list_output_devices());
    // A machine without a MIDI backend (e.g. no ALSA sequencer) still has
    // audio: say so instead of aborting.
    list("MIDI Input Devices", "MIDI", midi::list_inputs());
    list("MIDI Output Devices", "MIDI", midi::list_outputs());
    Ok(())
}

/// Airtime of one full message (all copies) in ms.
fn airtime_ms(config: &PhyConfig) -> f32 {
    repetition_samples(config) as f32 * config.repetition_k.max(1) as f32 * 1000.0
        / config.sample_rate_hz as f32
}

fn print_profile_entry(p: &ProfileInfo) {
    let config = (p.build)();
    let default = if p.is_default() { " (default)" } else { "" };
    let also = if p.aliases.is_empty() {
        String::new()
    } else {
        format!("   also: {}", p.aliases.join(", "))
    };
    println!("  {}{}{}", p.name, default, also);
    println!("      for:        {}", p.line);
    println!("      processing: {}", p.processing);
    println!(
        "      latency:    {}; airtime per message {:.1} ms",
        p.latency,
        airtime_ms(&config)
    );
    println!("      use when:   {}", p.use_when);
}

fn print_profiles() {
    println!("Cricket profiles. Both ends must use the same one; names are case-insensitive and");
    println!("`-` = `_`. The older (wire-format) names are still accepted everywhere.");
    println!();
    println!("Which profile?");
    println!("  1. Analog line, or any bus you don't control (EQ, dynamics, unknown gain)?");
    println!("       -> analog-safe. Dropouts of 50 ms or more: analog-safe-repeat.");
    println!("  2. A dedicated line with nothing on it (no EQ, compression, limiter)?");
    println!("       analog  -> clean-line (notes only, no error correction: analog-fast)");
    println!(
        "       digital -> clean-line-fast, or digital-line (Dante / MADI / AES at 48 / 96 kHz)"
    );
    println!("  3. `cricket pathcheck` says the path is bit-exact?");
    println!("       -> digital-bit-exact (< 1 ms; experimental)");
    println!();
    println!("Standard profiles:");
    for p in PROFILES.iter().filter(|p| p.tier == ProfileTier::Standard) {
        print_profile_entry(p);
    }
    println!();
    println!("Advanced: no error correction (and ltc2400_fec), for operators who know their line:");
    for p in PROFILES.iter().filter(|p| p.tier == ProfileTier::Advanced) {
        print_profile_entry(p);
    }
    println!();
    println!(
        "Instrument audio (experimental): MIDI embedded on an instrument's own signal, above the"
    );
    println!("music (19.0–21.5 kHz at 48 kHz), extracted by a box that passes the audio through.");
    println!("Try it on a recording with `cricket hf-mix` and `cricket hf-scan`:");
    for p in PROFILES
        .iter()
        .filter(|p| p.tier == ProfileTier::Instrument)
    {
        print_profile_entry(p);
    }
    println!();
    println!("A decoder profile with `+digital` (e.g. clean-line+digital) also accepts");
    println!("digital-bit-exact frames. Latencies are MIDI in -> MIDI out measured in-process");
    println!("(`cricket loopback`); real audio interfaces add their own buffering on top.");
    println!(
        "Transmit level: {:.0} dBFS peak by default (--tx-level-db; the instrument profiles {:.0} dBFS); \
         the receiver is level-independent.",
        DEFAULT_TX_LEVEL_DBFS, HF_TX_LEVEL_DBFS
    );
}

fn run_test_mode(
    wav_out: Option<&str>,
    wav_in: Option<&str>,
    profile: &str,
    tx_level_db: Option<f32>,
) -> Result<()> {
    let mut config =
        PhyConfig::from_name(profile).ok_or_else(|| anyhow!("unknown profile {:?}", profile))?;
    if let Some(db) = tx_level_db {
        config = config.with_tx_level_dbfs(db);
    }
    println!(
        "Running Cricket loopback test (profile={}, TX level {:.1} dBFS peak)...",
        profile,
        config.tx_level_dbfs()
    );
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

#[allow(clippy::too_many_arguments)]
fn run_loopback_metrics(
    profiles: &[String],
    notes: usize,
    chords: usize,
    chord_size: usize,
    throughput: usize,
    pacing: Pacing,
    pattern: NotePattern,
) -> Result<()> {
    println!(
        "Cricket engine loopback latency ({} MIDI in → MIDI out, {} notes)...",
        match pacing {
            Pacing::RealTime => "paced",
            Pacing::Instant => "zero-airtime",
        },
        match pattern {
            NotePattern::Detached => "detached",
            NotePattern::Legato => "legato",
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
        let s = measure_midi_loopback_pattern(profile, notes, pacing, pattern)?;
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

/// Exit status of a path check: 0 when the digital profile works, 3 when
/// it works with losses (`Marginal`), 1 when it would not work.
fn pathcheck_exit(report: &PathReport) -> ExitCode {
    match report.verdict {
        Verdict::BitExact | Verdict::DigitalOk => ExitCode::SUCCESS,
        Verdict::Marginal => ExitCode::from(EXIT_MARGINAL),
        Verdict::NoSignal | Verdict::Unusable => ExitCode::from(1),
    }
}

fn run_pathcheck(action: &PathcheckAction, options: &AudioOptions) -> Result<ExitCode> {
    use cricket_engine::pathcheck as engine_pc;
    let in_channel = options.input_channel + 1;
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
                return Ok(ExitCode::SUCCESS);
            }
            let stop = Arc::new(AtomicBool::new(false));
            install_ctrlc(&stop);
            let out_channel = options.output_channel + 1;
            engine_pc::play_pattern(
                AudioOut::Device(selector(*audio_out, audio_out_name)),
                options,
                seconds.map(Duration::from_secs_f64),
                &stop,
                |info| {
                    println!(
                        "Cricket path check: playing the test pattern ({}; peak -48 dBFS, \
                         on channel {}). Ctrl-C to stop.",
                        stream_line("output", info),
                        out_channel
                    );
                },
            )?;
            Ok(ExitCode::SUCCESS)
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
                println!("  {} (channel {})", stream_line("input", &info), in_channel);
                pathcheck::analyze(&samples, info.0)
            };
            print_report(&report);
            Ok(pathcheck_exit(&report))
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
            println!(
                "  {} (channel {})",
                stream_line("output", &out_info),
                options.output_channel + 1
            );
            println!(
                "  {} (channel {})",
                stream_line("input", &in_info),
                in_channel
            );
            let report = pathcheck::analyze(&samples, in_info.0);
            print_report(&report);
            Ok(pathcheck_exit(&report))
        }
        PathcheckAction::Sim { path } => {
            run_pathcheck_sim(path)?;
            Ok(ExitCode::SUCCESS)
        }
    }
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
        let report = pathcheck::analyze(&cricket_sim::simulate(path, &pattern), 48_000);
        print_report(&report);
        return Ok(());
    }
    println!("Cricket path check, simulated paths (3 s of pattern each):");
    println!(
        "{:<24} {:<10} {:>10} {:>12} {:>8}",
        "path", "verdict", "bit-exact", "symbols ok", "probes"
    );
    for (n, path) in &presets {
        let r = pathcheck::analyze(&cricket_sim::simulate(path, &pattern), 48_000);
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

// --- live monitoring (encode / decode) -------------------------------------

/// Set `flag` on Ctrl-C (once per process; a second handler is ignored).
fn install_ctrlc(flag: &Arc<AtomicBool>) {
    let flag = flag.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
}

/// Lines typed on stdin, for the `panic` command. The thread ends when
/// stdin closes (it is not a terminal in a service).
fn stdin_lines() -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("cricket-stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .ok();
    rx
}

/// Peak-hold time of the level readout in the status line.
const LEVEL_HOLD: Duration = Duration::from_millis(1500);
/// Interval of the status line.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// Level events in `--json` mode are thinned to this interval.
const JSON_LEVEL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Decoder,
    Encoder,
    /// Audio in → out with a tap decoder: reports like a decoder.
    Passthrough,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Decoder => "decoder",
            Role::Encoder => "encoder",
            Role::Passthrough => "passthrough",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Role::Decoder => "Decoder",
            Role::Encoder => "Encoder",
            Role::Passthrough => "Pass-through",
        }
    }
}

/// The running worker, whichever kind.
enum Handle {
    Decoder(DecoderHandle),
    Encoder(EncoderHandle),
    Passthrough(PassthroughHandle),
}

impl Handle {
    fn stop(&mut self) -> bool {
        match self {
            Handle::Decoder(d) => d.stop(),
            Handle::Encoder(e) => e.stop(),
            Handle::Passthrough(p) => p.stop(),
        }
    }

    /// Fail-safe burst: the decoder sends it to MIDI out, the encoder sends
    /// a Panic message over the link. Returns false when the worker has no
    /// such command (the pass-through sends its burst on stop only).
    fn panic(&self) -> bool {
        match self {
            Handle::Decoder(d) => d.panic(),
            Handle::Encoder(e) => e.send(TransportMessage::Panic),
            Handle::Passthrough(_) => return false,
        }
        true
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Prints events as a human log with a periodic status line, or as NDJSON.
///
/// Output goes through [`Monitor::print`]: when stdout is closed (the
/// consumer of `--json` went away, `| head`), the monitor asks for an
/// orderly stop instead of panicking, so the decoder still sends its
/// fail-safe burst.
struct Monitor {
    role: Role,
    json: bool,
    /// Set when a write to stdout failed; the monitor loop stops the worker.
    stdout_closed: Arc<AtomicBool>,
    level_db: f32,
    level_at: Instant,
    stats: Option<LinkStats>,
    last_msg: Option<String>,
    last_packet: Option<Instant>,
    tx: Option<(usize, f32, u64)>,
    last_status: Instant,
    last_json_level: Instant,
}

impl Monitor {
    fn new(role: Role, json: bool) -> Self {
        let now = Instant::now();
        Self {
            role,
            json,
            stdout_closed: Arc::new(AtomicBool::new(false)),
            level_db: -120.0,
            level_at: now,
            stats: None,
            last_msg: None,
            last_packet: None,
            tx: None,
            last_status: now,
            last_json_level: now - JSON_LEVEL_INTERVAL,
        }
    }

    /// One line to stdout; a failed write (closed pipe) requests a stop.
    fn print(&self, line: &str) {
        if self.stdout_closed.load(Ordering::Relaxed) {
            return;
        }
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{}", line).and_then(|_| out.flush()).is_err() {
            self.stdout_closed.store(true, Ordering::Relaxed);
        }
    }

    fn emit_json(&self, mut value: serde_json::Value) {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("ts_ms".into(), serde_json::json!(now_ms()));
            obj.insert("role".into(), serde_json::json!(self.role.name()));
        }
        self.print(&value.to_string());
    }

    /// A line the operator should see (setup warnings).
    fn warning(&self, message: &str) {
        if self.json {
            self.emit_json(serde_json::json!({ "event": "warning", "message": message }));
        } else {
            self.print(&format!("WARNING: {}", message));
        }
    }

    fn note(&self, message: &str) {
        if self.json {
            self.emit_json(serde_json::json!({ "event": "log", "message": message }));
        } else {
            self.print(message);
        }
    }

    fn handle(&mut self, event: &EngineEvent) {
        match event {
            EngineEvent::Level(rms) => {
                let db = rms_to_dbfs(*rms);
                if db >= self.level_db || self.level_at.elapsed() > LEVEL_HOLD {
                    self.level_db = db;
                    self.level_at = Instant::now();
                }
            }
            EngineEvent::Decoded(msg) => {
                self.last_packet = Some(Instant::now());
                if !matches!(msg, TransportMessage::Heartbeat) {
                    let (kind, payload) = format_message(msg);
                    self.last_msg = Some(format!("{} {}", kind, payload).trim_end().to_string());
                }
            }
            EngineEvent::Stats(s) => self.stats = Some(s.clone()),
            EngineEvent::TxStatus {
                queued,
                latency_ms,
                dropped,
            } => self.tx = Some((*queued, *latency_ms, *dropped)),
            EngineEvent::Log(line) if line.starts_with("TX ") => {
                self.last_msg = Some(line.trim_start_matches("TX ").to_string());
            }
            _ => {}
        }
        if self.json {
            self.print_json(event);
        } else {
            self.print_text(event);
        }
    }

    fn print_json(&mut self, event: &EngineEvent) {
        use serde_json::json;
        let value = match event {
            EngineEvent::Log(message) => json!({ "event": "log", "message": message }),
            EngineEvent::Level(rms) => {
                if self.last_json_level.elapsed() < JSON_LEVEL_INTERVAL {
                    return;
                }
                self.last_json_level = Instant::now();
                json!({ "event": "level", "rms": rms, "dbfs": rms_to_dbfs(*rms) })
            }
            EngineEvent::Decoded(msg) => {
                let (kind, payload) = format_message(msg);
                json!({
                    "event": "decoded",
                    "type": kind,
                    "payload": payload,
                    "heartbeat": matches!(msg, TransportMessage::Heartbeat),
                })
            }
            EngineEvent::Stats(s) => json!({
                "event": "stats",
                "state": cricket_engine::link_state_name(s.state),
                "light": LinkLight::from_stats(Some(s), self.level_db).label(),
                "link_alive": s.link_alive,
                "crc_pass": s.crc_pass,
                "crc_fail": s.crc_fail,
                "wrong_key_suspects": s.wrong_key_suspects,
                "crc_pass_rate": s.crc_pass_rate,
                "fec_corrected": s.fec_corrected,
                "sync_lost": s.sync_lost,
                "locks": s.locks(),
                "confidence_db": s.confidence_db,
                "last_seq": s.last_seq,
                "buffer_drops": s.buffer_drops,
                "airtime_ms": s.est_latency_ms,
                "level_dbfs": self.level_db,
            }),
            EngineEvent::Alarm { kind, message } => json!({
                "event": "alarm",
                "kind": kind.as_str(),
                "title": kind.title(),
                "message": message,
            }),
            EngineEvent::AlarmCleared { kind, message } => json!({
                "event": "alarm_cleared",
                "kind": kind.as_str(),
                "message": message,
            }),
            EngineEvent::TxStatus {
                queued,
                latency_ms,
                dropped,
            } => {
                json!({ "event": "tx_status", "queued": queued, "latency_ms": latency_ms, "dropped": dropped })
            }
            EngineEvent::Stopped => json!({ "event": "stopped" }),
            // An event this CLI predates.
            _ => return,
        };
        self.emit_json(value);
    }

    fn print_text(&self, event: &EngineEvent) {
        match event {
            EngineEvent::Log(line) => self.print(line),
            EngineEvent::Decoded(msg) => {
                if !matches!(msg, TransportMessage::Heartbeat) {
                    let (kind, payload) = format_message(msg);
                    self.print(&format!("[MSG] {} {}", kind, payload));
                }
            }
            EngineEvent::Alarm { kind, message } => {
                self.print(&format!("[ALARM] {}: {}", kind.title(), message))
            }
            EngineEvent::AlarmCleared { message, .. } => {
                self.print(&format!("[ALARM CLEARED] {}", message))
            }
            EngineEvent::Stopped => self.print("[STOPPED]"),
            EngineEvent::Level(_) | EngineEvent::Stats(_) | EngineEvent::TxStatus { .. } => {}
            // An event this CLI predates.
            _ => {}
        }
    }

    /// The periodic status line (text mode only).
    fn status_line(&mut self) {
        if self.json || self.last_status.elapsed() < STATUS_INTERVAL {
            return;
        }
        self.last_status = Instant::now();
        let last = self.last_msg.as_deref().unwrap_or("—");
        match self.role {
            Role::Decoder | Role::Passthrough => {
                let light = LinkLight::from_stats(self.stats.as_ref(), self.level_db);
                let ago = match self.last_packet {
                    Some(t) => format!("{:.1} s ago", t.elapsed().as_secs_f32()),
                    None => "never".to_string(),
                };
                let (crc, fec, conf) = match &self.stats {
                    Some(s) => (
                        if s.wrong_key_suspects > 0 {
                            format!(
                                "{} ok / {} fail ({} other key?)",
                                s.crc_pass, s.crc_fail, s.wrong_key_suspects
                            )
                        } else {
                            format!("{} ok / {} fail", s.crc_pass, s.crc_fail)
                        },
                        s.fec_corrected.to_string(),
                        format!("{:.1} dB", s.confidence_db),
                    ),
                    None => ("—".into(), "—".into(), "—".into()),
                };
                self.print(&format!(
                    "[status] {} · in {:.1} dBFS · conf {} · CRC {} · FEC fixed {} · last packet {} · last: {}",
                    light.label(),
                    self.level_db.max(-99.9),
                    conf,
                    crc,
                    fec,
                    ago,
                    last
                ));
                if let Some(hint) = light.hint() {
                    if light != LinkLight::Dead || self.last_packet.is_some() {
                        self.print(&format!("[hint] {}", hint));
                    }
                }
            }
            Role::Encoder => {
                let (queued, late, dropped) = self.tx.unwrap_or((0, 0.0, 0));
                self.print(&format!(
                    "[status] TX queue {} · oldest waiting {:.0} ms · dropped {} · last: {}",
                    queued, late, dropped, last
                ));
            }
        }
    }
}

/// Run `handle` until it stops on its own (an error: exit 1), Ctrl-C (the
/// worker is stopped in order, exit 0) or stdin closes with a `quit`.
fn monitor(
    mut handle: Handle,
    events: Receiver<EngineEvent>,
    mut mon: Monitor,
) -> Result<ExitCode> {
    let interrupted = Arc::new(AtomicBool::new(false));
    install_ctrlc(&interrupted);
    let commands = stdin_lines();
    let what = mon.role.title();
    loop {
        if mon.stdout_closed.load(Ordering::Relaxed) {
            // Whoever read our output is gone: stop in order (burst first).
            interrupted.store(true, Ordering::SeqCst);
        }
        if interrupted.swap(false, Ordering::SeqCst) {
            mon.note(match handle {
                Handle::Decoder(_) | Handle::Passthrough(_) => {
                    "Stopping — Sustain off + All Notes Off go to MIDI out first..."
                }
                Handle::Encoder(_) => "Stopping encoder...",
            });
            let stopped = handle.stop();
            // The worker's last events (the stop burst log, Stopped).
            for ev in events.try_iter() {
                mon.handle(&ev);
            }
            if !stopped {
                return Err(anyhow!("{} did not stop in time", what));
            }
            return Ok(ExitCode::SUCCESS);
        }
        while let Ok(line) = commands.try_recv() {
            match line.trim().to_ascii_lowercase().as_str() {
                "p" | "panic" => {
                    if handle.panic() {
                        mon.note("Panic requested.");
                    } else {
                        mon.note("No panic command in pass-through mode; `quit` sends the fail-safe burst and stops.");
                    }
                }
                "q" | "quit" | "exit" => interrupted.store(true, Ordering::SeqCst),
                "" => {}
                other => mon.note(&format!(
                    "Unknown command {:?}: type `panic` (All Notes Off) or `quit`.",
                    other
                )),
            }
        }
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(EngineEvent::Stopped) => {
                mon.handle(&EngineEvent::Stopped);
                // Only a device error makes a worker stop by itself.
                return Err(anyhow!("{} stopped", what));
            }
            Ok(ev) => mon.handle(&ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(anyhow!("{} vanished", what)),
        }
        mon.status_line();
    }
}

/// Link-level decoder options (spec §4.1 link key, §5.3 fail-safe).
struct DecoderLink {
    link_key: u16,
    link_dead_ms: Option<u32>,
    fail_safe_burst: bool,
}

/// Link-level encoder options (spec §4.1 link key, §5.4 MSC filter).
struct EncoderLink {
    link_key: u16,
    msc_filter: MscFilter,
}

#[allow(clippy::too_many_arguments)]
fn run_decoder(
    audio_in: DeviceSelector,
    midi_out: DeviceSelector,
    msc: MscTarget,
    profile: &str,
    audio: &AudioOptions,
    json: bool,
    link: DecoderLink,
) -> Result<ExitCode> {
    let mon = Monitor::new(Role::Decoder, json);
    mon.note("Initializing Cricket decoder...");
    let midi_out = match midi_out {
        DeviceSelector::Default => MidiOut::None,
        sel => {
            // Fail fast on a bad port, as before the engine.
            mon.note(&format!("MIDI Out: {}", midi::output_port_name(&sel)?));
            MidiOut::Device(sel)
        }
    };
    let config = PhyConfig::from_name(profile).ok_or_else(|| anyhow!("unknown profile"))?;
    let in_name = audio::resolve_device_name(&audio_in, true).ok();
    for w in audio::setup_warnings(&config, in_name.as_deref(), None) {
        mon.warning(&w);
    }
    if !json {
        if link.fail_safe_burst {
            mon.note("Type `panic` + Enter for Sustain off + All Notes Off on all channels; Ctrl-C stops (burst sent first).");
        } else {
            mon.note("Fail-safe burst off (--no-failsafe-burst): link-dead, `panic` and Ctrl-C send nothing to MIDI out.");
        }
    }
    let mut dec = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Device(audio_in),
        midi_out,
        profile: profile.into(),
        link_dead_ms: link.link_dead_ms,
        msc,
        audio: audio.clone(),
        link_key: link.link_key,
        fail_safe_burst: link.fail_safe_burst,
    })?;
    let events = dec.take_events();
    monitor(Handle::Decoder(dec), events, mon)
}

#[allow(clippy::too_many_arguments)]
fn run_passthrough(
    audio_in: DeviceSelector,
    audio_out: DeviceSelector,
    midi_out: DeviceSelector,
    msc: MscTarget,
    profile: &str,
    audio: &AudioOptions,
    json: bool,
    link: DecoderLink,
    strip: bool,
) -> Result<ExitCode> {
    let mon = Monitor::new(Role::Passthrough, json);
    mon.note("Initializing Cricket pass-through...");
    let midi_out = match midi_out {
        DeviceSelector::Default => MidiOut::None,
        sel => {
            mon.note(&format!("MIDI Out: {}", midi::output_port_name(&sel)?));
            MidiOut::Device(sel)
        }
    };
    let config = PhyConfig::from_name(profile).ok_or_else(|| anyhow!("unknown profile"))?;
    let in_name = audio::resolve_device_name(&audio_in, true).ok();
    let out_name = audio::resolve_device_name(&audio_out, false).ok();
    for w in audio::setup_warnings(&config, in_name.as_deref(), out_name.as_deref()) {
        mon.warning(&w);
    }
    if strip && config.rx_band_hz().is_none() {
        mon.warning(
            "--strip has nothing to remove on this profile (it uses the whole audio band); the output is unchanged.",
        );
    }
    if !json {
        mon.note("Audio passes through unchanged while the copy is decoded; Ctrl-C stops (fail-safe burst sent first).");
    }
    let mut tap = TapConfig::new(profile);
    tap.msc = msc;
    tap.link_dead_ms = link.link_dead_ms;
    tap.link_key = link.link_key;
    tap.fail_safe_burst = link.fail_safe_burst;
    tap.strip = strip;
    let processor = TapDecoder::new(tap)?;
    let mut cfg = PassthroughConfig::new(AudioIn::Device(audio_in), AudioOut::Device(audio_out));
    cfg.sample_rate_hz = config.sample_rate_hz;
    cfg.audio = audio.clone();
    cfg.midi_out = midi_out;
    let mut handle = PassthroughHandle::start(cfg, Box::new(processor))?;
    let events = handle.take_events();
    monitor(Handle::Passthrough(handle), events, mon)
}

#[allow(clippy::too_many_arguments)]
fn run_encoder(
    midi_in: DeviceSelector,
    audio_out: DeviceSelector,
    simulate: bool,
    profile: &str,
    tx_level_db: Option<f32>,
    audio: &AudioOptions,
    json: bool,
    link: EncoderLink,
) -> Result<ExitCode> {
    let mon = Monitor::new(Role::Encoder, json);
    mon.note("Initializing Cricket encoder...");
    let midi_in = match midi_in {
        DeviceSelector::Default => MidiIn::None,
        sel => {
            // Fail fast on a bad port, as before the engine.
            mon.note(&format!("MIDI In: {}", midi::input_port_name(&sel)?));
            MidiIn::Device(sel)
        }
    };
    if simulate {
        mon.note("Test scale ON: C-major scale, one note per second, plus heartbeats.");
    } else if matches!(midi_in, MidiIn::None) {
        mon.note("No MIDI in: heartbeats only (add --simulate for a test scale).");
    }
    let config = PhyConfig::from_name(profile).ok_or_else(|| anyhow!("unknown profile"))?;
    let out_name = audio::resolve_device_name(&audio_out, false).ok();
    for w in audio::setup_warnings(&config, None, out_name.as_deref()) {
        mon.warning(&w);
    }
    let mut enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Device(audio_out),
        midi_in,
        profile: profile.into(),
        simulate,
        tx_level_dbfs: tx_level_db,
        audio: audio.clone(),
        link_key: link.link_key,
        msc_filter: link.msc_filter,
    })?;
    let events = enc.take_events();
    monitor(Handle::Encoder(enc), events, mon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cricket_core::phy::profile_info;

    #[test]
    fn profile_parser_accepts_tiers_and_old_names_and_rejects_typos() {
        assert_eq!(parse_profile("robust").unwrap(), "analog-safe");
        assert_eq!(parse_profile("Analog_Safe").unwrap(), "analog-safe");
        assert_eq!(
            parse_profile("ltc9600_fec+digital").unwrap(),
            "clean-line-fast+digital"
        );
        let err = parse_profile("robsut").unwrap_err();
        assert!(err.starts_with("unknown profile `robsut`"), "{err}");
        assert!(err.contains("analog-safe (robust, robust_fec)"), "{err}");
        assert!(
            err.contains("advanced (no error correction): ltc (ltc4800)"),
            "{err}"
        );
    }

    #[test]
    fn tx_level_range() {
        assert_eq!(parse_tx_level("-12").unwrap(), -12.0);
        assert_eq!(parse_tx_level("0").unwrap(), 0.0);
        assert!(parse_tx_level("3").is_err());
        assert!(parse_tx_level("-61").is_err());
        assert!(parse_tx_level("loud").is_err());
    }

    #[test]
    fn pathcheck_exit_codes() {
        let mut report = pathcheck::analyze(&[0.0; 48_000], 48_000);
        assert_eq!(report.verdict, Verdict::NoSignal);
        assert_eq!(pathcheck_exit(&report), ExitCode::from(1));
        report.verdict = Verdict::Marginal;
        assert_eq!(pathcheck_exit(&report), ExitCode::from(EXIT_MARGINAL));
        report.verdict = Verdict::BitExact;
        assert_eq!(pathcheck_exit(&report), ExitCode::SUCCESS);
    }

    #[test]
    fn every_catalog_profile_is_listed_with_its_airtime() {
        for p in PROFILES {
            let c = (p.build)();
            assert!(airtime_ms(&c) > 0.0, "{}", p.name);
            assert_eq!(profile_info(p.name).map(|i| i.name), Some(p.name));
        }
    }
}
