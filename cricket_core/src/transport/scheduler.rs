//! Pure TX message scheduler (spec §5.5): priority tiers, latest-value-wins
//! coalescing of continuous controllers, idle NoteOff refresh, controller
//! state refresh and keepalive, and a bounded NoteOn queue. Time is a
//! sample counter supplied by the caller.
//!
//! The caller encodes only what [`TxScheduler::poll`] returns and assumes each
//! returned message is transmitted immediately. Do not call `poll` again until
//! that transmission has *started*; pass `now_samples` as the stream sample index
//! where that transmission begins.

use super::message::PITCH_BEND_CENTER;
use super::note_set::NoteSet;
use super::TransportMessage;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

const REFRESH_CAPACITY: usize = 32;
const REFRESH_LIFETIME_MS: u64 = 10_000;
/// Earliest start of repeat `k` (1-based) of a released note's NoteOff, in
/// message times after the user NoteOff: a Fibonacci spread, so consecutive
/// copies do not fall into one dropout and a release costs a few idle slots
/// spread over seconds instead of a burst, while all six repeats still fit
/// within link-dead time on Fast (0.74 s) and Robust (2.3 s). No note stayed
/// stuck past link-dead time in `tests/stuck_notes.rs` (up to 2 % symbol
/// errors).
const REFRESH_SCHEDULE: [u64; 6] = [1, 2, 3, 5, 8, 13];
/// Repeats per released note.
const REFRESH_MAX_REPEATS: u32 = REFRESH_SCHEDULE.len() as u32;
/// NoteOn airtime budget: queued NoteOns are capped so that they occupy at
/// most this (spec §5.5), unless the per-profile minimum below is larger.
const NOTEON_BUDGET_MS: u64 = 250;
/// Queued NoteOns that are always accepted, so a chord survives even when
/// one message is longer than the budget (Robust: 877 ms for 5 notes).
const MIN_NOTE_ONS: usize = 4;
/// Upper bound of the per-profile minimum: a 10-note chord (two hands) on
/// a profile whose message is short enough for 1 s to hold more than that.
const MAX_MIN_NOTE_ONS: usize = 10;
/// Airtime within which the per-profile minimum of queued NoteOns must fit
/// (the tier-3 queue bound; a chord that long is sent whole anyway).
const MIN_NOTE_ONS_MS: u64 = 1000;
/// Default bound on the airtime of everything queued in tier 3 (NoteOns,
/// controllers, Program Changes, system common, plus the NoteOffs held
/// behind them): the oldest droppable message goes when a new one would
/// exceed it. Coalescing alone does not bound the queue: every discrete
/// message (a NoteOn, a Program Change) pins the controller values of its
/// channel queued before it, and discrete messages other than NoteOn had
/// no budget of their own.
const QUEUE_BUDGET_MS: u64 = 1000;
/// Controllers that hold notes (sustain, sostenuto, hold 2). A NoteOff never
/// overtakes a queued one on its channel: pedal down then key up must not
/// arrive as key up then pedal down.
const PEDAL_CONTROLLERS: [u8; 3] = [64, 66, 69];
/// Channel state the transmitter re-sends in idle Heartbeat slots while it
/// differs from the default (spec §5.5, controller refresh): modulation
/// (CC 1, default 0), the pedals (CC 64, 66, 67, 69; held from 64 up) and
/// pitch bend (center). A receiver that restarted or ran its fail-safe
/// has lost them.
const STATE_CONTROLLERS: [u8; 5] = [1, 64, 66, 67, 69];

#[derive(Clone, Copy)]
struct RefreshEntry {
    channel: u8,
    note: u8,
    velocity: u8,
    /// Stream sample when the original user NoteOff was emitted.
    emitted_at: u64,
    sent: u32,
}

impl RefreshEntry {
    fn is(&self, channel: u8, note: u8) -> bool {
        self.channel == channel && self.note == note
    }

    fn message(&self) -> TransportMessage {
        TransportMessage::NoteOff {
            channel: self.channel,
            note: self.note,
            velocity: self.velocity,
        }
    }
}

/// Per-channel controller and pitch-bend state as sent, for the controller
/// refresh (`None`: never sent, or reset by CC 121).
#[derive(Clone, Copy, Default)]
struct ChannelState {
    controllers: [Option<u8>; STATE_CONTROLLERS.len()],
    bend: Option<u16>,
}

impl ChannelState {
    /// The messages of this channel that differ from the default state.
    fn stale(&self, channel: u8, out: &mut Vec<TransportMessage>) {
        for (i, &controller) in STATE_CONTROLLERS.iter().enumerate() {
            if let Some(value) = self.controllers[i] {
                let default = if controller == 1 {
                    value == 0
                } else {
                    value < 64
                };
                if !default {
                    out.push(TransportMessage::ControlChange {
                        channel,
                        controller,
                        value,
                    });
                }
            }
        }
        if let Some(value) = self.bend {
            if value != PITCH_BEND_CENTER {
                out.push(TransportMessage::PitchBend { channel, value });
            }
        }
    }
}

/// A NoteOff waiting for the queued message it must not overtake.
#[derive(Clone, Copy)]
struct HeldOff {
    channel: u8,
    note: u8,
    velocity: u8,
    push_sample: u64,
}

impl HeldOff {
    fn message(&self) -> TransportMessage {
        TransportMessage::NoteOff {
            channel: self.channel,
            note: self.note,
            velocity: self.velocity,
        }
    }
}

struct Queued {
    msg: TransportMessage,
    push_sample: u64,
    /// NoteOffs that arrived while this message (a NoteOn for their note, or
    /// a pedal controller on their channel) was queued. They are sent right
    /// after it: a short note is not released before it is struck, and a
    /// key released after the pedal went down stays sustained.
    held: Vec<HeldOff>,
}

impl Queued {
    fn new(msg: TransportMessage, push_sample: u64) -> Self {
        Self {
            msg,
            push_sample,
            held: Vec::new(),
        }
    }

    /// Whether a NoteOff for (`channel`, `note`) must wait behind this.
    fn holds_release_of(&self, channel: u8, note: u8) -> bool {
        match self.msg {
            TransportMessage::NoteOn {
                channel: c,
                note: n,
                ..
            } => c == channel && n == note,
            TransportMessage::ControlChange {
                channel: c,
                controller,
                ..
            } => c == channel && PEDAL_CONTROLLERS.contains(&controller),
            _ => false,
        }
    }
}

/// Coalescing key of a continuous update: (kind, channel, controller / note).
type Key = (u8, u8, u8);

/// How the scheduler treats a message (spec §5.5).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Panic, and the channel mode messages that end all notes on a channel
    /// (CC 120 All Sound Off, 123 All Notes Off, 124–127 Omni / Mono / Poly).
    Panic,
    /// MIDI Show Control: a cue must not wait behind a chord release.
    Msc,
    /// NoteOff.
    NoteOff,
    /// Continuous data, latest value wins: CC (except below), pitch bend,
    /// channel and poly pressure.
    Continuous(Key),
    /// Everything else, never coalesced: NoteOn, Program Change, Bank Select
    /// (CC 0 / 32), RPN / NRPN / Data Entry (CC 6, 38, 96–101), CC 121 / 122,
    /// system common.
    Discrete,
}

fn class(msg: &TransportMessage) -> Class {
    use TransportMessage as M;
    match *msg {
        M::Panic => Class::Panic,
        M::ControlChange {
            controller: 120 | 123..=127,
            ..
        } => Class::Panic,
        M::Msc(_) => Class::Msc,
        M::NoteOff { .. } => Class::NoteOff,
        M::ControlChange {
            channel,
            controller,
            ..
        } if !matches!(controller, 0 | 32 | 6 | 38 | 96..=101 | 120..=127) => {
            Class::Continuous((0, channel, controller))
        }
        M::PitchBend { channel, .. } => Class::Continuous((1, channel, 0)),
        M::ChannelPressure { channel, .. } => Class::Continuous((2, channel, 0)),
        M::PolyPressure { channel, note, .. } => Class::Continuous((3, channel, note)),
        _ => Class::Discrete,
    }
}

