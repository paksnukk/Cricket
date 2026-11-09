//! Engine-level tests on in-memory endpoints (no audio/MIDI hardware).

use crate::testing::{
    FailingAudio, MemoryAudio, MemoryMidiInjector, MemoryMidiInput, RecordingMidiSink,
};
use crate::*;
use cricket_core::modem::CricketModemTx;
use cricket_core::phy::{LinkState, PhyConfig};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;

fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

/// Drain events until `Stopped`; then the channel must disconnect (the
/// worker closure has returned and dropped its sender).
fn expect_stopped_then_closed(events: &Receiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    loop {
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(EngineEvent::Stopped) => break,
            Ok(ev) => seen.push(ev),
            Err(e) => panic!("no Stopped event ({:?}); saw {:?}", e, seen),
        }
    }
    assert_eq!(
        events.recv_timeout(Duration::from_secs(5)),
        Err(RecvTimeoutError::Disconnected),
        "events after Stopped or worker still alive"
    );
    seen
}

fn log_lines(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Log(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn memory_decoder(source: testing::MemorySource, rec: &RecordingMidiSink) -> DecoderHandle {
    DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Custom(Box::new(source)),
        midi_out: MidiOut::Custom(Box::new(rec.clone())),
        profile: "fast".into(),
        link_dead_ms: None,
        msc: MscTarget::default(),
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        fail_safe_burst: true,
    })
    .unwrap()
}

/// MIDI in → encoder → samples → decoder → MIDI out, all through the engine.
#[test]
fn loopback_midi_in_to_midi_out() {
    let (sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let rec = RecordingMidiSink::new();
    let mut dec = memory_decoder(source, &rec);

    let (midi_in, injector) = MemoryMidiInput::new();
    let mut enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Custom(Box::new(sink)),
        midi_in: MidiIn::Custom(Box::new(midi_in)),
        profile: "fast".into(),
        simulate: false,
        tx_level_dbfs: None,
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        msc_filter: MscFilter::default(),
    })
    .unwrap();

    // The encoder connects its MIDI input on its own thread.
    assert!(wait_for(Duration::from_secs(5), || injector.inject(&[0x90, 64, 100])));
    injector.inject(&[0x80, 64, 0]);
    enc.send(TransportMessage::ProgramChange {
        channel: 0,
        program: 5,
    });
    // Other channels and message kinds, MSC as SysEx; clock, other SysEx
    // and an MSC cue that does not fit are not carried.
    injector.inject(&[0xF8]);
    injector.inject(&[0xF0, 0x43, 0x10, 0x4C, 0x00, 0xF7]);
    injector.inject(&[
        0xF0, 0x7F, 0x01, 0x02, 0x01, 0x01, b'1', b'.', b'2', b'5', 0xF7,
    ]);
    injector.inject(&[0xBA, 7, 99]);
    injector.inject(&[0xEF, 0x00, 0x60]);
    injector.inject(&[
        0xF0, 0x7F, 0x01, 0x02, 0x01, 0x01, b'1', b'2', b'.', b'5', 0x00, b'2', 0xF7,
    ]);

    let note_on = vec![0x90, 64, 100];
    let note_off = vec![0x80, 64, 0];
    let program = vec![0xC0, 5];
    let cc = vec![0xBA, 7, 99];
    let bend = vec![0xEF, 0x00, 0x60];
    // GO 12.5 list 2, re-addressed to the receiver's device ID and format.
    let msc = vec![
        0xF0, 0x7F, 0x7F, 0x02, 0x01, 0x01, b'1', b'2', b'.', b'5', 0x00, b'2', 0xF7,
    ];
    let want = [&note_on, &note_off, &program, &cc, &bend, &msc];
    let got_all = wait_for(Duration::from_secs(15), || {
        let m = rec.messages();
        want.iter().all(|w| m.contains(w))
    });
    let midi = rec.messages();
    assert!(got_all, "MIDI out: {:?}", midi);
    let pos = |m: &Vec<u8>| midi.iter().position(|x| x == m).unwrap();
    assert!(pos(&note_on) < pos(&note_off), "order: {:?}", midi);
    assert!(pos(&note_off) < pos(&program), "order: {:?}", midi);
    assert_eq!(
        midi.iter().filter(|m| m.first() == Some(&0xF0)).count(),
        1,
        "only the representable MSC: {:?}",
        midi
    );

    assert!(enc.stop());
    assert!(dec.stop());

    let dec_events: Vec<EngineEvent> = dec.events().try_iter().collect();
    assert!(
        dec_events.contains(&EngineEvent::Decoded(TransportMessage::NoteOn {
            channel: 0,
            note: 64,
            velocity: 100
        }))
    );
    assert!(dec_events
        .iter()
        .any(|e| matches!(e, EngineEvent::Level(_))));
    assert_eq!(dec_events.last(), Some(&EngineEvent::Stopped));

    let enc_events: Vec<EngineEvent> = enc.events().try_iter().collect();
    assert_eq!(
        alarms(&enc_events, AlarmKind::MessageRejected),
        vec![
            "Can't be sent over this link: MSC cue 1.25 not representable (cues 0–999.9 with at most one decimal) — not sent"
                .to_string()
        ]
    );
    let enc_logs = log_lines(&enc_events);
    assert!(
        enc_logs
            .iter()
            .any(|l| l == "TX NoteOn ch=1 note=64 vel=100"),
        "{:?}",
        enc_logs
    );
    assert!(
        enc_logs.iter().any(|l| l == "Encoder stopped."),
        "{:?}",
        enc_logs
    );
}

