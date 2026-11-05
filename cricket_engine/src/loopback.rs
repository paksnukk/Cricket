//! MIDI-in → audio → MIDI-out latency measurement (hardware-free, paced).
//!
//! Built on the in-memory endpoints in [`crate::memory`]: an ideal audio
//! device with no buffer of its own, so the result is the codec's airtime
//! plus the engine's own overhead. Latencies are taken from the time a
//! message is injected to the time the decoder hands it to its MIDI sink.

use crate::memory::{MemoryAudio, MemoryMidiInjector, MemoryMidiInput, RecordingMidiSink};
use crate::{
    AudioIn, AudioOptions, AudioOut, DecoderConfig, DecoderHandle, EncoderConfig, EncoderHandle,
    MidiIn, MidiOut, MscTarget,
};
use anyhow::{anyhow, Result};
use cricket_core::modem::repetition_samples;
use cricket_core::phy::PhyConfig;
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Each note is held this long after its NoteOn arrives, then released ...
const HOLD: Duration = Duration::from_millis(150);
/// ... and the next note is struck 150–450 ms after the release (a fixed
/// spread, so notes do not lock to the heartbeat cadence and always land on
/// the same point of it). Releases make the encoder repeat NoteOffs, which a
/// following NoteOn may wait behind.
fn rest(i: usize) -> Duration {
    Duration::from_millis(150 + (i as u64 * 7919 % 31) * 10)
}

/// Summary of one loopback run (milliseconds).
#[derive(Debug, Clone, PartialEq)]
pub struct LatencySummary {
    pub n: usize,
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub jitter_ms: f64,
}

/// Arithmetic mean; empty slice returns 0.
pub fn mean_ms(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<f64>() / samples.len() as f64
}

/// Population standard deviation (jitter); empty or single sample → 0.
pub fn jitter_ms(samples: &[f64]) -> f64 {
    let n = samples.len();
    if n < 2 {
        return 0.0;
    }
    let m = mean_ms(samples);
    let var = samples.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n as f64;
    var.sqrt()
}

/// Nearest-rank percentile (e.g. p = 95 → 95th).
pub fn percentile_nearest_rank_ms(mut samples: Vec<f64>, p: f64) -> f64 {
    assert!((0.0..=100.0).contains(&p));
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = samples.len();
    let rank = (p / 100.0 * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    samples[idx]
}

fn summary_from_latencies(latencies: &[f64]) -> LatencySummary {
    LatencySummary {
        n: latencies.len(),
        mean_ms: mean_ms(latencies),
        p95_ms: percentile_nearest_rank_ms(latencies.to_vec(), 95.0),
        jitter_ms: jitter_ms(latencies),
    }
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}

/// How the in-memory cable delivers audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pacing {
    /// At the sample rate: a packet takes its airtime to arrive (default).
    RealTime,
    /// As soon as it is written: measures the engine's own overhead only.
    Instant,
}

/// Encoder → in-memory cable → decoder, with a MIDI injector and recorder.
struct Rig {
    enc: EncoderHandle,
    dec: DecoderHandle,
    injector: MemoryMidiInjector,
    rec: RecordingMidiSink,
}

