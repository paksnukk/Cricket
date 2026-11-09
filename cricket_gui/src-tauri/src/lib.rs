//! Tauri shell over `cricket_engine`: commands start/stop engine handles kept in
//! app state, and one forwarder thread per handle turns `EngineEvent`s into
//! the frontend's Tauri events (`log`, `input_level`, `decoded_frame`,
//! `link_stats`, `alarm`, `alarm_cleared`, `tx_status`, `worker_stopped`).
//! The profile catalog (`get_profiles`), device channel counts and the
//! setup warnings (built-in mic / speaker, WASAPI) come from the engine and
//! core too, so the GUI shows the same names and advice as the CLI.
//!
//! Each start command returns a run id that is also carried by that run's
//! `worker_stopped` event, so the frontend can tell "the worker I just
//! started died" (missing device, open error, device error, panic) from a
//! late `worker_stopped` of an earlier run. Ordering alone is not enough:
//! a stop that times out (`STOP_TIMEOUT`) detaches the old worker and its
//! forwarder, whose `worker_stopped` may then arrive at any later time, and
//! Tauri does not promise that an event emitted during a command reaches the
//! webview before that command's response.
#![forbid(unsafe_code)]

pub mod settings;

use cricket_core::modem::repetition_samples;
use cricket_core::phy::{PhyConfig, ProfileTier, PROFILES};
use cricket_engine::{
    audio, format_message, link_state_name, midi, AudioIn, AudioOptions, AudioOut, DecoderConfig,
    DecoderHandle, DeviceSelector, EncoderConfig, EncoderHandle, EngineEvent, MidiIn, MidiOut,
    MscTarget, TransportMessage, MIN_TX_LEVEL_DBFS,
};
use settings::{load_settings, save_settings, AppSettings};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use tauri::{AppHandle, Emitter, State};