/// With the test scale switched on the encoder plays it (first note: 60).
#[test]
fn simulated_scale_reaches_decoder() {
    let (sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let rec = RecordingMidiSink::new();
    let _dec = memory_decoder(source, &rec);
    let _enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Custom(Box::new(sink)),
        midi_in: MidiIn::None,
        profile: "fast".into(),
        simulate: true,
        tx_level_dbfs: None,
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        msc_filter: MscFilter::default(),
    })
    .unwrap();
    let ok = wait_for(Duration::from_secs(15), || {
        let m = rec.messages();
        m.contains(&vec![0x90, 60, 100]) && m.contains(&vec![0x80, 60, 0x40])
    });
    assert!(ok, "MIDI out: {:?}", rec.messages());
    // Handles dropped here: Drop must stop and join both workers.
}

/// Without MIDI in (and the test scale off, the default) the encoder sends
/// heartbeats only: a decoder on stage must never get notes nobody played.
#[test]
fn no_midi_in_sends_heartbeats_only() {
    let (sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let rec = RecordingMidiSink::new();
    let mut dec = memory_decoder(source, &rec);
    let enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Custom(Box::new(sink)),
        midi_in: MidiIn::None,
        profile: "analog-fast".into(),
        simulate: false,
        tx_level_dbfs: None,
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        msc_filter: MscFilter::default(),
    })
    .unwrap();
    // Two heartbeat intervals plus airtime: the scale would have struck
    // its first note long ago.
    let events = collect_until(dec.events(), Duration::from_secs(5), |e| {
        matches!(e, EngineEvent::Decoded(TransportMessage::Heartbeat))
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        rec.messages().is_empty(),
        "no MIDI must come out of heartbeats: {:?}",
        rec.messages()
    );
    assert!(events
        .iter()
        .all(|e| !matches!(e, EngineEvent::Decoded(TransportMessage::NoteOn { .. }))));
    let enc_logs = log_lines(&enc.events().try_iter().collect::<Vec<_>>());
    assert!(
        enc_logs
            .iter()
            .any(|l| l == "No MIDI in selected — sending heartbeats only."),
        "{:?}",
        enc_logs
    );
    assert!(dec.stop());
}

#[test]
fn stop_joins_worker_and_ends_with_stopped() {
    let (sink, source) = MemoryAudio::pair(RATE, RATE as usize);
    let rec = RecordingMidiSink::new();
    let mut dec = memory_decoder(source, &rec);
    let mut enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Custom(Box::new(sink)),
        midi_in: MidiIn::None,
        profile: "fast".into(),
        simulate: true,
        tx_level_dbfs: None,
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        msc_filter: MscFilter::default(),
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(dec.is_running() && enc.is_running());

    assert!(enc.stop(), "encoder did not stop in time");
    assert!(dec.stop(), "decoder did not stop in time");
    assert!(!dec.is_running() && !enc.is_running());
    // Stopping again is a no-op.
    assert!(dec.stop() && enc.stop());

    let enc_logs = log_lines(&expect_stopped_then_closed(enc.events()));
    assert_eq!(
        enc_logs.last().map(String::as_str),
        Some("Encoder stopped.")
    );
    let dec_logs = log_lines(&expect_stopped_then_closed(dec.events()));
    assert_eq!(
        dec_logs.first().map(String::as_str),
        Some("Starting decoder (profile=analog-fast)...")
    );
    assert_eq!(
        dec_logs.last().map(String::as_str),
        Some("Decoder stopped.")
    );
}

