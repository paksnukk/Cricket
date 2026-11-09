//! Pass-through worker: audio in → a user [`AudioProcessor`] → audio out,
//! on one input and one output stream.
//!
//! This is the duplex node a hardware box needs: the instrument's audio
//! goes through unchanged while a copy is decoded ([`TapDecoder`]), with an
//! optional *strip* stage that removes the data band from the output
//! ([`StripFilter`]). [`Passthrough`] is the identity processor (bit-exact).
//!
//! # Scheduling and latency
//!
//! The worker sleeps in [`AudioSource::wait`], which the cpal input callback
//! ends (`Thread::unpark`) the moment it has queued samples; the worker then
//! pulls everything that is waiting (at most [`MAX_BLOCK`] samples per call),
//! runs the processor on that block and pushes the result into the output
//! ring, which the output callback drains at its next pass. In-process, with
//! the in-memory endpoints, the block that comes in is the block that goes
//! out: **zero samples of added delay** and a wall-clock turnaround of
//! microseconds ([`measure_passthrough_latency`]). On real devices the two
//! device buffers set the latency, as for the decoder and the encoder; a
//! processor with memory (the strip filter) adds its own group delay, which
//! it reports through [`AudioProcessor::latency_samples`].
//!
//! The output ring is **bounded** ([`PassthroughConfig::max_ahead_ms`]): if
//! the output device consumes slower than the input delivers (two devices on
//! different clocks, or a stalled output) the worker drops the oldest
//! waiting audio instead of letting the delay grow, and counts the drops
//! (`Log` every 1000). An output that runs dry plays silence.
//!
//! Events: whatever the processor emits (the `TapDecoder` emits what the
//! decoder worker emits: `Level`, `Decoded`, `Stats`, the link-dead and
//! MIDI-out alarms), plus `AudioDeviceError` from either stream,
//! `AudioInputStalled` / `AlarmCleared` from the wall-clock watchdog, and
//! `Stopped` last.

use crate::audio::{AudioIn, AudioOptions, AudioOut, AudioSink, AudioSource};
use crate::decoder::{burst_text, link_key_label, link_stats, MidiOutState};
use crate::events::{AlarmKind, EngineEvent};
use crate::midi::{self, MidiOut, MidiSink};
use crate::{closed_receiver, Emitter, Worker};
use anyhow::{anyhow, Result};
use cricket_core::modem::{CricketModemRx, ModemOutput};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::MscTarget;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Most input samples the worker takes in one block (so a long stall does
/// not turn into one huge block).
pub const MAX_BLOCK: usize = 4096;
/// Longest wait for input before the loop checks the stop flag and the
/// stall watchdog again (arriving samples end the wait at once).
const IDLE_WAIT: Duration = Duration::from_millis(5);
/// Minimum interval between `Stats` events of the tap decoder.
const STATS_INTERVAL: Duration = Duration::from_millis(200);

/// What a processor sees besides the samples: the stream, a way to report,
/// and the worker's MIDI output ([`PassthroughConfig::midi_out`], opened in
/// the worker thread so the processor itself stays `Send`).
pub struct ProcessContext<'a> {
    /// The stream's sample rate.
    pub sample_rate_hz: u32,
    /// Stream position (samples since the worker started) of the first
    /// sample of this block.
    pub position: u64,
    emitter: &'a Emitter,
    midi: &'a mut MidiOutState,
}

impl ProcessContext<'_> {
    /// Report an event to the owner of the worker (the same channel as the
    /// worker's own events).
    pub fn emit(&self, event: EngineEvent) {
        self.emitter.send(event);
    }

    /// A `Log` line.
    pub fn log(&self, message: impl Into<String>) {
        self.emitter.log(message);
    }

    /// Whether a MIDI output is connected.
    pub fn midi_connected(&self) -> bool {
        self.midi.is_connected()
    }

    /// Send one MIDI message to the worker's MIDI output (nothing without
    /// one). A failing device raises `AlarmKind::MidiOutputError` once per
    /// failure streak, as in the decoder worker.
    pub fn send_midi(&mut self, bytes: &[u8]) {
        self.midi.send(bytes, self.emitter);
    }

    /// Send several MIDI messages.
    pub fn send_midi_all(&mut self, messages: &[Vec<u8>]) {
        self.midi.send_all(messages, self.emitter);
    }
}

