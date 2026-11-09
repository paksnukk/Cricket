//! TX worker: MIDI in / injected messages → `CricketModemTx` → audio out.
//!
//! Latency: MIDI input and [`EncoderHandle::send`] wake the worker through a
//! condition variable (no polling interval), and the worker encodes every
//! queued user message whose audio fits below the refill threshold at once,
//! so a chord goes out back to back in the next audio callback. The device
//! buffer ([`AudioOptions::buffer_frames`]) is then the main delay.
//!
//! Without a MIDI input the encoder sends **heartbeats only**. The test
//! scale ([`EncoderConfig::simulate`]) is an explicit opt-in: a decoder on
//! stage must never receive notes nobody played.

use crate::audio::{AudioOptions, AudioOut, AudioSink};
use crate::decoder::link_key_label;
use crate::events::{format_message, AlarmKind, EngineEvent};
use crate::midi::{self, MidiCallback, MidiIn};
use crate::{closed_receiver, Emitter, Worker};
use anyhow::{anyhow, Result};
use cricket_core::modem::{
    refresh_quiet_samples, refresh_spacing_samples, repetition_samples, CricketModemTx,
};
use cricket_core::phy::{Modulation, PhyConfig};
use cricket_core::transport::{
    midi_input_filtered, MidiInput, MscFilter, TransportMessage, TxScheduler,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Lowest transmit level [`EncoderConfig::tx_level_dbfs`] accepts.
pub const MIN_TX_LEVEL_DBFS: f32 = -60.0;

/// Encoder settings.
pub struct EncoderConfig {
    pub audio_out: AudioOut,
    pub midi_in: MidiIn,
    /// PHY profile name, user-facing or wire-format (see
    /// `PhyConfig::from_name`: "analog-safe", "clean-line", "digital-line",
    /// "robust", "ltc_fec", …). An unknown name makes
    /// [`EncoderHandle::start`] fail: it never falls back to another profile.
    pub profile: String,
    /// Play a C-major test scale (one note per second) in addition to
    /// whatever arrives on MIDI in. Off by default; with no MIDI in and this
    /// off, the encoder sends heartbeats only.
    pub simulate: bool,
    /// Transmit level, peak dBFS (0 = full scale), clamped to
    /// [`MIN_TX_LEVEL_DBFS`]..=0. `None`: the profile default
    /// (`PhyConfig::DEFAULT_TX_LEVEL_DBFS`, −12 dBFS). Ignored by the
    /// `digital` profile, whose level is part of its wire format.
    pub tx_level_dbfs: Option<f32>,
    /// Device buffer size, sample format and output channel (cpal devices
    /// only).
    pub audio: AudioOptions,
    /// Link key (spec §4.1), the same as the decoder's;
    /// `cricket_core::link::DEFAULT_LINK_KEY` (`0xFFFF`) is the unkeyed
    /// link.
    pub link_key: u16,
    /// Which MSC the encoder carries (spec §5.4, multi-device rigs): by
    /// default every device ID and command format. With a filter, MSC
    /// addressed to another device is not sent (logged once).
    pub msc_filter: MscFilter,
}

/// What the MIDI callback and [`EncoderHandle::send`] hand to the worker.
enum Incoming {
    Message(TransportMessage),
    /// A message that cannot be carried exactly; reported, not sent.
    Rejected(String),
    /// MSC for another device ID / command format than the filter accepts.
    NotAddressed {
        device_id: u8,
        command_format: u8,
    },
}

/// Messages for the worker and the condition variable that wakes it.
#[derive(Default)]
struct PendingQueue {
    queue: Mutex<VecDeque<Incoming>>,
    arrived: Condvar,
}

impl PendingQueue {
    fn push(&self, item: Incoming) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(item);
            self.arrived.notify_one();
        }
    }

    /// Everything queued; waits up to `timeout` for something if empty.
    fn take(&self, timeout: Duration) -> Vec<Incoming> {
        let Ok(mut q) = self.queue.lock() else {
            return Vec::new();
        };
        if q.is_empty() && !timeout.is_zero() {
            q = match self.arrived.wait_timeout(q, timeout) {
                Ok((q, _)) => q,
                Err(_) => return Vec::new(),
            };
        }
        q.drain(..).collect()
    }
}