#[test]
fn drop_stops_worker() {
    let (_sink, source) = MemoryAudio::pair(RATE, RATE as usize);
    let rec = RecordingMidiSink::new();
    let mut dec = memory_decoder(source, &rec);
    let events = dec.take_events();
    // The handle's own channel is now closed.
    assert!(matches!(
        dec.events().try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
    drop(dec);
    let logs = log_lines(&expect_stopped_then_closed(&events));
    assert_eq!(logs.last().map(String::as_str), Some("Decoder stopped."));
}

#[test]
fn audio_open_error_is_logged_and_worker_ends() {
    let dec = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Custom(Box::new(FailingAudio("boom".into()))),
        midi_out: MidiOut::None,
        profile: "robust".into(),
        link_dead_ms: None,
        msc: MscTarget::default(),
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        fail_safe_burst: true,
    })
    .unwrap();
    let logs = log_lines(&expect_stopped_then_closed(dec.events()));
    assert_eq!(
        logs,
        vec![
            "Starting decoder (profile=analog-safe)...".to_string(),
            "Link key none (unkeyed link); link-dead after 2000 ms; fail-safe burst on.".to_string(),
            "No MIDI out selected — decode to UI only.".to_string(),
            "Audio input error: boom".to_string(),
        ]
    );
    assert!(wait_for(Duration::from_secs(1), || !dec.is_running()));

    let enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Custom(Box::new(FailingAudio("bang".into()))),
        midi_in: MidiIn::None,
        profile: "robust".into(),
        simulate: false,
        tx_level_dbfs: None,
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        msc_filter: MscFilter::default(),
    })
    .unwrap();
    let logs = log_lines(&expect_stopped_then_closed(enc.events()));
    assert_eq!(
        logs.last().map(String::as_str),
        Some("Audio output error: bang")
    );
}

// --- C2: wall-clock watchdog and device errors -------------------------------

const SUSTAIN_OFF: [u8; 3] = [0xB0, 0x40, 0x00];
const ALL_NOTES_OFF: [u8; 3] = [0xB0, 0x7B, 0x00];
/// Short link-dead timeout so the stall tests run quickly.
const TEST_LINK_DEAD_MS: u64 = 400;

fn count(midi: &[Vec<u8>], bytes: &[u8]) -> usize {
    midi.iter().filter(|m| m.as_slice() == bytes).count()
}

fn alarms(events: &[EngineEvent], kind: AlarmKind) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Alarm { kind: k, message } if *k == kind => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn cleared(events: &[EngineEvent], kind: AlarmKind) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, EngineEvent::AlarmCleared { kind: k, .. } if *k == kind))
        .count()
}

/// Fast-profile audio for one NoteOn followed by `silence_ms` of silence
/// (enough for the decoder to finish the packet, far less than link-dead).
fn packet_audio(tx: &mut CricketModemTx, note: u8, silence_ms: usize) -> Vec<f32> {
    let mut audio = tx.encode_message(&TransportMessage::NoteOn {
        channel: 0,
        note,
        velocity: 100,
    });
    audio.extend(std::iter::repeat_n(0.0, RATE as usize * silence_ms / 1000));
    audio
}

fn stall_decoder(source: testing::MemorySource, rec: &RecordingMidiSink) -> DecoderHandle {
    DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Custom(Box::new(source)),
        midi_out: MidiOut::Custom(Box::new(rec.clone())),
        profile: "fast".into(),
        link_dead_ms: Some(TEST_LINK_DEAD_MS as u32),
        msc: MscTarget::default(),
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        fail_safe_burst: true,
    })
    .unwrap()
}

/// Push `samples` and return `(t_push, t_drained)`: when they were queued and
/// when the decoder had pulled them all (t_drained >= time of the last pull).
fn push_and_drain(sink: &mut testing::MemorySink, samples: &[f32]) -> (Instant, Instant) {
    let t_push = Instant::now();
    assert_eq!(sink.push(samples), samples.len());
    loop {
        if sink.buffered() == 0 {
            return (t_push, Instant::now());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Wait until the recorder has `n` All-Notes-Off messages; returns when.
fn wait_for_bursts(rec: &RecordingMidiSink, n: usize, timeout: Duration) -> Option<Instant> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if count(&rec.messages(), &ALL_NOTES_OFF) >= n {
            return Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    None
}

fn collect_until(
    events: &Receiver<EngineEvent>,
    timeout: Duration,
    mut done: impl FnMut(&EngineEvent) -> bool,
) -> Vec<EngineEvent> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(e) => {
                let stop = done(&e);
                seen.push(e);
                if stop {
                    return seen;
                }
            }
            Err(_) => break,
        }
    }
    panic!("condition not met; saw {:?}", seen);
}

