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
//! re-arms link-dead as usual and clears the `LinkDead` alarm.
//!
//! **Fail-safe on stop and Panic.** When the worker is stopped
//! ([`DecoderHandle::stop`], drop, GUI Stop, Ctrl-C in the CLI) it sends the
//! same fail-safe burst to MIDI out before closing it, so stopping Cricket
//! can never leave a note droning on stage. [`DecoderHandle::panic`] sends
//! the burst immediately while running.
//!
//! **MIDI output failures** are counted: the first failed send raises
//! `Alarm { MidiOutputError }`, the next successful one
//! `AlarmCleared { MidiOutputError }`.
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
use anyhow::{anyhow, Result};
use cricket_core::link::DEFAULT_LINK_KEY;
use cricket_core::modem::{CricketModemRx, ModemOutput, ModemStats};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::MscTarget;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Decoder settings.
pub struct DecoderConfig {
    pub audio_in: AudioIn,
    pub midi_out: MidiOut,
    /// PHY profile name, user-facing or wire-format (see
    /// `PhyConfig::from_name`). An unknown name makes
    /// [`DecoderHandle::start`] fail: it never falls back to another profile.
    pub profile: String,
    /// Override the profile's link-dead timeout in ms (both the modem's
    /// sample-based link-dead and the wall-clock input watchdog). `None`
    /// uses the profile value; mainly for tests.
    pub link_dead_ms: Option<u32>,
    /// Device ID and command format of the MSC SysEx sent to MIDI out
    /// (spec §5.4; default all-call `0x7F`, Lighting (General) `0x01`).
    pub msc: MscTarget,
    /// Device buffer size, sample format and input channel (cpal devices
    /// only).
    pub audio: AudioOptions,
    /// Link key (spec §4.1), the same as the encoder's;
    /// `cricket_core::link::DEFAULT_LINK_KEY` (`0xFFFF`) is the unkeyed
    /// link. Packets from a link with another key are rejected (and counted
    /// in `LinkStats::wrong_key_suspects`).
    pub link_key: u16,
    /// Send the fail-safe burst (Note Offs for sounding notes, Sustain off,
    /// All Notes Off, pitch bend to centre; spec §5.3) on link-dead, Panic,
    /// input stall and stop. Default true; off for cue-only (MSC) links,
    /// where a stray All Notes Off is unwanted. Link-dead is still
    /// detected and reported.
    pub fail_safe_burst: bool,
}

/// A running decoder worker. Dropping it stops the worker (which sends the
/// fail-safe burst to MIDI out first).
pub struct DecoderHandle {
    worker: Worker,
    events: Receiver<EngineEvent>,
    panic: Arc<AtomicBool>,
}

