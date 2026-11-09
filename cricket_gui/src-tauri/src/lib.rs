//! Tauri shell over `cricket_engine`: commands start/stop engine handles kept in
//! app state, and one forwarder thread per handle turns `EngineEvent`s into
//! the frontend's Tauri events (`log`, `input_level`, `decoded_frame`,
//! `link_stats`, `alarm`, `alarm_cleared`, `worker_stopped`).
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

use cricket_core::phy::PhyConfig;
use cricket_engine::{
    audio, format_message, link_state_name, midi, AudioIn, AudioOptions, AudioOut, DecoderConfig,
    DecoderHandle, DeviceSelector, EncoderConfig, EncoderHandle, EngineEvent, MidiIn, MidiOut,
    MscTarget, TransportMessage,
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

fn parse_profile(profile: &str) -> PhyConfig {
    PhyConfig::from_name(profile)
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
            message,
        }),
        EngineEvent::AlarmCleared { kind, message } => FrontendEvent::AlarmCleared(AlarmEvent {
            kind: kind.as_str().into(),
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

#[tauri::command]
fn get_profile_info(profile: String) -> Result<serde_json::Value, String> {
    let cfg = parse_profile(&profile);
    Ok(serde_json::json!({
        "profile": profile,
        "est_latency_ms": cfg.estimated_first_copy_ms(),
        "symbol_duration_ms": cfg.symbol_duration_ms,
        "guard_gap_ms": cfg.guard_gap_ms,
        "repetition_k": cfg.repetition_k,
        "sample_rate_hz": cfg.sample_rate_hz,
    }))
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
        for profile in ["fast", "robust"] {
            let s = cricket_engine::measure_midi_loopback(profile, SELF_TEST_NOTES)
                .map_err(|e| format!("{}: {}", profile, e))?;
            lines.push(format_latency_line(profile, &s));
        }
        Ok(lines)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn encoder_panic(state: State<'_, AppState>) -> Result<(), String> {
    let slot = state.encoder.lock().unwrap();
    let Some(running) = slot.as_ref() else {
        return Err("Encoder is not running".into());
    };
    running.handle.send(TransportMessage::Panic);
    Ok(())
}

/// Start the decoder. `audio_in` / `midi_out` are device names as listed by
/// `get_input_devices` / `get_midi_outputs` (`None` = default audio input /
/// no MIDI out). `msc_device_id` / `msc_command_format` address the MSC
/// SysEx sent to MIDI out (`None` = all-call `0x7F` / Lighting `0x01`). A name that is not present fails in the engine (reported as
/// a log line); no other device is picked. Returns the run id carried by
/// this run's `worker_stopped` event.
#[tauri::command]
fn start_decoder(
    app: AppHandle,
    state: State<'_, AppState>,
    audio_in: Option<String>,
    midi_out: Option<String>,
    profile: String,
    msc_device_id: Option<u8>,
    msc_command_format: Option<u8>,
) -> Result<u64, String> {
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
        audio: AudioOptions::default(),
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
/// no MIDI in). A name that is not present fails in the engine; no other
/// device is picked. Returns the run id carried by this run's
/// `worker_stopped` event. With no MIDI in, the engine sends the simulated
/// test scale regardless of `simulate`.
#[tauri::command]
fn start_encoder(
    app: AppHandle,
    state: State<'_, AppState>,
    audio_out: Option<String>,
    midi_in: Option<String>,
    profile: String,
    simulate: bool,
) -> Result<u64, String> {
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
        audio: AudioOptions::default(),
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
            get_profile_info,
            run_self_test,
            encoder_panic,
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
    fn run_ids_are_unique_and_increasing() {
        let state = AppState::default();
        let a = state.next_run_id();
        let b = state.next_run_id();
        assert!(a >= 1 && b > a);
    }
}