/// Goal 4: a packet, then the device stops delivering → exactly one burst and
/// one `AudioInputStalled` alarm, between link_dead_ms and link_dead_ms + 100.
#[test]
fn input_stall_after_packet_sends_one_burst_in_time() {
    let (mut sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let rec = RecordingMidiSink::new();
    let mut dec = stall_decoder(source, &rec);
    let mut tx = CricketModemTx::new(PhyConfig::fast());

    let (t_push, t_drained) = push_and_drain(&mut sink, &packet_audio(&mut tx, 60, 100));
    let t_burst = wait_for_bursts(&rec, 1, Duration::from_secs(3)).expect("no burst");
    let link_dead = Duration::from_millis(TEST_LINK_DEAD_MS);
    assert!(
        t_burst - t_push >= link_dead,
        "burst too early: {:?} after push",
        t_burst - t_push
    );
    assert!(
        t_burst - t_drained <= link_dead + Duration::from_millis(100),
        "burst too late: {:?} after last samples",
        t_burst - t_drained
    );

    // Stay stalled well past another timeout: still a single burst.
    std::thread::sleep(link_dead * 2);
    let midi = rec.messages();
    assert_eq!(midi.first(), Some(&vec![0x90, 60, 100]), "{:?}", midi);
    assert_eq!(count(&midi, &ALL_NOTES_OFF), 1, "{:?}", midi);
    assert_eq!(count(&midi, &SUSTAIN_OFF), 1, "{:?}", midi);
    // The burst covers every channel and starts with the explicit Note Off
    // for the note this receiver turned on.
    assert_eq!(count(&midi, &[0xBF, 0x7B, 0x00]), 1, "{:?}", midi);
    assert_eq!(midi.get(1), Some(&vec![0x80, 60, 0x40]), "{:?}", midi);
    assert_eq!(midi.len(), 1 + 33, "{:?}", midi);

    assert!(dec.stop());
    let events: Vec<EngineEvent> = dec.events().try_iter().collect();
    let stalls = alarms(&events, AlarmKind::AudioInputStalled);
    assert_eq!(stalls.len(), 1, "{:?}", stalls);
    assert!(stalls[0].contains("All Notes Off"), "{}", stalls[0]);
    assert!(alarms(&events, AlarmKind::LinkDead).is_empty());
    assert!(log_lines(&events)
        .iter()
        .any(|l| l.starts_with("Audio input stalled")));
    // The UI sees the link go dead right away (no samples → no modem stats).
    let last_stats = events
        .iter()
        .rev()
        .find_map(|e| match e {
            EngineEvent::Stats(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(last_stats.state, LinkState::Dead);
    assert!(!last_stats.link_alive);
}

/// A stall before any valid packet: alarm, but no burst.
#[test]
fn input_stall_without_link_sends_no_burst() {
    let (mut sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let rec = RecordingMidiSink::new();
    let mut dec = stall_decoder(source, &rec);
    push_and_drain(&mut sink, &vec![0.0; RATE as usize / 10]);
    let events = collect_until(dec.events(), Duration::from_secs(3), |e| {
        matches!(
            e,
            EngineEvent::Alarm {
                kind: AlarmKind::AudioInputStalled,
                ..
            }
        )
    });
    std::thread::sleep(Duration::from_millis(TEST_LINK_DEAD_MS));
    assert!(rec.messages().is_empty(), "{:?}", rec.messages());
    let stalls = alarms(&events, AlarmKind::AudioInputStalled);
    assert_eq!(stalls.len(), 1);
    assert!(!stalls[0].contains("All Notes Off"), "{}", stalls[0]);
    // Stopping always sends the fail-safe burst, link or no link.
    assert!(dec.stop());
    let midi = rec.messages();
    assert_eq!(count(&midi, &ALL_NOTES_OFF), 1, "{:?}", midi);
    assert_eq!(count(&midi, &SUSTAIN_OFF), 1, "{:?}", midi);
    assert_eq!(count(&midi, &[0xBF, 0x7B, 0x00]), 1, "{:?}", midi);
    assert_eq!(midi.len(), 32, "{:?}", midi);
}

/// Samples resume after the stall (with more than link-dead of silence): the
/// alarm is cleared and the sample-based link-dead does not send a second
/// burst for the same outage. A new packet followed by a new stall is a new
/// outage and gets its own burst.
#[test]
fn input_resume_after_stall_no_second_burst() {
    let (mut sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let rec = RecordingMidiSink::new();
    let mut dec = stall_decoder(source, &rec);
    let mut tx = CricketModemTx::new(PhyConfig::fast());
    let link_dead = Duration::from_millis(TEST_LINK_DEAD_MS);

    push_and_drain(&mut sink, &packet_audio(&mut tx, 60, 100));
    wait_for_bursts(&rec, 1, Duration::from_secs(3)).expect("no stall burst");

    // Resume with silence covering 2x link-dead of stream time.
    let quiet = vec![0.0; RATE as usize * 2 * TEST_LINK_DEAD_MS as usize / 1000];
    push_and_drain(&mut sink, &quiet);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        count(&rec.messages(), &ALL_NOTES_OFF),
        1,
        "{:?}",
        rec.messages()
    );
    let mut events: Vec<EngineEvent> = dec.events().try_iter().collect();
    assert_eq!(cleared(&events, AlarmKind::AudioInputStalled), 1);
    assert!(log_lines(&events)
        .iter()
        .any(|l| l.starts_with("Audio input resumed after")));
    assert!(alarms(&events, AlarmKind::LinkDead).is_empty());

    // New packet, new outage: one more burst.
    push_and_drain(&mut sink, &packet_audio(&mut tx, 62, 100));
    wait_for_bursts(&rec, 2, Duration::from_secs(3)).expect("no second-outage burst");
    std::thread::sleep(link_dead);
    // Before the stop (which adds its own burst).
    let midi = rec.messages();
    assert!(dec.stop());
    events.extend(dec.events().try_iter());
    assert_eq!(count(&midi, &ALL_NOTES_OFF), 2, "{:?}", midi);
    assert!(midi.contains(&vec![0x90, 62, 100]), "{:?}", midi);
    assert_eq!(alarms(&events, AlarmKind::AudioInputStalled).len(), 2);
}

/// Errors from the device's error callback reach the owner as alarms.
#[test]
fn device_errors_become_alarms() {
    let (sink, source) = MemoryAudio::pair(RATE, RATE as usize);
    let in_errors = source.errors();
    let out_errors = sink.errors();
    let rec = RecordingMidiSink::new();
    let dec = memory_decoder(source, &rec);
    let enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Custom(Box::new(sink)),
        midi_in: MidiIn::None,
        profile: "fast".into(),
        simulate: false,
        tx_level_dbfs: None,
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        msc_filter: MscFilter::default(),
    })
    .unwrap();

    let is_device_error = |e: &EngineEvent| {
        matches!(
            e,
            EngineEvent::Alarm {
                kind: AlarmKind::AudioDeviceError,
                ..
            }
        )
    };
    in_errors.push("input stream: device unplugged");
    let events = collect_until(dec.events(), Duration::from_secs(3), is_device_error);
    assert_eq!(
        alarms(&events, AlarmKind::AudioDeviceError),
        vec!["Audio input device error: input stream: device unplugged".to_string()]
    );

    out_errors.push("output stream: backend error");
    let events = collect_until(enc.events(), Duration::from_secs(3), is_device_error);
    assert_eq!(
        alarms(&events, AlarmKind::AudioDeviceError),
        vec!["Audio output device error: output stream: backend error".to_string()]
    );
}

#[test]
fn device_error_queue_is_bounded() {
    let q = DeviceErrorQueue::new();
    for i in 0..20 {
        q.push(format!("e{}", i));
    }
    let all: Vec<String> = std::iter::from_fn(|| q.pop()).collect();
    assert_eq!(all.len(), 17);
    assert_eq!(all[0], "e0");
    assert_eq!(all[15], "e15");
    assert_eq!(all[16], "4 further audio device errors not shown");
    assert_eq!(q.pop(), None);
}

// --- Stage safety: stop / panic bursts, MIDI-out failures, link restored ---

/// Stopping a decoder with a MIDI out sends the fail-safe burst first, and
/// `panic()` sends it on demand while running.
#[test]
fn stop_and_panic_send_fail_safe_burst() {
    let (_sink, source) = MemoryAudio::pair(RATE, RATE as usize);
    let rec = RecordingMidiSink::new();
    let mut dec = memory_decoder(source, &rec);
    std::thread::sleep(Duration::from_millis(100));
    assert!(rec.is_empty());

    dec.panic();
    assert!(wait_for(Duration::from_secs(2), || rec.len() >= 32));
    let midi = rec.messages();
    assert_eq!(midi.len(), 32, "{:?}", midi);
    assert_eq!(count(&midi, &SUSTAIN_OFF), 1);
    assert_eq!(count(&midi, &ALL_NOTES_OFF), 1);
    assert_eq!(count(&midi, &[0xBF, 0x7B, 0x00]), 1);

    assert!(dec.stop());
    let midi = rec.messages();
    assert_eq!(midi.len(), 64, "{:?}", midi);
    let logs = log_lines(&dec.events().try_iter().collect::<Vec<_>>());
    assert!(
        logs.iter().any(|l| l.starts_with("Panic — Sustain off")),
        "{:?}",
        logs
    );
    assert!(
        logs.iter().any(|l| l.starts_with("Stop — Sustain off")),
        "{:?}",
        logs
    );
    assert_eq!(logs.last().map(String::as_str), Some("Decoder stopped."));

    // Without a MIDI out there is nothing to send and nothing is logged.
    let (_sink, source) = MemoryAudio::pair(RATE, RATE as usize);
    let mut dec = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Custom(Box::new(source)),
        midi_out: MidiOut::None,
        profile: "analog-fast".into(),
        link_dead_ms: None,
        msc: MscTarget::default(),
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        fail_safe_burst: true,
    })
    .unwrap();
    assert!(dec.stop());
    let logs = log_lines(&dec.events().try_iter().collect::<Vec<_>>());
    assert!(!logs.iter().any(|l| l.starts_with("Stop — ")), "{:?}", logs);
}

/// A failing MIDI output raises one alarm per failure streak and clears it
/// when a send succeeds again; the lost messages are counted.
#[test]
fn midi_out_failure_is_one_alarm_and_clears() {
    let (mut sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let failing = testing::FailingMidiSink::default();
    let mut dec = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Custom(Box::new(source)),
        midi_out: MidiOut::Custom(Box::new(failing.clone())),
        profile: "fast".into(),
        link_dead_ms: None,
        msc: MscTarget::default(),
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        fail_safe_burst: true,
    })
    .unwrap();
    let mut tx = CricketModemTx::new(PhyConfig::fast());

    failing.set_failing(true);
    push_and_drain(&mut sink, &packet_audio(&mut tx, 60, 20));
    push_and_drain(&mut sink, &packet_audio(&mut tx, 62, 20));
    let events = collect_until(dec.events(), Duration::from_secs(3), |e| {
        matches!(
            e,
            EngineEvent::Decoded(TransportMessage::NoteOn { note: 62, .. })
        )
    });
    let raised = alarms(&events, AlarmKind::MidiOutputError);
    assert_eq!(raised.len(), 1, "{:?}", raised);
    assert!(
        raised[0].starts_with("MIDI out failed: MIDI send: port closed"),
        "{}",
        raised[0]
    );
    assert!(failing.recorder.is_empty());

    failing.set_failing(false);
    push_and_drain(&mut sink, &packet_audio(&mut tx, 64, 20));
    let events = collect_until(dec.events(), Duration::from_secs(3), |e| {
        matches!(
            e,
            EngineEvent::AlarmCleared {
                kind: AlarmKind::MidiOutputError,
                ..
            }
        )
    });
    let Some(EngineEvent::AlarmCleared { message, .. }) = events.last() else {
        panic!("{:?}", events);
    };
    assert_eq!(
        message,
        "MIDI out is working again (2 messages lost meanwhile)"
    );
    assert!(alarms(&events, AlarmKind::MidiOutputError).is_empty());
    assert!(wait_for(Duration::from_secs(2), || !failing
        .recorder
        .is_empty()));
    assert_eq!(failing.recorder.messages()[0], vec![0x90, 64, 100]);
    assert!(dec.stop());
}