type Pending = Arc<PendingQueue>;

/// A running encoder worker. Dropping it stops the worker.
pub struct EncoderHandle {
    worker: Worker,
    events: Receiver<EngineEvent>,
    pending: Pending,
}

impl EncoderHandle {
    /// Spawn the encoder thread. An unknown profile name is an error here.
    /// Device errors (audio or MIDI) do not fail this call; they are
    /// reported as `Log` events, after which the worker either continues
    /// without MIDI in or ends with `Stopped`.
    pub fn start(config: EncoderConfig) -> Result<Self> {
        let phy = PhyConfig::from_name(&config.profile).ok_or_else(|| {
            anyhow!(
                "unknown profile {:?} (run `cricket profiles` for the list)",
                config.profile
            )
        })?;
        let pending: Pending = Arc::new(PendingQueue::default());
        let pending_w = pending.clone();
        let (worker, events) = Worker::spawn("cricket-encoder", move |ev, stop| {
            run(config, phy, pending_w, ev, stop)
        })?;
        Ok(Self {
            worker,
            events,
            pending,
        })
    }

    /// Queue a message for transmission (same queue as MIDI input).
    pub fn send(&self, msg: TransportMessage) {
        self.pending.push(Incoming::Message(msg));
    }

    /// The worker's event channel.
    pub fn events(&self) -> &Receiver<EngineEvent> {
        &self.events
    }

    /// Take ownership of the event channel (e.g. to forward it from another
    /// thread). Afterwards [`events`](Self::events) returns a closed channel.
    pub fn take_events(&mut self) -> Receiver<EngineEvent> {
        std::mem::replace(&mut self.events, closed_receiver())
    }

    /// True until the worker thread has finished.
    pub fn is_running(&self) -> bool {
        self.worker.is_running()
    }

    /// Stop and join the worker (waits at most [`crate::STOP_TIMEOUT`]).
    /// Returns false if the worker did not exit in time.
    pub fn stop(&mut self) -> bool {
        self.worker.stop()
    }
}

impl Drop for EncoderHandle {
    fn drop(&mut self) {
        self.worker.stop();
    }
}

/// Longest sleep of the loop (heartbeats, simulated notes, status events and
/// the stop flag are checked at least this often; MIDI input wakes it at once).
const TICK: Duration = Duration::from_millis(5);
/// Shortest sleep while waiting for buffered audio to drain.
const MIN_WAIT: Duration = Duration::from_micros(500);
/// Simulated note interval.
const SIM_INTERVAL: Duration = Duration::from_millis(1000);
/// Buffered audio below which the next user message is encoded (spec
/// §5.5). Above `TICK`, so back-to-back messages play without a gap.
const REFILL_MS: u64 = 10;
/// Minimum interval between `TxStatus` events.
const STATUS_INTERVAL: Duration = Duration::from_millis(200);

