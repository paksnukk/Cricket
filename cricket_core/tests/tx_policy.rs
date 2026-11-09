//! TX scheduler policy measurements (spec §5.5), without audio: NoteOn
//! latency under legato playing, where the NoteOff repeats of the previous
//! note compete with the next strike, and chord delivery per profile.
//!
//! The line model is the engine's: the next message is chosen when the one
//! in flight ends; while idle the scheduler is polled every 5 ms (the
//! encoder's tick). A NoteOn's latency is its queue wait plus one on-wire
//! copy. Run with `-- --nocapture` for the tables.

use cricket_core::modem::{
    refresh_quiet_samples, refresh_schedule, refresh_spacing_samples, repetition_samples,
};
use cricket_core::phy::PhyConfig;
use cricket_core::transport::{TransportMessage, TxScheduler};

const TICK_MS: u64 = 5;

fn scheduler(config: &PhyConfig) -> (TxScheduler, u64, u64) {
    let rate = config.sample_rate_hz as u64;
    let rep = repetition_samples(config);
    let msg = rep * config.repetition_k.max(1) as u64;
    let hb = rate * config.heartbeat_interval_ms as u64 / 1000;
    let s = TxScheduler::new(config.sample_rate_hz, hb, msg)
        .with_refresh_schedule(refresh_schedule(config))
        .with_refresh_spacing(refresh_spacing_samples(config, config.sample_rate_hz))
        .with_refresh_quiet(refresh_quiet_samples(config, config.sample_rate_hz));
    (s, msg, rep)
}

fn ms(config: &PhyConfig, samples: u64) -> f64 {
    samples as f64 * 1000.0 / config.sample_rate_hz as f64
}

fn on(note: u8) -> TransportMessage {
    TransportMessage::NoteOn {
        channel: 0,
        note,
        velocity: 90,
    }
}

fn off(note: u8) -> TransportMessage {
    TransportMessage::NoteOff {
        channel: 0,
        note,
        velocity: 0,
    }
}

#[derive(Debug, Default)]
struct Legato {
    /// Per NoteOn: queue wait + one copy, in samples.
    latency: Vec<u64>,
    /// NoteOns pushed while a refresh or heartbeat was on the line.
    behind_idle: usize,
    refreshes: usize,
    heartbeats: usize,
    /// Messages on the line, user and idle, in samples.
    airtime: u64,
    end: u64,
}

/// `notes` legato notes, paced as `cricket loopback --legato` paces them:
/// the next note is struck 300–600 ms after the previous strike (and not
/// before the previous NoteOn has arrived), the previous note is released
/// 60 ms after that strike.
fn legato(config: &PhyConfig, notes: usize) -> Legato {
    let rate = config.sample_rate_hz as u64;
    let (mut s, msg, rep) = scheduler(config);
    let tick = rate * TICK_MS / 1000;
    let overlap = rate * 60 / 1000;
    let interval = |i: usize| rate * (300 + (i as u64 * 7919 % 31) * 10) / 1000;

    let mut out = Legato::default();
    let mut pushed_at = std::collections::HashMap::new();
    // Pushes still to come, sorted by time.
    let mut pending: Vec<(u64, TransportMessage)> = Vec::new();
    let mut strikes = 0usize;
    let mut prev: Option<u8> = None;
    // The next strike: at `t_strike` unless the previous NoteOn arrives later.
    let mut t_strike = Some(rate / 2);
    let mut now = 0u64;
    let mut busy_until = 0u64;
    // End of the last refresh or heartbeat put on the line.
    let mut idle_until = 0u64;
    loop {
        if let Some(t) = t_strike {
            if t <= now && strikes < notes {
                let note = 48 + (strikes % 40) as u8;
                pending.push((t, on(note)));
                if let Some(p) = prev {
                    pending.push((t + overlap, off(p)));
                }
                prev = Some(note);
                strikes += 1;
                t_strike = None; // set when this NoteOn goes out
                if strikes == notes {
                    pending.push((t + overlap + rate, off(note)));
                }
            }
        }
        pending.sort_by_key(|e| e.0);
        while pending.first().is_some_and(|e| e.0 <= now) {
            let (at, m) = pending.remove(0);
            if let TransportMessage::NoteOn { note, .. } = m {
                pushed_at.insert(note, at);
            }
            s.push(m, at);
        }
        if strikes == notes && pending.is_empty() && s.queued() == 0 {
            break;
        }
        let next_push = pending
            .first()
            .map_or(u64::MAX, |e| e.0)
            .min(t_strike.unwrap_or(u64::MAX));
        if now < busy_until {
            now = busy_until.min(next_push.max(now + 1));
            continue;
        }
        match s.poll_detailed(now) {
            Some(r) => {
                if r.user {
                    if let TransportMessage::NoteOn { note, .. } = r.msg {
                        let at = pushed_at[&note];
                        out.latency.push(now - at + rep);
                        if at < idle_until {
                            out.behind_idle += 1;
                        }
                        t_strike = Some((at + interval(strikes - 1)).max(now + rep));
                    }
                } else {
                    idle_until = now + msg;
                    match r.msg {
                        TransportMessage::Heartbeat => out.heartbeats += 1,
                        _ => out.refreshes += 1,
                    }
                }
                out.airtime += msg;
                busy_until = now + msg;
            }
            None => now = (now + tick).min(next_push.max(now + 1)),
        }
    }
    out.end = now;
    out
}