/// A running engine handle plus the thread forwarding its events.
struct Running<H> {
    handle: H,
    forwarder: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct AppState {
    decoder: Mutex<Option<Running<DecoderHandle>>>,
    encoder: Mutex<Option<Running<EncoderHandle>>>,
    /// Last run id handed out (shared by both roles; ids start at 1).
    last_run_id: AtomicU64,
}

impl AppState {
    fn next_run_id(&self) -> u64 {
        self.last_run_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Which worker a forwarder belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Decoder,
    Encoder,
}

/// The run a forwarder serves: its role and the id its start command returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    role: Role,
    id: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct DecodedFrameEvent {
    frame_type: String,
    payload: String,
    is_heartbeat: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct LogEvent {
    message: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct LinkStatsEvent {
    state: String,
    crc_pass: u64,
    crc_fail: u64,
    fec_corrected: u64,
    confidence_db: f32,
    last_seq: Option<u8>,
    sync_lost: u64,
    crc_pass_rate: f32,
    est_latency_ms: f32,
    link_alive: bool,
    buffer_drops: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct AlarmEvent {
    kind: String,
    /// Plain-language title of the kind ("Link dead", "Message not sent").
    title: String,
    message: String,
}

/// Payload of `worker_stopped`: `{ "role": "decoder" | "encoder", "run_id": n }`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct WorkerStoppedEvent {
    role: Role,
    run_id: u64,
}

/// A Tauri event for the frontend: name plus typed payload.
#[derive(Debug, Clone, PartialEq)]
enum FrontendEvent {
    Log(LogEvent),
    InputLevel(f32),
    DecodedFrame(DecodedFrameEvent),
    LinkStats(LinkStatsEvent),
    Alarm(AlarmEvent),
    AlarmCleared(AlarmEvent),
    WorkerStopped(WorkerStoppedEvent),
    TxStatus(TxStatusEvent),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct TxStatusEvent {
    queued: usize,
    latency_ms: f32,
}

impl FrontendEvent {
    fn emit(self, app: &AppHandle) -> tauri::Result<()> {
        match self {
            FrontendEvent::Log(p) => app.emit("log", p),
            FrontendEvent::InputLevel(p) => app.emit("input_level", p),
            FrontendEvent::DecodedFrame(p) => app.emit("decoded_frame", p),
            FrontendEvent::LinkStats(p) => app.emit("link_stats", p),
            FrontendEvent::Alarm(p) => app.emit("alarm", p),
            FrontendEvent::AlarmCleared(p) => app.emit("alarm_cleared", p),
            FrontendEvent::WorkerStopped(p) => app.emit("worker_stopped", p),
            FrontendEvent::TxStatus(p) => app.emit("tx_status", p),
        }
    }
}

/// The profile, or the error the frontend shows (never another profile).
fn parse_profile(profile: &str) -> Result<PhyConfig, String> {
    PhyConfig::from_name(profile)
        .ok_or_else(|| format!("unknown profile {:?}: choose one from the list", profile))
}

/// One entry of the GUI's profile selector (from the core catalog).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct ProfileEntry {
    /// Canonical name (the value stored in settings and sent to the engine).
    name: String,
    display: String,
    /// `"standard"` or `"advanced"` (no error correction).
    tier: String,
    is_default: bool,
    coded: bool,
    aliases: Vec<String>,
    line: String,
    processing: String,
    latency: String,
    use_when: String,
    /// Airtime of one full message (all copies), ms.
    airtime_ms: f32,
    /// Confidence (dB) below which the receiver does not accept a packet.
    min_confidence_db: f32,
}

fn profile_entries() -> Vec<ProfileEntry> {
    PROFILES
        .iter()
        .map(|p| {
            let c = (p.build)();
            ProfileEntry {
                name: p.name.into(),
                display: p.display.into(),
                tier: match p.tier {
                    ProfileTier::Standard => "standard".into(),
                    ProfileTier::Advanced => "advanced".into(),
                },
                is_default: p.is_default(),
                coded: p.coded,
                aliases: p.aliases.iter().map(|a| a.to_string()).collect(),
                line: p.line.into(),
                processing: p.processing.into(),
                latency: p.latency.into(),
                use_when: p.use_when.into(),
                airtime_ms: repetition_samples(&c) as f32 * c.repetition_k.max(1) as f32 * 1000.0
                    / c.sample_rate_hz as f32,
                min_confidence_db: c.min_confidence_db,
            }
        })
        .collect()
}

/// Map one engine event of `run` to the frontend's Tauri event, if it has
/// one (same names and payloads as before the engine existed, plus
/// `worker_stopped`). Pure, so the mapping is testable without a Tauri app.
fn to_frontend(event: EngineEvent, run: Run) -> Option<FrontendEvent> {
    Some(match event {
        EngineEvent::Log(message) => FrontendEvent::Log(LogEvent { message }),
        EngineEvent::Level(rms) => FrontendEvent::InputLevel(rms),
        EngineEvent::Decoded(msg) => {
            let (frame_type, payload) = format_message(&msg);
            FrontendEvent::DecodedFrame(DecodedFrameEvent {
                frame_type,
                payload,
                is_heartbeat: matches!(msg, TransportMessage::Heartbeat),
            })
        }
        EngineEvent::Stats(s) => FrontendEvent::LinkStats(LinkStatsEvent {
            state: link_state_name(s.state).into(),
            crc_pass: s.crc_pass,
            crc_fail: s.crc_fail,
            fec_corrected: s.fec_corrected,
            confidence_db: s.confidence_db,
            last_seq: s.last_seq,
            sync_lost: s.sync_lost,
            crc_pass_rate: s.crc_pass_rate,
            est_latency_ms: s.est_latency_ms,
            link_alive: s.link_alive,
            buffer_drops: s.buffer_drops,
        }),
        EngineEvent::Alarm { kind, message } => FrontendEvent::Alarm(AlarmEvent {
            kind: kind.as_str().into(),
            title: kind.title().into(),
            message,
        }),
        EngineEvent::AlarmCleared { kind, message } => FrontendEvent::AlarmCleared(AlarmEvent {
            kind: kind.as_str().into(),
            title: kind.title().into(),
            message,
        }),
        EngineEvent::Stopped => FrontendEvent::WorkerStopped(WorkerStoppedEvent {
            role: run.role,
            run_id: run.id,
        }),
        EngineEvent::TxStatus { queued, latency_ms } => {
            FrontendEvent::TxStatus(TxStatusEvent { queued, latency_ms })
        }
    })
}

/// Forward events until the worker drops its sender.
fn spawn_forwarder(
    app: AppHandle,
    events: Receiver<EngineEvent>,
    run: Run,
) -> Option<JoinHandle<()>> {
    thread::Builder::new()
        .name("cricket-gui-events".into())
        .spawn(move || {
            for event in events {
                if let Some(ev) = to_frontend(event, run) {
                    let _ = ev.emit(&app);
                }
            }
        })
        .ok()
}

/// Stop a running handle; once the worker has exited, wait for its forwarder
/// so its last events reach the frontend before anything new starts.
fn shutdown<H>(running: Option<Running<H>>, stop: impl FnOnce(&mut H) -> bool) {
    if let Some(mut r) = running {
        if stop(&mut r.handle) {
            if let Some(f) = r.forwarder.take() {
                let _ = f.join();
            }
        }
    }
}

#[tauri::command]
fn get_input_devices() -> Result<Vec<String>, String> {
    audio::list_input_devices().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_output_devices() -> Result<Vec<String>, String> {
    audio::list_output_devices().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_midi_inputs() -> Result<Vec<String>, String> {
    midi::list_inputs().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_midi_outputs() -> Result<Vec<String>, String> {
    midi::list_outputs().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings() -> AppSettings {
    load_settings()
}

#[tauri::command]
fn set_settings(settings: AppSettings) -> Result<(), String> {
    save_settings(&settings)
}

/// The profile selector's entries: standard tiers first, then the
/// advanced (uncoded) group.
#[tauri::command]
fn get_profiles() -> Vec<ProfileEntry> {
    profile_entries()
}

/// Details of one profile by any accepted name; an unknown name is an
/// error (the frontend then falls back to the default and says so).
#[tauri::command]
fn get_profile_info(profile: String) -> Result<serde_json::Value, String> {
    let cfg = parse_profile(&profile)?;
    let canonical = PhyConfig::canonical_name(&profile).unwrap_or_else(|| profile.clone());
    Ok(serde_json::json!({
        "profile": profile,
        "canonical": canonical,
        "est_latency_ms": cfg.estimated_first_copy_ms(),
        "airtime_ms": repetition_samples(&cfg) as f32 * cfg.repetition_k.max(1) as f32 * 1000.0
            / cfg.sample_rate_hz as f32,
        "symbol_duration_ms": cfg.symbol_duration_ms,
        "guard_gap_ms": cfg.guard_gap_ms,
        "repetition_k": cfg.repetition_k,
        "sample_rate_hz": cfg.sample_rate_hz,
        "min_confidence_db": cfg.min_confidence_db,
        "tx_level_dbfs": cfg.tx_level_dbfs(),
    }))
}

/// Channel count of a device (`None` = the default device) in its default
/// configuration, for the channel selectors.
#[tauri::command]
fn get_device_channels(name: Option<String>, input: bool) -> Result<usize, String> {
    audio::channel_count(&DeviceSelector::from_name(name), input).map_err(|e| e.to_string())
}

/// Warnings for the chosen setup (built-in microphone / speaker, Windows
/// shared-mode audio with the digital profiles). Devices are names as
/// listed (`None` = default device); a device that cannot be resolved
/// produces no warning here (starting it reports the error).
#[tauri::command]
fn check_setup(
    profile: String,
    audio_in: Option<String>,
    audio_out: Option<String>,
    check_in: bool,
    check_out: bool,
) -> Result<Vec<String>, String> {
    let cfg = parse_profile(&profile)?;
    let resolve = |name: Option<String>, input: bool| {
        audio::resolve_device_name(&DeviceSelector::from_name(name), input).ok()
    };
    let in_name = if check_in {
        resolve(audio_in, true)
    } else {
        None
    };
    let out_name = if check_out {
        resolve(audio_out, false)
    } else {
        None
    };
    Ok(audio::setup_warnings(
        &cfg,
        in_name.as_deref(),
        out_name.as_deref(),
    ))
}

const SELF_TEST_NOTES: usize = 8;

fn format_latency_line(profile: &str, s: &cricket_engine::LatencySummary) -> String {
    format!(
        "{}: n={} mean={:.1} ms p95={:.1} ms jitter={:.1} ms (measured)",
        profile, s.n, s.mean_ms, s.p95_ms, s.jitter_ms
    )
}

/// Runs on a blocking worker thread: the measurement plays notes in real time
/// for several seconds and must not stall the UI.
#[tauri::command]
async fn run_self_test() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut lines = Vec::new();
        for profile in ["analog-fast", "analog-safe"] {
            let s = cricket_engine::measure_midi_loopback(profile, SELF_TEST_NOTES)
                .map_err(|e| format!("{}: {}", profile, e))?;
            lines.push(format_latency_line(profile, &s));
        }
        Ok(lines)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Encoder-side Panic: an All Notes Off message over the link.
#[tauri::command]
fn encoder_panic(state: State<'_, AppState>) -> Result<(), String> {
    let slot = state.encoder.lock().unwrap();
    let Some(running) = slot.as_ref() else {
        return Err("Encoder is not running".into());
    };
    running.handle.send(TransportMessage::Panic);
    Ok(())
}

/// Decoder-side Panic: the fail-safe burst (Sustain off + All Notes Off on
/// all 16 channels) straight to MIDI out, as on link-dead.
#[tauri::command]
fn decoder_panic(state: State<'_, AppState>) -> Result<(), String> {
    let slot = state.decoder.lock().unwrap();
    let Some(running) = slot.as_ref() else {
        return Err("Decoder is not running".into());
    };
    running.handle.panic();
    Ok(())
}

/// 1-based channel from the UI to the engine's 0-based one (`None` = 1).
fn channel_index(channel: Option<u16>) -> usize {
    usize::from(channel.unwrap_or(1).max(1)) - 1
}

/// Start the decoder. `audio_in` / `midi_out` are device names as listed by
/// `get_input_devices` / `get_midi_outputs` (`None` = default audio input /
/// no MIDI out); `audio_in_channel` is 1-based (`None` = 1).
/// `msc_device_id` / `msc_command_format` address the MSC SysEx sent to
/// MIDI out (`None` = all-call `0x7F` / Lighting `0x01`). A name that is
/// not present fails in the engine (reported as a log line); no other
/// device is picked. An unknown profile is an error. Returns the run id
/// carried by this run's `worker_stopped` event. Stopping (or restarting)
/// sends the fail-safe burst to MIDI out first.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn start_decoder(
    app: AppHandle,
    state: State<'_, AppState>,
    audio_in: Option<String>,
    audio_in_channel: Option<u16>,
    midi_out: Option<String>,
    profile: String,
    msc_device_id: Option<u8>,
    msc_command_format: Option<u8>,
) -> Result<u64, String> {
    parse_profile(&profile)?;
    let mut slot = state.decoder.lock().unwrap();
    let defaults = MscTarget::default();
    let msc = MscTarget {
        device_id: msc_device_id.unwrap_or(defaults.device_id) & 0x7F,
        command_format: msc_command_format.unwrap_or(defaults.command_format) & 0x7F,
    };
    shutdown(slot.take(), DecoderHandle::stop);

    let mut handle = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Device(DeviceSelector::from_name(audio_in)),
        midi_out: match DeviceSelector::from_name(midi_out) {
            DeviceSelector::Default => MidiOut::None,
            sel => MidiOut::Device(sel),
        },
        profile,
        link_dead_ms: None,
        msc,
        audio: AudioOptions {
            input_channel: channel_index(audio_in_channel),
            ..AudioOptions::default()
        },
    })
    .map_err(|e| e.to_string())?;
    let run = Run {
        role: Role::Decoder,
        id: state.next_run_id(),
    };
    let forwarder = spawn_forwarder(app, handle.take_events(), run);
    *slot = Some(Running { handle, forwarder });
    Ok(run.id)
}

#[tauri::command]
fn stop_decoder(state: State<'_, AppState>) -> Result<(), String> {
    let mut slot = state.decoder.lock().unwrap();
    shutdown(slot.take(), DecoderHandle::stop);
    Ok(())
}

/// Start the encoder. `audio_out` / `midi_in` are device names as listed by
/// `get_output_devices` / `get_midi_inputs` (`None` = default audio output /
/// no MIDI in); `audio_out_channel` is 1-based (`None` = 1), the other
/// channels stay silent; `tx_level_db` is the peak level in dBFS (`None` =
/// the default, −12). A name that is not present fails in the engine; no
/// other device is picked. An unknown profile is an error. Returns the run
/// id carried by this run's `worker_stopped` event. With no MIDI in the
/// encoder sends heartbeats only; `simulate` adds the test scale.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn start_encoder(
    app: AppHandle,
    state: State<'_, AppState>,
    audio_out: Option<String>,
    audio_out_channel: Option<u16>,
    midi_in: Option<String>,
    profile: String,
    simulate: bool,
    tx_level_db: Option<f32>,
) -> Result<u64, String> {
    parse_profile(&profile)?;
    let mut slot = state.encoder.lock().unwrap();
    shutdown(slot.take(), EncoderHandle::stop);

    let mut handle = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Device(DeviceSelector::from_name(audio_out)),
        midi_in: match DeviceSelector::from_name(midi_in) {
            DeviceSelector::Default => MidiIn::None,
            sel => MidiIn::Device(sel),
        },
        profile,
        simulate,
        tx_level_dbfs: tx_level_db.map(|db| db.clamp(MIN_TX_LEVEL_DBFS, 0.0)),
        audio: AudioOptions {
            output_channel: channel_index(audio_out_channel),
            ..AudioOptions::default()
        },
    })
    .map_err(|e| e.to_string())?;
    let run = Run {
        role: Role::Encoder,
        id: state.next_run_id(),
    };
    let forwarder = spawn_forwarder(app, handle.take_events(), run);
    *slot = Some(Running { handle, forwarder });
    Ok(run.id)
}

#[tauri::command]
fn stop_encoder(state: State<'_, AppState>) -> Result<(), String> {
    let mut slot = state.encoder.lock().unwrap();
    shutdown(slot.take(), EncoderHandle::stop);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            get_input_devices,
            get_output_devices,
            get_midi_inputs,
            get_midi_outputs,
            get_settings,
            set_settings,
            get_profiles,
            get_profile_info,
            get_device_channels,
            check_setup,
            run_self_test,
            encoder_panic,
            decoder_panic,
            start_decoder,
            stop_decoder,
            start_encoder,
            stop_encoder
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use cricket_engine::AlarmKind;

    const RUN: Run = Run {
        role: Role::Encoder,
        id: 7,
    };

    #[test]
    fn stopped_maps_to_worker_stopped_with_role_and_run_id() {
        assert_eq!(
            to_frontend(EngineEvent::Stopped, RUN),
            Some(FrontendEvent::WorkerStopped(WorkerStoppedEvent {
                role: Role::Encoder,
                run_id: 7,
            }))
        );
        let dec = Run {
            role: Role::Decoder,
            id: 1,
        };
        let Some(FrontendEvent::WorkerStopped(p)) = to_frontend(EngineEvent::Stopped, dec) else {
            panic!("Stopped must map to worker_stopped");
        };
        assert_eq!(
            serde_json::to_value(p).unwrap(),
            serde_json::json!({ "role": "decoder", "run_id": 1 })
        );
    }

    #[test]
    fn other_events_keep_their_mapping() {
        assert_eq!(
            to_frontend(EngineEvent::Log("hi".into()), RUN),
            Some(FrontendEvent::Log(LogEvent {
                message: "hi".into()
            }))
        );
        assert_eq!(
            to_frontend(EngineEvent::Decoded(TransportMessage::Heartbeat), RUN),
            Some(FrontendEvent::DecodedFrame(DecodedFrameEvent {
                frame_type: "Heartbeat".into(),
                payload: String::new(),
                is_heartbeat: true,
            }))
        );
        assert_eq!(
            to_frontend(
                EngineEvent::Alarm {
                    kind: AlarmKind::AudioDeviceError,
                    message: "x".into()
                },
                RUN
            ),
            Some(FrontendEvent::Alarm(AlarmEvent {
                kind: "audio_device_error".into(),
                title: "Audio device error".into(),
                message: "x".into()
            }))
        );
        assert_eq!(
            to_frontend(
                EngineEvent::TxStatus {
                    queued: 3,
                    latency_ms: 12.5
                },
                RUN
            ),
            Some(FrontendEvent::TxStatus(TxStatusEvent {
                queued: 3,
                latency_ms: 12.5
            }))
        );
    }

    #[test]
    fn profile_catalog_and_parsing() {
        let entries = profile_entries();
        assert_eq!(entries.iter().filter(|e| e.is_default).count(), 1);
        assert_eq!(entries[0].name, "analog-safe");
        assert!(entries
            .iter()
            .any(|e| e.tier == "advanced" && e.name == "ltc"));
        assert!(entries.iter().all(|e| e.airtime_ms > 0.0));
        assert!(parse_profile("robust").is_ok());
        assert!(parse_profile("Clean-Line").is_ok());
        assert!(parse_profile("robsut")
            .unwrap_err()
            .contains("unknown profile"));
        assert_eq!(channel_index(None), 0);
        assert_eq!(channel_index(Some(0)), 0);
        assert_eq!(channel_index(Some(3)), 2);
    }

    #[test]
    fn run_ids_are_unique_and_increasing() {
        let state = AppState::default();
        let a = state.next_run_id();
        let b = state.next_run_id();
        assert!(a >= 1 && b > a);
    }
}
