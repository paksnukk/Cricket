//! Audio I/O: the [`AudioSource`] / [`AudioSink`] traits the workers use, and
//! their cpal implementations (the single copy of the stream setup formerly
//! duplicated in the GUI and CLI).
//!
//! Workers see mono `f32` samples only. The cpal source takes one channel of
//! the input ([`AudioOptions::input_channel`]); the cpal sink writes the mono
//! signal to one output channel ([`AudioOptions::output_channel`]) and
//! silence to the others, so a Cricket bus never leaks into a neighbouring
//! channel of a multichannel interface.
//! Integer device formats are converted with power-of-two scaling and
//! rounding (2¹⁵ for 16-bit, 2³¹ for 32-bit), so a 24-bit sample value
//! survives the trip through `f32` exactly in both directions: the
//! bit-transparent `digital` profile depends on it.
//!
//! [`AudioOptions`] picks the device buffer size (`BufferSize::Fixed`; the
//! host default is often long: ALSA uses a 100 ms buffer) and the sample
//! format.
//!
//! `cpal::Stream` is `!Send`, so endpoints are described by a `Send` *opener*
//! ([`AudioIn`] / [`AudioOut`]) and opened inside the worker thread, which
//! then owns the stream for its whole life. The cpal input callback wakes
//! that thread ([`AudioSource::wait`]) as soon as it has queued samples.

use crate::devices::DeviceSelector;
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cricket_core::phy::PhyConfig;
use ringbuf::{HeapConsumer, HeapProducer, HeapRb};
use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::Duration;

/// Most errors a [`DeviceErrorQueue`] holds; further ones are dropped (and
/// counted) until the worker drains it.
const MAX_QUEUED_ERRORS: usize = 16;

/// Errors reported by an audio stream outside the worker thread (cpal's error
/// callback), queued until the worker polls them with `take_error`.
///
/// Pushing locks a mutex, so call it only from the stream's *error* callback
/// (or other non-real-time code), never from the audio data callback.
/// Clones share the same queue.
#[derive(Clone, Default)]
pub struct DeviceErrorQueue {
    inner: Arc<Mutex<(VecDeque<String>, u64)>>,
}

impl DeviceErrorQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an error message.
    pub fn push(&self, message: impl Into<String>) {
        if let Ok(mut q) = self.inner.lock() {
            if q.0.len() < MAX_QUEUED_ERRORS {
                q.0.push_back(message.into());
            } else {
                q.1 += 1;
            }
        }
    }

    /// Oldest unreported error. Errors dropped because the queue was full are
    /// reported as one summary message after the queued ones.
    pub fn pop(&self) -> Option<String> {
        let mut q = self.inner.lock().ok()?;
        if let Some(m) = q.0.pop_front() {
            return Some(m);
        }
        if q.1 > 0 {
            let n = std::mem::take(&mut q.1);
            return Some(format!("{} further audio device errors not shown", n));
        }
        None
    }
}

/// Sample format to open a cpal device in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SampleFormatPref {
    /// The device default, except for the `digital` profile, which prefers
    /// a 32-bit integer stream, then `f32`, then 16-bit integer (fewer
    /// float conversions between Cricket and the hardware).
    #[default]
    Auto,
    F32,
    I16,
    I32,
}

impl SampleFormatPref {
    fn formats(self, digital: bool) -> Vec<cpal::SampleFormat> {
        use cpal::SampleFormat as S;
        match self {
            Self::Auto if digital => vec![S::I32, S::F32, S::I16],
            Self::Auto => vec![],
            Self::F32 => vec![S::F32],
            Self::I16 => vec![S::I16],
            Self::I32 => vec![S::I32],
        }
    }
}

impl FromStr for SampleFormatPref {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "f32" => Ok(Self::F32),
            "i16" => Ok(Self::I16),
            "i32" => Ok(Self::I32),
            _ => Err(format!("{s}: expected auto, f32, i16 or i32")),
        }
    }
}