/// Something that turns input audio into output audio, block by block, in
/// the pass-through worker's thread. Blocks are mono `f32` at the stream
/// rate, of any length; `input` and `output` have the same length.
pub trait AudioProcessor: Send {
    /// Called once in the worker thread, after the streams are open and
    /// before the first block: the place to build state for the actual
    /// sample rate and to open devices that must live in this thread. An
    /// error stops the worker.
    fn start(&mut self, _ctx: &mut ProcessContext<'_>) -> Result<()> {
        Ok(())
    }

    /// Process one block.
    fn process(&mut self, input: &[f32], output: &mut [f32], ctx: &mut ProcessContext<'_>);

    /// The input delivered no samples for the watchdog period (device
    /// unplugged, driver stall); called once per stall.
    fn input_stalled(&mut self, _ctx: &mut ProcessContext<'_>) {}

    /// Called once when the worker stops (an orderly stop or a device
    /// error after `start`).
    fn stop(&mut self, _ctx: &mut ProcessContext<'_>) {}

    /// Delay this processor adds between input and output, in samples (a
    /// filter's group delay); 0 for a memoryless processor.
    fn latency_samples(&self) -> usize {
        0
    }

    /// Name for the log.
    fn name(&self) -> &str {
        "processor"
    }
}

/// The identity: output = input, bit-exact.
#[derive(Debug, Default, Clone, Copy)]
pub struct Passthrough;

impl AudioProcessor for Passthrough {
    fn process(&mut self, input: &[f32], output: &mut [f32], _ctx: &mut ProcessContext<'_>) {
        output.copy_from_slice(input);
    }

    fn name(&self) -> &str {
        "passthrough"
    }
}

// ---------------------------------------------------------------------------
// Strip filter

/// Removes a frequency band from the audio: a linear-phase FIR band-stop
/// (Kaiser-windowed sinc) designed for the band and sample rate.
///
/// For the HF (instrument-audio) profiles the band is the receiver band,
/// 18–22.5 kHz at 48 kHz, whose upper edge is at the converter's passband
/// edge, so the filter degenerates to a low-pass: the design puts the
/// passband edge `transition_hz` (1 kHz) below the band and the stopband
/// edge at the band. With [`StripFilter::DEFAULT_ATTENUATION_DB`] (46 dB)
/// the result is **129 taps**: ≤ 0.04 dB of ripple below 17 kHz (−0.04 dB
/// at 17 kHz), −46.7 dB at 18 kHz, −58 dB at the lowest tone (19 kHz) and
/// 56 dB of attenuation of an `instrument` burst as a whole, with a group
/// delay of **64 samples (1.33 ms at 48 kHz)**, which is added latency on
/// the stripped output (the unstripped tap is not delayed). Measured in
/// `passthrough_tests`.
#[derive(Debug, Clone)]
pub struct StripFilter {
    taps: Vec<f32>,
    /// Circular history of the last `taps.len()` inputs.
    history: Vec<f32>,
    head: usize,
}

impl StripFilter {
    /// Stopband attenuation (and, for a Kaiser design, the passband ripple:
    /// 46 dB ≙ ±0.044 dB) of [`StripFilter::for_band`].
    pub const DEFAULT_ATTENUATION_DB: f32 = 46.0;
    /// Transition width of [`StripFilter::for_band`]: the passband ends this
    /// far below the band (and resumes this far above it).
    pub const DEFAULT_TRANSITION_HZ: f32 = 1000.0;

    /// A band-stop for `(low_hz, high_hz)` at `sample_rate_hz` with the
    /// default transition and attenuation.
    pub fn for_band(band_hz: (f32, f32), sample_rate_hz: u32) -> Self {
        Self::design(
            band_hz,
            sample_rate_hz,
            Self::DEFAULT_TRANSITION_HZ,
            Self::DEFAULT_ATTENUATION_DB,
        )
    }

    /// The strip filter for a profile: its receiver band
    /// ([`PhyConfig::rx_band_hz`]), or `None` for a phy without one (the
    /// LTC and digital profiles occupy the whole audio band: nothing can be
    /// stripped).
    pub fn for_profile(config: &PhyConfig, sample_rate_hz: u32) -> Option<Self> {
        config
            .rx_band_hz()
            .map(|band| Self::for_band(band, sample_rate_hz))
    }

    /// Kaiser-window design: stopband `[low, high]` with `attenuation_db` of
    /// rejection, passband ripple of the same size, `transition_hz` wide
    /// transitions on each side. A band whose upper edge reaches the
    /// converter passband (≥ 0.46 × sample rate) becomes a low-pass.
    pub fn design(
        band_hz: (f32, f32),
        sample_rate_hz: u32,
        transition_hz: f32,
        attenuation_db: f32,
    ) -> Self {
        let fs = sample_rate_hz.max(1) as f64;
        let (low, high) = (band_hz.0 as f64, band_hz.1 as f64);
        let transition = (transition_hz as f64).max(1.0);
        let a = (attenuation_db as f64).max(21.0);
        // Kaiser: beta and length for attenuation A over a transition of
        // delta_omega (Oppenheim & Schafer 7.4).
        let beta = if a > 50.0 {
            0.1102 * (a - 8.7)
        } else {
            0.5842 * (a - 21.0).powf(0.4) + 0.078_86 * (a - 21.0)
        };
        let delta_omega = 2.0 * std::f64::consts::PI * transition / fs;
        let mut n = ((a - 7.95) / (2.285 * delta_omega)).ceil() as usize + 1;
        if n.is_multiple_of(2) {
            n += 1;
        }
        let m = (n - 1) / 2;
        // Cutoffs at the middle of each transition.
        let f1 = (low - transition / 2.0).max(0.0) / fs;
        let lowpass_only = high >= 0.46 * fs;
        let f2 = (high + transition / 2.0).min(fs / 2.0) / fs;
        let sinc = |f: f64, k: f64| -> f64 {
            if k == 0.0 {
                2.0 * f
            } else {
                (2.0 * std::f64::consts::PI * f * k).sin() / (std::f64::consts::PI * k)
            }
        };
        let i0_beta = bessel_i0(beta);
        let taps: Vec<f32> = (0..n)
            .map(|i| {
                let k = i as f64 - m as f64;
                // band-stop = low-pass(f1) + high-pass(f2) = lp(f1) + delta - lp(f2)
                let mut h = sinc(f1, k);
                if !lowpass_only {
                    h -= sinc(f2, k);
                    if k == 0.0 {
                        h += 1.0;
                    }
                }
                let r = k / m.max(1) as f64;
                let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
                (h * w) as f32
            })
            .collect();
        Self {
            history: vec![0.0; taps.len()],
            taps,
            head: 0,
        }
    }

    /// Number of taps.
    pub fn len(&self) -> usize {
        self.taps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    /// Group delay in samples (linear phase: half the length).
    pub fn group_delay_samples(&self) -> usize {
        (self.taps.len().max(1) - 1) / 2
    }

    /// The filter's coefficients.
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }

    /// Magnitude response at `hz` in dB (for tests and reports).
    pub fn response_db(&self, hz: f32, sample_rate_hz: u32) -> f32 {
        let w = 2.0 * std::f64::consts::PI * hz as f64 / sample_rate_hz as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (k, &h) in self.taps.iter().enumerate() {
            re += h as f64 * (w * k as f64).cos();
            im -= h as f64 * (w * k as f64).sin();
        }
        (20.0 * (re * re + im * im).sqrt().max(1e-12).log10()) as f32
    }

    /// Filter `input` into `output` (same length), continuing from the
    /// previous block.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let n = self.taps.len();
        for (x, y) in input.iter().zip(output.iter_mut()) {
            self.history[self.head] = *x;
            // history[head] is the newest sample: tap 0.
            let mut acc = 0.0f32;
            let mut idx = self.head;
            for &h in &self.taps {
                acc += h * self.history[idx];
                idx = if idx == 0 { n - 1 } else { idx - 1 };
            }
            *y = acc;
            self.head = (self.head + 1) % n;
        }
    }

    /// Forget the history (a fresh start).
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.head = 0;
    }
}

/// Modified Bessel function of the first kind, order 0 (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..200 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-15 {
            break;
        }
    }
    sum
}