impl DecoderHandle {
    /// Spawn the decoder thread. An unknown profile name is an error here.
    /// Device errors (audio or MIDI) do not fail this call; they are
    /// reported as `Log` events, after which the worker either continues
    /// without MIDI or ends with `Stopped`.
    pub fn start(config: DecoderConfig) -> Result<Self> {
        let phy = PhyConfig::from_name(&config.profile).ok_or_else(|| {
            anyhow!(
                "unknown profile {:?} (run `cricket profiles` for the list)",
                config.profile
            )
        })?;
        let panic = Arc::new(AtomicBool::new(false));
        let panic_w = panic.clone();
        let (worker, events) = Worker::spawn("cricket-decoder", move |ev, stop| {
            run(config, phy, &panic_w, ev, stop)
        })?;
        Ok(Self {
            worker,
            events,
            panic,
        })
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

    /// Send the fail-safe burst now (Sustain off + All Notes Off on all 16
    /// channels, pitch bend re-centered where it was left bent) to MIDI out,
    /// as on link-dead. The decoder keeps running.
    pub fn panic(&self) {
        self.panic.store(true, Ordering::SeqCst);
    }

    /// Stop and join the worker (waits at most [`crate::STOP_TIMEOUT`]). The
    /// worker sends the fail-safe burst to MIDI out before it exits.
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

/// The MIDI sink plus the state of its send-failure alarm (shared with the
/// pass-through worker's tap decoder).
pub(crate) struct MidiOutState {
    pub(crate) sink: Option<Box<dyn MidiSink>>,
    /// A send has failed and no send has succeeded since.
    pub(crate) failing: bool,
    /// Messages lost in the current failure streak.
    pub(crate) lost: u64,
}

impl MidiOutState {
    pub(crate) fn is_connected(&self) -> bool {
        self.sink.is_some()
    }

    pub(crate) fn send(&mut self, bytes: &[u8], ev: &Emitter) {
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        match sink.send(bytes) {
            Ok(()) => {
                if std::mem::take(&mut self.failing) {
                    let message = format!(
                        "MIDI out is working again ({} message{} lost meanwhile)",
                        self.lost,
                        if self.lost == 1 { "" } else { "s" }
                    );
                    self.lost = 0;
                    ev.log(message.clone());
                    ev.send(EngineEvent::AlarmCleared {
                        kind: AlarmKind::MidiOutputError,
                        message,
                    });
                }
            }
            Err(e) => {
                self.lost += 1;
                if !self.failing {
                    self.failing = true;
                    let message = format!(
                        "MIDI out failed: {} — decoded MIDI is not reaching the device",
                        e
                    );
                    ev.log(message.clone());
                    ev.send(EngineEvent::Alarm {
                        kind: AlarmKind::MidiOutputError,
                        message,
                    });
                }
            }
        }
    }

    pub(crate) fn send_all(&mut self, messages: &[Vec<u8>], ev: &Emitter) {
        for bytes in messages {
            self.send(bytes, ev);
        }
    }
}

/// The fail-safe burst as a log line.
const BURST_TEXT: &str = "Sustain off + All Notes Off sent on all 16 channels";

/// What the fail-safe did, for a log line.
pub(crate) fn burst_text(rx: &CricketModemRx) -> &'static str {
    if rx.fail_safe_burst_enabled() {
        BURST_TEXT
    } else {
        "fail-safe burst is off: nothing sent to MIDI out"
    }
}

/// The link key for a log line: a keyed link is shown masked (a passphrase
/// should not end up in logs), the unkeyed link as such.
pub fn link_key_label(link_key: u16) -> &'static str {
    if link_key == DEFAULT_LINK_KEY {
        "none (unkeyed link)"
    } else {
        "set (0x••••)"
    }
}

