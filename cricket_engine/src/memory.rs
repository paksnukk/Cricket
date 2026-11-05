//! In-memory audio and MIDI endpoints: the hardware-free loopback used by
//! [`crate::loopback`] (CLI `loopback`, GUI self-test) and by tests.

use crate::audio::{AudioSink, AudioSource, DeviceErrorQueue, OpenAudioSink, OpenAudioSource};
use crate::midi::{MidiCallback, MidiInputGuard, MidiSink, MidiSource};
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

/// Shortest sleep while a paced burst is waiting for its next sample.
const PACED_MIN_SLEEP: Duration = Duration::from_micros(50);

#[derive(Default)]
struct PipeState {
    queue: VecDeque<f32>,
    /// The thread reading the source (woken when samples are pushed).
    reader: Option<Thread>,
    /// Paced reading: when the current burst started playing (the push into
    /// an empty pipe) and how many of its samples were delivered.
    burst_start: Option<Instant>,
    burst_delivered: u64,
}

type Pipe = Arc<Mutex<PipeState>>;

/// An in-memory audio cable: whatever the [`MemorySink`] receives, the
/// [`MemorySource`] delivers, in order, as fast as it is pulled (there is no
/// real-time clock and no silence is inserted when the pipe runs dry). A pipe
/// that runs dry therefore behaves like a stalled audio device.
///
/// Each end has a [`DeviceErrorQueue`] (see `errors()`) through which tests
/// inject device errors, as cpal's error callback would.
pub struct MemoryAudio;

impl MemoryAudio {
    /// A connected `(sink, source)` pair at `sample_rate`; the sink reports
    /// full once `capacity` samples are waiting.
    pub fn pair(sample_rate: u32, capacity: usize) -> (MemorySink, MemorySource) {
        let pipe: Pipe = Arc::new(Mutex::new(PipeState::default()));
        (
            MemorySink {
                pipe: pipe.clone(),
                sample_rate,
                capacity,
                errors: DeviceErrorQueue::new(),
            },
            MemorySource {
                pipe,
                sample_rate,
                errors: DeviceErrorQueue::new(),
            },
        )
    }
}

/// Write end of [`MemoryAudio`]. Use via `AudioOut::Custom(Box::new(sink))`.
pub struct MemorySink {
    pipe: Pipe,
    sample_rate: u32,
    capacity: usize,
    errors: DeviceErrorQueue,
}

impl MemorySink {
    /// Handle for injecting device errors (reported via `take_error`).
    pub fn errors(&self) -> DeviceErrorQueue {
        self.errors.clone()
    }
}

impl AudioSink for MemorySink {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> usize {
        1
    }
    fn start(&mut self) -> Result<()> {
        Ok(())
    }
    fn push(&mut self, samples: &[f32]) -> usize {
        let mut st = self.pipe.lock().unwrap();
        let n = samples
            .len()
            .min(self.capacity.saturating_sub(st.queue.len()));
        if n == 0 {
            return 0;
        }
        if st.queue.is_empty() && st.burst_start.is_none() {
            // An ideal device plays a burst from the moment it is queued.
            st.burst_start = Some(Instant::now());
            st.burst_delivered = 0;
        }
        st.queue.extend(&samples[..n]);
        if let Some(reader) = &st.reader {
            reader.unpark();
        }
        n
    }
    fn is_full(&self) -> bool {
        self.pipe.lock().unwrap().queue.len() >= self.capacity
    }
    fn buffered(&self) -> usize {
        self.pipe.lock().unwrap().queue.len()
    }
    fn take_error(&mut self) -> Option<String> {
        self.errors.pop()
    }
}

impl OpenAudioSink for MemorySink {
    fn open(
        self: Box<Self>,
        _preferred_rate: u32,
        _buffer_samples: usize,
    ) -> Result<Box<dyn AudioSink>> {
        Ok(self)
    }
}

/// Read end of [`MemoryAudio`]. Use via `AudioIn::Custom(Box::new(source))`.
pub struct MemorySource {
    pipe: Pipe,
    sample_rate: u32,
    errors: DeviceErrorQueue,
}

impl MemorySource {
    /// Handle for injecting device errors (reported via `take_error`).
    pub fn errors(&self) -> DeviceErrorQueue {
        self.errors.clone()
    }

    /// Same pipe, but samples are delivered at real-time rate when pulled.
    pub fn into_paced(self) -> PacedMemorySource {
        PacedMemorySource {
            pipe: self.pipe,
            sample_rate: self.sample_rate,
            errors: self.errors,
        }
    }
}

/// Make the calling thread the one a push wakes.
fn register_reader(pipe: &Pipe) {
    pipe.lock().unwrap().reader = Some(thread::current());
}

impl AudioSource for MemorySource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> usize {
        1
    }
    fn start(&mut self) -> Result<()> {
        register_reader(&self.pipe);
        Ok(())
    }
    fn pull(&mut self, out: &mut [f32]) -> usize {
        let mut st = self.pipe.lock().unwrap();
        let n = out.len().min(st.queue.len());
        for (dst, src) in out.iter_mut().zip(st.queue.drain(..n)) {
            *dst = src;
        }
        if st.queue.is_empty() {
            st.burst_start = None;
        }
        n
    }
    fn wait(&mut self, timeout: Duration) {
        if self.pipe.lock().unwrap().queue.is_empty() {
            thread::park_timeout(timeout);
        }
    }
    fn take_error(&mut self) -> Option<String> {
        self.errors.pop()
    }
}

impl OpenAudioSource for MemorySource {
    fn open(
        self: Box<Self>,
        _preferred_rate: u32,
        _buffer_samples: usize,
    ) -> Result<Box<dyn AudioSource>> {
        register_reader(&self.pipe);
        Ok(self)
    }
}