fn run(cfg: EncoderConfig, phy: PhyConfig, pending: Pending, ev: &Emitter, stop: &AtomicBool) {
    let name = PhyConfig::canonical_name(&cfg.profile).unwrap_or_else(|| cfg.profile.clone());
    let mut config = phy.with_link_key(cfg.link_key);
    let digital = matches!(config.modulation, Modulation::Digital);
    if let Some(db) = cfg.tx_level_dbfs {
        if digital {
            ev.log("TX level ignored: the digital-bit-exact profile's level is fixed by its wire format.");
        } else {
            config = config.with_tx_level_dbfs(db.clamp(MIN_TX_LEVEL_DBFS, 0.0));
        }
    }
    ev.log(format!(
        "Starting encoder (profile={}, TX level {:.1} dBFS peak, test scale {})...",
        name,
        config.tx_level_dbfs(),
        if cfg.simulate { "ON" } else { "off" }
    ));
    ev.log(format!(
        "Link key {}; MSC accepted {}.",
        link_key_label(config.link_key),
        msc_filter_label(&cfg.msc_filter)
    ));

    let est_latency = config.estimated_first_copy_ms();
    let preferred_sample_rate = config.sample_rate_hz;
    let use_sim = cfg.simulate;

    let on_midi = {
        let pending = pending.clone();
        let filter = cfg.msc_filter;
        move || -> MidiCallback {
            let pending = pending.clone();
            Box::new(move |message: &[u8]| {
                let incoming = match midi_input_filtered(message, &filter) {
                    MidiInput::Message(msg) => Incoming::Message(msg),
                    MidiInput::Rejected(e) => Incoming::Rejected(format!(
                        "Can't be sent over this link: {} — not sent",
                        e
                    )),
                    MidiInput::NotAddressed {
                        device_id,
                        command_format,
                    } => Incoming::NotAddressed {
                        device_id,
                        command_format,
                    },
                    MidiInput::Ignored => return,
                };
                pending.push(incoming);
            })
        }
    };
    let mut not_addressed_logged = false;
    // Keeps the MIDI input connected for the worker's lifetime.
    let _midi_conn = match cfg.midi_in {
        MidiIn::None => {
            if !use_sim {
                ev.log("No MIDI in selected — sending heartbeats only.");
            }
            None
        }
        MidiIn::Device(sel) => match midi::open_input(&sel, on_midi()) {
            Ok(c) => {
                ev.log(format!("MIDI in {} connected.", sel));
                Some(midi::MidiInputGuard::new(c))
            }
            Err(e) => {
                ev.log(format!("MIDI in error: {}", e));
                None
            }
        },
        MidiIn::Custom(source) => match source.connect(on_midi()) {
            Ok(g) => {
                ev.log("MIDI in (custom source) connected.");
                Some(g)
            }
            Err(e) => {
                ev.log(format!("MIDI in error: {}", e));
                None
            }
        },
    };

    // The ring holds the refill threshold plus one message, and at least two
    // messages (5 ms minimum).
    let ring_floor = (preferred_sample_rate as u64 * 5 / 1000).max(1);
    let est_msg = repetition_samples(&config) * config.repetition_k.max(1) as u64;
    let refill_est = preferred_sample_rate as u64 * REFILL_MS / 1000;
    let ring_samples = (est_msg * 2).max(refill_est + est_msg).max(ring_floor) as usize;

    let drops = AtomicU64::new(0);
    let mut audio = cfg.audio.clone();
    audio.digital = digital;
    let mut sink = match cfg
        .audio_out
        .open(preferred_sample_rate, ring_samples, &audio)
    {
        Ok(s) => s,
        Err(e) => {
            ev.log(format!("Audio output error: {}", e));
            return;
        }
    };
    let stream_sample_rate = sink.sample_rate();

    let details = sink.describe();
    ev.log(format!(
        "Audio out {} Hz, sending on ch {} of {}{}. First-copy airtime {:.1} ms.",
        stream_sample_rate,
        sink.channel() + 1,
        sink.channels(),
        if details.is_empty() {
            String::new()
        } else {
            format!(", {}", details)
        },
        est_latency
    ));
    if stream_sample_rate != preferred_sample_rate {
        config = config.with_sample_rate(stream_sample_rate);
    }
    let message_samples = repetition_samples(&config) * config.repetition_k.max(1) as u64;
    let heartbeat_samples =
        config.sample_rate_hz as u64 * config.heartbeat_interval_ms as u64 / 1000;
    // User messages are encoded while less than 10 ms is buffered (spec
    // §5.5); refreshes and heartbeats only when less than one message is,
    // so with short messages (LTC at high rates, digital) a note arriving
    // meanwhile waits behind at most one of them.
    let refill_user = config.sample_rate_hz as u64 * REFILL_MS / 1000;
    let refill_idle = refill_user.min(message_samples);

    if let Err(e) = sink.start() {
        ev.log(format!("Failed to play output: {}", e));
        return;
    }

    let mut tx = CricketModemTx::new(config.clone());
    let mut scheduler = TxScheduler::new(stream_sample_rate, heartbeat_samples, message_samples)
        .with_refresh_spacing(refresh_spacing_samples(&config, stream_sample_rate))
        .with_refresh_quiet(refresh_quiet_samples(&config, stream_sample_rate));
    let stream_start = Instant::now();
    let mut note_idx = 0usize;
    let scale = [0u8, 2, 4, 5, 7, 9, 11, 12];
    let mut last_sim = Instant::now() - SIM_INTERVAL;
    let mut sim_sounding: Option<u8> = None;
    let mut last_status = Instant::now();
    let mut dropped_total: u64 = 0;
    let sink = sink.as_mut();
    let rate = f64::from(stream_sample_rate.max(1));
    let mut wait = Duration::ZERO;

    while !stop.load(Ordering::SeqCst) {
        let incoming = pending.take(wait);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let tx_time = (stream_start.elapsed().as_secs_f64() * rate) as u64;
        for item in incoming {
            match item {
                Incoming::Message(msg) => scheduler.push(msg, tx_time),
                Incoming::Rejected(message) => {
                    ev.log(format!("WARNING: {}", message));
                    ev.send(EngineEvent::Alarm {
                        kind: AlarmKind::MessageRejected,
                        message,
                    });
                }
                Incoming::NotAddressed {
                    device_id,
                    command_format,
                } => {
                    if !not_addressed_logged {
                        not_addressed_logged = true;
                        ev.log(format!(
                            "MSC for device {:#04x} / format {:#04x} not sent (this encoder accepts {}); further ones are not logged",
                            device_id,
                            command_format,
                            msc_filter_label(&cfg.msc_filter)
                        ));
                    }
                }
            }
        }

        let note_ons = core::mem::take(&mut scheduler.note_ons_dropped);
        let others = core::mem::take(&mut scheduler.queue_dropped);
        if note_ons > 0 || others > 0 {
            dropped_total += u64::from(note_ons) + u64::from(others);
            let message = drop_message(note_ons, others, scheduler.max_note_ons(), &name);
            ev.log(format!("WARNING: {}", message));
            ev.send(EngineEvent::Alarm {
                kind: AlarmKind::MessageDropped,
                message,
            });
        }

        if use_sim && last_sim.elapsed() >= SIM_INTERVAL {
            if let Some(prev) = sim_sounding.take() {
                scheduler.push(
                    TransportMessage::NoteOff {
                        channel: 0,
                        note: prev,
                        velocity: 64,
                    },
                    tx_time,
                );
            }
            let note = 60 + scale[note_idx % scale.len()];
            scheduler.push(
                TransportMessage::NoteOn {
                    channel: 0,
                    note,
                    velocity: 100,
                },
                tx_time,
            );
            sim_sounding = Some(note);
            ev.log(format!("TX test scale Note {}", note));
            note_idx += 1;
            last_sim = Instant::now();
        }

        // Choose the next message only when the audio ahead of it has nearly
        // played out, so a note that arrives meanwhile does not wait behind
        // an already encoded refresh or heartbeat; encode all user messages
        // that fit at once (a chord leaves in one audio callback).
        loop {
            let buffered = sink.buffered() as u64;
            let limit = if scheduler.queued() > 0 {
                refill_user
            } else {
                refill_idle
            };
            if buffered >= limit {
                break;
            }
            let Some(result) = scheduler.poll_detailed(tx_time + buffered) else {
                break;
            };
            if result.user {
                let (kind, payload) = format_message(&result.msg);
                ev.log(format!("TX {} {}", kind, payload).trim_end().to_string());
            }
            let audio = if matches!(result.msg, TransportMessage::Heartbeat) {
                tx.encode_heartbeat()
            } else {
                tx.encode_message(&result.msg)
            };
            push_audio(sink, &audio, stop, &drops);
        }

        while let Some(err) = sink.take_error() {
            ev.send(EngineEvent::Alarm {
                kind: AlarmKind::AudioDeviceError,
                message: format!("Audio output device error: {}", err),
            });
            ev.log(format!("Audio output device error: {}", err));
        }

        let d = drops.load(Ordering::Relaxed);
        if d > 0 && d.is_multiple_of(1000) {
            ev.log(format!("Audio buffer drops: {}", d));
        }

        if last_status.elapsed() >= STATUS_INTERVAL {
            // The oldest queued message's wait so far plus the audio already
            // buffered ahead of it.
            let queued = scheduler.queued();
            let late = scheduler.oldest_wait_samples(tx_time) + sink.buffered() as u64;
            let latency_ms = late as f32 * 1000.0 / stream_sample_rate.max(1) as f32;
            ev.send(EngineEvent::TxStatus {
                queued,
                latency_ms,
                dropped: dropped_total,
            });
            last_status = Instant::now();
        }

        // Sleep until MIDI arrives, the buffer has drained to the refill
        // threshold, or the next tick.
        let buffered = sink.buffered() as u64;
        let limit = if scheduler.queued() > 0 {
            refill_user
        } else {
            refill_idle
        };
        wait = if buffered >= limit {
            Duration::from_secs_f64((buffered - limit + 1) as f64 / rate).clamp(MIN_WAIT, TICK)
        } else {
            TICK
        };
    }

    ev.log("Encoder stopped.");
}