/// The link-dead alarm is cleared by the next valid packet.
#[test]
fn link_dead_alarm_is_cleared_by_next_packet() {
    let (mut sink, source) = MemoryAudio::pair(RATE, RATE as usize * 8);
    let rec = RecordingMidiSink::new();
    let mut dec = stall_decoder(source, &rec);
    let mut tx = CricketModemTx::new(PhyConfig::fast());

    // A packet, then more than link-dead of silence in the same stream: the
    // sample-based link-dead fires.
    let silence_ms = TEST_LINK_DEAD_MS as usize * 2;
    push_and_drain(&mut sink, &packet_audio(&mut tx, 60, silence_ms));
    let events = collect_until(dec.events(), Duration::from_secs(3), |e| {
        matches!(
            e,
            EngineEvent::Alarm {
                kind: AlarmKind::LinkDead,
                ..
            }
        )
    });
    let dead = alarms(&events, AlarmKind::LinkDead);
    assert_eq!(dead.len(), 1, "{:?}", dead);
    assert!(
        dead[0].starts_with("Link dead: no valid packet for 400 ms"),
        "{}",
        dead[0]
    );
    assert_eq!(count(&rec.messages(), &ALL_NOTES_OFF), 1);

    push_and_drain(&mut sink, &packet_audio(&mut tx, 62, 20));
    let events = collect_until(dec.events(), Duration::from_secs(3), |e| {
        matches!(
            e,
            EngineEvent::Decoded(TransportMessage::NoteOn { note: 62, .. })
        )
    });
    let restored: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::AlarmCleared {
                kind: AlarmKind::LinkDead,
                message,
            } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(restored.len(), 1, "{:?}", events);
    assert!(
        restored[0].starts_with("Link restored after"),
        "{}",
        restored[0]
    );
    assert!(dec.stop());
}