/// Like [`MemorySource`], but releases samples only as wall-clock time × sample
/// rate allows (real-time pacing for loopback latency tests): an ideal,
/// zero-latency device. A burst pushed into an empty pipe starts playing at
/// the push, so a packet takes exactly its own airtime to arrive, and
/// idle time never becomes credit.
pub struct PacedMemorySource {
    pipe: Pipe,
    sample_rate: u32,
    errors: DeviceErrorQueue,
}

impl PacedMemorySource {
    /// Samples of the current burst due by now (`st.burst_start` is set).
    fn due(&self, st: &PipeState, now: Instant) -> u64 {
        let elapsed = st
            .burst_start
            .map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        ((elapsed * self.sample_rate as f64).floor() as u64).saturating_sub(st.burst_delivered)
    }
}

impl AudioSource for PacedMemorySource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> usize {
        1
    }
    fn start(&mut self) -> Result<()> {
        register_reader(&self.pipe);
        Ok(())
    }
    fn pull(&mut self, out: &mut [f32]) -> usize {
        let mut st = self.pipe.lock().unwrap();
        if st.queue.is_empty() {
            st.burst_start = None;
            return 0;
        }
        let budget = self.due(&st, Instant::now()) as usize;
        let n = out.len().min(budget).min(st.queue.len());
        for (dst, src) in out.iter_mut().zip(st.queue.drain(..n)) {
            *dst = src;
        }
        st.burst_delivered += n as u64;
        if st.queue.is_empty() {
            st.burst_start = None;
        }
        n
    }
    fn wait(&mut self, timeout: Duration) {
        let st = self.pipe.lock().unwrap();
        match st.burst_start {
            Some(start) if !st.queue.is_empty() => {
                // Sleep until the next sample of the burst is due.
                let next = (st.burst_delivered + 1) as f64 / self.sample_rate as f64;
                let at = start + Duration::from_secs_f64(next);
                drop(st);
                let d = at.saturating_duration_since(Instant::now());
                thread::sleep(d.clamp(PACED_MIN_SLEEP, timeout.max(PACED_MIN_SLEEP)));
            }
            _ => {
                drop(st);
                thread::park_timeout(timeout);
            }
        }
    }
    fn take_error(&mut self) -> Option<String> {
        self.errors.pop()
    }
}

impl OpenAudioSource for PacedMemorySource {
    fn open(
        self: Box<Self>,
        _preferred_rate: u32,
        _buffer_samples: usize,
    ) -> Result<Box<dyn AudioSource>> {
        register_reader(&self.pipe);
        Ok(self)
    }
}

#[derive(Default)]
struct Recording {
    messages: Mutex<Vec<(Instant, Vec<u8>)>>,
    arrived: Condvar,
}

/// Records every MIDI message sent to it, with the time it was sent. Clones
/// share the same record.
#[derive(Clone, Default)]
pub struct RecordingMidiSink {
    inner: Arc<Recording>,
}

impl RecordingMidiSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of everything received so far.
    pub fn messages(&self) -> Vec<Vec<u8>> {
        self.timed_messages().into_iter().map(|(_, m)| m).collect()
    }

    /// Snapshot of everything received so far, with arrival times.
    pub fn timed_messages(&self) -> Vec<(Instant, Vec<u8>)> {
        self.inner.messages.lock().unwrap().clone()
    }

    /// Number of messages received so far.
    pub fn len(&self) -> usize {
        self.inner.messages.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Wait (woken by each arrival) until `pred` holds for the record or
    /// `timeout` passes; returns whether it held.
    pub fn wait_until(
        &self,
        timeout: Duration,
        mut pred: impl FnMut(&[(Instant, Vec<u8>)]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.messages.lock().unwrap();
        loop {
            if pred(&guard) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = self
                .inner
                .arrived
                .wait_timeout(guard, deadline - now)
                .unwrap()
                .0;
        }
    }
}

impl MidiSink for RecordingMidiSink {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let now = Instant::now();
        self.inner
            .messages
            .lock()
            .unwrap()
            .push((now, bytes.to_vec()));
        self.inner.arrived.notify_all();
        Ok(())
    }
}

type SharedCallback = Arc<Mutex<Option<MidiCallback>>>;

/// A MIDI input fed from test code through its [`MemoryMidiInjector`].
pub struct MemoryMidiInput {
    callback: SharedCallback,
}

/// Injects raw MIDI into a connected [`MemoryMidiInput`].
#[derive(Clone)]
pub struct MemoryMidiInjector {
    callback: SharedCallback,
}

impl MemoryMidiInput {
    pub fn new() -> (MemoryMidiInput, MemoryMidiInjector) {
        let callback: SharedCallback = Arc::new(Mutex::new(None));
        (
            MemoryMidiInput {
                callback: callback.clone(),
            },
            MemoryMidiInjector { callback },
        )
    }
}

impl MidiSource for MemoryMidiInput {
    fn connect(self: Box<Self>, on_message: MidiCallback) -> Result<MidiInputGuard> {
        *self.callback.lock().unwrap() = Some(on_message);
        Ok(MidiInputGuard::new(DisconnectOnDrop(self.callback)))
    }
}

struct DisconnectOnDrop(SharedCallback);

impl Drop for DisconnectOnDrop {
    fn drop(&mut self) {
        if let Ok(mut cb) = self.0.lock() {
            *cb = None;
        }
    }
}

impl MemoryMidiInjector {
    /// Deliver `bytes` as an incoming MIDI message. Returns false if the
    /// input is not (or no longer) connected.
    pub fn inject(&self, bytes: &[u8]) -> bool {
        match self.callback.lock().unwrap().as_mut() {
            Some(cb) => {
                cb(bytes);
                true
            }
            None => false,
        }
    }
}
