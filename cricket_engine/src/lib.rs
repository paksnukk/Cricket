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
//! })?;
//! let enc = EncoderHandle::start(EncoderConfig {
//!     audio_out: AudioOut::Device(DeviceSelector::Default),
//!     midi_in: MidiIn::None,
//!     profile: "analog-safe".into(),
//!     simulate: false,
//!     tx_level_dbfs: None,
//!     audio: AudioOptions { buffer_frames: Some(256), ..Default::default() },
//! })?;
//! enc.send(TransportMessage::Panic);
//! dec.panic(); // fail-safe burst to MIDI out, as on link-dead
//! for ev in dec.events().try_iter() { println!("{:?}", ev); }
//! // Drop or `.stop()` shuts the workers down and joins them; the decoder
//! // sends the fail-safe burst before it closes MIDI out.
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! In-memory endpoints live in [`memory`]; test-only ones in [`testing`]
//! (feature `testing`).

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
pub use cricket_core::transport::{MscTarget, TransportMessage};
pub use decoder::{DecoderConfig, DecoderHandle};
pub use devices::DeviceSelector;
pub use encoder::{EncoderConfig, EncoderHandle, MIN_TX_LEVEL_DBFS};
pub use events::{
    format_message, link_state_name, rms_to_dbfs, AlarmKind, EngineEvent, LinkLight, LinkStats,
    NO_SIGNAL_DBFS,
};
pub use loopback::{
    jitter_ms, mean_ms, measure_chord_loopback, measure_midi_loopback, measure_midi_loopback_with,
    measure_throughput, percentile_nearest_rank_ms, ChordSummary, LatencySummary, Pacing,
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
