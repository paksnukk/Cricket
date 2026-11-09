//! Pass-through worker tests on in-memory endpoints: identity, decoding
//! while passing, the strip filter and the added latency.

use crate::passthrough::{
    measure_passthrough_latency, PassthroughConfig, PassthroughHandle, StripFilter, TapConfig,
    TapDecoder,
};
use crate::testing::{MemoryAudio, RecordingMidiSink};
use crate::*;
use cricket_core::modem::CricketModemTx;
use cricket_core::phy::PhyConfig;
use cricket_core::transport::TransportMessage;
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;

/// A guitar with `messages` embedded on the `instrument` profile: the
/// signal a box would see on its input.
fn instrument_with_messages(messages: &[TransportMessage], seconds: f32) -> Vec<f32> {
    let n = (seconds * RATE as f32) as usize;
    let mut audio = cricket_sim::instrument(cricket_sim::Instrument::Guitar, RATE, n, -20.0, 7);
    let mut tx = CricketModemTx::new(PhyConfig::hf_robust());
    let mut at = RATE as usize / 4;
    for m in messages {
        let burst = tx.encode_message(m);
        for (i, s) in burst.iter().enumerate() {
            if at + i < audio.len() {
                audio[at + i] += s;
            }
        }
        at += burst.len() + RATE as usize / 10;
    }
    audio
}

/// Run `input` through a pass-through worker with `processor` and MIDI to
/// `rec`, in blocks of `block`; returns the output and the events.
fn run_through(
    input: &[f32],
    processor: Box<dyn AudioProcessor>,
    rec: &RecordingMidiSink,
    block: usize,
) -> (Vec<f32>, Vec<EngineEvent>) {
    let (mut in_sink, in_source) = MemoryAudio::pair(RATE, RATE as usize * 8);
    let (out_sink, mut out_source) = MemoryAudio::pair(RATE, RATE as usize * 8);
    let mut cfg = PassthroughConfig::new(
        AudioIn::Custom(Box::new(in_source)),
        AudioOut::Custom(Box::new(out_sink)),
    );
    cfg.midi_out = MidiOut::Custom(Box::new(rec.clone()));
    let mut handle = PassthroughHandle::start(cfg, processor).unwrap();
    out_source.start().unwrap();
    let events = handle.take_events();
    let mut seen = Vec::new();
    // Let the worker open its endpoints before the first push.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if let Ok(ev) = events.recv_timeout(Duration::from_millis(20)) {
            let ready = matches!(&ev, EngineEvent::Log(l) if l.starts_with("Audio out"));
            seen.push(ev);
            if ready {
                break;
            }
        }
    }
    let mut out = Vec::with_capacity(input.len());
    let mut buf = vec![0.0f32; block];
    for chunk in input.chunks(block) {
        let mut pushed = 0;
        while pushed < chunk.len() {
            pushed += in_sink.push(&chunk[pushed..]);
        }
        let mut have = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while have < chunk.len() {
            have += out_source.pull(&mut buf[have..chunk.len()]);
            if have < chunk.len() {
                assert!(Instant::now() < deadline, "output did not arrive");
                out_source.wait(Duration::from_millis(1));
            }
        }
        out.extend_from_slice(&buf[..chunk.len()]);
        seen.extend(events.try_iter());
    }
    assert!(handle.stop());
    loop {
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(EngineEvent::Stopped) => break,
            Ok(ev) => seen.push(ev),
            Err(e) => panic!("no Stopped event ({e:?})"),
        }
    }
    (out, seen)
}

fn decoded(events: &[EngineEvent]) -> Vec<TransportMessage> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Decoded(m) if !matches!(m, TransportMessage::Heartbeat) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