fn stats(config: &PhyConfig, v: &[u64]) -> (f64, f64) {
    let mut sorted = v.to_vec();
    sorted.sort_unstable();
    let mean = sorted.iter().sum::<u64>() as f64 / sorted.len() as f64;
    let p95 = sorted[((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1)];
    (ms(config, mean as u64), ms(config, p95))
}

fn profiles() -> Vec<(&'static str, PhyConfig)> {
    vec![
        ("robust", PhyConfig::robust()),
        ("robust_repeat", PhyConfig::robust_repeat()),
        ("fast", PhyConfig::fast()),
        ("ltc_fec", PhyConfig::ltc_fec()),
        (
            "ltc14400_fec",
            PhyConfig::from_name("ltc14400_fec").unwrap(),
        ),
    ]
}

/// Legato NoteOn latency per profile. The spread of strike intervals makes
/// this the scheduler-only counterpart of `cricket loopback --legato`.
#[test]
fn legato_latency_report() {
    println!("\n| profile | airtime ms | NoteOn latency mean / p95 ms | behind a refresh or heartbeat | refreshes | heartbeats | line duty |");
    println!("|---|---|---|---|---|---|---|");
    for (name, config) in profiles() {
        let (_, msg, _) = scheduler(&config);
        let r = legato(&config, 200);
        let (mean, p95) = stats(&config, &r.latency);
        println!(
            "| {name} | {:.1} | {mean:.0} / {p95:.0} | {} of {} | {} | {} | {:.0} % |",
            ms(&config, msg),
            r.behind_idle,
            r.latency.len(),
            r.refreshes,
            r.heartbeats,
            100.0 * r.airtime as f64 / r.end as f64
        );
        assert_eq!(r.latency.len(), 200, "{name}: NoteOns dropped");
    }
}

/// Notes of an `n`-note chord (all struck at once) that are sent, and the
/// airtime until the last one, per profile.
#[test]
fn chord_delivery_report() {
    println!("\n| profile | chord size | notes sent | dropped (struck order, 1 = first) | last note after ms |");
    println!("|---|---|---|---|---|");
    for (name, config) in profiles() {
        for n in [4usize, 5, 6, 8, 10, 12] {
            let (mut s, msg, rep) = scheduler(&config);
            for k in 0..n {
                s.push(on(40 + k as u8), 0);
            }
            let mut sent = Vec::new();
            let mut t = 0u64;
            while s.queued() > 0 {
                if let Some(TransportMessage::NoteOn { note, .. }) = s.poll(t) {
                    sent.push(note - 40 + 1);
                }
                t += msg;
            }
            let dropped: Vec<usize> = (1..=n).filter(|k| !sent.contains(&(*k as u8))).collect();
            println!(
                "| {name} | {n} | {} | {:?} | {:.0} |",
                sent.len(),
                dropped,
                ms(&config, t - msg + rep)
            );
            assert!(
                sent.len() >= 4.min(n),
                "{name}: {n}-note chord lost too much"
            );
        }
    }
}