impl fmt::Display for SampleFormatPref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::F32 => "f32",
            Self::I16 => "i16",
            Self::I32 => "i32",
        })
    }
}

/// How cpal devices are opened (ignored by custom endpoints).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioOptions {
    /// Device buffer size in frames (`None`: host default). Clamped to the
    /// range the device reports. Smaller buffers mean lower latency and
    /// more risk of dropouts; on ALSA the buffer is split into 4 periods.
    pub buffer_frames: Option<u32>,
    pub sample_format: SampleFormatPref,
    /// Set by the workers: the profile is `digital` (for
    /// [`SampleFormatPref::Auto`]).
    pub digital: bool,
    /// Input channel the decoder listens on, 0-based (the UI shows it
    /// 1-based). Opening fails if the device has fewer channels.
    pub input_channel: usize,
    /// Output channel the encoder sends on, 0-based; the other channels
    /// get silence. Opening fails if the device has fewer channels.
    pub output_channel: usize,
}

/// A live mono capture stream.
pub trait AudioSource {
    /// Actual stream sample rate (may differ from the preferred one).
    fn sample_rate(&self) -> u32;
    /// Channel count of the underlying device.
    fn channels(&self) -> usize;
    /// The device channel delivered (0-based).
    fn channel(&self) -> usize {
        0
    }
    /// Start delivering samples.
    fn start(&mut self) -> Result<()>;
    /// Non-blocking: copy up to `out.len()` buffered samples into `out` and
    /// return how many were written (0 when nothing is buffered).
    fn pull(&mut self, out: &mut [f32]) -> usize;
    /// Block until samples may be available, at most `timeout`. Sources that
    /// can signal new samples return as soon as they arrive; the default
    /// just sleeps.
    fn wait(&mut self, timeout: Duration) {
        thread::sleep(timeout);
    }
    /// Samples lost because the capture buffer was full.
    fn dropped(&self) -> u64 {
        0
    }
    /// Next error the device reported since the last call (e.g. the stream
    /// was invalidated because the device was unplugged). Polled by the
    /// decoder worker and reported as `AlarmKind::AudioDeviceError`.
    fn take_error(&mut self) -> Option<String> {
        None
    }
    /// Stream details for the log (format, buffer), if known.
    fn describe(&self) -> String {
        String::new()
    }
}

/// A live mono playback stream.
pub trait AudioSink {
    /// Actual stream sample rate (may differ from the preferred one).
    fn sample_rate(&self) -> u32;
    /// Channel count of the underlying device.
    fn channels(&self) -> usize;
    /// The device channel the signal goes to (0-based).
    fn channel(&self) -> usize {
        0
    }
    /// Start playback.
    fn start(&mut self) -> Result<()>;
    /// Non-blocking: queue as many of `samples` as fit and return that count.
    fn push(&mut self, samples: &[f32]) -> usize;
    /// True when no further sample can be queued right now.
    fn is_full(&self) -> bool;
    /// Samples queued but not yet played.
    fn buffered(&self) -> usize;
    /// Next error the device reported since the last call. Polled by the
    /// encoder worker and reported as `AlarmKind::AudioDeviceError`.
    fn take_error(&mut self) -> Option<String> {
        None
    }
    /// Stream details for the log (format, buffer), if known.
    fn describe(&self) -> String {
        String::new()
    }
}

/// Opens an [`AudioSource`] inside the worker thread.
pub trait OpenAudioSource: Send {
    /// `preferred_rate` is the profile rate; `buffer_samples` the size of the
    /// capture buffer between the device callback and the worker.
    fn open(
        self: Box<Self>,
        preferred_rate: u32,
        buffer_samples: usize,
    ) -> Result<Box<dyn AudioSource>>;
}

