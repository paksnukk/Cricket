//! RX worker: audio in → `CricketModemRx` → MIDI out + events.
//!
//! Besides the sample-driven link-dead of `CricketModemRx`, the worker runs a
//! **wall-clock watchdog** (v1.1 goal 4): if the audio source delivers no
//! samples for `link_dead_ms`, the sample clock stops too and link-dead could
//! never fire. Then the worker
//! - emits `Alarm { AudioInputStalled }` and a log line (once per stall);
//! - if the link is alive (a valid packet received and link-dead not fired
//!   since), declares link-dead on the modem and sends the fail-safe burst
//!   (CC64=0 + CC123 on every channel, spec §5.3) to the MIDI sink, exactly
//!   once.
//!
//! When samples resume it emits `AlarmCleared { AudioInputStalled }`. The
//! outage has already been handled: `declare_link_dead` disarmed the modem's
//! monitor, so the samples arriving after the stall do not fire a second
//! burst for it. The link stays `Dead` until the next valid packet, which
//! re-arms link-dead as usual.
//!
//! Device errors reported by the source (cpal's error callback) are polled
//! every loop iteration and emitted as `Alarm { AudioDeviceError }`.
//!
//! Latency: the worker sleeps in [`crate::AudioSource::wait`], which the cpal input
//! callback (and the in-memory sources) end as soon as samples arrive, so a
//! packet is decoded within microseconds of the callback that delivered its
//! last sample; decoded MIDI goes to the sink immediately.