// ---------------------------------------------------------------------------
// Tap decoder

/// Settings of a [`TapDecoder`].
pub struct TapConfig {
    /// PHY profile name, user-facing or wire-format (see
    /// `PhyConfig::from_name`). An unknown name is an error. Decoded MIDI
    /// goes to the worker's MIDI output ([`PassthroughConfig::midi_out`]).
    pub profile: String,
    /// Device ID and command format of the MSC SysEx sent to MIDI out.
    pub msc: MscTarget,
    /// Override the profile's link-dead timeout, ms.
    pub link_dead_ms: Option<u32>,
    /// Link key (spec §4.1), the same as the transmitter's.
    pub link_key: u16,
    /// Send the fail-safe burst on link-dead, input stall and stop.
    pub fail_safe_burst: bool,
    /// Remove the data band from the output ([`StripFilter::for_profile`]):
    /// the box's "stripped" output for a chain that must not carry the
    /// tones on. Adds the filter's group delay to the output. Ignored (with
    /// a log line) for a profile without a receiver band.
    pub strip: bool,
}

impl TapConfig {
    /// A tap for `profile` with the defaults of the decoder worker (all-call
    /// MSC, unkeyed link, fail-safe on, no strip).
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            msc: MscTarget::default(),
            link_dead_ms: None,
            link_key: cricket_core::link::DEFAULT_LINK_KEY,
            fail_safe_burst: true,
            strip: false,
        }
    }
}