/// Opens an [`AudioSink`] inside the worker thread.
pub trait OpenAudioSink: Send {
    /// `preferred_rate` is the profile rate; `buffer_samples` the size of the
    /// playback buffer between the worker and the device callback.
    fn open(
        self: Box<Self>,
        preferred_rate: u32,
        buffer_samples: usize,
    ) -> Result<Box<dyn AudioSink>>;
}

/// Where the decoder reads audio from.
pub enum AudioIn {
    /// A cpal input device.
    Device(DeviceSelector),
    /// Any other source (e.g. in-memory for tests).
    Custom(Box<dyn OpenAudioSource>),
}

impl AudioIn {
    /// Open inside the thread that will use the source (cpal streams are
    /// `!Send`; the cpal source wakes this thread when samples arrive).
    pub fn open(
        self,
        preferred_rate: u32,
        buffer_samples: usize,
        options: &AudioOptions,
    ) -> Result<Box<dyn AudioSource>> {
        match self {
            AudioIn::Device(sel) => Ok(Box::new(CpalSource::open(
                &sel,
                preferred_rate,
                buffer_samples,
                options,
            )?)),
            AudioIn::Custom(opener) => opener.open(preferred_rate, buffer_samples),
        }
    }
}

/// Where the encoder writes audio to.
pub enum AudioOut {
    /// A cpal output device.
    Device(DeviceSelector),
    /// Any other sink (e.g. in-memory for tests).
    Custom(Box<dyn OpenAudioSink>),
}

impl AudioOut {
    /// Open inside the thread that will use the sink (cpal streams are
    /// `!Send`).
    pub fn open(
        self,
        preferred_rate: u32,
        buffer_samples: usize,
        options: &AudioOptions,
    ) -> Result<Box<dyn AudioSink>> {
        match self {
            AudioOut::Device(sel) => Ok(Box::new(CpalSink::open(
                &sel,
                preferred_rate,
                buffer_samples,
                options,
            )?)),
            AudioOut::Custom(opener) => opener.open(preferred_rate, buffer_samples),
        }
    }
}

/// Name of the default cpal host (e.g. "ALSA").
pub fn host_name() -> &'static str {
    cpal::default_host().id().name()
}

/// Name of the host's default input (`input`) or output device, if any.
pub fn default_device_name(input: bool) -> Option<String> {
    let host = cpal::default_host();
    let device = if input {
        host.default_input_device()?
    } else {
        host.default_output_device()?
    };
    Some(device_name(&device))
}

/// Channel count of the selected input (`input`) or output device in its
/// default configuration (what the channel selectors offer).
pub fn channel_count(selector: &DeviceSelector, input: bool) -> Result<usize> {
    let device = find_device(selector, input)?;
    let config = if input {
        device.default_input_config()?
    } else {
        device.default_output_config()?
    };
    Ok(usize::from(config.channels()))
}

/// Resolve `selector` to the device's name (the default device's name for
/// [`DeviceSelector::Default`]), for warnings and logs.
pub fn resolve_device_name(selector: &DeviceSelector, input: bool) -> Result<String> {
    Ok(device_name(&find_device(selector, input)?))
}

/// Does this device name look like a laptop's built-in microphone (`input`)
/// or speaker? Cricket needs a line in/out: a microphone hears the room,
/// not the bus, and tones from a speaker go into the room instead of the
/// bus. Returns the warning to show, or `None`.
pub fn builtin_device_warning(name: &str, input: bool) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    const BUILTIN: &[&str] = &[
        "built-in",
        "builtin",
        "internal",
        "macbook",
        "imac",
        "mac mini",
        "laptop",
        "realtek",
        "hda intel",
        "conexant",
        "cirrus",
        "sof-hda",
        "webcam",
        "camera",
    ];
    let generic = BUILTIN.iter().any(|w| lower.contains(w));
    if input {
        let mic = lower.contains("microphone")
            || lower.contains("mic ")
            || lower.ends_with(" mic")
            || lower.contains(" mic)")
            || lower.contains("(mic");
        (generic || mic).then(|| {
            format!(
                "Audio in \"{}\" looks like a built-in microphone. Cricket needs a line input: \
                 a microphone hears the room, not the Cricket bus.",
                name
            )
        })
    } else {
        let speaker = lower.contains("speaker")
            || lower.contains("headphone")
            || lower.contains("hdmi")
            || lower.contains("displayport");
        (generic || speaker).then(|| {
            format!(
                "Audio out \"{}\" looks like a built-in speaker. Cricket needs a line output: \
                 the tones would play into the room instead of the Cricket bus.",
                name
            )
        })
    }
}