/// A typo in the profile name is an error, never another profile.
#[test]
fn unknown_profile_is_an_error() {
    let (sink, source) = MemoryAudio::pair(RATE, RATE as usize);
    let err = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Custom(Box::new(source)),
        midi_out: MidiOut::None,
        profile: "robsut".into(),
        link_dead_ms: None,
        msc: MscTarget::default(),
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        fail_safe_burst: true,
    })
    .err()
    .expect("unknown profile must fail");
    assert!(
        err.to_string().contains("unknown profile \"robsut\""),
        "{}",
        err
    );
    let err = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Custom(Box::new(sink)),
        midi_in: MidiIn::None,
        profile: "ltc_fec_".into(),
        simulate: false,
        tx_level_dbfs: None,
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        msc_filter: MscFilter::default(),
    })
    .err()
    .expect("unknown profile must fail");
    assert!(err.to_string().contains("unknown profile"), "{}", err);
}

/// The transmit level option changes the encoded amplitude (default −12 dBFS
/// peak), and the level event reports it in a way a meter can show.
#[test]
fn tx_level_is_applied() {
    for (level, want_peak) in [(None, 0.25f32), (Some(-20.0), 0.1), (Some(0.0), 1.0)] {
        let (sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
        let rec = RecordingMidiSink::new();
        let mut dec = DecoderHandle::start(DecoderConfig {
            audio_in: AudioIn::Custom(Box::new(source)),
            midi_out: MidiOut::Custom(Box::new(rec.clone())),
            profile: "robust".into(),
            link_dead_ms: None,
            msc: MscTarget::default(),
            audio: AudioOptions::default(),
            link_key: DEFAULT_LINK_KEY,
            fail_safe_burst: true,
        })
        .unwrap();
        let mut enc = EncoderHandle::start(EncoderConfig {
            audio_out: AudioOut::Custom(Box::new(sink)),
            midi_in: MidiIn::None,
            profile: "analog-safe".into(),
            simulate: false,
            tx_level_dbfs: level,
            audio: AudioOptions::default(),
            link_key: DEFAULT_LINK_KEY,
            msc_filter: MscFilter::default(),
        })
        .unwrap();
        // A heartbeat is 169 ms of tone at the TX level: the loudest 50 ms
        // level window reads close to the tone's RMS (peak / sqrt 2).
        let events = collect_until(dec.events(), Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::Decoded(TransportMessage::Heartbeat))
        });
        let peak_rms = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Level(rms) => Some(*rms),
                _ => None,
            })
            .fold(0.0f32, f32::max);
        let want_rms = want_peak * core::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (peak_rms - want_rms).abs() < want_rms * 0.25,
            "level {:?}: peak RMS {} (want ~{})",
            level,
            peak_rms,
            want_rms
        );
        let logs = log_lines(&enc.events().try_iter().collect::<Vec<_>>());
        let want_db = 20.0 * want_peak.log10();
        assert!(
            logs.iter()
                .any(|l| l.contains(&format!("TX level {:.1} dBFS", want_db))),
            "{:?}",
            logs
        );
        assert!(enc.stop() && dec.stop());
    }
}

