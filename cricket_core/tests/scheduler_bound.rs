//! The TX scheduler's queue bound (spec §5.5) under sustained overload: a
//! controller, a NoteOn and a Program Change every millisecond for 20 s on
//! Fast (about 3 000 messages/s into a link that carries ~17/s). The queue
//! and the wait of its oldest message must stay bounded, the final
//! controller value must arrive, and NoteOffs must never be dropped.
//!
//! Run with `-- --nocapture` for the numbers.

use cricket_core::modem::repetition_samples;
use cricket_core::phy::PhyConfig;
use cricket_core::transport::{TransportMessage, TxScheduler};

fn scheduler(config: &PhyConfig) -> (TxScheduler, u64) {
    let rate = config.sample_rate_hz as u64;
    let msg = repetition_samples(config) * config.repetition_k.max(1) as u64;
    let hb = rate * config.heartbeat_interval_ms as u64 / 1000;
    (TxScheduler::new(config.sample_rate_hz, hb, msg), msg)
}

#[derive(Debug, Default)]
struct Outcome {
    max_queued: usize,
    /// Longest wait of the oldest queued message, in samples.
    max_oldest_wait: u64,
    sent: usize,
    note_ons_sent: usize,
    program_changes_sent: usize,
    /// Value of the last CC 7 sent, and whether any CC 7 followed the
    /// final pushed value.
    last_cc7: Option<u8>,
    note_offs_pushed: usize,
    note_offs_sent: usize,
    note_on_drops: bool,
    queue_dropped: u32,
}

/// `seconds` of CC 7 + NoteOn + Program Change at 1 kHz, polled whenever
/// the line is free. With `with_note_offs`, every 200 ms one of the notes
/// struck so far is released (a NoteOff for a sounding note is never
/// dropped, so its rate must stay below what the link carries).
fn overload(config: &PhyConfig, seconds: u64, with_note_offs: bool) -> Outcome {
    let rate = config.sample_rate_hz as u64;
    let (mut s, msg) = scheduler(config);
    let step = rate / 1000;
    let end = rate * seconds;
    let mut out = Outcome::default();
    let mut now = 0u64;
    let mut next_push = 0u64;
    let mut i = 0u64;
    // Notes sounding at the far end (struck and sent, not released): their
    // releases are NoteOffs for sounding notes, which are never dropped.
    // A note whose release is queued is not struck again until it is out.
    let mut sounding: Vec<u8> = Vec::new();
    let mut releasing: Vec<u8> = Vec::new();
    if with_note_offs {
        for note in 100..110u8 {
            s.push(
                TransportMessage::NoteOn {
                    channel: 0,
                    note,
                    velocity: 100,
                },
                now,
            );
            assert!(s.poll(now).is_some());
            now += msg;
            sounding.push(note);
        }
    }
    let final_cc = ((seconds * 1000 - 1) % 128) as u8;
    while now < end + 10 * rate {
        while next_push <= now && next_push < end {
            let value = (i % 128) as u8;
            if with_note_offs && i % 200 == 199 {
                if let Some(note) = sounding.pop() {
                    releasing.push(note);
                    s.push(
                        TransportMessage::NoteOff {
                            channel: 0,
                            note,
                            velocity: 64,
                        },
                        next_push,
                    );
                    out.note_offs_pushed += 1;
                }
            }
            s.push(
                TransportMessage::ControlChange {
                    channel: 0,
                    controller: 7,
                    value,
                },
                next_push,
            );
            s.push(
                TransportMessage::NoteOn {
                    channel: 0,
                    note: value,
                    velocity: 100,
                },
                next_push,
            );
            s.push(
                TransportMessage::ProgramChange {
                    channel: 0,
                    program: value,
                },
                next_push,
            );
            i += 1;
            next_push += step;
        }
        out.max_queued = out.max_queued.max(s.queued());
        out.max_oldest_wait = out.max_oldest_wait.max(s.oldest_wait_samples(now));
        match s.poll_detailed(now) {
            Some(r) => {
                if r.user {
                    out.sent += 1;
                    match r.msg {
                        TransportMessage::NoteOn { note, .. } => {
                            out.note_ons_sent += 1;
                            if !sounding.contains(&note) && !releasing.contains(&note) {
                                sounding.push(note);
                            }
                        }
                        TransportMessage::NoteOff { note, .. } => {
                            out.note_offs_sent += 1;
                            releasing.retain(|n| *n != note);
                        }
                        TransportMessage::ProgramChange { .. } => out.program_changes_sent += 1,
                        TransportMessage::ControlChange {
                            controller: 7,
                            value,
                            ..
                        } => out.last_cc7 = Some(value),
                        _ => {}
                    }
                }
                now += msg;
            }
            None => {
                if next_push >= end && s.queued() == 0 {
                    break;
                }
                now += step;
            }
        }
    }
    out.note_on_drops = s.note_ons_dropped > 0;
    out.queue_dropped = s.queue_dropped;
    assert_eq!(out.last_cc7, Some(final_cc), "final CC 7 value");
    out
}