/// Passes the audio through unchanged (or through the strip filter) while a
/// copy is decoded by a `CricketModemRx`, emitting what the decoder worker
/// emits (`Level`, `Decoded`, `Stats`, `LinkDead` / `MidiOutputError`
/// alarms) and sending decoded MIDI to the MIDI output.
pub struct TapDecoder {
    cfg: Option<TapConfig>,
    config: PhyConfig,
    rx: Option<CricketModemRx>,
    strip: Option<StripFilter>,
    link_alive: bool,
    dead_since: Option<Instant>,
    sync_lost: u64,
    last_stats_emit: Instant,
    rms_accumulator: f32,
    meter_count: usize,
    meter_interval: usize,
    est_latency_ms: f32,
}

impl TapDecoder {
    /// A tap decoder for `cfg`; an unknown profile name is an error here.
    pub fn new(cfg: TapConfig) -> Result<Self> {
        let mut config = PhyConfig::from_name(&cfg.profile)
            .ok_or_else(|| {
                anyhow!(
                    "unknown profile {:?} (run `cricket profiles` for the list)",
                    cfg.profile
                )
            })?
            .with_link_key(cfg.link_key);
        if let Some(ms) = cfg.link_dead_ms {
            config.link_dead_ms = ms;
        }
        Ok(Self {
            est_latency_ms: config.estimated_first_copy_ms(),
            cfg: Some(cfg),
            config,
            rx: None,
            strip: None,
            link_alive: false,
            dead_since: None,
            sync_lost: 0,
            // The first `Stats` goes out with the first block.
            last_stats_emit: Instant::now() - STATS_INTERVAL,
            rms_accumulator: 0.0,
            meter_count: 0,
            meter_interval: 2400,
        })
    }

    /// The profile in use (at the profile rate until `start`, then at the
    /// stream rate).
    pub fn config(&self) -> &PhyConfig {
        &self.config
    }

    /// The strip filter, once started with `strip` on.
    pub fn strip_filter(&self) -> Option<&StripFilter> {
        self.strip.as_ref()
    }

    fn emit_stats(&mut self, ctx: &ProcessContext<'_>, rx_stats: &cricket_core::ModemStats) {
        ctx.emit(EngineEvent::Stats(link_stats(
            rx_stats,
            self.sync_lost,
            self.est_latency_ms,
            self.link_alive,
            0,
        )));
        self.last_stats_emit = Instant::now();
    }
}

