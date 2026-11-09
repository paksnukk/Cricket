//! Cricket real-time engine.
//!
//! Owns everything between the operating system and the `cricket_core` modem:
//! audio streams (cpal), MIDI ports (midir), and the decoder / encoder worker
//! threads. The GUI and CLI are thin shells over this crate.
//!
//! ```no_run
//! use cricket_engine::*;
//! let dec = DecoderHandle::start(DecoderConfig {
//!     audio_in: AudioIn::Device(DeviceSelector::Default),
//!     midi_out: MidiOut::Device(DeviceSelector::Index(0)),
//!     profile: "analog-safe".into(),
//!     link_dead_ms: None,
//!     msc: MscTarget::default(),
//!     audio: AudioOptions::default(),
//!     link_key: DEFAULT_LINK_KEY,
//!     fail_safe_burst: true,
//! })?;
//! let enc = EncoderHandle::start(EncoderConfig {
//!     audio_out: AudioOut::Device(DeviceSelector::Default),
//!     midi_in: MidiIn::None,
//!     profile: "analog-safe".into(),
//!     simulate: false,
//!     tx_level_dbfs: None,
//!     audio: AudioOptions { buffer_frames: Some(256), ..Default::default() },
//!     link_key: DEFAULT_LINK_KEY,
//!     msc_filter: MscFilter::default(),
//! })?;
//! enc.send(TransportMessage::Panic);
//! dec.panic(); // fail-safe burst to MIDI out, as on link-dead
//! for ev in dec.events().try_iter() { println!("{:?}", ev); }
//! // Drop or `.stop()` shuts the workers down and joins them; the decoder
//! // sends the fail-safe burst before it closes MIDI out.
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! In-memory endpoints live in [`memory`]; test-only ones in `testing`
//! (feature `testing`).
#![forbid(unsafe_code)]

pub mod audio;
pub mod decoder;
pub mod devices;
pub mod encoder;
pub mod events;
pub mod loopback;
pub mod memory;
pub mod midi;
pub mod pathcheck;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod loopback_tests;

pub use audio::{
    AudioIn, AudioOptions, AudioOut, AudioSink, AudioSource, DeviceErrorQueue, OpenAudioSink,
    OpenAudioSource, SampleFormatPref,
};
pub use cricket_core::link::{parse_link_key, DEFAULT_LINK_KEY};
pub use cricket_core::transport::{MscFilter, MscTarget, TransportMessage};
pub use decoder::{link_key_label, DecoderConfig, DecoderHandle};
pub use devices::DeviceSelector;
pub use encoder::{EncoderConfig, EncoderHandle, MIN_TX_LEVEL_DBFS};
pub use events::{
    format_message, link_state_name, rms_to_dbfs, AlarmKind, EngineEvent, LinkLight, LinkStats,
    NO_SIGNAL_DBFS,
};
pub use loopback::{
    jitter_ms, mean_ms, measure_chord_loopback, measure_midi_loopback,
    measure_midi_loopback_pattern, measure_midi_loopback_with, measure_throughput,
    percentile_nearest_rank_ms, ChordSummary, LatencySummary, NotePattern, Pacing, LEGATO_OVERLAP,
};
pub use midi::{MidiCallback, MidiIn, MidiInputGuard, MidiOut, MidiSink, MidiSource};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long `stop()` waits for a worker before giving up and detaching it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Sending half of a worker's event channel.
pub(crate) struct Emitter(Sender<EngineEvent>);

impl Emitter {
    pub(crate) fn send(&self, event: EngineEvent) {
        // The owner may have dropped the receiver; the worker keeps going
        // until told to stop.
        let _ = self.0.send(event);
    }

    pub(crate) fn log(&self, message: impl Into<String>) {
        self.send(EngineEvent::Log(message.into()));
    }
}

/// Runs when the worker body returns or unwinds: reports `Stopped`, then
/// clears the running flag.
struct ExitGuard {
    emitter: Emitter,
    running: Arc<AtomicBool>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.emitter.send(EngineEvent::Stopped);
        self.running.store(false, Ordering::SeqCst);
    }
}

/// A worker thread with a stop flag.
pub(crate) struct Worker {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    /// Spawn `body(events, stop)`; it must return soon after `stop` is set.
    pub(crate) fn spawn<F>(name: &str, body: F) -> anyhow::Result<(Self, Receiver<EngineEvent>)>
    where
        F: FnOnce(&Emitter, &AtomicBool) + Send + 'static,
    {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let stop_w = stop.clone();
        let running_w = running.clone();
        let join = thread::Builder::new().name(name.into()).spawn(move || {
            let guard = ExitGuard {
                emitter: Emitter(tx),
                running: running_w,
            };
            body(&guard.emitter, &stop_w);
        })?;
        Ok((
            Self {
                stop,
                running,
                join: Some(join),
            },
            rx,
        ))
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Request stop and join, waiting at most [`STOP_TIMEOUT`]. Returns true
    /// if the thread has exited (also when already stopped); false if it is
    /// still running, in which case it is detached.
    pub(crate) fn stop(&mut self) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        let Some(join) = self.join.take() else {
            return true;
        };
        let deadline = Instant::now() + STOP_TIMEOUT;
        while self.is_running() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if self.is_running() {
            return false;
        }
        let _ = join.join();
        true
    }
}

/// A receiver whose sender is gone (returned once events were taken).
pub(crate) fn closed_receiver() -> Receiver<EngineEvent> {
    channel().1
}

/// A warning when an MFSK profile's tones sit above what the stream's
/// converter passes (0.4535 × sample rate, the passband edge of a sharp
/// converter): the instrument profiles (19.0–21.5 kHz) on a device that
/// opened at 44.1 kHz. `None` when the plan fits.
pub fn hf_rate_warning(
    config: &cricket_core::PhyConfig,
    stream_sample_rate: u32,
) -> Option<String> {
    if config.n_tones() == 0 {
        return None;
    }
    let top = config.tone_hz(config.n_tones() as u8 - 1);
    let edge = 0.4535 * stream_sample_rate as f32;
    (top > edge).then(|| {
        format!(
            "WARNING: the profile's tones reach {:.1} kHz but the audio device runs at {} Hz, whose \
             converter passes about {:.1} kHz: the instrument profiles need a 48 kHz (or higher) \
             device end to end. Set the device to 48 kHz.",
            top / 1000.0,
            stream_sample_rate,
            edge / 1000.0
        )
    })
}

#[cfg(test)]
mod hf_rate_warning_tests {
    use super::hf_rate_warning;
    use cricket_core::PhyConfig;

    #[test]
    fn warns_only_when_the_plan_does_not_fit() {
        assert!(hf_rate_warning(&PhyConfig::hf_robust(), 48_000).is_none());
        assert!(hf_rate_warning(&PhyConfig::hf_robust(), 44_100).is_some());
        assert!(hf_rate_warning(&PhyConfig::robust(), 44_100).is_none());
        assert!(hf_rate_warning(&PhyConfig::ltc_fec(), 44_100).is_none());
        assert!(hf_rate_warning(&PhyConfig::hf_fast_96k(), 48_000).is_some());
        assert!(hf_rate_warning(&PhyConfig::hf_fast_96k(), 96_000).is_none());
    }
}