#[test]
fn overload_keeps_queue_and_wait_bounded() {
    let config = PhyConfig::fast();
    let (s, msg) = scheduler(&config);
    let rate = config.sample_rate_hz as u64;
    let budget = s.queue_budget_samples();
    assert_eq!(budget, rate); // 1 s
    for with_note_offs in [false, true] {
        let out = overload(&config, 20, with_note_offs);
        println!(
            "fast, 20 s at 3 kHz{}: max queued {} msgs, oldest wait max {:.0} ms, sent {} \
             ({} NoteOn, {} PC, {}/{} NoteOff), {} dropped for the bound, NoteOn drops {}",
            if with_note_offs { " + NoteOffs" } else { "" },
            out.max_queued,
            out.max_oldest_wait as f64 * 1000.0 / rate as f64,
            out.sent,
            out.note_ons_sent,
            out.program_changes_sent,
            out.note_offs_sent,
            out.note_offs_pushed,
            out.queue_dropped,
            out.note_on_drops,
        );
        // The queue holds the bound's worth of messages plus the NoteOns
        // (at most the profile's limit, with their held NoteOffs), plus the
        // undroppable tiers (NoteOffs at 5/s on a 17/s link: at most a
        // couple).
        let bound_msgs = (budget / msg) as usize + 2 * s.max_note_ons() + 1;
        let undroppable = if with_note_offs { 3 } else { 0 };
        assert!(
            out.max_queued <= bound_msgs + undroppable,
            "queue reached {} messages (bound {bound_msgs} + {undroppable})",
            out.max_queued
        );
        // The oldest message waits at most the bound plus the NoteOns
        // beyond it, what is ahead of it in tiers 1-2 and the message in
        // flight.
        assert!(
            out.max_oldest_wait
                <= budget + (s.max_note_ons() as u64 + undroppable as u64 + 2) * msg,
            "oldest wait {} samples",
            out.max_oldest_wait
        );
        assert!(out.queue_dropped > 0 && out.note_on_drops);
        assert!(out.program_changes_sent > 0 && out.note_ons_sent > 0);
        assert_eq!(
            out.note_offs_sent, out.note_offs_pushed,
            "NoteOffs are never dropped"
        );
    }
}

/// With plenty of airtime (the digital profile: ~1 ms per message), the
/// same 3 kHz stream still exceeds the bound; everything stays bounded and
/// the push cost does not grow with the queue (20 s complete quickly).
#[test]
fn overload_on_a_fast_link_stays_bounded() {
    let config = PhyConfig::digital();
    let (s, msg) = scheduler(&config);
    let rate = config.sample_rate_hz as u64;
    let out = overload(&config, 20, true);
    println!(
        "digital, 20 s at 3 kHz + NoteOffs: max queued {} msgs ({} fit the bound), oldest \
         wait max {:.0} ms, {} dropped for the bound",
        out.max_queued,
        s.queue_budget_samples() / msg,
        out.max_oldest_wait as f64 * 1000.0 / rate as f64,
        out.queue_dropped
    );
    assert!(
        out.max_queued <= (s.queue_budget_samples() / msg) as usize + 2 * s.max_note_ons() + 1 + 3
    );
    assert!(out.max_oldest_wait <= s.queue_budget_samples() + (s.max_note_ons() as u64 + 10) * msg);
    assert_eq!(out.note_offs_sent, out.note_offs_pushed);
}