impl AudioProcessor for TapDecoder {
    fn start(&mut self, ctx: &mut ProcessContext<'_>) -> Result<()> {
        let cfg = self.cfg.take().ok_or_else(|| anyhow!("started twice"))?;
        let name = PhyConfig::canonical_name(&cfg.profile).unwrap_or_else(|| cfg.profile.clone());
        ctx.log(format!(
            "Tap decoder (profile={}): link key {}; link-dead after {} ms; fail-safe burst {}.",
            name,
            link_key_label(self.config.link_key),
            self.config.link_dead_ms,
            if cfg.fail_safe_burst { "on" } else { "off" }
        ));
        if ctx.sample_rate_hz != self.config.sample_rate_hz {
            ctx.log(format!(
                "Sample rate adapted {} → {} Hz",
                self.config.sample_rate_hz, ctx.sample_rate_hz
            ));
            self.config = self.config.clone().with_sample_rate(ctx.sample_rate_hz);
        }
        if let Some(w) = crate::hf_rate_warning(&self.config, ctx.sample_rate_hz) {
            ctx.log(w);
        }
        if !ctx.midi_connected() {
            ctx.log("No MIDI out — decode to events only.");
        }
        if cfg.strip {
            match StripFilter::for_profile(&self.config, ctx.sample_rate_hz) {
                Some(f) => {
                    ctx.log(format!(
                        "Strip filter on: {}–{} Hz removed from the output ({} taps, {:.2} ms group delay).",
                        self.config.rx_band_hz().map_or(0.0, |b| b.0),
                        self.config.rx_band_hz().map_or(0.0, |b| b.1),
                        f.len(),
                        f.group_delay_samples() as f32 * 1000.0 / ctx.sample_rate_hz as f32
                    ));
                    self.strip = Some(f);
                }
                None => ctx.log(
                    "Strip requested but the profile has no receiver band (it uses the whole audio band): output is unchanged.",
                ),
            }
        }
        let mut rx = CricketModemRx::new(self.config.clone());
        rx.set_msc_target(cfg.msc);
        rx.set_fail_safe_burst(cfg.fail_safe_burst);
        self.rx = Some(rx);
        self.meter_interval = (ctx.sample_rate_hz as usize / 20).max(1);
        Ok(())
    }

    fn process(&mut self, input: &[f32], output: &mut [f32], ctx: &mut ProcessContext<'_>) {
        // The audio path first: it is what the latency budget is about.
        match self.strip.as_mut() {
            Some(f) => f.process(input, output),
            None => output.copy_from_slice(input),
        }
        for &sample in input {
            self.rms_accumulator += sample * sample;
            self.meter_count += 1;
            if self.meter_count >= self.meter_interval {
                let rms = (self.rms_accumulator / self.meter_count as f32).sqrt();
                ctx.emit(EngineEvent::Level(rms));
                self.rms_accumulator = 0.0;
                self.meter_count = 0;
            }
        }
        let Some(rx) = self.rx.as_mut() else {
            return;
        };
        let link_dead_ms = self.config.link_dead_ms;
        let mut stats_due = None;
        for out in rx.push_samples(input) {
            match out {
                ModemOutput::Message(msg) => {
                    self.link_alive = true;
                    if let Some(since) = self.dead_since.take() {
                        let message = format!(
                            "Link restored after {:.1} s (valid packet received)",
                            since.elapsed().as_secs_f32()
                        );
                        ctx.log(message.clone());
                        ctx.emit(EngineEvent::AlarmCleared {
                            kind: AlarmKind::LinkDead,
                            message,
                        });
                    }
                    ctx.emit(EngineEvent::Decoded(msg));
                }
                ModemOutput::Midi(bytes) => ctx.send_midi(&bytes),
                ModemOutput::LinkDead => {
                    self.link_alive = false;
                    self.dead_since = Some(Instant::now());
                    let message = format!(
                        "Link dead: no valid packet for {} ms — {}",
                        link_dead_ms,
                        burst_text(rx)
                    );
                    ctx.emit(EngineEvent::Alarm {
                        kind: AlarmKind::LinkDead,
                        message,
                    });
                    ctx.log(if rx.fail_safe_burst_enabled() {
                        "Link dead — All Notes Off"
                    } else {
                        "Link dead (fail-safe burst off)"
                    });
                }
                ModemOutput::SyncLost => self.sync_lost = self.sync_lost.saturating_add(1),
                ModemOutput::Stats(stats) if self.last_stats_emit.elapsed() >= STATS_INTERVAL => {
                    stats_due = Some(stats);
                }
                _ => {}
            }
        }
        if let Some(stats) = stats_due {
            self.emit_stats(ctx, &stats);
        }
    }