/// Setup problems worth a warning before going on air (none is an error):
/// built-in microphones / speakers (see [`builtin_device_warning`]) and, on
/// Windows, the bit-exact and 14400 bit/s profiles through WASAPI shared
/// mode (the Windows mixer processes the audio; see the README's digital
/// section). `input_device` / `output_device` are the resolved device names
/// of the devices in use.
pub fn setup_warnings(
    config: &PhyConfig,
    input_device: Option<&str>,
    output_device: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(w) = input_device.and_then(|n| builtin_device_warning(n, true)) {
        out.push(w);
    }
    if let Some(w) = output_device.and_then(|n| builtin_device_warning(n, false)) {
        out.push(w);
    }
    let needs_clean_digital_path =
        config.needs_bit_exact_path() || config.bmc().is_some_and(|p| p.bit_rate >= 14_400);
    if cfg!(target_os = "windows") && needs_clean_digital_path {
        out.push(
            "On Windows, Cricket uses WASAPI shared mode: the Windows mixer applies volume, \
             enhancements and format conversion. digital-bit-exact needs an unchanged path and \
             digital-line / ltc14400 need 16 kHz of bandwidth: run `cricket pathcheck`, set the \
             device to 24-bit / 48 kHz with enhancements off and volume at 100 %, or use \
             clean-line (or macOS / Linux) instead."
                .to_string(),
        );
    }
    out
}

/// The selected device (the host default for [`DeviceSelector::Default`]).
fn find_device(selector: &DeviceSelector, input: bool) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let (mut devices, what): (Vec<cpal::Device>, &str) = if input {
        (host.input_devices()?.collect(), "Input device")
    } else {
        (host.output_devices()?.collect(), "Output device")
    };
    let names: Vec<String> = devices.iter().map(device_name).collect();
    Ok(match selector.resolve(&names, what)? {
        Some(index) => devices.swap_remove(index),
        None if input => host
            .default_input_device()
            .ok_or_else(|| anyhow!("No default input device available"))?,
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow!("No default output device available"))?,
    })
}

fn device_name(device: &cpal::Device) -> String {
    device
        .name()
        .unwrap_or_else(|_| "Unknown Device".to_string())
}

/// Input device names in enumeration order (index = [`DeviceSelector::Index`];
/// each name is valid for [`DeviceSelector::Name`]).
pub fn list_input_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    Ok(host.input_devices()?.map(|d| device_name(&d)).collect())
}

/// Output device names in enumeration order (index = [`DeviceSelector::Index`];
/// each name is valid for [`DeviceSelector::Name`]).
pub fn list_output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    Ok(host.output_devices()?.map(|d| device_name(&d)).collect())
}