/// The MSC filter for a log line.
fn msc_filter_label(filter: &MscFilter) -> String {
    let field = |v: Option<u8>| match v {
        Some(v) => format!("{v:#04x}"),
        None => "any".to_string(),
    };
    if filter.is_filtering() {
        format!(
            "device {} / format {} (and all-call)",
            field(filter.device_id),
            field(filter.command_format)
        )
    } else {
        "any device ID and command format".to_string()
    }
}

/// The alarm text for `note_ons` NoteOns refused beyond the chord limit
/// `max_note_ons` and `others` tier-3 messages dropped for the queue bound.
fn drop_message(note_ons: u32, others: u32, max_note_ons: usize, profile: &str) -> String {
    let plural = |n: u32, what: &str| {
        if n == 1 {
            format!("1 {what}")
        } else {
            format!("{n} {what}s")
        }
    };
    let mut parts = Vec::new();
    if note_ons > 0 {
        parts.push(format!(
            "{} (more than {} notes queued at once; the ones struck first were sent)",
            plural(note_ons, "NoteOn"),
            max_note_ons
        ));
    }
    if others > 0 {
        parts.push(format!(
            "{} (more than 1 s of controller / program / system traffic queued; the oldest went)",
            plural(others, "message")
        ));
    }
    format!(
        "TX dropped {} — the link is overloaded for {}; play fewer notes at once or use a faster profile",
        parts.join(" and "),
        profile
    )
}

/// Queue `samples`, waiting up to ~2 s while the sink is full (then counting
/// the rest as dropped). Returns the number of samples actually queued.
fn push_audio(
    sink: &mut dyn AudioSink,
    samples: &[f32],
    stop: &AtomicBool,
    drops: &AtomicU64,
) -> usize {
    let mut pushed = 0usize;
    let mut attempts = 0;
    while pushed < samples.len() {
        let n = sink.push(&samples[pushed..]);
        pushed += n;
        if pushed == samples.len() {
            break;
        }
        if stop.load(Ordering::Relaxed) || attempts > 2000 {
            drops.fetch_add((samples.len() - pushed) as u64, Ordering::Relaxed);
            break;
        }
        if n == 0 {
            thread::sleep(Duration::from_millis(1));
            attempts += 1;
        }
    }
    pushed
}