use crate::audio::{AudioIn, AudioOptions};
use crate::events::{AlarmKind, EngineEvent, LinkStats};
use crate::midi::{self, MidiOut, MidiSink};
use crate::{closed_receiver, Emitter, Worker};
use anyhow::Result;
use cricket_core::modem::{CricketModemRx, ModemOutput, ModemStats};
use cricket_core::phy::{LinkState, Modulation, PhyConfig};
use cricket_core::transport::{MscTarget, TransportMessage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Decoder settings.
pub struct DecoderConfig {
    pub audio_in: AudioIn,
    pub midi_out: MidiOut,
    /// PHY profile name (see `PhyConfig::from_name`; unknown names mean robust).
    pub profile: String,
    /// Override the profile's link-dead timeout in ms (both the modem's
    /// sample-based link-dead and the wall-clock input watchdog). `None`
    /// uses the profile value; mainly for tests.
    pub link_dead_ms: Option<u32>,
    /// Device ID and command format of the MSC SysEx sent to MIDI out
    /// (spec §5.4; default all-call `0x7F`, Lighting (General) `0x01`).
    pub msc: MscTarget,
    /// Device buffer size and sample format (cpal devices only).
    pub audio: AudioOptions,
}

/// A running decoder worker. Dropping it stops the worker.
pub struct DecoderHandle {
    worker: Worker,
    events: Receiver<EngineEvent>,
}

impl DecoderHandle {
    /// Spawn the decoder thread. Device errors (audio or MIDI) do not fail
    /// this call; they are reported as `Log` events, after which the worker
    /// either continues without MIDI or ends with `Stopped`.
    pub fn start(config: DecoderConfig) -> Result<Self> {
        let (worker, events) =
            Worker::spawn("cricket-decoder", move |ev, stop| run(config, ev, stop))?;
        Ok(Self { worker, events })
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

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        self.worker.stop();
    }
}

/// Samples per pull from the source.
const CHUNK: usize = 512;
/// Longest wait for input before the loop checks the stop flag and the
/// stall watchdog again (arriving samples end the wait at once).
const IDLE_WAIT: Duration = Duration::from_millis(5);
/// Minimum interval between `Stats` events.
const STATS_INTERVAL: Duration = Duration::from_millis(200);

fn run(cfg: DecoderConfig, ev: &Emitter, stop: &AtomicBool) {
    ev.log(format!("Starting decoder (profile={})...", cfg.profile));

    let mut config = PhyConfig::from_name(&cfg.profile);
    if let Some(ms) = cfg.link_dead_ms {
        config.link_dead_ms = ms;
    }
    let link_dead_timeout = Duration::from_millis(config.link_dead_ms as u64);
    let est_latency = config.estimated_first_copy_ms();
    let preferred_sample_rate = config.sample_rate_hz;

    let mut midi_out: Option<Box<dyn MidiSink>> = match cfg.midi_out {
        MidiOut::None => {
            ev.log("No MIDI out selected — decode to UI only.");
            None
        }
        MidiOut::Device(sel) => match midi::open_output(&sel) {
            Ok(c) => Some(Box::new(c)),
            Err(e) => {
                ev.log(format!("MIDI out error: {}", e));
                None
            }
        },
        MidiOut::Custom(sink) => Some(sink),
    };

    let mut audio = cfg.audio.clone();
    audio.digital = matches!(config.modulation, Modulation::Digital);
    let mut source = match cfg.audio_in.open(
        preferred_sample_rate,
        preferred_sample_rate as usize * 2,
        &audio,
    ) {
        Ok(s) => s,
        Err(e) => {
            ev.log(format!("Audio input error: {}", e));
            return;
        }
    };
    let stream_sample_rate = source.sample_rate();

    let details = source.describe();
    ev.log(format!(
        "Audio in {} Hz, {} ch (ch0){}. est latency {:.1} ms.",
        stream_sample_rate,
        source.channels(),
        if details.is_empty() {
            String::new()
        } else {
            format!(", {}", details)
        },
        est_latency
    ));
    if stream_sample_rate != preferred_sample_rate {
        ev.log(format!(
            "Sample rate adapted {} → {} Hz",
            preferred_sample_rate, stream_sample_rate
        ));
        config = config.with_sample_rate(stream_sample_rate);
    }

    if let Err(e) = source.start() {
        ev.log(format!("Failed to play input: {}", e));
        return;
    }

    let mut rx = CricketModemRx::new(config);
    rx.set_msc_target(cfg.msc);
    let mut rms_accumulator = 0.0f32;
    let mut sample_count_for_meter = 0usize;
    let meter_report_interval = (stream_sample_rate as usize) / 20;
    let mut last_stats_emit = Instant::now();
    let mut sync_lost: u64 = 0;
    let mut link_alive = false;
    let mut buf = [0.0f32; CHUNK];
    // Wall-clock watchdog state: when the last sample was pulled, and whether
    // the current gap has already been reported as a stall.
    let mut last_sample_at = Instant::now();
    let mut stalled = false;

    while !stop.load(Ordering::SeqCst) {
        while let Some(err) = source.take_error() {
            ev.send(EngineEvent::Alarm {
                kind: AlarmKind::AudioDeviceError,
                message: format!("Audio input device error: {}", err),
            });
            ev.log(format!("Audio input device error: {}", err));
        }

        let n = source.pull(&mut buf);
        let chunk = &buf[..n];
        if n > 0 {
            let gap = last_sample_at.elapsed();
            last_sample_at = Instant::now();
            if std::mem::take(&mut stalled) {
                let message = format!(
                    "Audio input resumed after {} ms without samples",
                    gap.as_millis()
                );
                ev.log(message.clone());
                ev.send(EngineEvent::AlarmCleared {
                    kind: AlarmKind::AudioInputStalled,
                    message,
                });
            }
        }
        for &sample in chunk {
            rms_accumulator += sample * sample;
            sample_count_for_meter += 1;
            if sample_count_for_meter >= meter_report_interval {
                let rms = (rms_accumulator / sample_count_for_meter as f32).sqrt();
                ev.send(EngineEvent::Level(rms));
                rms_accumulator = 0.0;
                sample_count_for_meter = 0;
            }
        }

        if chunk.is_empty() {
            if !stalled && last_sample_at.elapsed() >= link_dead_timeout {
                stalled = true;
                let silent_ms = last_sample_at.elapsed().as_millis();
                if rx.declare_link_dead() {
                    let burst = rx.fail_safe_burst();
                    if let Some(sink) = midi_out.as_mut() {
                        for bytes in burst {
                            let _ = sink.send(&bytes);
                        }
                    }
                    link_alive = false;
                    let message = format!(
                        "Audio input stalled ({} ms without samples) — All Notes Off sent",
                        silent_ms
                    );
                    ev.send(EngineEvent::Alarm {
                        kind: AlarmKind::AudioInputStalled,
                        message: message.clone(),
                    });
                    ev.log(message);
                    let stats = rx.stats();
                    ev.send(EngineEvent::Stats(link_stats(
                        &stats,
                        sync_lost,
                        est_latency,
                        link_alive,
                        source.dropped(),
                    )));
                    last_stats_emit = Instant::now();
                } else {
                    let message = format!("Audio input stalled ({} ms without samples)", silent_ms);
                    ev.send(EngineEvent::Alarm {
                        kind: AlarmKind::AudioInputStalled,
                        message: message.clone(),
                    });
                    ev.log(message);
                }
            }
            source.wait(IDLE_WAIT);
            continue;
        }

        for out in rx.push_samples(chunk) {
            match out {
                ModemOutput::Message(msg) => {
                    if matches!(msg, TransportMessage::Heartbeat) {
                        link_alive = true;
                    }
                    ev.send(EngineEvent::Decoded(msg));
                }
                ModemOutput::Midi(bytes) => {
                    if let Some(sink) = midi_out.as_mut() {
                        let _ = sink.send(&bytes);
                    }
                }
                ModemOutput::LinkDead => {
                    link_alive = false;
                    ev.send(EngineEvent::Alarm {
                        kind: AlarmKind::LinkDead,
                        message: "Link dead — All Notes Off sent".into(),
                    });
                    ev.log("Link dead — All Notes Off");
                }
                ModemOutput::SyncLost => {
                    sync_lost = sync_lost.saturating_add(1);
                }
                ModemOutput::Stats(stats) => {
                    if last_stats_emit.elapsed() >= STATS_INTERVAL {
                        if matches!(stats.link_state, LinkState::Locked) {
                            link_alive = true;
                        }
                        if matches!(stats.link_state, LinkState::Dead) {
                            link_alive = false;
                        }
                        ev.send(EngineEvent::Stats(link_stats(
                            &stats,
                            sync_lost,
                            est_latency,
                            link_alive,
                            source.dropped(),
                        )));
                        last_stats_emit = Instant::now();
                    }
                }
            }
        }
    }

    ev.log("Decoder stopped.");
}

fn link_stats(
    stats: &ModemStats,
    sync_lost: u64,
    est_latency_ms: f32,
    link_alive: bool,
    buffer_drops: u64,
) -> LinkStats {
    let total = stats.crc_pass + stats.crc_fail;
    let crc_pass_rate = if total == 0 {
        100.0
    } else {
        100.0 * stats.crc_pass as f32 / total as f32
    };
    LinkStats {
        state: stats.link_state,
        crc_pass: stats.crc_pass,
        crc_fail: stats.crc_fail,
        confidence_db: stats.last_confidence_db,
        last_seq: stats.last_seq,
        sync_lost,
        crc_pass_rate,
        est_latency_ms,
        link_alive,
        buffer_drops,
    }
}
