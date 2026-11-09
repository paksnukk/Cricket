//! Stuck-note test (v1.1 plan A6, goal 2): random NoteOn/NoteOff pairs on
//! several MIDI channels sent through a corrupted channel must end with every
//! note released, either by its NoteOff or by the fail-safe burst (CC64 = 0,
//! CC123 on every channel).
//!
//! A small MIDI receiver model consumes the decoder's `ModemOutput::Midi`
//! messages in order and tracks the sounding notes. The stream mimics the real
//! encoder: messages are serialized back to back, idle periods carry a
//! Heartbeat every 500 ms, and some gaps between note messages are longer than
//! `link_dead_ms` (the link stays alive through them thanks to heartbeats).
//!
//! Size: 1503 pairs in total, 167 per configuration (3 profiles: robust,
//! robust_repeat and fast, x 3 corruption levels), SNR 20 dB, random start
//! offset and a random chunk size per `push_samples` call. The nine
//! configurations run on parallel threads and are
//! simulated once per test process (shared by the tests below).
//!
//! Run with `-- --nocapture` to see the per-configuration report.
//!
//! Idle TX slots use [`TxScheduler`]: any non-zero gap can start one packet, the
//! way the encoder does when it does not yet know the next note is coming.
//! Recently released notes are repeated up to 6 times each on the schedule
//! of spec §5.5 (the Fibonacci spread on the MFSK profiles, after a quiet
//! message time on Robust); Heartbeat slots carry pending repeats first.

mod support;

use cricket_core::modem::{
    refresh_quiet_samples, refresh_schedule, refresh_spacing_samples, repetition_samples,
    CricketModemRx, CricketModemTx, ModemOutput,
};
use cricket_core::phy::{Modulation, PhyConfig};
use cricket_core::transport::{TransportMessage, TxScheduler};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use support::{ChannelSim, XorShift64};

const PAIRS_PER_CONFIG: usize = 167;
const CORRUPTION_LEVELS: [f64; 3] = [0.005, 0.01, 0.02];
const SNR_DB: f32 = 20.0;
const CHUNK_SIZES: [usize; 6] = [64, 128, 240, 512, 1024, 4096];
const MAX_HELD: usize = 6;
/// MIDI channels the notes are spread over (1, 2, 10 and 16).
const CHANNELS: [u8; 4] = [0, 1, 9, 15];
/// TX keeps idling (heartbeats only) this long after the last NoteOff before
/// the "link alive" checkpoint; longer than `link_dead_ms`.
const IDLE_TAIL_MS: usize = 3000;

fn ms_to_samples(config: &PhyConfig, ms: usize) -> usize {
    config.sample_rate_hz as usize * ms / 1000
}

// ---------------------------------------------------------------------------
// MIDI receiver model
// ---------------------------------------------------------------------------

/// A note on a channel: (channel 0–15, note).
type Key = (u8, u8);

/// Minimal multi-channel MIDI synth state: held keys, notes ringing on the
/// sustain pedal, and the pedal itself, per channel.
#[derive(Debug, Default, Clone)]
struct MidiReceiver {
    held: BTreeSet<Key>,
    sustained: BTreeSet<Key>,
    sustain: [bool; 16],
    /// Notes silenced by CC123 / CC64 = 0 instead of their own NoteOff:
    /// (note, stream sample of the clear).
    cleared_by_burst: Vec<(Key, u64)>,
}

impl MidiReceiver {
    fn sounding(&self) -> BTreeSet<Key> {
        let mut s = self.held.clone();
        s.extend(
            self.sustained
                .iter()
                .filter(|(ch, _)| self.sustain[usize::from(*ch)]),
        );
        s
    }

    fn note_off(&mut self, key: Key) {
        if self.held.remove(&key) && self.sustain[usize::from(key.0)] {
            self.sustained.insert(key);
        }
    }