// --- Link key, MSC filter, fail-safe burst switch ---------------------------

/// Encoder → decoder with per-end link keys and the encoder's MSC filter.
fn keyed_rig(
    enc_key: u16,
    dec_key: u16,
    msc_filter: MscFilter,
    fail_safe_burst: bool,
) -> (
    EncoderHandle,
    DecoderHandle,
    MemoryMidiInjector,
    RecordingMidiSink,
) {
    let (sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let rec = RecordingMidiSink::new();
    let dec = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Custom(Box::new(source)),
        midi_out: MidiOut::Custom(Box::new(rec.clone())),
        profile: "fast".into(),
        link_dead_ms: None,
        msc: MscTarget::default(),
        audio: AudioOptions::default(),
        link_key: dec_key,
        fail_safe_burst,
    })
    .unwrap();
    let (midi_in, injector) = MemoryMidiInput::new();
    let enc = EncoderHandle::start(EncoderConfig {
        audio_out: AudioOut::Custom(Box::new(sink)),
        midi_in: MidiIn::Custom(Box::new(midi_in)),
        profile: "fast".into(),
        simulate: false,
        tx_level_dbfs: None,
        audio: AudioOptions::default(),
        link_key: enc_key,
        msc_filter,
    })
    .unwrap();
    assert!(wait_for(Duration::from_secs(5), || injector.inject(&[0xF6])));
    (enc, dec, injector, rec)
}

/// With different link keys nothing is delivered and the decoder counts
/// the encoder's packets as wrong-key suspects; with the same key (a
/// passphrase on both ends) notes arrive.
#[test]
fn link_key_mismatch_delivers_nothing() {
    let key = parse_link_key("stage left");
    let (mut enc, mut dec, injector, rec) =
        keyed_rig(key, DEFAULT_LINK_KEY, MscFilter::default(), true);
    injector.inject(&[0x90, 60, 100]);
    std::thread::sleep(Duration::from_millis(800));
    assert!(rec.is_empty(), "{:?}", rec.messages());
    let events: Vec<EngineEvent> = dec.events().try_iter().collect();
    let suspects = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Stats(s) => Some(s.wrong_key_suspects),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    assert!(suspects >= 1, "{:?}", events);
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::Decoded(_))));
    let logs = log_lines(&events);
    assert!(
        logs.iter()
            .any(|l| l
                == "Link key none (unkeyed link); link-dead after 2000 ms; fail-safe burst on."),
        "{:?}",
        logs
    );
    assert!(enc.stop() && dec.stop());

    let (mut enc, mut dec, injector, rec) = keyed_rig(key, key, MscFilter::default(), true);
    injector.inject(&[0x90, 61, 100]);
    assert!(wait_for(Duration::from_secs(3), || rec
        .messages()
        .contains(&vec![0x90, 61, 100])));
    let logs = log_lines(&enc.events().try_iter().collect::<Vec<_>>());
    assert!(
        logs.iter()
            .any(|l| l.starts_with("Link key set (0x••••); MSC accepted any device")),
        "{:?}",
        logs
    );
    assert!(enc.stop() && dec.stop());
}

