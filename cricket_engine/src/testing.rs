//! Endpoints for hardware-free tests: the in-memory ones from
//! [`crate::memory`], plus endpoints that fail on purpose.
//!
//! Available under `cfg(test)` and with the `testing` feature.

use crate::audio::{AudioSink, AudioSource, OpenAudioSink, OpenAudioSource};
use crate::midi::MidiSink;
use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub use crate::memory::*;

/// A [`RecordingMidiSink`] whose sends fail while [`FailingMidiSink::fail`]
/// is set (nothing is recorded then), like a MIDI interface that was
/// unplugged.
#[derive(Clone, Default)]
pub struct FailingMidiSink {
    pub recorder: RecordingMidiSink,
    pub fail: Arc<AtomicBool>,
}

impl FailingMidiSink {
    pub fn set_failing(&self, failing: bool) {
        self.fail.store(failing, Ordering::SeqCst);
    }
}

impl MidiSink for FailingMidiSink {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(anyhow!("MIDI send: port closed"));
        }
        self.recorder.send(bytes)
    }
}

/// An audio input that fails to open (exercises the error path).
pub struct FailingAudio(pub String);

impl OpenAudioSource for FailingAudio {
    fn open(self: Box<Self>, _: u32, _: usize) -> Result<Box<dyn AudioSource>> {
        Err(anyhow!("{}", self.0))
    }
}

impl OpenAudioSink for FailingAudio {
    fn open(self: Box<Self>, _: u32, _: usize) -> Result<Box<dyn AudioSink>> {
        Err(anyhow!("{}", self.0))
    }
}