    fn input_stalled(&mut self, ctx: &mut ProcessContext<'_>) {
        let Some(rx) = self.rx.as_mut() else {
            return;
        };
        if rx.declare_link_dead() {
            let burst = rx.fail_safe_burst();
            ctx.send_midi_all(&burst);
            self.link_alive = false;
            ctx.log(format!("Audio input stalled — {}", burst_text(rx)));
            let stats = rx.stats();
            self.emit_stats(ctx, &stats);
        }
    }

    fn stop(&mut self, ctx: &mut ProcessContext<'_>) {
        if let Some(rx) = self.rx.as_mut() {
            if ctx.midi_connected() && rx.fail_safe_burst_enabled() {
                let burst = rx.fail_safe_burst();
                ctx.send_midi_all(&burst);
                ctx.log(format!("Stop — {}", burst_text(rx)));
            }
        }
    }

    fn latency_samples(&self) -> usize {
        self.strip.as_ref().map_or(0, |f| f.group_delay_samples())
    }

    fn name(&self) -> &str {
        "tap decoder"
    }
}

// ---------------------------------------------------------------------------
// Worker

/// Pass-through settings.
pub struct PassthroughConfig {
    pub audio_in: AudioIn,
    pub audio_out: AudioOut,
    /// Preferred stream sample rate (the devices may open at another; the
    /// processor is told the actual one). 48 000 for the Cricket profiles.
    pub sample_rate_hz: u32,
    /// Device buffer size, sample format and channels (cpal devices only).
    pub audio: AudioOptions,
    /// Most audio allowed to wait in the output ring before the oldest is
    /// dropped (bounds the latency when the output falls behind the input).
    /// At least two [`MAX_BLOCK`]s are always allowed.
    pub max_ahead_ms: u32,
    /// Input silence (no samples delivered) that raises
    /// `AlarmKind::AudioInputStalled` and tells the processor.
    pub stall_ms: u32,
    /// MIDI output for the processor ([`ProcessContext::send_midi`]), opened
    /// in the worker thread. `MidiOut::None` by default.
    pub midi_out: MidiOut,
}

impl PassthroughConfig {
    /// Defaults: 48 kHz, host device buffers, 40 ms of output headroom,
    /// 2 s stall watchdog.
    pub fn new(audio_in: AudioIn, audio_out: AudioOut) -> Self {
        Self {
            audio_in,
            audio_out,
            sample_rate_hz: 48_000,
            audio: AudioOptions::default(),
            max_ahead_ms: 40,
            stall_ms: 2000,
            midi_out: MidiOut::None,
        }
    }
}

/// A running pass-through worker. Dropping it stops the worker (which
/// gives the processor its `stop` first).
pub struct PassthroughHandle {
    worker: Worker,
    events: Receiver<EngineEvent>,
}

impl PassthroughHandle {
    /// Spawn the worker with `processor`. Device errors do not fail this
    /// call; they are reported as `Log` events followed by `Stopped`.
    pub fn start(config: PassthroughConfig, processor: Box<dyn AudioProcessor>) -> Result<Self> {
        let (worker, events) = Worker::spawn("cricket-passthrough", move |ev, stop| {
            run(config, processor, ev, stop)
        })?;
        Ok(Self { worker, events })
    }

    /// The worker's event channel.
    pub fn events(&self) -> &Receiver<EngineEvent> {
        &self.events
    }

    /// Take ownership of the event channel. Afterwards
    /// [`events`](Self::events) returns a closed channel.
    pub fn take_events(&mut self) -> Receiver<EngineEvent> {
        std::mem::replace(&mut self.events, closed_receiver())
    }

    /// True until the worker thread has finished.
    pub fn is_running(&self) -> bool {
        self.worker.is_running()
    }

    /// Stop and join the worker (waits at most [`crate::STOP_TIMEOUT`]).
    pub fn stop(&mut self) -> bool {
        self.worker.stop()
    }
}