impl Rig {
    fn start(profile: &str, pacing: Pacing) -> Result<Self> {
        let config = PhyConfig::from_name(profile);
        let packet_samples = repetition_samples(&config) * config.repetition_k.max(1) as u64;
        let sink_capacity = (packet_samples * 2).max(48_000) as usize;
        let (sink, source) = MemoryAudio::pair(RATE, sink_capacity);
        let audio_in = match pacing {
            Pacing::RealTime => AudioIn::Custom(Box::new(source.into_paced())),
            Pacing::Instant => AudioIn::Custom(Box::new(source)),
        };
        let rec = RecordingMidiSink::new();
        let dec = DecoderHandle::start(DecoderConfig {
            audio_in,
            midi_out: MidiOut::Custom(Box::new(rec.clone())),
            profile: profile.into(),
            link_dead_ms: None,
            msc: MscTarget::default(),
            audio: AudioOptions::default(),
        })?;
        let (midi_in, injector) = MemoryMidiInput::new();
        let enc = EncoderHandle::start(EncoderConfig {
            audio_out: AudioOut::Custom(Box::new(sink)),
            midi_in: MidiIn::Custom(Box::new(midi_in)),
            profile: profile.into(),
            simulate: false,
            audio: AudioOptions::default(),
        })?;
        let rig = Self {
            enc,
            dec,
            injector,
            rec,
        };
        if !wait_until(Duration::from_secs(5), || rig.injector.inject(&[0xF6])) {
            return Err(anyhow!("MIDI input did not connect"));
        }
        // Let the link settle (encoder may send heartbeats while idle).
        std::thread::sleep(Duration::from_millis(300));
        Ok(rig)
    }

    fn inject(&self, bytes: &[u8]) -> Result<()> {
        if self.injector.inject(bytes) {
            Ok(())
        } else {
            Err(anyhow!("MIDI inject failed: {:02X?}", bytes))
        }
    }

    /// Arrival time of each of `wanted` received after record position
    /// `from`, waiting at most `timeout`.
    fn arrivals(&self, from: usize, wanted: &[Vec<u8>], timeout: Duration) -> Option<Vec<Instant>> {
        let found = |rec: &[(Instant, Vec<u8>)]| -> Option<Vec<Instant>> {
            if rec.len().saturating_sub(from) < wanted.len() {
                return None;
            }
            wanted
                .iter()
                .map(|w| {
                    rec.get(from..)?
                        .iter()
                        .find(|(_, m)| m == w)
                        .map(|(t, _)| *t)
                })
                .collect()
        };
        if self.rec.wait_until(timeout, |rec| found(rec).is_some()) {
            found(&self.rec.timed_messages())
        } else {
            None
        }
    }

    fn finish(mut self) -> Result<()> {
        let enc_stopped = self.enc.stop();
        let dec_stopped = self.dec.stop();
        if !enc_stopped || !dec_stopped {
            return Err(anyhow!("loopback workers did not stop"));
        }
        Ok(())
    }
}

/// Measure wall-clock MIDI-in → MIDI-out NoteOn latency for `profile`
/// (a [`PhyConfig::from_name`] name such as `"robust"`, `"robust_repeat"`,
/// `"fast"`, `"ltc_fec"` or `"digital"`) over `note_count` notes, each held
/// for [`HOLD`], released, and followed by a [`rest`] before the next one.
pub fn measure_midi_loopback(profile: &str, note_count: usize) -> Result<LatencySummary> {
    measure_midi_loopback_with(profile, note_count, Pacing::RealTime)
}

/// [`measure_midi_loopback`] with a choice of [`Pacing`].
pub fn measure_midi_loopback_with(
    profile: &str,
    note_count: usize,
    pacing: Pacing,
) -> Result<LatencySummary> {
    if note_count == 0 {
        return Err(anyhow!("note_count must be at least 1"));
    }
    let rig = Rig::start(profile, pacing)?;
    let result = play_notes(&rig, note_count);
    let finished = rig.finish();
    let latencies = result?;
    finished?;
    Ok(summary_from_latencies(&latencies))
}

/// Plays the notes and returns each NoteOn's latency in milliseconds.
fn play_notes(rig: &Rig, note_count: usize) -> Result<Vec<f64>> {
    let mut latencies = Vec::with_capacity(note_count);
    for i in 0..note_count {
        let note = 48 + (i % 40) as u8;
        let expected = vec![0x90, note, 90];
        let before = rig.rec.len();
        let t0 = Instant::now();
        rig.inject(&expected)?;
        let Some(at) = rig.arrivals(before, std::slice::from_ref(&expected), DELIVERY_TIMEOUT)
        else {
            return Err(anyhow!(
                "NoteOn note={} not delivered within {:.0} s",
                note,
                DELIVERY_TIMEOUT.as_secs_f64()
            ));
        };
        latencies.push(at[0].duration_since(t0).as_secs_f64() * 1000.0);

        std::thread::sleep(HOLD);
        rig.inject(&[0x80, note, 0])?;
        std::thread::sleep(rest(i));
    }
    Ok(latencies)
}