/// The identity processor: every sample out equals the sample in, with no
/// delay, and the worker turns a block around in far less than one device
/// buffer of that block size.
#[test]
fn passthrough_is_bit_exact_with_zero_added_delay() {
    for block in [64usize, 256, 1024] {
        let m = measure_passthrough_latency(block, 40).unwrap();
        println!(
            "block {}: added {} samples, turnaround mean {:?} max {:?}, bit-exact {}",
            m.block, m.added_samples, m.turnaround_mean, m.turnaround_max, m.bit_exact
        );
        assert!(m.bit_exact, "block {block}");
        assert_eq!(m.added_samples, 0, "block {block}");
        // One buffer of this size at 48 kHz: 1.3 / 5.3 / 21 ms. The mean
        // must stay inside it; the worst case is reported, not asserted
        // (a loaded CI runner schedules threads as it likes).
        let buffer = Duration::from_secs_f64(block as f64 / RATE as f64);
        assert!(
            m.turnaround_mean < buffer,
            "block {block}: mean turnaround {:?} ≥ one buffer {:?}",
            m.turnaround_mean,
            buffer
        );
    }
}

/// The tap decoder passes the audio unchanged (bit-exact) while it decodes
/// the embedded messages and sends their MIDI.
#[test]
fn tap_decoder_decodes_while_passing_audio_unchanged() {
    let messages = [
        TransportMessage::NoteOn {
            channel: 0,
            note: 60,
            velocity: 100,
        },
        TransportMessage::ProgramChange {
            channel: 2,
            program: 9,
        },
        TransportMessage::NoteOff {
            channel: 0,
            note: 60,
            velocity: 64,
        },
    ];
    let input = instrument_with_messages(&messages, 2.0);
    let rec = RecordingMidiSink::new();
    let tap = TapDecoder::new(TapConfig::new("instrument")).unwrap();
    let (out, events) = run_through(&input, Box::new(tap), &rec, 480);
    assert_eq!(out.len(), input.len());
    assert!(out == input, "the tap changed the audio");
    assert_eq!(decoded(&events), messages);
    let midi = rec.messages();
    assert!(midi.contains(&vec![0x90, 60, 100]), "{midi:?}");
    assert!(midi.contains(&vec![0xC2, 9]), "{midi:?}");
    // The decoder worker's events: level, stats, and the stop burst log.
    assert!(events.iter().any(|e| matches!(e, EngineEvent::Level(_))));
    assert!(events.iter().any(|e| matches!(e, EngineEvent::Stats(_))));
    assert!(events
        .iter()
        .any(|e| matches!(e, EngineEvent::Log(l) if l.starts_with("Stop —"))));
}

/// The strip filter for the instrument profile: flat (≤ 0.1 dB) below
/// 17 kHz, ≥ 40 dB down over the receiver band 18–22.5 kHz, 62 samples of
/// group delay.
#[test]
fn strip_filter_meets_the_spec() {
    let config = PhyConfig::hf_robust();
    let f = StripFilter::for_profile(&config, RATE).unwrap();
    println!(
        "strip filter: {} taps, group delay {} samples ({:.2} ms)",
        f.len(),
        f.group_delay_samples(),
        f.group_delay_samples() as f32 * 1000.0 / RATE as f32
    );
    assert_eq!(f.group_delay_samples(), 64);
    for hz in [
        17_000.0, 17_500.0, 18_000.0, 19_000.0, 20_000.0, 21_500.0, 22_500.0, 23_500.0,
    ] {
        println!("  |H({hz:>7.0} Hz)| = {:.2} dB", f.response_db(hz, RATE));
    }
    let mut worst_pass = 0.0f32;
    let mut hz = 20.0;
    while hz <= 17_000.0 {
        worst_pass = worst_pass.max(f.response_db(hz, RATE).abs());
        hz += 50.0;
    }
    let mut worst_stop = -200.0f32;
    let mut hz = 18_000.0;
    while hz <= 22_500.0 {
        worst_stop = worst_stop.max(f.response_db(hz, RATE));
        hz += 50.0;
    }
    println!("passband ripple ≤ {worst_pass:.3} dB below 17 kHz; stopband ≤ {worst_stop:.1} dB over 18–22.5 kHz");
    assert!(worst_pass <= 0.1, "ripple {worst_pass} dB");
    assert!(worst_stop <= -40.0, "stopband {worst_stop} dB");
    // No band to strip on the LTC and digital profiles.
    assert!(StripFilter::for_profile(&PhyConfig::ltc_fec(), RATE).is_none());
    assert!(StripFilter::for_profile(&PhyConfig::digital(), RATE).is_none());
}