impl Drop for PassthroughHandle {
    fn drop(&mut self) {
        self.worker.stop();
    }
}

fn run(
    cfg: PassthroughConfig,
    mut processor: Box<dyn AudioProcessor>,
    ev: &Emitter,
    stop: &AtomicBool,
) {
    ev.log(format!("Starting pass-through ({})...", processor.name()));
    let rate = cfg.sample_rate_hz.max(1);
    // Input ring: a second of audio (the worker drains it continuously).
    let mut source = match cfg.audio_in.open(rate, rate as usize, &cfg.audio) {
        Ok(s) => s,
        Err(e) => {
            ev.log(format!("Audio input error: {}", e));
            return;
        }
    };
    let stream_rate = source.sample_rate();
    let max_ahead = (stream_rate as usize * cfg.max_ahead_ms as usize / 1000).max(2 * MAX_BLOCK);
    let mut sink = match cfg.audio_out.open(stream_rate, max_ahead, &cfg.audio) {
        Ok(s) => s,
        Err(e) => {
            ev.log(format!("Audio output error: {}", e));
            return;
        }
    };
    if sink.sample_rate() != stream_rate {
        ev.log(format!(
            "Audio output opened at {} Hz while the input runs at {} Hz: the devices must share a clock and a rate.",
            sink.sample_rate(),
            stream_rate
        ));
        return;
    }
    for (what, details, ch, n) in [
        ("in", source.describe(), source.channel(), source.channels()),
        ("out", sink.describe(), sink.channel(), sink.channels()),
    ] {
        ev.log(format!(
            "Audio {} {} Hz, ch {} of {}{}.",
            what,
            stream_rate,
            ch + 1,
            n,
            if details.is_empty() {
                String::new()
            } else {
                format!(", {}", details)
            }
        ));
    }

    let mut midi = MidiOutState {
        sink: match cfg.midi_out {
            MidiOut::None => None,
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
    let mut ctx = ProcessContext {
        sample_rate_hz: stream_rate,
        position: 0,
        emitter: ev,
        midi: &mut midi,
    };
    if let Err(e) = processor.start(&mut ctx) {
        ev.log(format!("Processor failed to start: {}", e));
        return;
    }
    if processor.latency_samples() > 0 {
        ev.log(format!(
            "Processor delay {} samples ({:.2} ms).",
            processor.latency_samples(),
            processor.latency_samples() as f32 * 1000.0 / stream_rate as f32
        ));
    }
    if let Err(e) = sink.start().and_then(|_| source.start()) {
        ev.log(format!("Failed to start the streams: {}", e));
        processor.stop(&mut ctx);
        return;
    }

    let stall = Duration::from_millis(cfg.stall_ms as u64);
    let mut input = vec![0.0f32; MAX_BLOCK];
    let mut output = vec![0.0f32; MAX_BLOCK];
    // Audio processed but not yet accepted by the output ring (it was full).
    let mut pending: VecDeque<f32> = VecDeque::new();
    let mut dropped: u64 = 0;
    let mut reported_drops: u64 = 0;
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
        while let Some(err) = sink.take_error() {
            ev.send(EngineEvent::Alarm {
                kind: AlarmKind::AudioDeviceError,
                message: format!("Audio output device error: {}", err),
            });
            ev.log(format!("Audio output device error: {}", err));
        }

        let n = source.pull(&mut input);
        if n == 0 {
            if !stalled && last_sample_at.elapsed() >= stall {
                stalled = true;
                let message = format!(
                    "Audio input stalled ({} ms without samples)",
                    last_sample_at.elapsed().as_millis()
                );
                ev.send(EngineEvent::Alarm {
                    kind: AlarmKind::AudioInputStalled,
                    message: message.clone(),
                });
                ev.log(message);
                processor.input_stalled(&mut ctx);
            }
            source.wait(IDLE_WAIT);
            continue;
        }
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

        processor.process(&input[..n], &mut output[..n], &mut ctx);
        ctx.position += n as u64;

        // Output: what did not fit last time first, then this block. If the
        // ring is still full the oldest waiting audio is dropped so the
        // delay stays bounded.
        pending.extend(&output[..n]);
        let (a, b) = pending.as_slices();
        let mut pushed = sink.push(a);
        if pushed == a.len() {
            pushed += sink.push(b);
        }
        pending.drain(..pushed);
        if pending.len() > max_ahead {
            let excess = pending.len() - max_ahead;
            pending.drain(..excess);
            dropped += excess as u64;
        }
        if dropped - reported_drops >= 1000 {
            reported_drops = dropped;
            ev.log(format!(
                "Audio output behind the input: {} samples dropped so far",
                dropped
            ));
        }
    }

    processor.stop(&mut ctx);
    ev.log(format!(
        "Pass-through stopped ({} samples through, {} dropped).",
        ctx.position, dropped
    ));
}

// ---------------------------------------------------------------------------
// Measurement

/// What [`measure_passthrough_latency`] found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PassthroughLatency {
    /// Samples between a sample entering and the same sample leaving, as
    /// seen on the streams (0 = sample-exact; a filter adds its group
    /// delay).
    pub added_samples: i64,
    /// Wall-clock time from the push of a block into the input until the
    /// whole block is available at the output, per block: mean and worst.
    pub turnaround_mean: Duration,
    pub turnaround_max: Duration,
    /// Every output sample equalled its input sample.
    pub bit_exact: bool,
    /// The block size used.
    pub block: usize,
}