/// Whether two messages' channel scopes overlap (`None` = not a channel
/// message, overlaps everything).
fn overlaps(a: Option<u8>, b: Option<u8>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

fn is_note_on(msg: &TransportMessage) -> bool {
    matches!(msg, TransportMessage::NoteOn { .. })
}

/// Result of [`TxScheduler::poll_detailed`].
pub struct PollResult {
    pub msg: TransportMessage,
    /// True for messages from the user queues (everything except NoteOff
    /// refreshes, controller refreshes and Heartbeats).
    pub user: bool,
}

/// TX-side scheduler shared by the engine encoder and offline stream builders.
pub struct TxScheduler {
    heartbeat_interval_samples: u64,
    message_samples: u64,
    refresh_lifetime_samples: u64,
    queue_budget_samples: u64,
    /// Most NoteOns ever queued (spec §5.5): the NoteOn airtime budget, or
    /// the per-profile minimum if that is larger.
    max_note_ons: usize,

    /// Panic-class messages.
    panic_q: VecDeque<Queued>,
    /// MSC, in arrival order.
    msc_q: VecDeque<Queued>,
    /// NoteOff, in arrival order.
    off_q: VecDeque<Queued>,
    /// Everything else (NoteOn, controllers, Program Change, system common).
    normal_q: VecDeque<Queued>,
    /// Running counts over `normal_q`, so the budget checks on every push
    /// do not scan the queue: NoteOns and NoteOffs held behind any message.
    note_ons: usize,
    held_total: usize,

    /// Notes whose NoteOn has been emitted (sounding on the far side).
    sounding: NoteSet,
    /// Notes whose NoteOn was refused for the NoteOn limit and that are not
    /// sounding from an earlier strike: their NoteOff is not sent either.
    dropped: NoteSet,
    refresh: VecDeque<RefreshEntry>,
    /// Controller and bend state per channel, as sent.
    state: [ChannelState; 16],
    /// Round-robin position of the controller refresh.
    state_cursor: usize,

    last_emit_sample: Option<u64>,
    /// When the last user message was emitted (start sample).
    last_user_emit_sample: Option<u64>,
    /// Least time between the start of consecutive repeats of one released
    /// note (0: the schedule alone decides).
    refresh_spacing_samples: u64,
    /// Idle time the line must have had, after the message in flight, before
    /// a NoteOff refresh may start (default 0: the first free slot), so a
    /// note struck right after another never waits behind a refresh. See
    /// [`with_refresh_quiet`](Self::with_refresh_quiet).
    refresh_quiet_samples: u64,
    /// NoteOns refused because the NoteOn limit ([`max_note_ons`](Self::max_note_ons))
    /// was reached, since the caller last cleared it. The caller reports
    /// and clears it.
    pub note_ons_dropped: u32,
    /// Tier-3 messages dropped because the queue exceeded its airtime bound
    /// ([`with_queue_budget`](Self::with_queue_budget)), since the caller
    /// last cleared it.
    pub queue_dropped: u32,
}

impl TxScheduler {
    pub fn new(sample_rate_hz: u32, heartbeat_interval_samples: u64, message_samples: u64) -> Self {
        let hz = sample_rate_hz.max(1) as u64;
        let msg = message_samples.max(1);
        let by_budget = (hz * NOTEON_BUDGET_MS / 1000 / msg) as usize;
        let by_second = (hz * MIN_NOTE_ONS_MS / 1000 / msg) as usize;
        let min_kept = by_second.clamp(MIN_NOTE_ONS, MAX_MIN_NOTE_ONS);
        Self {
            heartbeat_interval_samples,
            message_samples,
            refresh_lifetime_samples: hz * REFRESH_LIFETIME_MS / 1000,
            queue_budget_samples: hz * QUEUE_BUDGET_MS / 1000,
            max_note_ons: by_budget.max(min_kept),
            panic_q: VecDeque::new(),
            msc_q: VecDeque::new(),
            off_q: VecDeque::new(),
            normal_q: VecDeque::new(),
            note_ons: 0,
            held_total: 0,
            sounding: NoteSet::default(),
            dropped: NoteSet::default(),
            refresh: VecDeque::new(),
            state: [ChannelState::default(); 16],
            state_cursor: 0,
            last_emit_sample: None,
            last_user_emit_sample: None,
            refresh_spacing_samples: 0,
            refresh_quiet_samples: 0,
            note_ons_dropped: 0,
            queue_dropped: 0,
        }
    }

    /// Bound the airtime of everything queued in tier 3 (NoteOns,
    /// controllers, Program Changes, system common and the NoteOffs held
    /// behind them) to `samples` (default: 1 s). Before a tier-3 message is
    /// queued, the oldest droppable messages are dropped until it fits:
    /// every tier-3 message except NoteOns (which have their own limit,
    /// [`max_note_ons`](Self::max_note_ons)) and pedal messages with
    /// NoteOffs held behind them, so the queue can exceed the bound only by
    /// those. Panic-class messages, MSC and NoteOffs are never dropped and
    /// have no bound: their queues grow with the input.
    pub fn with_queue_budget(mut self, samples: u64) -> Self {
        self.queue_budget_samples = samples;
        self
    }

    /// The tier-3 airtime bound, in samples (see
    /// [`with_queue_budget`](Self::with_queue_budget)).
    pub fn queue_budget_samples(&self) -> u64 {
        self.queue_budget_samples
    }

    /// Most NoteOns ever queued: ⌊250 ms / message⌋, but at least the
    /// per-profile minimum min(10, max(4, ⌊1 s / message⌋)): 5 on Robust,
    /// 4 on Robust repeat, 10 on Fast, 11 on `ltc_fec`, 14 on `ltc`. A
    /// NoteOn arriving beyond it is refused (drop-newest) and counted in
    /// [`note_ons_dropped`](Self::note_ons_dropped): for a chord the notes
    /// struck first (the bass, the hand that leads) are the ones that go
    /// out, and a note that would only arrive late is reported instead.
    pub fn max_note_ons(&self) -> usize {
        self.max_note_ons
    }

    /// Space the repeats of one released note at least `samples` apart
    /// (start to start, counted from the user NoteOff too), where the
    /// doubling schedule would put them closer. For profiles whose messages
    /// are a few samples long (`digital`), where repeats within one message
    /// time would all fall into one short dropout; 0 (the default) leaves
    /// the schedule alone.
    pub fn with_refresh_spacing(mut self, samples: u64) -> Self {
        self.refresh_spacing_samples = samples;
        self
    }

    /// Idle line required after the message in flight before a due NoteOff
    /// repeat starts; 0 (the default) starts it in the first free slot.
    /// With one message time (`crate::modem::refresh_quiet_samples`, the
    /// coded profiles) a note struck shortly after another never waits
    /// behind a repeat, and under sustained playing the repeats wait for a
    /// pause or the Heartbeat slot: the trade for profiles that lose few
    /// NoteOffs. Uncoded profiles lose many more and keep the first free
    /// slot.
    pub fn with_refresh_quiet(mut self, samples: u64) -> Self {
        self.refresh_quiet_samples = samples;
        self
    }

    /// Earliest start of the next repeat of `e`.
    fn refresh_due(&self, e: &RefreshEntry) -> u64 {
        let k = u64::from(e.sent);
        let by_schedule = REFRESH_SCHEDULE[e.sent as usize] * self.message_samples;
        let by_spacing = (k + 1) * self.refresh_spacing_samples;
        e.emitted_at + by_schedule.max(by_spacing)
    }

    /// Earliest sample a NoteOff refresh may start after the last message:
    /// the message in flight plus the quiet time.
    fn quiet_from(&self) -> u64 {
        self.last_emit_sample
            .map_or(0, |t| t + self.message_samples + self.refresh_quiet_samples)
    }

    /// Earliest start of the next repeat of `e`: when due and the line has
    /// been quiet.
    fn refresh_eligible_from(&self, e: &RefreshEntry) -> u64 {
        self.refresh_due(e).max(self.quiet_from())
    }

    /// While NoteOff refreshes are pending, the earliest sample the next one
    /// may start at (`None`: no refresh pending).
    pub fn next_refresh_sample(&self) -> Option<u64> {
        self.refresh
            .iter()
            .map(|e| self.refresh_eligible_from(e))
            .min()
    }

    /// Earliest sample at which an idle poll returns something: a due
    /// refresh, or the Heartbeat slot (which a pending refresh or a stale
    /// controller state may take instead of a Heartbeat).
    pub fn next_idle_sample(&self) -> u64 {
        let keepalive = self.next_keepalive_sample();
        self.next_refresh_sample()
            .map_or(keepalive, |due| due.min(keepalive))
    }

    /// Messages waiting to be sent (held NoteOffs count).
    pub fn queued(&self) -> usize {
        self.panic_q.len()
            + self.msc_q.len()
            + self.off_q.len()
            + self.normal_q.len()
            + self.held_total
    }

    /// How long the oldest queued message has waited at `now_samples`.
    pub fn oldest_wait_samples(&self, now_samples: u64) -> u64 {
        [&self.panic_q, &self.msc_q, &self.off_q, &self.normal_q]
            .iter()
            .filter_map(|q| q.iter().map(|e| e.push_sample).min())
            .min()
            .map_or(0, |oldest| now_samples.saturating_sub(oldest))
    }

    /// Earliest sample for the next Heartbeat slot (one heartbeat interval
    /// after the last emission of any kind).
    pub fn next_keepalive_sample(&self) -> u64 {
        match self.last_emit_sample {
            Some(last) => last + self.heartbeat_interval_samples,
            None => self.heartbeat_interval_samples,
        }
    }

    pub fn push(&mut self, msg: TransportMessage, push_sample: u64) {
        match class(&msg) {
            Class::Panic => {
                // A NoteOn queued before it would sound after it. Its held
                // NoteOffs go with it: the Panic releases the note anyway.
                let scope = msg.channel();
                let mut i = 0;
                while i < self.normal_q.len() {
                    let q = &self.normal_q[i];
                    if is_note_on(&q.msg) && overlaps(scope, q.msg.channel()) {
                        self.remove_normal(i);
                    } else {
                        i += 1;
                    }
                }
                match scope {
                    Some(ch) => self.dropped.clear_channel(ch),
                    None => self.dropped.clear(),
                }
                self.panic_q.push_back(Queued::new(msg, push_sample));
            }
            Class::Msc => self.msc_q.push_back(Queued::new(msg, push_sample)),
            Class::NoteOff => {
                let TransportMessage::NoteOff {
                    channel,
                    note,
                    velocity,
                } = msg
                else {
                    unreachable!("NoteOff class")
                };
                // Release after the latest queued strike of this note or
                // pedal on its channel, if any; otherwise the note is (or
                // will be) sounding from an earlier, already sent NoteOn
                // and the release goes out with priority.
                if let Some(i) = self
                    .normal_q
                    .iter()
                    .rposition(|q| q.holds_release_of(channel, note))
                {
                    self.hold_off(
                        i,
                        HeldOff {
                            channel,
                            note,
                            velocity,
                            push_sample,
                        },
                    );
                    return;
                }
                // The strike was refused and the note is not sounding from
                // an earlier one: the far end never heard it.
                if self.dropped.contains(channel, note) && !self.sounding.contains(channel, note) {
                    self.dropped.remove(channel, note);
                    return;
                }
                self.off_q.push_back(Queued::new(msg, push_sample));
            }
            Class::Continuous(key) => {
                if !self.coalesce(&msg, key) {
                    self.enforce_queue_budget();
                    self.push_normal(Queued::new(msg, push_sample));
                }
            }
            Class::Discrete => {
                if let TransportMessage::NoteOn { channel, note, .. } = msg {
                    if self.note_ons >= self.max_note_ons {
                        self.note_ons_dropped = self.note_ons_dropped.saturating_add(1);
                        self.dropped.insert(channel, note);
                        return;
                    }
                    self.dropped.remove(channel, note);
                }
                self.enforce_queue_budget();
                self.push_normal(Queued::new(msg, push_sample));
            }
        }
    }

    /// Drop the oldest droppable tier-3 messages until one more message of
    /// `message_samples` fits the queue's airtime bound. Strictly oldest
    /// first, so a controller value never outlives a discrete message that
    /// kept it apart from a newer value: no merging is needed afterwards.
    fn enforce_queue_budget(&mut self) {
        while self
            .tier3_airtime_samples()
            .saturating_add(self.message_samples)
            > self.queue_budget_samples
        {
            // NoteOns have their own limit; a pedal with NoteOffs held
            // behind it is never dropped (the held releases must not
            // overtake it).
            let Some(pos) = self
                .normal_q
                .iter()
                .position(|q| !is_note_on(&q.msg) && q.held.is_empty())
            else {
                break;
            };
            self.drop_normal(pos);
            self.queue_dropped = self.queue_dropped.saturating_add(1);
        }
    }

    /// Latest value wins: replace the newest queued update with the same key
    /// in place, unless a message it must stay ordered with is queued after
    /// it (a non-continuous message of the same channel or a system message,
    /// or NoteOffs held behind a pedal). Updates of other controllers are
    /// independent and do not block. Returns false if `msg` must be appended.
    fn coalesce(&mut self, msg: &TransportMessage, key: Key) -> bool {
        let channel = Some(key.1);
        for q in self.normal_q.iter_mut().rev() {
            let same_channel = overlaps(q.msg.channel(), channel);
            match class(&q.msg) {
                Class::Continuous(k) if k == key => {
                    if !q.held.is_empty() {
                        return false;
                    }
                    q.msg = msg.clone();
                    return true;
                }
                Class::Continuous(_) if q.held.is_empty() || !same_channel => {}
                _ if same_channel => return false,
                _ => {}
            }
        }
        false
    }

    pub fn poll(&mut self, now_samples: u64) -> Option<TransportMessage> {
        self.poll_detailed(now_samples).map(|r| r.msg)
    }

    pub fn poll_detailed(&mut self, now_samples: u64) -> Option<PollResult> {
        if let Some(q) = self.panic_q.pop_front() {
            match q.msg.channel() {
                Some(ch) => self.sounding.clear_channel(ch),
                None => self.sounding.clear(),
            }
            return Some(self.emit(q.msg, now_samples, true));
        }
        if let Some(q) = self.msc_q.pop_front() {
            return Some(self.emit(q.msg, now_samples, true));
        }
        if let Some(q) = self.off_q.pop_front() {
            if let TransportMessage::NoteOff {
                channel,
                note,
                velocity,
            } = q.msg
            {
                self.sounding.remove(channel, note);
                self.push_refresh(channel, note, velocity, now_samples);
            }
            return Some(self.emit(q.msg, now_samples, true));
        }
        if let Some(i) = self.next_normal() {
            let q = self.remove_normal(i);
            if let TransportMessage::NoteOn { channel, note, .. } = q.msg {
                self.cancel_refresh(channel, note);
                self.sounding.insert(channel, note);
                self.dropped.remove(channel, note);
            }
            for h in &q.held {
                self.off_q
                    .push_back(Queued::new(h.message(), h.push_sample));
            }
            return Some(self.emit(q.msg, now_samples, true));
        }

        self.prune_refresh(now_samples);
        if self.last_emit_sample == Some(now_samples) {
            return None;
        }

        // A due repeat, once the line has been quiet.
        if let Some(i) = self.pick_refresh(|s, e| s.refresh_eligible_from(e) <= now_samples) {
            return Some(self.emit_refresh(i, now_samples));
        }
        if now_samples < self.next_keepalive_sample() {
            return None;
        }
        // The Heartbeat slot: a pending repeat, then a stale controller
        // state, then a Heartbeat. Each keeps the link alive.
        if let Some(i) = self.pick_refresh(|_, _| true) {
            return Some(self.emit_refresh(i, now_samples));
        }
        if let Some(msg) = self.next_state_refresh() {
            return Some(self.emit(msg, now_samples, false));
        }
        Some(self.emit(TransportMessage::Heartbeat, now_samples, false))
    }

    /// Index of the next normal-tier message. Non-continuous messages go in
    /// arrival order, each after the continuous updates of its own channel
    /// queued before it, but ahead of those of other channels (a NoteOn on
    /// one channel does not wait behind a fader on another). With only
    /// continuous updates queued, the oldest goes first.
    fn next_normal(&self) -> Option<usize> {
        let first_discrete = self
            .normal_q
            .iter()
            .position(|q| !matches!(class(&q.msg), Class::Continuous(_)));
        let Some(d) = first_discrete else {
            return (!self.normal_q.is_empty()).then_some(0);
        };
        // Everything before `d` is a continuous update; the first one of
        // `d`'s channel (or `d` itself) goes now.
        let scope = self.normal_q[d].msg.channel();
        self.normal_q
            .iter()
            .position(|e| overlaps(e.msg.channel(), scope))
    }

    fn emit(&mut self, msg: TransportMessage, now_samples: u64, user: bool) -> PollResult {
        self.last_emit_sample = Some(now_samples);
        if user {
            self.last_user_emit_sample = Some(now_samples);
            self.track_state(&msg);
        }
        PollResult { msg, user }
    }

    /// Remember the controller and bend state a user message leaves at the
    /// far end (see `STATE_CONTROLLERS`).
    fn track_state(&mut self, msg: &TransportMessage) {
        match *msg {
            TransportMessage::ControlChange {
                channel,
                controller: 121,
                ..
            } => self.state[usize::from(channel & 0x0F)] = ChannelState::default(),
            TransportMessage::ControlChange {
                channel,
                controller,
                value,
            } => {
                if let Some(i) = STATE_CONTROLLERS.iter().position(|&c| c == controller) {
                    self.state[usize::from(channel & 0x0F)].controllers[i] = Some(value);
                }
            }
            TransportMessage::PitchBend { channel, value } => {
                self.state[usize::from(channel & 0x0F)].bend = Some(value);
            }
            _ => {}
        }
    }

    /// The next stale controller state to re-send, round robin over the
    /// channels and controllers that differ from their default.
    fn next_state_refresh(&mut self) -> Option<TransportMessage> {
        let mut stale = Vec::new();
        for (ch, state) in self.state.iter().enumerate() {
            state.stale(ch as u8, &mut stale);
        }
        if stale.is_empty() {
            return None;
        }
        self.state_cursor = (self.state_cursor + 1) % stale.len();
        Some(stale.swap_remove(self.state_cursor))
    }

    fn push_refresh(&mut self, channel: u8, note: u8, velocity: u8, emitted_at: u64) {
        self.cancel_refresh(channel, note);
        if self.refresh.len() >= REFRESH_CAPACITY {
            self.refresh.pop_front();
        }
        self.refresh.push_back(RefreshEntry {
            channel,
            note,
            velocity,
            emitted_at,
            sent: 0,
        });
    }

    /// The refresh entry to repeat next among those `eligible`: the note
    /// with the fewest repeats so far, then the oldest release. A fresh
    /// NoteOff is not stuck behind notes that already had several repeats,
    /// and the release that has waited longest is not stuck behind newer
    /// ones.
    fn pick_refresh(&self, eligible: impl Fn(&Self, &RefreshEntry) -> bool) -> Option<usize> {
        (0..self.refresh.len())
            .filter(|&i| eligible(self, &self.refresh[i]))
            .min_by_key(|&i| (self.refresh[i].sent, self.refresh[i].emitted_at))
    }

    fn emit_refresh(&mut self, i: usize, now_samples: u64) -> PollResult {
        let e = &mut self.refresh[i];
        e.sent += 1;
        let msg = e.message();
        if e.sent >= REFRESH_MAX_REPEATS {
            self.refresh.remove(i);
        }
        self.emit(msg, now_samples, false)
    }

    fn cancel_refresh(&mut self, channel: u8, note: u8) {
        self.refresh.retain(|e| !e.is(channel, note));
    }

    fn prune_refresh(&mut self, now_samples: u64) {
        self.refresh
            .retain(|e| now_samples.saturating_sub(e.emitted_at) <= self.refresh_lifetime_samples);
    }

    /// Airtime of everything queued in tier 3, held NoteOffs included.
    fn tier3_airtime_samples(&self) -> u64 {
        self.message_samples
            .saturating_mul((self.normal_q.len() + self.held_total) as u64)
    }

    fn push_normal(&mut self, q: Queued) {
        self.note_ons += usize::from(is_note_on(&q.msg));
        self.held_total += q.held.len();
        self.normal_q.push_back(q);
    }

    /// Remove `normal_q[i]`, keeping the running counts.
    fn remove_normal(&mut self, i: usize) -> Queued {
        let q = self.normal_q.remove(i).expect("index in range");
        self.note_ons -= usize::from(is_note_on(&q.msg));
        self.held_total -= q.held.len();
        q
    }

    /// Hold `h` behind `normal_q[i]` (once per note).
    fn hold_off(&mut self, i: usize, h: HeldOff) {
        let q = &mut self.normal_q[i];
        if q.held
            .iter()
            .any(|x| x.channel == h.channel && x.note == h.note)
        {
            return;
        }
        q.held.push(h);
        self.held_total += 1;
    }

    /// Drop `normal_q[pos]` for the queue bound. A held NoteOff goes with
    /// its pedal only if its note is not sounding; otherwise the release
    /// must still go out, with priority.
    fn drop_normal(&mut self, pos: usize) {
        let q = self.remove_normal(pos);
        for h in q.held {
            if self.sounding.contains(h.channel, h.note) {
                self.off_q
                    .push_back(Queued::new(h.message(), h.push_sample));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::repetition_samples;
    use crate::phy::PhyConfig;
    use crate::transport::msc::{MscCommand, MscCue, MscMessage};
    use alloc::vec;
    use TransportMessage::{ControlChange, Heartbeat, Panic, PitchBend, ProgramChange};

    /// A scheduler for `config` with the quiet rule of the coded profiles
    /// (one message time), whatever the profile, so the mechanism is
    /// tested on the short Fast messages too.
    fn scheduler(config: &PhyConfig) -> TxScheduler {
        let msg_samples = repetition_samples(config) * config.repetition_k.max(1) as u64;
        let hb = config.sample_rate_hz as u64 * config.heartbeat_interval_ms as u64 / 1000;
        TxScheduler::new(config.sample_rate_hz, hb, msg_samples).with_refresh_quiet(msg_samples)
    }

    fn fast_scheduler() -> TxScheduler {
        scheduler(&PhyConfig::fast())
    }

    /// Fast with Heartbeats so far apart that the slot never comes.
    fn fast_without_heartbeat() -> TxScheduler {
        let config = PhyConfig::fast();
        let msg = repetition_samples(&config) * config.repetition_k.max(1) as u64;
        TxScheduler::new(config.sample_rate_hz, 1 << 40, msg).with_refresh_quiet(msg)
    }

    fn on(note: u8) -> TransportMessage {
        on_ch(0, note)
    }

    fn on_ch(channel: u8, note: u8) -> TransportMessage {
        TransportMessage::NoteOn {
            channel,
            note,
            velocity: 100,
        }
    }

    fn off(note: u8) -> TransportMessage {
        off_ch(0, note)
    }

    fn off_ch(channel: u8, note: u8) -> TransportMessage {
        TransportMessage::NoteOff {
            channel,
            note,
            velocity: 64,
        }
    }

    fn cc(channel: u8, controller: u8, value: u8) -> TransportMessage {
        ControlChange {
            channel,
            controller,
            value,
        }
    }

    fn go() -> TransportMessage {
        TransportMessage::Msc(MscMessage::cue(
            MscCommand::Go,
            MscCue::from_tenths(10).unwrap(),
            None,
        ))
    }

    /// Plays `note` and releases it; returns the sample after the release.
    fn play_and_release(s: &mut TxScheduler, note: u8, t: u64) -> u64 {
        let msg = s.message_samples;
        s.push(on(note), t);
        assert_eq!(s.poll(t), Some(on(note)));
        s.push(off(note), t + msg);
        assert_eq!(s.poll(t + msg), Some(off(note)));
        t + msg
    }

    fn refresh_notes(s: &TxScheduler) -> Vec<u8> {
        s.refresh.iter().map(|e| e.note).collect()
    }

    /// Start of repeat `k` (1-based) of a NoteOff emitted at `t`, when the
    /// line is otherwise idle and no Heartbeat slot intervenes: the
    /// schedule, or one quiet message time after the previous emission
    /// ended, whichever is later.
    fn repeat_at(s: &TxScheduler, t: u64, k: u64) -> u64 {
        let msg = s.message_samples;
        let mut prev = t;
        for i in 1..=k {
            prev = (t + REFRESH_SCHEDULE[i as usize - 1] * msg).max(prev + 2 * msg);
        }
        prev
    }

    /// Everything queued, in send order (polling at one time: no refresh
    /// or heartbeat).
    fn drain(s: &mut TxScheduler) -> Vec<TransportMessage> {
        let msg = s.message_samples;
        let mut out = Vec::new();
        let mut t = 1_000_000;
        while s.queued() > 0 {
            out.push(s.poll(t).unwrap());
            t += msg;
        }
        out
    }

    #[test]
    fn priority_panic_note_off_note_on() {
        let mut s = fast_scheduler();
        s.push(off(70), 0);
        s.push(Panic, 0);
        s.push(on(60), 0);
        assert_eq!(s.poll(0), Some(Panic));
        assert_eq!(s.poll(0), Some(off(70)));
        assert_eq!(s.poll(0), Some(on(60)));
        assert_eq!(s.poll(0), None);
    }

    #[test]
    fn panic_discards_note_ons_queued_before_it() {
        let mut s = fast_scheduler();
        s.push(on(60), 0);
        s.push(on_ch(5, 61), 0);
        s.push(Panic, 0);
        assert_eq!(s.queued(), 1);
        assert_eq!(s.poll(0), Some(Panic));
        assert_eq!(s.poll(0), None);
    }

    /// All Notes Off / All Sound Off / mode changes are Panic-class for
    /// their channel only; Reset All Controllers and Local Control keep
    /// their place among the other messages.
    #[test]
    fn channel_mode_messages() {
        let mut s = fast_scheduler();
        s.push(on_ch(1, 60), 0);
        s.push(on_ch(2, 61), 0);
        s.push(cc(1, 7, 100), 0);
        s.push(cc(1, 121, 0), 0);
        s.push(cc(1, 123, 0), 0);
        assert_eq!(
            drain(&mut s),
            vec![cc(1, 123, 0), on_ch(2, 61), cc(1, 7, 100), cc(1, 121, 0)]
        );
        for c in [120, 124, 125, 126, 127] {
            assert!(class(&cc(0, c, 0)) == Class::Panic);
        }
        assert!(class(&cc(0, 122, 0)) == Class::Discrete);
        // Not coalesced: two in a row are both sent.
        s.push(cc(3, 123, 0), 0);
        s.push(cc(3, 123, 0), 0);
        assert_eq!(s.queued(), 2);
    }

    #[test]
    fn channel_all_notes_off_clears_only_its_channel() {
        let mut s = fast_scheduler();
        s.push(on_ch(1, 60), 0);
        s.push(on_ch(2, 60), 0);
        drain(&mut s);
        s.push(cc(1, 123, 0), 0);
        drain(&mut s);
        assert!(!s.sounding.contains(1, 60));
        assert!(s.sounding.contains(2, 60));
    }

    /// MSC goes ahead of NoteOffs (a cue must not wait behind a chord
    /// release) and both ahead of notes.
    #[test]
    fn msc_goes_ahead_of_note_offs_and_notes() {
        let mut s = fast_scheduler();
        s.push(on(60), 0);
        s.push(
            ProgramChange {
                channel: 0,
                program: 3,
            },
            0,
        );
        s.push(off(50), 0);
        s.push(off(51), 0);
        s.push(go(), 0);
        assert_eq!(s.queued(), 5);
        assert_eq!(
            drain(&mut s),
            vec![
                go(),
                off(50),
                off(51),
                on(60),
                ProgramChange {
                    channel: 0,
                    program: 3
                }
            ]
        );
    }

    #[test]
    fn no_heartbeat_while_queued_then_wait_interval() {
        let mut s = fast_scheduler();
        let hb = s.heartbeat_interval_samples;
        s.push(on(1), 0);
        assert_eq!(s.poll(0), Some(on(1)));
        assert_eq!(s.poll(hb - 1), None);
        assert_eq!(s.poll(hb), Some(Heartbeat));
    }

    /// The first repeat goes out once the line has been quiet for one
    /// message time after the NoteOff, the next ones on the Fibonacci
    /// schedule (2, 3, 5, 8, 13 message times after the NoteOff), each
    /// after a quiet message time.
    #[test]
    fn refresh_schedule_doubles_after_a_quiet_line() {
        let mut s = fast_without_heartbeat();
        let msg = s.message_samples;
        let t = play_and_release(&mut s, 60, 0);
        assert_eq!(s.poll(t), None);
        assert_eq!(s.poll(t + msg), None, "the line must be quiet first");
        assert_eq!(s.next_refresh_sample(), Some(t + 2 * msg));
        assert_eq!(s.next_idle_sample(), t + 2 * msg);
        assert_eq!(s.poll(t + 2 * msg - 1), None);
        assert_eq!(s.poll(t + 2 * msg), Some(off(60)));
        assert_eq!(s.poll(t + 2 * msg), None);
        // Repeat 2 is due at t + 2 msg but the line must be quiet until
        // t + 4 msg; repeat 3 is due at t + 3 msg, quiet until t + 6 msg.
        let mut prev = t + 2 * msg;
        for k in 2..=REFRESH_MAX_REPEATS as u64 {
            let due = repeat_at(&s, t, k);
            assert_eq!(s.next_refresh_sample(), Some(due), "repeat {k}");
            assert_eq!(s.poll(prev + msg), None, "repeat {k}: line busy");
            assert_eq!(s.poll(due - 1), None, "repeat {k}");
            assert_eq!(s.poll(due), Some(off(60)), "repeat {k}");
            prev = due;
        }
        assert!(refresh_notes(&s).is_empty());
        assert_eq!(s.next_refresh_sample(), None);
    }

    /// With no quiet requirement a due repeat takes the first free slot.
    #[test]
    fn refresh_without_quiet_takes_the_next_slot() {
        let mut s = fast_scheduler().with_refresh_quiet(0);
        let msg = s.message_samples;
        let t = play_and_release(&mut s, 60, 0);
        assert_eq!(s.poll(t + msg), Some(off(60)));
        assert_eq!(s.poll(t + 2 * msg), Some(off(60)));
        assert_eq!(s.poll(t + 3 * msg), Some(off(60)));
        assert_eq!(s.poll(t + 4 * msg), None, "repeat 4 is due at t + 5 msg");
        assert_eq!(s.poll(t + 5 * msg), Some(off(60)));
    }

    /// While repeats are pending but not due, the Heartbeat slot carries
    /// one of them instead of a Heartbeat (the link stays alive either
    /// way); only an empty refresh list yields Heartbeats.
    #[test]
    fn heartbeat_slot_carries_a_pending_refresh() {
        let mut s = scheduler(&PhyConfig::robust());
        let msg = s.message_samples;
        let hb = s.heartbeat_interval_samples;
        assert!(hb > 2 * msg && hb < 8 * msg);
        let t = play_and_release(&mut s, 60, 0);
        // Repeats 1–5 go on the schedule / quiet rule (t + 2, 4, 6, 8, 10
        // message times), each before the Heartbeat slot after the
        // previous one.
        let mut prev = t;
        for k in 1..=5u64 {
            let r = repeat_at(&s, t, k);
            assert!(r <= prev + hb, "repeat {k}");
            assert_eq!(s.poll(r), Some(off(60)), "repeat {k}");
            prev = r;
        }
        assert_eq!(prev, t + 10 * msg);
        // Repeat 6 is due at t + 13 msg; the Heartbeat slot comes first
        // and carries it, as idle traffic.
        let slot = prev + hb;
        assert!(slot < t + 13 * msg);
        assert_eq!(s.poll(slot - 1), None);
        assert_eq!(
            s.poll_detailed(slot).map(|r| (r.msg, r.user)),
            Some((off(60), false))
        );
        assert!(refresh_notes(&s).is_empty());
        assert_eq!(s.poll(slot + hb), Some(Heartbeat));
    }

    /// Idle Heartbeat slots re-send pedal, modulation and pitch-bend state
    /// that differs from the default, round robin, so a receiver that
    /// restarted recovers it; a value back at the default stops its
    /// refresh, and Reset All Controllers clears the channel's state.
    #[test]
    fn heartbeat_slot_carries_stale_controller_state() {
        let mut s = fast_scheduler();
        let hb = s.heartbeat_interval_samples;
        let bend = PitchBend {
            channel: 2,
            value: 9000,
        };
        for (i, m) in [cc(0, 64, 127), cc(0, 7, 100), bend.clone(), cc(5, 1, 40)]
            .into_iter()
            .enumerate()
        {
            s.push(m, i as u64);
            assert!(s.poll(i as u64).is_some());
        }
        let t = 3;
        let mut sent = Vec::new();
        for k in 1..=6u64 {
            let r = s.poll_detailed(t + k * hb).expect("idle slot");
            assert!(!r.user);
            sent.push(r.msg);
        }
        for m in [cc(0, 64, 127), bend.clone(), cc(5, 1, 40)] {
            assert_eq!(sent.iter().filter(|x| **x == m).count(), 2, "{m:?}");
        }
        assert!(!sent.contains(&cc(0, 7, 100)), "CC 7 is not state");
        assert!(!sent.contains(&Heartbeat));
        // Pedal up and modulation to 0: no longer stale; CC 121 on channel
        // 3 (= 2 here) clears its bend.
        let t = t + 6 * hb;
        s.push(cc(0, 64, 0), t);
        s.push(cc(5, 1, 0), t);
        s.push(cc(2, 121, 0), t);
        // The discrete CC 121 goes first (no continuous update of its
        // channel is queued before it), then the controller values.
        assert_eq!(s.poll(t), Some(cc(2, 121, 0)));
        assert_eq!(s.poll(t + 1), Some(cc(0, 64, 0)));
        assert_eq!(s.poll(t + 2), Some(cc(5, 1, 0)));
        assert_eq!(s.poll(t + 2 + hb), Some(Heartbeat));
        // A bend back to center is not stale either.
        s.push(bend, t + 3 + hb);
        s.poll(t + 3 + hb);
        s.push(
            PitchBend {
                channel: 2,
                value: crate::transport::PITCH_BEND_CENTER,
            },
            t + 4 + hb,
        );
        s.poll(t + 4 + hb);
        assert_eq!(s.poll(t + 4 + 2 * hb), Some(Heartbeat));
    }

    /// Refreshes repeat the NoteOff as sent (channel and velocity).
    #[test]
    fn refresh_keeps_channel_and_velocity() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        let release = TransportMessage::NoteOff {
            channel: 9,
            note: 38,
            velocity: 12,
        };
        s.push(on_ch(9, 38), 0);
        s.poll(0);
        s.push(release.clone(), msg);
        assert_eq!(s.poll(msg), Some(release.clone()));
        assert_eq!(s.poll(3 * msg), Some(release));
    }

    /// Among due repeats, the note with the fewest so far goes first, then
    /// the oldest release.
    #[test]
    fn two_notes_refresh_round_robin() {
        let mut s = fast_scheduler().with_refresh_quiet(0);
        let msg = s.message_samples;
        play_and_release(&mut s, 10, 0);
        let t = play_and_release(&mut s, 20, 2 * msg);
        // Both first repeats are due: the older release first.
        assert_eq!(s.poll(t + msg), Some(off(10)));
        assert_eq!(s.poll(t + 2 * msg), Some(off(20)));
        // Second repeats: note 10 is due (released at msg, 2 msg later =
        // 3 msg), note 20 at t + 2 msg = 5 msg; note 10's third repeat is
        // due at 4 msg but note 20 has fewer repeats.
        assert_eq!(s.poll(t + 3 * msg), Some(off(10)));
        assert_eq!(s.poll(t + 4 * msg), Some(off(20)));
        assert_eq!(s.poll(t + 5 * msg), Some(off(10)));
    }

    #[test]
    fn note_on_push_does_not_cancel_refresh() {
        let mut s = fast_scheduler();
        let t = play_and_release(&mut s, 60, 0);
        s.push(on(70), t + 1);
        assert!(refresh_notes(&s).contains(&60));
        assert_eq!(s.poll(t + s.message_samples), Some(on(70)));
        assert!(refresh_notes(&s).contains(&60));
    }

    #[test]
    fn note_on_emit_cancels_refresh() {
        let mut s = fast_scheduler();
        let hb = s.heartbeat_interval_samples;
        let t = play_and_release(&mut s, 60, 0);
        // The same note on another channel is another note.
        s.push(on_ch(1, 60), t + 1);
        assert_eq!(s.poll(t + 1), Some(on_ch(1, 60)));
        assert_eq!(refresh_notes(&s), vec![60]);
        s.push(on(60), t + 2);
        assert_eq!(s.poll(t + 2), Some(on(60)));
        assert!(refresh_notes(&s).is_empty());
        assert_eq!(s.poll(t + 2 + hb), Some(Heartbeat));
    }

    #[test]
    fn refresh_evicts_oldest_at_capacity() {
        let mut s = fast_scheduler();
        let mut t = 0u64;
        for note in 0..33u8 {
            s.push(on(note), t);
            let _ = s.poll(t);
            s.push(off(note), t + 1);
            let _ = s.poll(t + 1);
            t += 2;
        }
        assert!(!refresh_notes(&s).contains(&0));
        assert_eq!(s.poll(t + 2 * s.message_samples), Some(off(1)));
    }

    #[test]
    fn refresh_expires_after_lifetime() {
        let mut s = fast_scheduler();
        let hb = s.heartbeat_interval_samples;
        let t = play_and_release(&mut s, 60, 0);
        assert_eq!(s.poll(t + s.refresh_lifetime_samples + hb), Some(Heartbeat));
    }

    /// With a refresh spacing (digital profile), refreshes keep that distance
    /// from the user NoteOff and from each other, a NoteOn still goes at
    /// once, and no heartbeat fills the gaps.
    #[test]
    fn refresh_spacing_spreads_the_repeats() {
        let config = PhyConfig::digital();
        let spacing = crate::modem::refresh_spacing_samples(&config, 48_000);
        assert_eq!(spacing, 960);
        assert_eq!(
            crate::modem::refresh_spacing_samples(&PhyConfig::fast(), 48_000),
            0
        );
        let mut s = scheduler(&config).with_refresh_spacing(spacing);
        let msg = s.message_samples;
        let t = play_and_release(&mut s, 60, 0);
        assert_eq!(s.next_refresh_sample(), Some(t + spacing));
        for k in 1..=REFRESH_MAX_REPEATS as u64 {
            let due = t + k * spacing;
            assert_eq!(s.poll(due - spacing + msg), None, "repeat {k}");
            assert_eq!(s.poll(due - 1), None, "repeat {k}");
            assert_eq!(s.poll(due), Some(off(60)), "repeat {k}");
        }
        assert_eq!(s.next_refresh_sample(), None);
        let end = t + REFRESH_MAX_REPEATS as u64 * spacing;
        assert_eq!(s.poll(end + s.heartbeat_interval_samples), Some(Heartbeat));
        // User traffic is not held back by the spacing.
        let hb = s.heartbeat_interval_samples;
        let t = play_and_release(&mut s, 61, end + hb + msg);
        s.push(on(62), t + msg);
        assert_eq!(s.poll(t + msg), Some(on(62)));
    }

    /// Each release costs a bounded number of repeats, then heartbeats resume.
    #[test]
    fn refresh_stops_after_max_repeats() {
        let mut s = fast_scheduler();
        let t = play_and_release(&mut s, 60, 0);
        let mut last = t;
        for k in 1..=REFRESH_MAX_REPEATS as u64 {
            last = repeat_at(&s, t, k);
            assert_eq!(s.poll(last), Some(off(60)), "repeat {k}");
        }
        assert!(refresh_notes(&s).is_empty());
        assert_eq!(s.poll(last + s.message_samples), None);
        assert_eq!(s.poll(last + s.heartbeat_interval_samples), Some(Heartbeat));
    }

    #[test]
    fn note_off_while_note_on_queued_keeps_both_in_order() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        s.push(on(60), 0);
        s.push(off(60), 0);
        assert_eq!(s.queued(), 2);
        assert_eq!(s.poll(0), Some(on(60)));
        assert_eq!(s.poll(msg), Some(off(60)));
    }

    /// Off, on, off on a note that is already sounding (a trill or a roll)
    /// must end released, not with the NoteOn sent last.
    #[test]
    fn retrigger_while_sounding_keeps_order() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        s.push(on(60), 0);
        assert_eq!(s.poll(0), Some(on(60)));
        s.push(off(60), 10);
        s.push(on(60), 20);
        s.push(off(60), 30);
        s.push(on(60), 40);
        s.push(off(60), 50);
        let sent: Vec<_> = (1..=5).map(|i| s.poll(i * msg).unwrap()).collect();
        assert_eq!(sent, vec![off(60), on(60), off(60), on(60), off(60)]);
        assert_eq!(s.queued(), 0);
        assert_eq!(refresh_notes(&s), vec![60]);
    }

    #[test]
    fn emitted_note_off_never_dropped() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        s.push(on(60), 0);
        let _ = s.poll(0);
        s.push(off(60), msg);
        for i in 0..50u8 {
            s.push(on(70 + i), msg + i as u64);
        }
        assert_eq!(s.poll(msg), Some(off(60)));
    }

    /// Beyond the NoteOn limit the newest NoteOn is refused and counted;
    /// the ones struck first stay, and Panic still goes first.
    #[test]
    fn noteon_limit_refuses_newest_not_panic() {
        let mut s = fast_scheduler();
        let max = s.max_note_ons();
        assert_eq!(max, 10);
        for i in 0..max + 2 {
            s.push(on(i as u8), 0);
        }
        assert_eq!(s.note_ons_dropped, 2);
        assert_eq!(s.note_ons, max);
        s.push(Panic, 0);
        assert_eq!(s.poll(0), Some(Panic));
        assert_eq!(s.queued(), 0, "Panic discards the queued NoteOns");
    }

    /// A Robust message is longer than the NoteOn airtime budget; a chord
    /// of up to the per-profile minimum (4 on Robust repeat, 5 on Robust)
    /// pushed at once is still sent whole, first struck first.
    #[test]
    fn robust_chord_is_not_dropped() {
        let mut s = scheduler(&PhyConfig::robust_repeat());
        assert_eq!(s.max_note_ons(), 4);
        for n in [60u8, 64, 67, 72] {
            s.push(on(n), 0);
        }
        assert_eq!(s.note_ons_dropped, 0);
        let sent: Vec<_> = (0..4).map(|i| s.poll(i * s.message_samples)).collect();
        assert_eq!(
            sent,
            vec![Some(on(60)), Some(on(64)), Some(on(67)), Some(on(72))]
        );
        let mut s = scheduler(&PhyConfig::robust());
        assert_eq!(s.max_note_ons(), 5);
        for n in [48u8, 60, 64, 67, 72, 76] {
            s.push(on(n), 0);
        }
        assert_eq!(s.note_ons_dropped, 1);
        assert_eq!(
            drain(&mut s),
            vec![on(48), on(60), on(64), on(67), on(72)],
            "the bass stays, the last struck note goes"
        );
    }

    /// The NoteOff of a refused NoteOn is not sent (the far end never heard
    /// the note), unless the note is sounding from an earlier strike: then
    /// the release still goes out with priority. A later strike of the
    /// note is accepted normally.
    #[test]
    fn refused_note_on_suppresses_its_note_off_unless_sounding() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        s.push(on(60), 0);
        assert_eq!(s.poll(0), Some(on(60)));
        for n in 70..80u8 {
            s.push(on(n), 1);
        }
        assert_eq!(s.note_ons, s.max_note_ons());
        s.push(on(60), 2); // a re-strike, refused
        s.push(on(61), 2); // refused
        assert_eq!(s.note_ons_dropped, 2);
        let queued = s.queued();
        s.push(off(61), 3);
        assert_eq!(s.queued(), queued, "release of a note never struck");
        s.push(off(60), 3);
        assert_eq!(s.queued(), queued + 1, "release of the sounding note");
        assert_eq!(s.poll(msg), Some(off(60)));
        drain(&mut s);
        s.push(on(61), 100 * msg);
        s.push(off(61), 100 * msg);
        assert_eq!(s.queued(), 2);
        assert_eq!(s.poll(100 * msg), Some(on(61)));
        assert_eq!(s.poll(101 * msg), Some(off(61)));
        // Panic forgets the refused notes: a NoteOff afterwards is sent
        // (the far end may have the note from before the transmitter
        // started).
        s.push(on(62), 200 * msg);
        s.push(on(63), 200 * msg);
        for n in 70..80u8 {
            s.push(on(n), 200 * msg);
        }
        s.push(on(64), 200 * msg); // refused
        s.push(Panic, 200 * msg);
        assert_eq!(s.poll(200 * msg), Some(Panic));
        s.push(off(64), 201 * msg);
        assert_eq!(s.queued(), 1);
    }

    // -- Coalescing --------------------------------------------------------

    #[test]
    fn continuous_updates_coalesce_latest_value_wins() {
        let mut s = fast_scheduler();
        for v in 0..100u8 {
            s.push(cc(0, 7, v), 0);
            s.push(
                PitchBend {
                    channel: 0,
                    value: v as u16 * 100,
                },
                0,
            );
            s.push(
                TransportMessage::ChannelPressure {
                    channel: 0,
                    pressure: v,
                },
                0,
            );
            s.push(
                TransportMessage::PolyPressure {
                    channel: 0,
                    note: 60,
                    pressure: v,
                },
                0,
            );
            s.push(cc(1, 7, v), 0); // another channel: its own entry
        }
        assert_eq!(s.queued(), 5);
        assert_eq!(
            drain(&mut s),
            vec![
                cc(0, 7, 99),
                PitchBend {
                    channel: 0,
                    value: 9900
                },
                TransportMessage::ChannelPressure {
                    channel: 0,
                    pressure: 99
                },
                TransportMessage::PolyPressure {
                    channel: 0,
                    note: 60,
                    pressure: 99
                },
                cc(1, 7, 99),
            ]
        );
    }

    /// A controller value queued before a NoteOn of its channel stays before
    /// it (the note is struck with that value); newer values go after it.
    #[test]
    fn coalescing_never_crosses_a_note_of_the_channel() {
        let mut s = fast_scheduler();
        s.push(cc(0, 74, 10), 0);
        s.push(on(60), 0);
        s.push(cc(0, 74, 20), 0);
        s.push(cc(0, 74, 30), 0);
        s.push(cc(2, 74, 1), 0); // other channel
        s.push(cc(2, 74, 2), 0);
        assert_eq!(
            drain(&mut s),
            vec![cc(0, 74, 10), on(60), cc(0, 74, 30), cc(2, 74, 2)]
        );
    }

    /// Bank Select, RPN / NRPN and Data Entry are never coalesced and stay
    /// in order with the Program Change after them.
    #[test]
    fn bank_select_rpn_and_program_change_keep_order() {
        let mut s = fast_scheduler();
        let seq = vec![
            cc(0, 0, 1),
            cc(0, 32, 2),
            ProgramChange {
                channel: 0,
                program: 5,
            },
            cc(0, 101, 0),
            cc(0, 100, 0),
            cc(0, 6, 12),
            cc(0, 38, 0),
            cc(0, 101, 127),
            cc(0, 100, 127),
            cc(0, 0, 1),
            cc(0, 32, 3),
            ProgramChange {
                channel: 0,
                program: 6,
            },
            ProgramChange {
                channel: 0,
                program: 6,
            },
        ];
        for m in &seq {
            s.push(m.clone(), 0);
        }
        // A fader on the same channel between them does not move them.
        s.push(cc(0, 7, 1), 0);
        s.push(cc(0, 7, 2), 0);
        let mut want = seq.clone();
        want.push(cc(0, 7, 2));
        assert_eq!(drain(&mut s), want);
    }

    /// Pedal down then key up must not become key up then pedal down: the
    /// NoteOff waits behind the queued pedal, and the pedal is not
    /// coalesced past it.
    #[test]
    fn note_off_never_overtakes_sustain_pedal() {
        let mut s = fast_scheduler();
        s.push(on(60), 0);
        assert_eq!(s.poll(0), Some(on(60)));
        s.push(cc(0, 64, 127), 1);
        s.push(off(60), 2);
        s.push(off_ch(1, 61), 2); // other channel: not held
        s.push(cc(0, 64, 0), 3);
        assert_eq!(
            drain(&mut s),
            vec![off_ch(1, 61), cc(0, 64, 127), off(60), cc(0, 64, 0)]
        );
        // Without a NoteOff in between, pedal values coalesce.
        s.push(cc(0, 64, 127), 10);
        s.push(cc(0, 64, 0), 11);
        s.push(cc(0, 64, 90), 12);
        assert_eq!(drain(&mut s), vec![cc(0, 64, 90)]);
    }

    /// Notes on one channel do not wait behind controller updates of other
    /// channels; the controller updates follow.
    #[test]
    fn notes_overtake_controllers_of_other_channels_only() {
        let mut s = fast_scheduler();
        for c in 1..8u8 {
            s.push(cc(c, 7, 1), 0);
        }
        s.push(cc(0, 1, 5), 0);
        s.push(on(60), 0);
        s.push(on_ch(3, 61), 0);
        // Each NoteOn waits for its own channel's update only.
        assert_eq!(
            drain(&mut s),
            vec![
                cc(0, 1, 5),
                on(60),
                cc(3, 7, 1),
                on_ch(3, 61),
                cc(1, 7, 1),
                cc(2, 7, 1),
                cc(4, 7, 1),
                cc(5, 7, 1),
                cc(6, 7, 1),
                cc(7, 7, 1),
            ]
        );
        // System messages keep their place relative to everything.
        s.push(cc(4, 7, 1), 0);
        s.push(TransportMessage::SongSelect { song: 2 }, 0);
        s.push(on(1), 0);
        assert_eq!(
            drain(&mut s),
            vec![cc(4, 7, 1), TransportMessage::SongSelect { song: 2 }, on(1)]
        );
    }

    #[test]
    fn fast_10_per_sec_60s_latency_under_250ms() {
        let mut s = fast_scheduler();
        let hz = PhyConfig::fast().sample_rate_hz as u64;
        let msg = s.message_samples;
        let interval = hz / 10;
        let duration = hz * 60;
        let budget = hz * NOTEON_BUDGET_MS / 1000;
        let (mut next_push, mut stream, mut max_wait) = (0u64, 0u64, 0u64);
        let (mut note, mut user_emitted) = (0u8, 0u64);

        while stream < duration || s.queued() > 0 {
            while next_push <= stream && next_push < duration {
                s.push(on(note & 0x7F), next_push);
                note = note.wrapping_add(1);
                next_push += interval;
            }
            let wait = s.oldest_wait_samples(stream);
            if let Some(result) = s.poll_detailed(stream) {
                if result.user {
                    user_emitted += 1;
                    max_wait = max_wait.max(wait);
                }
                stream += msg;
            } else {
                stream += 1;
            }
        }
        assert!(user_emitted >= duration / interval);
        assert!(
            max_wait <= budget,
            "max queue wait {} samples (> {} = 250 ms)",
            max_wait,
            budget
        );
    }

    // -- Queue bound -------------------------------------------------------

    fn pc(program: u8) -> TransportMessage {
        ProgramChange {
            channel: 0,
            program,
        }
    }

    /// The running counts match the queue.
    fn check_counts(s: &TxScheduler) {
        let note_ons = s.normal_q.iter().filter(|q| is_note_on(&q.msg)).count();
        let held_total: usize = s.normal_q.iter().map(|q| q.held.len()).sum();
        assert_eq!((s.note_ons, s.held_total), (note_ons, held_total));
        assert_eq!(
            s.queued(),
            s.panic_q.len() + s.msc_q.len() + s.off_q.len() + s.normal_q.len() + held_total
        );
    }

    /// The queue bound drops the oldest droppable tier-3 message, Program
    /// Changes included, and counts it; a pedal with a NoteOff held behind
    /// it is never dropped.
    #[test]
    fn queue_bound_drops_oldest_discrete_messages() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        s = s.with_queue_budget(5 * msg);
        s.push(on(60), 0);
        assert_eq!(s.poll(0), Some(on(60)));
        s.push(cc(0, 64, 127), 1);
        s.push(off(60), 2); // held behind the pedal: 2 messages of airtime
        s.push(pc(1), 3);
        s.push(pc(2), 4);
        s.push(pc(3), 5); // 5 messages: full
        assert_eq!(s.queue_dropped, 0);
        s.push(pc(4), 6); // pc(1) goes, the pedal stays
        assert_eq!(s.queue_dropped, 1);
        s.push(on(70), 7); // pc(2) goes
        assert_eq!(s.queue_dropped, 2);
        assert_eq!(s.note_ons_dropped, 0);
        check_counts(&s);
        assert_eq!(
            drain(&mut s),
            vec![cc(0, 64, 127), off(60), pc(3), pc(4), on(70)]
        );
        check_counts(&s);
    }

    /// NoteOns are kept even when they alone exceed the bound (their own
    /// limit caps them); the bound drops the other messages around them.
    #[test]
    fn queue_bound_keeps_note_ons() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        s = s.with_queue_budget(2 * msg);
        for n in 60..64u8 {
            s.push(on(n), 0);
        }
        assert_eq!((s.queued(), s.queue_dropped), (4, 0));
        s.push(pc(1), 1); // nothing droppable: appended beyond the bound
        assert_eq!((s.queued(), s.queue_dropped), (5, 0));
        // A fifth NoteOn is accepted (the limit is 10); pc(1) goes for the
        // bound; the NoteOns stay.
        s.push(on(64), 2);
        assert_eq!(s.note_ons_dropped, 0);
        assert_eq!(s.queue_dropped, 1);
        check_counts(&s);
        assert_eq!(drain(&mut s), vec![on(60), on(61), on(62), on(63), on(64)]);
        // With a bound far below the NoteOn limit, NoteOns are still never
        // dropped for the bound.
        let mut s = scheduler(&PhyConfig::digital()).with_queue_budget(3 * 42);
        for n in 0..5u8 {
            s.push(on(n), 0);
        }
        assert_eq!((s.queued(), s.queue_dropped), (5, 0));
        assert_eq!(s.note_ons_dropped, 0);
        assert_eq!(drain(&mut s), vec![on(0), on(1), on(2), on(3), on(4)]);
    }

    /// The bound drops strictly oldest first, system common messages and
    /// controller values included (only the protected NoteOns and held
    /// pedals are skipped), so the oldest message waits a bounded time.
    #[test]
    fn queue_bound_drops_oldest_first_system_messages_included() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        s = s.with_queue_budget(3 * msg);
        s.push(cc(0, 7, 1), 0);
        s.push(TransportMessage::TuneRequest, 0);
        s.push(cc(0, 7, 2), 0); // kept apart by the TuneRequest: 3 queued
        assert_eq!((s.queued(), s.queue_dropped), (3, 0));
        s.push(cc(1, 7, 1), 0); // cc(0, 7, 1) goes
        assert_eq!((s.queued(), s.queue_dropped), (3, 1));
        s.push(pc(9), 0); // the TuneRequest goes
        assert_eq!((s.queued(), s.queue_dropped), (3, 2));
        check_counts(&s);
        assert_eq!(drain(&mut s), vec![cc(0, 7, 2), pc(9), cc(1, 7, 1)]);
    }

    /// Spec §5.5: at most max(⌊250 ms / airtime⌋, min(10, max(4, ⌊1 s /
    /// airtime⌋))) NoteOns are ever queued, per profile; the rest are
    /// refused and counted.
    #[test]
    fn max_queued_note_ons_per_profile() {
        for (name, config, want) in [
            ("robust", PhyConfig::robust(), 5usize),
            ("robust_repeat", PhyConfig::robust_repeat(), 4),
            ("fast", PhyConfig::fast(), 10),
            ("ltc", PhyConfig::ltc(), 14),
            ("ltc_fec", PhyConfig::ltc_fec(), 11),
            ("ltc9600", PhyConfig::ltc_with_bit_rate(9600), 29),
            ("ltc14400", PhyConfig::from_name("ltc14400").unwrap(), 46),
            ("digital", PhyConfig::digital(), 285),
        ] {
            let mut s = scheduler(&config);
            assert_eq!(s.max_note_ons(), want, "{name}");
            for n in 0..=127u8 {
                s.push(on_ch(n & 1, n), 0);
                s.push(on_ch(2 + (n & 1), n), 0);
                s.push(on_ch(4, n), 0);
            }
            assert_eq!(s.note_ons, want, "{name}");
            assert_eq!(s.note_ons_dropped as usize, 3 * 128 - want, "{name}");
            check_counts(&s);
        }
    }
}