fn run(cfg: DecoderConfig, phy: PhyConfig, panic: &AtomicBool, ev: &Emitter, stop: &AtomicBool) {
    let name = PhyConfig::canonical_name(&cfg.profile).unwrap_or_else(|| cfg.profile.clone());
    ev.log(format!("Starting decoder (profile={})...", name));

    let mut config = phy.with_link_key(cfg.link_key);
    if let Some(ms) = cfg.link_dead_ms {
        config.link_dead_ms = ms;
    }
    let link_dead_timeout = Duration::from_millis(config.link_dead_ms as u64);
    ev.log(format!(
        "Link key {}; link-dead after {} ms; fail-safe burst {}.",
        link_key_label(config.link_key),
        config.link_dead_ms,
        if cfg.fail_safe_burst { "on" } else { "off" }
    ));
    let est_latency = config.estimated_first_copy_ms();
    let preferred_sample_rate = config.sample_rate_hz;

    let mut midi_out = MidiOutState {
        sink: match cfg.midi_out {
            MidiOut::None => {
                ev.log("No MIDI out selected — decode to UI only.");
                None
            }
            MidiOut::Device(sel) => match midi::open_output(&sel) {
                Ok(c) => Some(Box::new(c) as Box<dyn MidiSink>),
                Err(e) => {
                    ev.log(format!("MIDI out error: {}", e));
                    None
                }
            },
            MidiOut::Custom(sink) => Some(sink),
        },
        failing: false,
        lost: 0,
    };

    let mut audio = cfg.audio.clone();
    audio.digital = config.needs_bit_exact_path();
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
        "Audio in {} Hz, listening on ch {} of {}{}. First-copy airtime {:.1} ms.",
        stream_sample_rate,
        source.channel() + 1,
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
    if let Some(w) = crate::hf_rate_warning(&config, stream_sample_rate) {
        ev.log(w);
    }

    if let Err(e) = source.start() {
        ev.log(format!("Failed to play input: {}", e));
        return;
    }

    let mut rx = CricketModemRx::new(config);
    rx.set_msc_target(cfg.msc);
    rx.set_fail_safe_burst(cfg.fail_safe_burst);
    let mut rms_accumulator = 0.0f32;
    let mut sample_count_for_meter = 0usize;
    let meter_report_interval = (stream_sample_rate as usize) / 20;
    let mut last_stats_emit = Instant::now();
    let mut sync_lost: u64 = 0;
    let mut link_alive = false;
    // Set when `Alarm { LinkDead }` was sent; the next valid packet clears it.
    let mut dead_since: Option<Instant> = None;
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

        if panic.swap(false, Ordering::SeqCst) {
            let burst = rx.fail_safe_burst();
            midi_out.send_all(&burst, ev);
            ev.log(format!("Panic — {}", burst_text(&rx)));
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
                    midi_out.send_all(&burst, ev);
                    link_alive = false;
                    let message = format!(
                        "Audio input stalled ({} ms without samples) — {}",
                        silent_ms,
                        burst_text(&rx)
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
                    // Any valid packet (heartbeats included) means the link is up.
                    link_alive = true;
                    if let Some(since) = dead_since.take() {
                        let message = format!(
                            "Link restored after {:.1} s (valid packet received)",
                            since.elapsed().as_secs_f32()
                        );
                        ev.log(message.clone());
                        ev.send(EngineEvent::AlarmCleared {
                            kind: AlarmKind::LinkDead,
                            message,
                        });
                    }
                    ev.send(EngineEvent::Decoded(msg));
                }
                ModemOutput::Midi(bytes) => {
                    midi_out.send(&bytes, ev);
                }
                ModemOutput::LinkDead => {
                    link_alive = false;
                    dead_since = Some(Instant::now());
                    let message = format!(
                        "Link dead: no valid packet for {} ms — {}",
                        link_dead_timeout.as_millis(),
                        burst_text(&rx)
                    );
                    ev.send(EngineEvent::Alarm {
                        kind: AlarmKind::LinkDead,
                        message,
                    });
                    ev.log(if rx.fail_safe_burst_enabled() {
                        "Link dead — All Notes Off"
                    } else {
                        "Link dead (fail-safe burst off)"
                    });
                }
                ModemOutput::SyncLost => {
                    sync_lost = sync_lost.saturating_add(1);
                }
                ModemOutput::Stats(stats) if last_stats_emit.elapsed() >= STATS_INTERVAL => {
                    ev.send(EngineEvent::Stats(link_stats(
                        &stats,
                        sync_lost,
                        est_latency,
                        link_alive,
                        source.dropped(),
                    )));
                    last_stats_emit = Instant::now();
                }
                // Stats between reports, and outputs this engine predates.
                _ => {}
            }
        }
    }

    // Orderly stop: never leave a note hanging on the far end.
    if midi_out.is_connected() && rx.fail_safe_burst_enabled() {
        let burst = rx.fail_safe_burst();
        midi_out.send_all(&burst, ev);
        ev.log(format!("Stop — {}", burst_text(&rx)));
    }
    ev.log("Decoder stopped.");
}

pub(crate) fn link_stats(
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
        wrong_key_suspects: stats.wrong_key_suspects,
        fec_corrected: stats.fec_corrected,
        confidence_db: stats.last_confidence_db,
        last_seq: stats.last_seq,
        sync_lost,
        crc_pass_rate,
        est_latency_ms,
        link_alive,
        buffer_drops,
    }
}