/// The encoder's MSC filter carries only cues for its device (or all-call);
/// the first cue for another device is logged once.
#[test]
fn msc_filter_drops_cues_for_other_devices() {
    let filter = MscFilter {
        device_id: Some(3),
        command_format: Some(0x10),
    };
    let (mut enc, mut dec, injector, rec) =
        keyed_rig(DEFAULT_LINK_KEY, DEFAULT_LINK_KEY, filter, true);
    let go = |device: u8, format: u8, cue: u8| -> Vec<u8> {
        vec![0xF0, 0x7F, device, 0x02, format, 0x01, b'0' + cue, 0xF7]
    };
    injector.inject(&go(1, 0x10, 1)); // another device
    injector.inject(&go(3, 0x01, 2)); // another format
    injector.inject(&go(3, 0x10, 3)); // ours
    injector.inject(&go(0x7F, 0x7F, 4)); // all-call, all-types
    injector.inject(&go(0x7F, 0x10, 5)); // all-call, our format

    // Re-stamped with the decoder's target (all-call, Lighting).
    let out = |cue: u8| vec![0xF0, 0x7F, 0x7F, 0x02, 0x01, 0x01, b'0' + cue, 0xF7];
    assert!(wait_for(Duration::from_secs(5), || rec
        .messages()
        .contains(&out(5))));
    std::thread::sleep(Duration::from_millis(300));
    // (The rig's connection probe, a Tune Request, arrives too.)
    let midi: Vec<Vec<u8>> = rec
        .messages()
        .into_iter()
        .filter(|m| m.first() == Some(&0xF0))
        .collect();
    assert_eq!(midi, vec![out(3), out(4), out(5)], "{:?}", midi);
    let logs = log_lines(&enc.events().try_iter().collect::<Vec<_>>());
    assert_eq!(
        logs.iter()
            .filter(|l| l.starts_with("MSC for device 0x01 / format 0x10 not sent"))
            .count(),
        1,
        "{:?}",
        logs
    );
    assert!(enc.stop() && dec.stop());
}

/// With the fail-safe burst off, Panic, link-dead and stop send nothing to
/// MIDI out, while link-dead is still reported.
#[test]
fn fail_safe_burst_can_be_switched_off() {
    let (mut sink, source) = MemoryAudio::pair(RATE, RATE as usize * 4);
    let rec = RecordingMidiSink::new();
    let mut dec = DecoderHandle::start(DecoderConfig {
        audio_in: AudioIn::Custom(Box::new(source)),
        midi_out: MidiOut::Custom(Box::new(rec.clone())),
        profile: "fast".into(),
        link_dead_ms: Some(TEST_LINK_DEAD_MS as u32),
        msc: MscTarget::default(),
        audio: AudioOptions::default(),
        link_key: DEFAULT_LINK_KEY,
        fail_safe_burst: false,
    })
    .unwrap();
    dec.panic();
    std::thread::sleep(Duration::from_millis(100));
    assert!(rec.is_empty());

    let mut tx = CricketModemTx::new(PhyConfig::fast());
    let audio = packet_audio(&mut tx, 60, TEST_LINK_DEAD_MS as usize + 400);
    push_and_drain(&mut sink, &audio);
    let events = collect_until(dec.events(), Duration::from_secs(3), |e| {
        matches!(
            e,
            EngineEvent::Alarm {
                kind: AlarmKind::LinkDead,
                ..
            }
        )
    });
    assert!(
        alarms(&events, AlarmKind::LinkDead)
            .iter()
            .any(|m| m.ends_with("fail-safe burst is off: nothing sent to MIDI out")),
        "{:?}",
        events
    );
    assert_eq!(rec.messages(), vec![vec![0x90, 60, 100]]);
    assert!(dec.stop());
    assert_eq!(
        rec.messages(),
        vec![vec![0x90, 60, 100]],
        "no burst on stop"
    );
    let logs = log_lines(&dec.events().try_iter().collect::<Vec<_>>());
    assert!(
        logs.iter().any(|l| l == "Link dead (fail-safe burst off)"),
        "{:?}",
        logs
    );
}
