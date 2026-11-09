//! `cricket pathcheck` on real devices: play the path-check pattern
//! ([`cricket_core::pathcheck`]) on an output, record an input, or both at
//! once; the analysis is [`cricket_core::pathcheck::analyze`].
//!
//! Devices are opened like the workers open them for the `digital` profile
//! ([`AudioOptions::digital`] set: with [`crate::SampleFormatPref::Auto`]
//! an integer stream is preferred), so the check covers the same conversions.

use crate::audio::{AudioIn, AudioOptions, AudioOut};
use anyhow::{anyhow, Result};
use cricket_core::pathcheck::{pattern_samples, PERIOD};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;

fn digital(options: &AudioOptions) -> AudioOptions {
    AudioOptions {
        digital: true,
        ..options.clone()
    }
}

/// Opened output: `(sample rate, channels, stream details)`.
pub type StreamInfo = (u32, usize, String);

/// Play the pattern on `audio_out` until `duration` has passed (`None`:
/// until `stop` is set). `on_open` gets the stream details once it plays.
pub fn play_pattern(
    audio_out: AudioOut,
    options: &AudioOptions,
    duration: Option<Duration>,
    stop: &AtomicBool,
    on_open: impl FnOnce(&StreamInfo),
) -> Result<()> {
    let ring = 4 * PERIOD;
    let mut sink = audio_out.open(RATE, ring, &digital(options))?;
    on_open(&(sink.sample_rate(), sink.channels(), sink.describe()));
    sink.start()?;
    let one = pattern_samples(1);
    let mut pos = 0usize;
    let start = Instant::now();
    while !stop.load(Ordering::SeqCst) && duration.is_none_or(|d| start.elapsed() < d) {
        while sink.buffered() < 2 * PERIOD {
            let n = sink.push(&one[pos..]);
            pos = (pos + n) % PERIOD;
            if n == 0 {
                break;
            }
        }
        if let Some(err) = sink.take_error() {
            return Err(anyhow!("audio output: {}", err));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Let the buffered pattern play out.
    let drain = Instant::now();
    while sink.buffered() > 0 && drain.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// Record `duration` of `audio_in` (the channel selected in `options`).
/// Returns the samples and the
/// stream details.
pub fn capture(
    audio_in: AudioIn,
    options: &AudioOptions,
    duration: Duration,
) -> Result<(Vec<f32>, StreamInfo)> {
    let mut source = audio_in.open(RATE, RATE as usize * 2, &digital(options))?;
    let info = (source.sample_rate(), source.channels(), source.describe());
    source.start()?;
    let want = (duration.as_secs_f64() * info.0 as f64) as usize;
    let mut out = Vec::with_capacity(want);
    let mut buf = vec![0.0f32; 4096];
    let deadline = Instant::now() + duration + Duration::from_secs(2);
    while out.len() < want && Instant::now() < deadline {
        let n = source.pull(&mut buf);
        out.extend_from_slice(&buf[..n.min(want - out.len())]);
        if n == 0 {
            source.wait(Duration::from_millis(5));
        }
        if let Some(err) = source.take_error() {
            return Err(anyhow!("audio input: {}", err));
        }
    }
    if source.dropped() > 0 {
        return Err(anyhow!(
            "audio input dropped {} samples (capture buffer overrun)",
            source.dropped()
        ));
    }
    Ok((out, info))
}

/// Play on `audio_out` and record `audio_in` at the same time (both ends on
/// this machine). Returns the recording and both streams' details.
pub fn loop_check(
    audio_out: AudioOut,
    audio_in: AudioIn,
    options: &AudioOptions,
    duration: Duration,
) -> Result<(Vec<f32>, StreamInfo, StreamInfo)> {
    let stop = AtomicBool::new(false);
    let opts = options.clone();
    std::thread::scope(|scope| {
        let (info_tx, info_rx) = std::sync::mpsc::channel();
        let (opts, stop) = (&opts, &stop);
        let player = scope.spawn(move || {
            play_pattern(audio_out, opts, None, stop, |i| {
                let _ = info_tx.send(i.clone());
            })
        });
        let out_info = match info_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(i) => i,
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                return match player.join() {
                    Ok(Err(e)) => Err(e),
                    _ => Err(anyhow!("audio output did not start")),
                };
            }
        };
        // Give the output a moment to reach the input before recording.
        std::thread::sleep(Duration::from_millis(200));
        let recorded = capture(audio_in, options, duration);
        stop.store(true, Ordering::SeqCst);
        let played = player.join().map_err(|_| anyhow!("player panicked"))?;
        played?;
        let (samples, in_info) = recorded?;
        Ok((samples, out_info, in_info))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryAudio;
    use cricket_core::pathcheck::{analyze, Verdict};

    /// Through the in-memory cable (a bit-exact path) the check reports
    /// bit-exact; the same recording negated reports inverted polarity.
    #[test]
    fn loop_check_over_memory_cable_is_bit_exact() {
        let (sink, source) = MemoryAudio::pair(RATE, 4 * PERIOD);
        let (samples, out_info, in_info) = loop_check(
            AudioOut::Custom(Box::new(sink)),
            AudioIn::Custom(Box::new(source.into_paced())),
            &AudioOptions::default(),
            Duration::from_millis(700),
        )
        .unwrap();
        assert_eq!((out_info.0, in_info.0), (RATE, RATE));
        let report = analyze(&samples, RATE);
        assert_eq!(report.verdict, Verdict::BitExact, "{:?}", report);
        let inverted: Vec<f32> = samples.iter().map(|x| -x).collect();
        let report = analyze(&inverted, RATE);
        assert!(report.inverted);
        assert_eq!(report.verdict, Verdict::BitExact);
    }
}