/// Pick a stream config: with the default channel count, the first of
/// `formats` (empty: the default format) that a supported range offers at
/// `preferred_rate`, else at the default rate; otherwise the device default.
/// `supported` is only called when something other than the default is
/// wanted.
fn select_config<I, F>(
    default_config: cpal::SupportedStreamConfig,
    preferred_rate: Option<u32>,
    formats: &[cpal::SampleFormat],
    supported: F,
) -> cpal::SupportedStreamConfig
where
    I: Iterator<Item = cpal::SupportedStreamConfigRange>,
    F: FnOnce() -> Option<I>,
{
    let default_format = default_config.sample_format();
    let default_channels = default_config.channels();
    let default_rate = default_config.sample_rate().0;
    let formats: Vec<cpal::SampleFormat> = if formats.is_empty() {
        vec![default_format]
    } else {
        formats.to_vec()
    };
    let rate = preferred_rate.unwrap_or(default_rate);
    if rate == default_rate && formats[0] == default_format {
        return default_config;
    }
    let ranges: Vec<cpal::SupportedStreamConfigRange> = supported()
        .into_iter()
        .flatten()
        .filter(|r| r.channels() == default_channels)
        .collect();
    for want_rate in [rate, default_rate] {
        for &format in &formats {
            for range in &ranges {
                if range.sample_format() == format
                    && range.min_sample_rate().0 <= want_rate
                    && want_rate <= range.max_sample_rate().0
                {
                    return (*range).with_sample_rate(cpal::SampleRate(want_rate));
                }
            }
        }
    }
    default_config
}

/// `options.buffer_frames` clamped to what the device reports.
fn buffer_size(config: &cpal::SupportedStreamConfig, options: &AudioOptions) -> cpal::BufferSize {
    match options.buffer_frames {
        None => cpal::BufferSize::Default,
        Some(n) => match *config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                cpal::BufferSize::Fixed(n.clamp(min, max))
            }
            cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(n),
        },
    }
}

fn describe_stream(format: cpal::SampleFormat, buffer: &cpal::BufferSize) -> String {
    match buffer {
        cpal::BufferSize::Default => format!("{format}, default buffer"),
        cpal::BufferSize::Fixed(n) => format!("{format}, buffer {n} frames"),
    }
}

/// cpal error callback: queue the error for the worker (this callback is not
/// the real-time data callback, so locking is fine) and keep the stderr line.
fn error_callback(
    errors: &DeviceErrorQueue,
    direction: &'static str,
) -> impl FnMut(cpal::StreamError) + Send + 'static {
    let errors = errors.clone();
    move |err| {
        eprintln!("an error occurred on {} stream: {}", direction, err);
        errors.push(format!("{} stream: {}", direction, err));
    }
}

/// A device sample type and its exact conversion to and from `f32`
/// (power-of-two scaling, rounding, clamping; no allocation).
trait Pcm: cpal::SizedSample + Send + 'static {
    fn to_f32(self) -> f32;
    fn from_f32(x: f32) -> Self;
}

impl Pcm for f32 {
    fn to_f32(self) -> f32 {
        self
    }
    fn from_f32(x: f32) -> Self {
        x
    }
}

impl Pcm for f64 {
    fn to_f32(self) -> f32 {
        self as f32
    }
    fn from_f32(x: f32) -> Self {
        x as f64
    }
}

