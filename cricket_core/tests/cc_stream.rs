//! A fast fader stream through the TX scheduler (spec §5.5): with
//! latest-value-wins coalescing the final value arrives promptly and notes
//! played meanwhile are not delayed by the stream.
//!
//! The stream is 200 CC/s for 2 s (400 updates); Fast carries ~17 messages/s,
//! Robust ~3.7/s, so without coalescing it would queue for 20–100 s. Notes
//! are played on the fader's channel and on another channel; their latency
//! (queue wait + one on-wire copy) is compared with the same notes without
//! the stream. Run with `-- --nocapture` for the table.

use cricket_core::modem::{
    refresh_quiet_samples, refresh_schedule, refresh_spacing_samples, repetition_samples,
};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::{TransportMessage, TxScheduler};

const FADER_CHANNEL: u8 = 0;
const FADER_CC: u8 = 7;
const STREAM_HZ: u64 = 200;
const STREAM_MS: u64 = 2000;

#[derive(Debug, Default)]
struct Outcome {
    /// Per NoteOn: queue wait + one copy, in samples.
    note_latency: Vec<u64>,
    /// Final fader value: push → end of its first copy, in samples.
    final_value_latency: Option<u64>,
    cc_pushed: usize,
    cc_sent: usize,
    /// Every NoteOff was sent after its NoteOn.
    notes_in_order: bool,
}

fn simulate(config: &PhyConfig, stream: bool, note_channel: u8) -> Outcome {
    let rate = config.sample_rate_hz as u64;
    let rep = repetition_samples(config);
    let msg = rep * config.repetition_k.max(1) as u64;
    let hb = rate * config.heartbeat_interval_ms as u64 / 1000;
    let mut s = TxScheduler::new(config.sample_rate_hz, hb, msg)
        .with_refresh_schedule(refresh_schedule(config))
        .with_refresh_spacing(refresh_spacing_samples(config, config.sample_rate_hz))
        .with_refresh_quiet(refresh_quiet_samples(config, config.sample_rate_hz));

    // Notes: as fast as the profile comfortably carries a NoteOn + NoteOff.
    let note_interval = (4 * msg).max(rate / 4);
    let hold = note_interval / 2;
    let mut events: Vec<(u64, TransportMessage)> = Vec::new();
    let mut note = 40u8;
    let mut t = rate / 10;
    while t < rate * (STREAM_MS + 500) / 1000 {
        events.push((
            t,
            TransportMessage::NoteOn {
                channel: note_channel,
                note,
                velocity: 100,
            },
        ));
        events.push((
            t + hold,
            TransportMessage::NoteOff {
                channel: note_channel,
                note,
                velocity: 0,
            },
        ));
        note += 1;
        t += note_interval;
    }
    let updates = STREAM_HZ * STREAM_MS / 1000;
    if stream {
        for i in 0..updates {
            events.push((
                i * rate / STREAM_HZ,
                TransportMessage::ControlChange {
                    channel: FADER_CHANNEL,
                    controller: FADER_CC,
                    value: (i * 127 / (updates - 1)) as u8,
                },
            ));
        }
    }
    events.sort_by_key(|e| e.0);
    let final_push = (updates - 1) * rate / STREAM_HZ;

    let mut out = Outcome {
        notes_in_order: true,
        ..Outcome::default()
    };
    let mut pushed_at = std::collections::HashMap::new();
    let mut struck = std::collections::HashSet::new();
    let (mut next, mut now) = (0usize, 0u64);
    while next < events.len() || s.queued() > 0 {
        while next < events.len() && events[next].0 <= now {
            let (at, m) = events[next].clone();
            match m {
                TransportMessage::NoteOn { note, .. } => {
                    pushed_at.insert(note, at);
                }
                TransportMessage::ControlChange { .. } => out.cc_pushed += 1,
                _ => {}
            }
            s.push(m, at);
            next += 1;
        }
        match s.poll_detailed(now) {
            Some(r) if r.user => {
                match r.msg {
                    TransportMessage::NoteOn { note, .. } => {
                        out.note_latency.push(now - pushed_at[&note] + rep);
                        struck.insert(note);
                    }
                    TransportMessage::NoteOff { note, .. } => {
                        out.notes_in_order &= struck.contains(&note);
                    }
                    TransportMessage::ControlChange { value, .. } => {
                        out.cc_sent += 1;
                        if value == 127 {
                            out.final_value_latency = Some(now - final_push + rep);
                        }
                    }
                    _ => {}
                }
                now += msg;
            }
            Some(_) => now += msg,
            None => now = events.get(next).map_or(now + 1, |e| e.0.max(now + 1)),
        }
    }
    out
}

fn ms(config: &PhyConfig, samples: u64) -> f64 {
    samples as f64 * 1000.0 / config.sample_rate_hz as f64
}

#[test]
fn fader_stream_does_not_delay_notes() {
    println!(
        "\n| profile | msg ms | CC pushed / sent | final value latency ms | note latency mean / max ms: \
         no stream | same channel | other channel |"
    );
    println!("|---|---|---|---|---|---|---|");
    for (name, config) in [
        ("robust", PhyConfig::robust()),
        ("robust_repeat", PhyConfig::robust_repeat()),
        ("fast", PhyConfig::fast()),
        ("ltc_fec", PhyConfig::ltc_fec()),
    ] {
        let msg = repetition_samples(&config) * config.repetition_k.max(1) as u64;
        let quiet = simulate(&config, false, FADER_CHANNEL);
        let same = simulate(&config, true, FADER_CHANNEL);
        let other = simulate(&config, true, 5);
        let stat = |o: &Outcome| {
            let mean = o.note_latency.iter().sum::<u64>() as f64 / o.note_latency.len() as f64;
            let max = *o.note_latency.iter().max().unwrap();
            (ms(&config, mean as u64), ms(&config, max), max)
        };
        let (q_mean, q_max, q_max_s) = stat(&quiet);
        let (s_mean, s_max, s_max_s) = stat(&same);
        let (o_mean, o_max, o_max_s) = stat(&other);
        let final_ms = ms(&config, same.final_value_latency.expect("final value sent"));
        println!(
            "| {name} | {:.0} | {} / {} | {final_ms:.0} | {q_mean:.0} / {q_max:.0} | {s_mean:.0} / {s_max:.0} | {o_mean:.0} / {o_max:.0} |",
            ms(&config, msg),
            same.cc_pushed,
            same.cc_sent,
        );
        for o in [&quiet, &same, &other] {
            assert!(o.notes_in_order, "[{name}] NoteOff before its NoteOn");
            assert_eq!(o.note_latency.len(), quiet.note_latency.len(), "[{name}]");
        }
        // The final value goes out within the message in flight plus one.
        assert!(
            same.final_value_latency.unwrap() <= 2 * msg + repetition_samples(&config),
            "[{name}] final value after {final_ms:.0} ms"
        );
        // Each note waits at most for the message in flight and one update
        // of its own channel queued before it.
        assert!(
            s_max_s <= q_max_s + 2 * msg,
            "[{name}] same channel: {s_max:.0} ms vs {q_max:.0} ms"
        );
        assert!(
            o_max_s <= q_max_s + msg,
            "[{name}] other channel: {o_max:.0} ms vs {q_max:.0} ms"
        );
        // The stream itself is thinned to what the link carries.
        assert!(same.cc_sent < same.cc_pushed);
    }
}