/// Run `blocks` blocks of `block` pseudo-random samples through a pass-through
/// worker with [`Passthrough`] on in-memory endpoints and measure what the
/// worker adds: the sample delay (expected 0) and the wall-clock turnaround
/// per block (expected microseconds, far under one device buffer).
pub fn measure_passthrough_latency(block: usize, blocks: usize) -> Result<PassthroughLatency> {
    use crate::memory::MemoryAudio;
    const RATE: u32 = 48_000;
    let block = block.clamp(1, MAX_BLOCK);
    let (in_sink, in_source) = MemoryAudio::pair(RATE, RATE as usize);
    let (out_sink, mut out_source) = MemoryAudio::pair(RATE, RATE as usize);
    let mut in_sink = in_sink;
    let mut handle = PassthroughHandle::start(
        PassthroughConfig::new(
            AudioIn::Custom(Box::new(in_source)),
            AudioOut::Custom(Box::new(out_sink)),
        ),
        Box::new(Passthrough),
    )?;
    out_source.start()?;
    let events = handle.take_events();
    // Wait until the worker has opened its streams (its first log lines).
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(EngineEvent::Log(l)) if l.starts_with("Audio out") => break,
            Ok(EngineEvent::Stopped) => return Err(anyhow!("pass-through stopped early")),
            _ => {}
        }
    }
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let mut turnarounds = Vec::with_capacity(blocks);
    let mut bit_exact = true;
    let mut added: Option<i64> = None;
    let mut got = vec![0.0f32; block];
    let mut total_in = 0u64;
    for _ in 0..blocks {
        let sent: Vec<f32> = (0..block).map(|_| next()).collect();
        let t0 = Instant::now();
        let mut pushed = 0;
        while pushed < block {
            pushed += in_sink.push(&sent[pushed..]);
        }
        let mut have = 0;
        let deadline = t0 + Duration::from_secs(2);
        while have < block {
            let n = out_source.pull(&mut got[have..]);
            have += n;
            if have < block {
                if Instant::now() > deadline {
                    return Err(anyhow!("output did not arrive"));
                }
                out_source.wait(Duration::from_millis(1));
            }
        }
        turnarounds.push(t0.elapsed());
        if got != sent {
            bit_exact = false;
            // Locate the delay by the first sample, if it is a pure delay.
            if added.is_none() {
                added = got.iter().position(|&g| g == sent[0]).map(|p| p as i64);
            }
        }
        total_in += block as u64;
    }
    let _ = total_in;
    let mean = turnarounds.iter().sum::<Duration>() / turnarounds.len().max(1) as u32;
    let max = turnarounds.iter().copied().max().unwrap_or_default();
    handle.stop();
    Ok(PassthroughLatency {
        added_samples: if bit_exact { 0 } else { added.unwrap_or(-1) },
        turnaround_mean: mean,
        turnaround_max: max,
        bit_exact,
        block,
    })
}