impl Pcm for i16 {
    fn to_f32(self) -> f32 {
        self as f32 / 32_768.0
    }
    fn from_f32(x: f32) -> Self {
        (x * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
    }
}

impl Pcm for i32 {
    fn to_f32(self) -> f32 {
        (self as f64 / 2_147_483_648.0) as f32
    }
    fn from_f32(x: f32) -> Self {
        (x as f64 * 2_147_483_648.0)
            .round()
            .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
    }
}

impl Pcm for u16 {
    fn to_f32(self) -> f32 {
        (self as f32 - 32_768.0) / 32_768.0
    }
    fn from_f32(x: f32) -> Self {
        ((x * 32_768.0).round().clamp(-32_768.0, 32_767.0) + 32_768.0) as u16
    }
}

/// The selected device, its stream config (buffer size applied), format and
/// the channel in use.
struct Opened {
    device: cpal::Device,
    config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    channel: usize,
}

fn open_device(
    selector: &DeviceSelector,
    input: bool,
    preferred_rate: u32,
    options: &AudioOptions,
) -> Result<Opened> {
    let device = find_device(selector, input)?;
    let channel = if input {
        options.input_channel
    } else {
        options.output_channel
    };
    let formats = options.sample_format.formats(options.digital);
    let selected = if input {
        let default = device.default_input_config()?;
        select_config(default, Some(preferred_rate), &formats, || {
            device.supported_input_configs().ok()
        })
    } else {
        let default = device.default_output_config()?;
        select_config(default, Some(preferred_rate), &formats, || {
            device.supported_output_configs().ok()
        })
    };
    let format = selected.sample_format();
    let buffer = buffer_size(&selected, options);
    let mut config: cpal::StreamConfig = selected.into();
    config.buffer_size = buffer;
    if channel >= usize::from(config.channels) {
        return Err(anyhow!(
            "{} \"{}\" has {} channel{}; channel {} is not available",
            if input {
                "Input device"
            } else {
                "Output device"
            },
            device_name(&device),
            config.channels,
            if config.channels == 1 { "" } else { "s" },
            channel + 1
        ));
    }
    Ok(Opened {
        device,
        config,
        format,
        channel,
    })
}

fn build_input<T: Pcm>(
    opened: &Opened,
    errors: &DeviceErrorQueue,
    mut producer: HeapProducer<f32>,
    drops: Arc<AtomicU64>,
    waiter: Thread,
) -> Result<cpal::Stream> {
    let channels = (opened.config.channels as usize).max(1);
    let channel = opened.channel.min(channels - 1);
    Ok(opened.device.build_input_stream(
        &opened.config,
        move |data: &[T], _: &_| {
            for frame in data.chunks(channels) {
                if let Some(&s) = frame.get(channel) {
                    if producer.push(s.to_f32()).is_err() {
                        drops.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            // Wake the worker now instead of letting it poll.
            waiter.unpark();
        },
        error_callback(errors, "input"),
        None,
    )?)
}

fn build_output<T: Pcm>(
    opened: &Opened,
    errors: &DeviceErrorQueue,
    mut consumer: HeapConsumer<f32>,
) -> Result<cpal::Stream> {
    let channels = (opened.config.channels as usize).max(1);
    let channel = opened.channel.min(channels - 1);
    let silence = T::from_f32(0.0);
    Ok(opened.device.build_output_stream(
        &opened.config,
        move |data: &mut [T], _: &_| {
            for frame in data.chunks_mut(channels) {
                let s = T::from_f32(consumer.pop().unwrap_or(0.0));
                for (i, o) in frame.iter_mut().enumerate() {
                    *o = if i == channel { s } else { silence };
                }
            }
        },
        error_callback(errors, "output"),
        None,
    )?)
}

/// cpal capture: one channel of the device into a lock-free ring buffer.
pub struct CpalSource {
    stream: cpal::Stream,
    consumer: HeapConsumer<f32>,
    drops: Arc<AtomicU64>,
    errors: DeviceErrorQueue,
    sample_rate: u32,
    channels: usize,
    channel: usize,
    describe: String,
}

impl CpalSource {
    /// Open (but do not start) the selected input device, preferring
    /// `preferred_rate`, with a ring of `buffer_samples` mono samples. The
    /// callback wakes the calling thread (see [`AudioSource::wait`]).
    pub fn open(
        selector: &DeviceSelector,
        preferred_rate: u32,
        buffer_samples: usize,
        options: &AudioOptions,
    ) -> Result<Self> {
        let drops = Arc::new(AtomicU64::new(0));
        let errors = DeviceErrorQueue::new();
        let mut opened = open_device(selector, true, preferred_rate, options)?;
        let attempt = |opened: &Opened| -> Result<(cpal::Stream, HeapConsumer<f32>)> {
            let (producer, consumer) = HeapRb::<f32>::new(buffer_samples).split();
            let (d, w) = (drops.clone(), thread::current());
            let stream = match opened.format {
                cpal::SampleFormat::F32 => build_input::<f32>(opened, &errors, producer, d, w),
                cpal::SampleFormat::F64 => build_input::<f64>(opened, &errors, producer, d, w),
                cpal::SampleFormat::I16 => build_input::<i16>(opened, &errors, producer, d, w),
                cpal::SampleFormat::I32 => build_input::<i32>(opened, &errors, producer, d, w),
                cpal::SampleFormat::U16 => build_input::<u16>(opened, &errors, producer, d, w),
                f => Err(anyhow!("Unsupported sample format: {}", f)),
            }?;
            Ok((stream, consumer))
        };
        let (stream, consumer) = match attempt(&opened) {
            Ok(ok) => ok,
            Err(e) if opened.config.buffer_size != cpal::BufferSize::Default => {
                eprintln!("input buffer size not accepted ({e}); using the default");
                opened.config.buffer_size = cpal::BufferSize::Default;
                attempt(&opened)?
            }
            Err(e) => return Err(e),
        };
        Ok(Self {
            stream,
            consumer,
            drops,
            errors,
            sample_rate: opened.config.sample_rate.0,
            channels: opened.config.channels as usize,
            channel: opened.channel,
            describe: describe_stream(opened.format, &opened.config.buffer_size),
        })
    }
}

impl AudioSource for CpalSource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> usize {
        self.channels
    }
    fn channel(&self) -> usize {
        self.channel
    }
    fn start(&mut self) -> Result<()> {
        self.stream.play()?;
        Ok(())
    }
    fn pull(&mut self, out: &mut [f32]) -> usize {
        self.consumer.pop_slice(out)
    }
    fn wait(&mut self, timeout: Duration) {
        if self.consumer.is_empty() {
            thread::park_timeout(timeout);
        }
    }
    fn dropped(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }
    fn take_error(&mut self) -> Option<String> {
        self.errors.pop()
    }
    fn describe(&self) -> String {
        self.describe.clone()
    }
}

/// cpal playback: mono ring buffer to one device channel (silence on the
/// others and on underrun).
pub struct CpalSink {
    stream: cpal::Stream,
    producer: HeapProducer<f32>,
    errors: DeviceErrorQueue,
    sample_rate: u32,
    channels: usize,
    channel: usize,
    describe: String,
}

impl CpalSink {
    /// Open (but do not start) the selected output device, preferring
    /// `preferred_rate`, with a ring of `buffer_samples` mono samples.
    pub fn open(
        selector: &DeviceSelector,
        preferred_rate: u32,
        buffer_samples: usize,
        options: &AudioOptions,
    ) -> Result<Self> {
        let errors = DeviceErrorQueue::new();
        let mut opened = open_device(selector, false, preferred_rate, options)?;
        let attempt = |opened: &Opened| -> Result<(cpal::Stream, HeapProducer<f32>)> {
            let (producer, consumer) = HeapRb::<f32>::new(buffer_samples).split();
            let stream = match opened.format {
                cpal::SampleFormat::F32 => build_output::<f32>(opened, &errors, consumer),
                cpal::SampleFormat::F64 => build_output::<f64>(opened, &errors, consumer),
                cpal::SampleFormat::I16 => build_output::<i16>(opened, &errors, consumer),
                cpal::SampleFormat::I32 => build_output::<i32>(opened, &errors, consumer),
                cpal::SampleFormat::U16 => build_output::<u16>(opened, &errors, consumer),
                f => Err(anyhow!("Unsupported sample format: {}", f)),
            }?;
            Ok((stream, producer))
        };
        let (stream, producer) = match attempt(&opened) {
            Ok(ok) => ok,
            Err(e) if opened.config.buffer_size != cpal::BufferSize::Default => {
                eprintln!("output buffer size not accepted ({e}); using the default");
                opened.config.buffer_size = cpal::BufferSize::Default;
                attempt(&opened)?
            }
            Err(e) => return Err(e),
        };
        Ok(Self {
            stream,
            producer,
            errors,
            sample_rate: opened.config.sample_rate.0,
            channels: opened.config.channels as usize,
            channel: opened.channel,
            describe: describe_stream(opened.format, &opened.config.buffer_size),
        })
    }
}

impl AudioSink for CpalSink {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> usize {
        self.channels
    }
    fn channel(&self) -> usize {
        self.channel
    }
    fn start(&mut self) -> Result<()> {
        self.stream.play()?;
        Ok(())
    }
    fn push(&mut self, samples: &[f32]) -> usize {
        self.producer.push_slice(samples)
    }
    fn is_full(&self) -> bool {
        self.producer.is_full()
    }
    fn buffered(&self) -> usize {
        self.producer.len()
    }
    fn take_error(&mut self) -> Option<String> {
        self.errors.pop()
    }
    fn describe(&self) -> String {
        self.describe.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every 24-bit value survives f32 -> device integer -> f32 exactly
    /// (spot check over the range), and 16-bit values survive i16.
    #[test]
    fn integer_conversions_are_exact_for_24_bit_values() {
        for code in (-(1i32 << 23)..(1 << 23))
            .step_by(4099)
            .chain([-1, 0, 1, 1024, -11_264])
        {
            let x = code as f32 / 8_388_608.0;
            assert_eq!(i32::from_f32(x).to_f32(), x, "{code}");
            assert_eq!(i32::from_f32(x), code << 8);
            assert_eq!(f64::from_f32(x).to_f32(), x);
        }
        for v in [-32_768i16, -11_264 / 256, -1, 0, 1, 44, 32_767] {
            let x = v.to_f32();
            assert_eq!(i16::from_f32(x), v);
            assert_eq!(u16::from_f32(x).to_f32(), x);
        }
        // Clamped, not wrapped.
        assert_eq!(i16::from_f32(1.0), 32_767);
        assert_eq!(i32::from_f32(-1.5), i32::MIN);
    }

    #[test]
    fn sample_format_names() {
        for (s, f) in [
            ("auto", SampleFormatPref::Auto),
            ("F32", SampleFormatPref::F32),
            ("i16", SampleFormatPref::I16),
            ("i32", SampleFormatPref::I32),
        ] {
            assert_eq!(s.parse::<SampleFormatPref>(), Ok(f));
        }
        assert!("s24".parse::<SampleFormatPref>().is_err());
        assert!(SampleFormatPref::Auto.formats(false).is_empty());
        assert_eq!(
            SampleFormatPref::Auto.formats(true)[0],
            cpal::SampleFormat::I32
        );
    }

    #[test]
    fn builtin_devices_are_recognized() {
        for name in [
            "MacBook Pro Microphone",
            "Built-in Audio Analog Stereo",
            "Microphone Array (Realtek(R) Audio)",
            "HDA Intel PCH: ALC256 Analog (hw:0,0)",
            "Internal Microphone",
        ] {
            assert!(builtin_device_warning(name, true).is_some(), "{name}");
        }
        for name in [
            "MacBook Pro Speakers",
            "Speakers (Realtek(R) Audio)",
            "HDMI / DisplayPort 1 Output",
            "Headphones",
        ] {
            assert!(builtin_device_warning(name, false).is_some(), "{name}");
        }
        for name in [
            "Dante Virtual Soundcard",
            "Scarlett 18i20 USB",
            "RME MADIface",
            "UltraLite-mk5",
            "hw:CARD=USB,DEV=0",
        ] {
            assert!(builtin_device_warning(name, true).is_none(), "{name}");
            assert!(builtin_device_warning(name, false).is_none(), "{name}");
        }
        let warnings = setup_warnings(
            &PhyConfig::robust(),
            Some("MacBook Pro Microphone"),
            Some("Dante Virtual Soundcard"),
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("built-in microphone"));
        let windows_only = setup_warnings(&PhyConfig::digital(), None, None);
        assert_eq!(windows_only.len(), usize::from(cfg!(target_os = "windows")));
    }
}