    fn apply(&mut self, msg: &[u8], at: u64) {
        let ch = msg[0] & 0x0F;
        match (msg[0] & 0xF0, msg.get(1).copied(), msg.get(2).copied()) {
            (0x90, Some(note), Some(0)) | (0x80, Some(note), _) => self.note_off((ch, note)),
            (0x90, Some(note), Some(_)) => {
                self.held.insert((ch, note));
                self.sustained.remove(&(ch, note));
            }
            (0xB0, Some(64), Some(v)) => {
                self.sustain[usize::from(ch)] = v >= 64;
                if v < 64 {
                    let (released, kept): (BTreeSet<Key>, BTreeSet<Key>) =
                        std::mem::take(&mut self.sustained)
                            .into_iter()
                            .partition(|k| k.0 == ch);
                    self.sustained = kept;
                    self.cleared_by_burst
                        .extend(released.into_iter().map(|k| (k, at)));
                }
            }
            (0xB0, Some(123), Some(_)) => {
                // All Notes Off releases keys; pedal-held notes keep ringing
                // until the pedal goes up (the fail-safe sends CC64 = 0 first).
                let (cleared, kept): (BTreeSet<Key>, BTreeSet<Key>) =
                    std::mem::take(&mut self.held)
                        .into_iter()
                        .partition(|k| k.0 == ch);
                self.held = kept;
                for k in cleared {
                    self.cleared_by_burst.push((k, at));
                    if self.sustain[usize::from(ch)] {
                        self.sustained.insert(k);
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// TX stream generator
// ---------------------------------------------------------------------------

/// A transmitted message and the stream sample where its audio starts.
struct Sent {
    msg: TransportMessage,
    start: usize,
}

struct Stream {
    audio: Vec<f32>,
    sent: Vec<Sent>,
    /// TX-side NoteOff times per note (stream samples), ascending.
    note_off_times: BTreeMap<Key, Vec<usize>>,
    /// Samples of one full transmission (all K repetitions).
    message_len: usize,
    /// End of the idle tail (TX alive, heartbeats only).
    alive_checkpoint: usize,
}

struct StreamBuilder {
    tx: CricketModemTx,
    scheduler: TxScheduler,
    period: usize,
    message_len: usize,
    audio: Vec<f32>,
    sent: Vec<Sent>,
    note_off_times: BTreeMap<Key, Vec<usize>>,
}

impl StreamBuilder {
    fn new(config: &PhyConfig) -> Self {
        let message_samples = repetition_samples(config) * config.repetition_k.max(1) as u64;
        let heartbeat_samples =
            config.sample_rate_hz as u64 * config.heartbeat_interval_ms as u64 / 1000;
        let mut tx = CricketModemTx::new(config.clone());
        let message_len = tx.encode_message(&TransportMessage::Heartbeat).len();
        Self {
            tx,
            scheduler: TxScheduler::new(config.sample_rate_hz, heartbeat_samples, message_samples)
                .with_refresh_schedule(refresh_schedule(config))
                .with_refresh_spacing(refresh_spacing_samples(config, config.sample_rate_hz))
                .with_refresh_quiet(refresh_quiet_samples(config, config.sample_rate_hz)),
            period: config.samples_per_symbol_period(),
            message_len,
            audio: Vec::new(),
            sent: Vec::new(),
            note_off_times: BTreeMap::new(),
        }
    }

    /// Pad with silence to `target`, rounded up to the symbol-period grid
    /// (`ChannelSim` corrupts grid-aligned symbol periods).
    fn pad_to(&mut self, target: usize) {
        let target = target.div_ceil(self.period) * self.period;
        if target > self.audio.len() {
            self.audio.resize(target, 0.0);
        }
    }

    fn encode_polled(&mut self, msg: TransportMessage) {
        let start = self.audio.len();
        if let TransportMessage::NoteOff { channel, note, .. } = msg {
            self.note_off_times
                .entry((channel, note))
                .or_default()
                .push(start);
        }
        let samples = if matches!(msg, TransportMessage::Heartbeat) {
            self.tx.encode_heartbeat()
        } else {
            self.tx.encode_message(&msg)
        };
        self.audio.extend(samples);
        debug_assert!(
            !matches!(self.tx.config().modulation, Modulation::Mfsk)
                || self.audio.len().is_multiple_of(self.period)
        );
        self.sent.push(Sent { msg, start });
    }

    fn drain_until(&mut self, target: usize) {
        while self.audio.len() < target {
            let now = self.audio.len() as u64;
            let remaining = target.saturating_sub(self.audio.len());
            if self.scheduler.queued() == 0 && remaining < self.message_len {
                // The leftover silence is shorter than a packet. The encoder
                // still starts one if a refresh or heartbeat is due; the next
                // note then waits out that packet.
                if let Some(msg) = self.scheduler.poll(now) {
                    self.encode_polled(msg);
                } else {
                    self.pad_to(target);
                }
                break;
            }
            if let Some(msg) = self.scheduler.poll(now) {
                self.encode_polled(msg);
                continue;
            }
            let next = self.scheduler.next_idle_sample().max(now + 1) as usize;
            self.pad_to(next.min(target));
        }
    }

    fn send(&mut self, msg: TransportMessage) {
        let now = self.audio.len() as u64;
        self.scheduler.push(msg, now);
        if let Some(m) = self.scheduler.poll(now) {
            self.encode_polled(m);
        }
    }

    /// Stay idle for `samples`, emitting keepalive / NoteOff refresh via the scheduler.
    fn idle(&mut self, samples: usize) {
        self.drain_until(self.audio.len() + samples);
    }
}

fn gap_ms(rng: &mut XorShift64) -> usize {
    let r = rng.next_f64();
    if r < 0.4 {
        0 // chord / fast run: back to back
    } else if r < 0.85 {
        20 + rng.below(380)
    } else if r < 0.97 {
        400 + rng.below(1100)
    } else {
        2200 + rng.below(1300) // longer than link_dead_ms
    }
}

fn build_stream(config: &PhyConfig, pairs: usize, seed: u64) -> Stream {
    let mut rng = XorShift64::new(seed);
    let mut b = StreamBuilder::new(config);
    b.idle(ms_to_samples(config, 200));
    let mut held: Vec<Key> = Vec::new();
    let mut ons = 0usize;
    while ons < pairs || !held.is_empty() {
        b.idle(ms_to_samples(config, gap_ms(&mut rng)));
        let start_note =
            ons < pairs && (held.is_empty() || (held.len() < MAX_HELD && rng.next_f64() < 0.55));
        if start_note {
            let (channel, note) = loop {
                let k = (
                    CHANNELS[rng.below(CHANNELS.len())],
                    36 + rng.below(61) as u8,
                );
                if !held.contains(&k) {
                    break k;
                }
            };
            let velocity = 1 + rng.below(127) as u8;
            held.push((channel, note));
            ons += 1;
            b.send(TransportMessage::NoteOn {
                channel,
                note,
                velocity,
            });
        } else {
            let (channel, note) = held.swap_remove(rng.below(held.len()));
            let velocity = rng.below(128) as u8;
            b.send(TransportMessage::NoteOff {
                channel,
                note,
                velocity,
            });
        }
    }
    b.idle(ms_to_samples(config, IDLE_TAIL_MS));
    let alive_checkpoint = b.audio.len();
    // TX stops: silence well past link-dead.
    let tail = ms_to_samples(config, config.link_dead_ms as usize + 500);
    b.pad_to(alive_checkpoint + tail);
    Stream {
        audio: b.audio,
        sent: b.sent,
        note_off_times: b.note_off_times,
        message_len: b.message_len,
        alive_checkpoint,
    }
}

// ---------------------------------------------------------------------------
// Simulation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RunResult {
    label: String,
    sent: usize,
    sent_notes: usize,
    delivered: usize,
    lost: usize,
    lost_note_offs: usize,
    false_msgs: Vec<TransportMessage>,
    /// Notes still sounding at the "link alive" checkpoint (TX idle and
    /// sending heartbeats for `IDLE_TAIL_MS` after the last NoteOff).
    sounding_while_alive: BTreeSet<Key>,
    /// Longest time (ms) a note kept sounding after its TX NoteOff, measured
    /// at the alive checkpoint for notes still sounding there.
    max_stuck_alive_ms: f64,
    /// Episodes where the model kept a note sounding after the TX released
    /// it (beyond the delivery latency allowance).
    stuck_episodes: usize,
    /// Stuck episodes longer than `link_dead_ms`.
    stuck_over_link_dead: usize,
    /// Longest stuck episode in ms.
    max_stuck_ms: f64,
    /// Final state after the trailing silence.
    final_sounding: BTreeSet<Key>,
    final_sustain: bool,
    /// Channels that carried notes in this run.
    channels_used: BTreeSet<u8>,
    /// Notes released by a mid-stream fail-safe burst (link-dead / Panic).
    burst_mid_stream: usize,
    /// Notes only released by the final link-dead burst.
    burst_final: usize,
    link_dead_events: usize,
}

/// Greedy in-order match of delivered messages against the sent list.
/// Returns (matched sent indices, unmatched delivered messages).
fn match_delivered(
    sent: &[Sent],
    delivered: &[TransportMessage],
) -> (Vec<usize>, Vec<TransportMessage>) {
    const LOOKAHEAD: usize = 512;
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    let mut next = 0usize;
    for d in delivered {
        let end = (next + LOOKAHEAD).min(sent.len());
        match (next..end).find(|&i| sent[i].msg == *d) {
            Some(i) => {
                matched.push(i);
                next = i + 1;
            }
            None => unmatched.push(d.clone()),
        }
    }
    (matched, unmatched)
}

/// Channel corruption on top of the base channel.
#[derive(Debug, Clone, Copy)]
enum Corruption {
    /// Wrong MFSK tone with this probability per symbol, at SNR 20 dB.
    WrongTone(f64),
    /// AWGN at this SNR (dB) plus impulse clicks: (per second, dB re peak).
    SnrClicks(f32, f32, f32),
}

fn run(name: &str, config: &PhyConfig, corruption: Corruption, seed: u64) -> RunResult {
    let stream = build_stream(config, PAIRS_PER_CONFIG, seed);
    let mut rng = XorShift64::new(seed ^ 0x5EED);
    let offset = rng.below(4 * config.samples_per_symbol_period());
    let sim = ChannelSim::new(config.clone(), rng.next_u64()).offset(offset);
    let rx_audio = match corruption {
        Corruption::WrongTone(p) => sim.snr_db(SNR_DB).symbol_error_prob(p),
        Corruption::SnrClicks(snr, rate, level) => {
            let sim = sim.snr_db(snr);
            if rate > 0.0 {
                sim.clicks(rate, level)
            } else {
                sim
            }
        }
    }
    .apply(&stream.audio);

    let alive_at = offset + stream.alive_checkpoint;
    let final_dead_from = alive_at as u64;
    let mut rx = CricketModemRx::new(config.clone());
    let mut model = MidiReceiver::default();
    let mut delivered = Vec::new();
    let mut snapshot: Option<MidiReceiver> = None;
    let mut link_dead_events = 0usize;
    // TX note timeline (start sample of each NoteOn / NoteOff), shifted into
    // RX stream time. A note the model sounds is stuck when the TX's latest
    // event for it is a NoteOff that started more than `allowance` (one full
    // transmission plus a chunk: worst-case delivery latency) ago.
    let allowance = stream.message_len + CHUNK_SIZES[CHUNK_SIZES.len() - 1];
    let timeline: Vec<(usize, Key, bool)> = stream
        .sent
        .iter()
        .filter_map(|s| match s.msg {
            TransportMessage::NoteOn { channel, note, .. } => {
                Some((s.start + offset, (channel, note), true))
            }
            TransportMessage::NoteOff { channel, note, .. } => {
                Some((s.start + offset, (channel, note), false))
            }
            _ => None,
        })
        .collect();
    let channels_used = timeline.iter().map(|(_, k, _)| k.0).collect();
    let mut tl_next = 0usize;
    let mut tx_last: BTreeMap<Key, (usize, bool)> = BTreeMap::new();
    let mut stuck_since: BTreeMap<Key, usize> = BTreeMap::new();
    let mut stuck_episodes: Vec<usize> = Vec::new();

    let mut pos = 0usize;
    while pos < rx_audio.len() {
        let mut end = (pos + CHUNK_SIZES[rng.below(CHUNK_SIZES.len())]).min(rx_audio.len());
        if pos < alive_at && end > alive_at {
            end = alive_at; // stop exactly at the checkpoint
        }
        for out in rx.push_samples(&rx_audio[pos..end]) {
            match out {
                ModemOutput::Midi(m) => model.apply(&m, end as u64),
                ModemOutput::Message(msg) => delivered.push(msg),
                ModemOutput::LinkDead => link_dead_events += 1,
                _ => {}
            }
        }
        pos = end;
        while tl_next < timeline.len() && timeline[tl_next].0 <= end {
            let (at, note, on) = timeline[tl_next];
            tx_last.insert(note, (at, on));
            tl_next += 1;
        }
        let stuck_now: BTreeSet<Key> = model
            .sounding()
            .into_iter()
            .filter(|n| matches!(tx_last.get(n), Some(&(at, false)) if at + allowance <= end))
            .collect();
        stuck_since.retain(|n, since| {
            let keep = stuck_now.contains(n);
            if !keep {
                stuck_episodes.push(end - *since);
            }
            keep
        });
        for n in stuck_now {
            stuck_since.entry(n).or_insert(end);
        }
        if pos == alive_at {
            snapshot = Some(model.clone());
        }
    }
    let at_alive = snapshot.expect("checkpoint reached");
    stuck_episodes.extend(stuck_since.values().map(|since| rx_audio.len() - since));
    let to_ms = |n: usize| n as f64 * 1000.0 / config.sample_rate_hz as f64;
    let link_dead = ms_to_samples(config, config.link_dead_ms as usize);

    let (matched, false_msgs) = match_delivered(&stream.sent, &delivered);
    let matched_set: BTreeSet<usize> = matched.iter().copied().collect();
    let lost_note_offs = stream
        .sent
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            matches!(s.msg, TransportMessage::NoteOff { .. }) && !matched_set.contains(i)
        })
        .count();
    let sent_notes = stream
        .sent
        .iter()
        .filter(|s| !matches!(s.msg, TransportMessage::Heartbeat))
        .count();

    // How long has each note sounding at the checkpoint been stuck?
    let stuck_ms = |note: Key, at: usize| -> f64 {
        let off = stream.note_off_times[&note]
            .iter()
            .rev()
            .find(|&&t| t + offset <= at)
            .copied()
            .unwrap_or(0);
        (at - offset - off) as f64 * 1000.0 / config.sample_rate_hz as f64
    };
    let sounding_while_alive = at_alive.sounding();
    let max_stuck_alive_ms = sounding_while_alive
        .iter()
        .map(|&n| stuck_ms(n, alive_at))
        .fold(0.0, f64::max);
    let burst_mid_stream = model
        .cleared_by_burst
        .iter()
        .filter(|(_, at)| *at <= final_dead_from)
        .count();

    RunResult {
        label: match corruption {
            Corruption::WrongTone(p) => format!("{name:10} p={:.1}%", p * 100.0),
            Corruption::SnrClicks(snr, rate, level) => {
                format!("{name:10} {snr}dB {rate}/s@{level:+}")
            }
        },
        sent: stream.sent.len(),
        sent_notes,
        delivered: matched.len(),
        lost: stream.sent.len() - matched.len(),
        lost_note_offs,
        false_msgs,
        sounding_while_alive,
        max_stuck_alive_ms,
        stuck_episodes: stuck_episodes.len(),
        stuck_over_link_dead: stuck_episodes.iter().filter(|&&d| d > link_dead).count(),
        max_stuck_ms: stuck_episodes.iter().map(|&d| to_ms(d)).fold(0.0, f64::max),
        final_sounding: model.sounding(),
        final_sustain: model.sustain.iter().any(|&s| s),
        channels_used,
        burst_mid_stream,
        burst_final: model.cleared_by_burst.len() - burst_mid_stream,
        link_dead_events,
    }
}

fn print_results(results: &[RunResult]) {
    println!(
        "\n{:<14} {:>5} {:>5} {:>5} {:>5} {:>6} {:>5} {:>6} {:>7} {:>9} {:>8} {:>8} {:>8} {:>4}",
        "config",
        "sent",
        "notes",
        "deliv",
        "lost",
        "lostNO",
        "false",
        "stuck",
        "stuck>LD",
        "maxStuck",
        "liveEnd",
        "burstMid",
        "burstEnd",
        "dead"
    );
    for r in results {
        println!(
            "{:<14} {:>5} {:>5} {:>5} {:>5} {:>6} {:>5} {:>6} {:>7} {:>7.0}ms {:>8} {:>8} {:>8} {:>4}",
            r.label,
            r.sent,
            r.sent_notes,
            r.delivered,
            r.lost,
            r.lost_note_offs,
            r.false_msgs.len(),
            r.stuck_episodes,
            r.stuck_over_link_dead,
            r.max_stuck_ms,
            r.sounding_while_alive.len(),
            r.burst_mid_stream,
            r.burst_final,
            r.link_dead_events,
        );
    }
}

fn results() -> &'static [RunResult] {
    static RESULTS: OnceLock<Vec<RunResult>> = OnceLock::new();
    RESULTS.get_or_init(|| {
        let mut cases = Vec::new();
        for (name, config) in [
            ("robust_repeat", PhyConfig::robust_repeat()),
            ("fast", PhyConfig::fast()),
            ("robust", PhyConfig::robust()),
        ] {
            for (i, &p) in CORRUPTION_LEVELS.iter().enumerate() {
                let seed = 0xA6_0000 + (cases.len() as u64) * 101 + i as u64;
                cases.push((name, config.clone(), Corruption::WrongTone(p), seed));
            }
        }
        let results: Vec<RunResult> = std::thread::scope(|s| {
            let handles: Vec<_> = cases
                .iter()
                .map(|(name, config, c, seed)| s.spawn(move || run(name, config, *c, *seed)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        print_results(&results);
        results
    })
}

/// No message may be delivered that was never sent (CRC must reject every
/// corrupted packet; dedup must not leak a repetition as a new message).
#[test]
fn no_false_messages() {
    for r in results() {
        assert!(
            r.false_msgs.is_empty(),
            "[{}] delivered messages never sent: {:?}",
            r.label,
            r.false_msgs
        );
    }
}

/// After the transmitter stops, link-dead fires and every note is released;
/// the sustain pedal is up.
#[test]
fn all_notes_released_after_link_dead() {
    for r in results() {
        assert!(
            r.final_sounding.is_empty() && !r.final_sustain,
            "[{}] still sounding after link-dead: {:?} (sustain {})",
            r.label,
            r.final_sounding,
            r.final_sustain
        );
    }
}

/// The runs exercise several MIDI channels (the fail-safe burst and the
/// NoteOff refreshes must work per channel).
#[test]
fn notes_span_several_channels() {
    for r in results() {
        assert_eq!(
            r.channels_used.len(),
            CHANNELS.len(),
            "[{}] channels used: {:?}",
            r.label,
            r.channels_used
        );
    }
}

/// While the transmitter is alive and idle (heartbeats only), no note may keep
/// sounding: a lost NoteOff must not leave a note stuck indefinitely.
#[test]
fn all_notes_released_while_link_alive() {
    for r in results() {
        assert!(
            r.sounding_while_alive.is_empty() && r.stuck_over_link_dead == 0,
            "[{}] {} stuck-note episodes longer than link_dead_ms (longest {:.0} ms); {} notes \
             still sounding {} ms after the last NoteOff with the link alive \
             (longest {:.0} ms after its NoteOff): {:?}",
            r.label,
            r.stuck_over_link_dead,
            r.max_stuck_ms,
            r.sounding_while_alive.len(),
            IDLE_TAIL_MS,
            r.max_stuck_alive_ms,
            r.sounding_while_alive
        );
    }
}

/// Back-to-back traffic report for the experimental LTC profiles, with and
/// without FEC (wrong-tone corruption is MFSK-specific; these get AWGN and
/// clicks instead). Not an assertion test apart from "no false messages".
///
/// ```sh
/// cargo test --release -p cricket_core --test stuck_notes -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement report; run in release with --ignored --nocapture"]
fn ltc_back_to_back_report() {
    let levels = [
        Corruption::SnrClicks(6.0, 0.0, 0.0),
        Corruption::SnrClicks(3.0, 0.0, 0.0),
        Corruption::SnrClicks(1.0, 0.0, 0.0),
        Corruption::SnrClicks(20.0, 200.0, 6.0),
    ];
    let mut cases = Vec::new();
    for (name, config) in [
        ("ltc", PhyConfig::ltc()),
        ("ltc_fec", PhyConfig::ltc_fec()),
        ("ltc2400", PhyConfig::ltc_with_bit_rate(2400)),
        ("ltc2400_fec", PhyConfig::ltc_fec_with_bit_rate(2400)),
    ] {
        for (i, &c) in levels.iter().enumerate() {
            cases.push((name, config.clone(), c, 0xB2B_0000 + i as u64));
        }
    }
    let results: Vec<RunResult> = std::thread::scope(|s| {
        let handles: Vec<_> = cases
            .iter()
            .map(|(name, config, c, seed)| s.spawn(move || run(name, config, *c, *seed)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    print_results(&results);
    for r in &results {
        assert!(r.false_msgs.is_empty(), "[{}] {:?}", r.label, r.false_msgs);
    }
}