/// Chord latency: first and last note of each chord.
#[derive(Debug, Clone, PartialEq)]
pub struct ChordSummary {
    pub first: LatencySummary,
    pub last: LatencySummary,
}

/// Strike `chords` chords of `size` notes (all NoteOns injected at once),
/// hold, release, rest; latency of the first and the last NoteOn of each.
pub fn measure_chord_loopback(profile: &str, chords: usize, size: usize) -> Result<ChordSummary> {
    if chords == 0 || size == 0 {
        return Err(anyhow!("need at least one chord of one note"));
    }
    let rig = Rig::start(profile, Pacing::RealTime)?;
    let result = (|| -> Result<(Vec<f64>, Vec<f64>)> {
        let (mut first, mut last) = (Vec::new(), Vec::new());
        for i in 0..chords {
            let notes: Vec<u8> = (0..size).map(|k| 48 + ((i + 4 * k) % 40) as u8).collect();
            let wanted: Vec<Vec<u8>> = notes.iter().map(|&n| vec![0x90, n, 90]).collect();
            let before = rig.rec.len();
            let t0 = Instant::now();
            for w in &wanted {
                rig.inject(w)?;
            }
            let at = rig
                .arrivals(before, &wanted, DELIVERY_TIMEOUT)
                .ok_or_else(|| anyhow!("chord {} not delivered", i))?;
            let ms = |t: &Instant| t.duration_since(t0).as_secs_f64() * 1000.0;
            first.push(at.iter().map(ms).fold(f64::MAX, f64::min));
            last.push(at.iter().map(ms).fold(0.0, f64::max));
            std::thread::sleep(HOLD);
            for &n in &notes {
                rig.inject(&[0x80, n, 0])?;
            }
            std::thread::sleep(rest(i));
        }
        Ok((first, last))
    })();
    let finished = rig.finish();
    let (first, last) = result?;
    finished?;
    Ok(ChordSummary {
        first: summary_from_latencies(&first),
        last: summary_from_latencies(&last),
    })
}

/// Sustained throughput: inject `messages` Program Changes (never coalesced
/// or dropped) at once and time until the last arrives. Returns messages
/// per second.
pub fn measure_throughput(profile: &str, messages: usize) -> Result<f64> {
    if messages == 0 {
        return Err(anyhow!("need at least one message"));
    }
    let rig = Rig::start(profile, Pacing::RealTime)?;
    let result = (|| -> Result<f64> {
        let wanted: Vec<Vec<u8>> = (0..messages)
            .map(|i| vec![0xC0 | (i % 16) as u8, ((i / 16) % 128) as u8])
            .collect();
        let before = rig.rec.len();
        let t0 = Instant::now();
        for w in &wanted {
            rig.inject(w)?;
        }
        let config = PhyConfig::from_name(profile);
        let airtime =
            repetition_samples(&config) as f64 * config.repetition_k.max(1) as f64 / RATE as f64;
        let timeout = Duration::from_secs_f64(5.0 + 2.0 * airtime * messages as f64);
        let at = rig
            .arrivals(before, &wanted, timeout)
            .ok_or_else(|| anyhow!("not all {} messages delivered", messages))?;
        let end = at.iter().max().copied().unwrap_or(t0);
        Ok(messages as f64 / end.duration_since(t0).as_secs_f64())
    })();
    let finished = rig.finish();
    let rate = result?;
    finished?;
    Ok(rate)
}