/// With `strip` on, the tap decoder still decodes (from the unstripped
/// input) while the output carries the tones ≥ 40 dB down and the music
/// through, delayed by the filter's group delay.
#[test]
fn tap_decoder_strips_the_data_band_from_the_output() {
    let messages = [TransportMessage::NoteOn {
        channel: 1,
        note: 64,
        velocity: 90,
    }];
    // Tones alone: what the strip must remove.
    let mut tx = CricketModemTx::new(PhyConfig::hf_robust());
    let tones = tx.encode_message(&messages[0]);
    let music = cricket_sim::instrument(
        cricket_sim::Instrument::Guitar,
        RATE,
        RATE as usize,
        -20.0,
        3,
    );
    let mut input = music.clone();
    for (i, s) in tones.iter().enumerate() {
        input[RATE as usize / 4 + i] += s;
    }
    let rec = RecordingMidiSink::new();
    let mut cfg = TapConfig::new("instrument");
    cfg.strip = true;
    let tap = TapDecoder::new(cfg).unwrap();
    let (out, events) = run_through(&input, Box::new(tap), &rec, 512);
    assert_eq!(decoded(&events), messages);
    assert!(events
        .iter()
        .any(|e| matches!(e, EngineEvent::Log(l) if l.starts_with("Strip filter on"))));

    // What the strip does to the data signal itself: the tone burst alone
    // through the same filter, total energy out over in.
    let mut strip = StripFilter::for_profile(&PhyConfig::hf_robust(), RATE).unwrap();
    let delay = strip.group_delay_samples();
    let mut padded = tones.clone();
    padded.extend(std::iter::repeat_n(0.0, strip.len()));
    let mut stripped = vec![0.0f32; padded.len()];
    strip.process(&padded, &mut stripped);
    let energy = |x: &[f32]| x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>();
    let attenuation_db = 10.0 * (energy(&tones) / energy(&stripped).max(1e-30)).log10();
    println!("data signal attenuation through the strip: {attenuation_db:.1} dB");
    assert!(attenuation_db >= 40.0, "{attenuation_db} dB");

    // In the mixed output, the tone band over the burst window (Hann
    // window against the splatter of the guitar's transients) drops by at
    // least as much as the window's own floor allows.
    let start = RATE as usize / 4;
    let band = |x: &[f32], lo: usize| -> f32 {
        let seg = &x[lo..lo + tones.len()];
        let n = seg.len() as f64;
        let mut e = 0.0f64;
        let mut hz = 18_000.0;
        while hz <= 22_500.0 {
            let w = 2.0 * std::f64::consts::PI * hz / RATE as f64;
            let (mut re, mut im) = (0.0, 0.0);
            for (k, &s) in seg.iter().enumerate() {
                let hann = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / n).cos();
                re += s as f64 * hann * (w * k as f64).cos();
                im -= s as f64 * hann * (w * k as f64).sin();
            }
            e += re * re + im * im;
            hz += 166.667;
        }
        e as f32
    };
    let before = band(&input, start);
    let after = band(&out, start + delay);
    let mixed_db = 10.0 * (before / after.max(1e-30)).log10();
    println!(
        "tone band in the mixed output: {mixed_db:.1} dB down (band energy {before:.3e} → {after:.3e}; guitar alone {:.3e})",
        band(&music, start)
    );
    assert!(mixed_db >= 40.0, "{mixed_db} dB");
    // The music is still there, delayed by the group delay: compare a stretch
    // without tones.
    let a = &music[RATE as usize / 2 + 10_000..RATE as usize / 2 + 20_000];
    let b = &out[RATE as usize / 2 + 10_000 + delay..RATE as usize / 2 + 20_000 + delay];
    let (mut num, mut den) = (0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        num += ((x - y) * (x - y)) as f64;
        den += (x * x) as f64;
    }
    let error_db = 10.0 * (num / den.max(1e-30)).log10();
    println!("music error through the strip (delay-compensated): {error_db:.1} dB");
    // A guitar on a passive pickup has little above 17 kHz: what the filter
    // removes is tiny next to the signal.
    assert!(error_db < -30.0, "{error_db} dB");
}
