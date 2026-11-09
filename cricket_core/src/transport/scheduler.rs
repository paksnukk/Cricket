//! Pure TX message scheduler (spec §5.5): priority tiers, latest-value-wins
//! coalescing of continuous controllers, idle keepalive / NoteOff refresh,
//! and bounded NoteOn dropping. Time is a sample counter supplied by the
//! caller.
//!
//! The caller encodes only what [`TxScheduler::poll`] returns and assumes each
//! returned message is transmitted immediately. Do not call `poll` again until
//! that transmission has *started*; pass `now_samples` as the stream sample index
//! where that transmission begins.

use super::TransportMessage;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

const REFRESH_CAPACITY: usize = 32;
const REFRESH_LIFETIME_MS: u64 = 10_000;
/// Repeats per released note. Enough that no note stayed stuck past
/// link-dead time in `tests/stuck_notes.rs` (up to 2 % symbol errors), while
/// each release costs a bounded burst of airtime instead of every idle slot.
const REFRESH_MAX_REPEATS: u32 = 6;
const NOTEON_BUDGET_MS: u64 = 250;
/// Queued NoteOns that are never dropped for the airtime budget, so a chord
/// survives even when one message is longer than the budget (Robust).
const MIN_KEPT_NOTE_ONS: usize = 4;
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

/// Allocation-free set of (channel, note) pairs.
#[derive(Clone, Copy, Default)]
struct NoteSet([u128; 16]);

impl NoteSet {
    fn insert(&mut self, channel: u8, note: u8) {
        self.0[usize::from(channel & 0x0F)] |= 1 << (note & 0x7F);
    }

    fn remove(&mut self, channel: u8, note: u8) {
        self.0[usize::from(channel & 0x0F)] &= !(1 << (note & 0x7F));
    }

    fn contains(&self, channel: u8, note: u8) -> bool {
        self.0[usize::from(channel & 0x0F)] & (1 << (note & 0x7F)) != 0
    }

    fn clear(&mut self) {
        self.0 = [0; 16];
    }

    fn clear_channel(&mut self, channel: u8) {
        self.0[usize::from(channel & 0x0F)] = 0;
    }
}

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
    /// NoteOff and MSC.
    Urgent,
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
        M::NoteOff { .. } | M::Msc(_) => Class::Urgent,
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

/// Channels (bit per channel) and system scope touched by dropped discrete
/// messages: where controller values may have become mergeable again.
#[derive(Clone, Copy, Default)]
struct DroppedScopes {
    channels: u16,
    system: bool,
}

impl DroppedScopes {
    fn add(&mut self, msg: &TransportMessage) {
        if matches!(class(msg), Class::Continuous(_)) {
            return; // Nothing was pinned behind a continuous message.
        }
        match msg.channel() {
            Some(ch) => self.channels |= 1 << (ch & 0x0F),
            None => self.system = true,
        }
    }

    fn is_empty(&self) -> bool {
        self.channels == 0 && !self.system
    }
}

/// Result of [`TxScheduler::poll_detailed`].
pub struct PollResult {
    pub msg: TransportMessage,
    /// True for messages from the user queues (everything except NoteOff
    /// refreshes and Heartbeats).
    pub user: bool,
}

/// TX-side scheduler shared by the engine encoder and offline stream builders.
pub struct TxScheduler {
    heartbeat_interval_samples: u64,
    message_samples: u64,
    refresh_lifetime_samples: u64,
    noteon_budget_samples: u64,
    queue_budget_samples: u64,

    /// Panic-class messages.
    panic_q: VecDeque<Queued>,
    /// NoteOff and MSC, in arrival order.
    urgent_q: VecDeque<Queued>,
    /// Everything else (NoteOn, controllers, Program Change, system common).
    normal_q: VecDeque<Queued>,
    /// Running counts over `normal_q`, so the budget checks on every push
    /// do not scan the queue: NoteOns, NoteOffs held behind any message,
    /// and NoteOffs held behind NoteOns.
    note_ons: usize,
    held_total: usize,
    held_note_on: usize,

    /// Notes whose NoteOn has been emitted (sounding on the far side).
    sounding: NoteSet,
    refresh: VecDeque<RefreshEntry>,

    last_emit_sample: Option<u64>,
    /// Least time between NoteOff refreshes, start to start, counted from
    /// the user NoteOff too (0: every free packet, as spec §5.5 describes).
    refresh_spacing_samples: u64,
    /// When the last NoteOff (user or refresh) was emitted.
    last_refresh_sample: Option<u64>,
    /// Set when a queued NoteOn was dropped because the queue exceeded an
    /// airtime budget (the NoteOn budget or the whole queue's). The caller
    /// reports and clears it.
    pub note_on_dropped: bool,
    /// Tier-3 messages (of any kind) dropped because the queue exceeded its
    /// airtime bound ([`with_queue_budget`](Self::with_queue_budget)), since
    /// the caller last cleared it. NoteOns dropped for the NoteOn budget
    /// alone are not counted here.
    pub queue_dropped: u32,
}

impl TxScheduler {
    pub fn new(sample_rate_hz: u32, heartbeat_interval_samples: u64, message_samples: u64) -> Self {
        let hz = sample_rate_hz.max(1) as u64;
        Self {
            heartbeat_interval_samples,
            message_samples,
            refresh_lifetime_samples: hz * REFRESH_LIFETIME_MS / 1000,
            noteon_budget_samples: hz * NOTEON_BUDGET_MS / 1000,
            queue_budget_samples: hz * QUEUE_BUDGET_MS / 1000,
            panic_q: VecDeque::new(),
            urgent_q: VecDeque::new(),
            normal_q: VecDeque::new(),
            note_ons: 0,
            held_total: 0,
            held_note_on: 0,
            sounding: NoteSet::default(),
            refresh: VecDeque::new(),
            last_emit_sample: None,
            refresh_spacing_samples: 0,
            last_refresh_sample: None,
            note_on_dropped: false,
            queue_dropped: 0,
        }
    }

    /// Bound the airtime of everything queued in tier 3 (NoteOns,
    /// controllers, Program Changes, system common and the NoteOffs held
    /// behind them) to `samples` (default: 1 s). Before a tier-3 message is
    /// queued, the oldest droppable messages are dropped until it fits:
    /// every tier-3 message except the `MIN_KEPT_NOTE_ONS` protected
    /// NoteOns and pedal messages with NoteOffs held behind them, so the
    /// queue can exceed the bound only by those. Dropping a discrete message
    /// lets the controller values it separated merge again (latest value
    /// wins). Panic-class messages, NoteOffs and MSC are never dropped and
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

    /// Space NoteOff refreshes at least `samples` apart (start to start,
    /// counted from the user NoteOff too). For profiles whose messages are
    /// a few samples long (`digital`), where back-to-back refreshes would
    /// all fall into one short dropout; 0 (the default) sends one in every
    /// free packet. Heartbeats still wait until the refresh list is empty.
    pub fn with_refresh_spacing(mut self, samples: u64) -> Self {
        self.refresh_spacing_samples = samples;
        self
    }

    /// While NoteOff refreshes are pending, the earliest sample the next one
    /// may start at (`None`: no refresh pending).
    pub fn next_refresh_sample(&self) -> Option<u64> {
        if self.refresh.is_empty() {
            return None;
        }
        Some(
            self.last_refresh_sample
                .map_or(0, |t| t + self.refresh_spacing_samples),
        )
    }

    /// Messages waiting to be sent (held NoteOffs count).
    pub fn queued(&self) -> usize {
        self.panic_q.len() + self.urgent_q.len() + self.normal_q.len() + self.held_total
    }

    /// How long the oldest queued message has waited at `now_samples`.
    pub fn oldest_wait_samples(&self, now_samples: u64) -> u64 {
        [&self.panic_q, &self.urgent_q, &self.normal_q]
            .iter()
            .filter_map(|q| q.iter().map(|e| e.push_sample).min())
            .min()
            .map_or(0, |oldest| now_samples.saturating_sub(oldest))
    }

    /// When the refresh list is empty, earliest sample for a Heartbeat.
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
                self.panic_q.push_back(Queued::new(msg, push_sample));
            }
            Class::Urgent => {
                if let TransportMessage::NoteOff {
                    channel,
                    note,
                    velocity,
                } = msg
                {
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
                }
                self.urgent_q.push_back(Queued::new(msg, push_sample));
            }
            Class::Continuous(key) => {
                if !self.coalesce(&msg, key) {
                    let dropped = self.enforce_queue_budget(false);
                    self.push_normal(Queued::new(msg, push_sample));
                    self.recoalesce(dropped);
                }
            }
            Class::Discrete => {
                let mut dropped = DroppedScopes::default();
                if is_note_on(&msg) {
                    while self
                        .note_on_airtime_samples()
                        .saturating_add(self.message_samples)
                        > self.noteon_budget_samples
                        && self.note_ons >= MIN_KEPT_NOTE_ONS
                    {
                        let Some(pos) = self.normal_q.iter().position(|q| is_note_on(&q.msg))
                        else {
                            break;
                        };
                        self.drop_normal(pos, &mut dropped);
                    }
                }
                let over = self.enforce_queue_budget(is_note_on(&msg));
                dropped.channels |= over.channels;
                dropped.system |= over.system;
                self.push_normal(Queued::new(msg, push_sample));
                self.recoalesce(dropped);
            }
        }
    }

    /// Drop the oldest droppable tier-3 messages until one more message of
    /// `message_samples` fits the queue's airtime bound; `incoming_note_on`
    /// says whether that message is a NoteOn (it counts towards the
    /// protected NoteOns). Returns the scopes of the dropped messages.
    fn enforce_queue_budget(&mut self, incoming_note_on: bool) -> DroppedScopes {
        let mut dropped = DroppedScopes::default();
        while self
            .tier3_airtime_samples()
            .saturating_add(self.message_samples)
            > self.queue_budget_samples
        {
            // NoteOns are droppable only beyond the protected ones; a pedal
            // with NoteOffs held behind it is never dropped (the held
            // releases must not overtake it).
            let note_ons_droppable =
                self.note_ons + usize::from(incoming_note_on) > MIN_KEPT_NOTE_ONS;
            let Some(pos) = self.normal_q.iter().position(|q| {
                if is_note_on(&q.msg) {
                    note_ons_droppable
                } else {
                    q.held.is_empty()
                }
            }) else {
                break;
            };
            self.drop_normal(pos, &mut dropped);
            self.queue_dropped = self.queue_dropped.saturating_add(1);
        }
        dropped
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
        if let Some(q) = self.urgent_q.pop_front() {
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
            }
            for h in &q.held {
                self.urgent_q
                    .push_back(Queued::new(h.message(), h.push_sample));
            }
            return Some(self.emit(q.msg, now_samples, true));
        }

        self.prune_refresh(now_samples);
        if self.last_emit_sample == Some(now_samples) {
            return None;
        }

        if let Some(due) = self.next_refresh_sample() {
            if now_samples < due {
                return None;
            }
        }
        if let Some(msg) = self.take_refresh() {
            self.last_refresh_sample = Some(now_samples);
            return Some(self.emit(msg, now_samples, false));
        }

        if now_samples < self.next_keepalive_sample() {
            return None;
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
        PollResult { msg, user }
    }

    fn push_refresh(&mut self, channel: u8, note: u8, velocity: u8, emitted_at: u64) {
        self.last_refresh_sample = Some(emitted_at);
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

    /// Next note to repeat. Notes that have been repeated fewer times go first, and among those the oldest
    /// release. A fresh NoteOff is not stuck behind notes that already had
    /// several repeats, and the release that has waited longest is not stuck
    /// behind newer ones.
    fn take_refresh(&mut self) -> Option<TransportMessage> {
        let i = (0..self.refresh.len())
            .min_by_key(|&i| (self.refresh[i].sent, self.refresh[i].emitted_at))?;
        let e = &mut self.refresh[i];
        e.sent += 1;
        let msg = TransportMessage::NoteOff {
            channel: e.channel,
            note: e.note,
            velocity: e.velocity,
        };
        if e.sent >= REFRESH_MAX_REPEATS {
            self.refresh.remove(i);
        }
        Some(msg)
    }

    fn cancel_refresh(&mut self, channel: u8, note: u8) {
        self.refresh.retain(|e| !e.is(channel, note));
    }

    fn prune_refresh(&mut self, now_samples: u64) {
        self.refresh
            .retain(|e| now_samples.saturating_sub(e.emitted_at) <= self.refresh_lifetime_samples);
    }

    /// Airtime of queued NoteOns, plus the NoteOffs held behind them.
    fn note_on_airtime_samples(&self) -> u64 {
        self.message_samples
            .saturating_mul((self.note_ons + self.held_note_on) as u64)
    }

    /// Airtime of everything queued in tier 3, held NoteOffs included.
    fn tier3_airtime_samples(&self) -> u64 {
        self.message_samples
            .saturating_mul((self.normal_q.len() + self.held_total) as u64)
    }

    fn push_normal(&mut self, q: Queued) {
        self.note_ons += usize::from(is_note_on(&q.msg));
        self.held_total += q.held.len();
        if is_note_on(&q.msg) {
            self.held_note_on += q.held.len();
        }
        self.normal_q.push_back(q);
    }

    /// Remove `normal_q[i]`, keeping the running counts.
    fn remove_normal(&mut self, i: usize) -> Queued {
        let q = self.normal_q.remove(i).expect("index in range");
        self.note_ons -= usize::from(is_note_on(&q.msg));
        self.held_total -= q.held.len();
        if is_note_on(&q.msg) {
            self.held_note_on -= q.held.len();
        }
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
        if is_note_on(&q.msg) {
            self.held_note_on += 1;
        }
    }

    /// Drop `normal_q[pos]` for an airtime budget. A held NoteOff goes with
    /// its NoteOn, unless the note is still sounding from an earlier
    /// strike: then the release must still go out, with priority.
    fn drop_normal(&mut self, pos: usize, dropped: &mut DroppedScopes) {
        let q = self.remove_normal(pos);
        for h in q.held {
            if self.sounding.contains(h.channel, h.note) {
                self.urgent_q
                    .push_back(Queued::new(h.message(), h.push_sample));
            }
        }
        if is_note_on(&q.msg) {
            self.note_on_dropped = true;
        }
        dropped.add(&q.msg);
    }

    /// Merge continuous updates that dropped messages of `scopes` had kept
    /// apart, as [`coalesce`](Self::coalesce) would have on arrival: the
    /// newer value replaces the older entry in place and the newer entry
    /// goes. A discrete message or a held pedal of the channel, or a system
    /// message, still separates the values either side of it.
    fn recoalesce(&mut self, scopes: DroppedScopes) {
        if scopes.is_empty() {
            return;
        }
        // Mergeable entry per key since the last separator of its channel.
        let mut open: Vec<(Key, usize)> = Vec::new();
        let mut i = 0;
        while i < self.normal_q.len() {
            let q = &self.normal_q[i];
            let channel = q.msg.channel();
            let affected =
                scopes.system || channel.is_none_or(|ch| scopes.channels & (1 << (ch & 0x0F)) != 0);
            if !affected {
                i += 1;
                continue;
            }
            match class(&q.msg) {
                Class::Continuous(key) if q.held.is_empty() => {
                    if let Some(&(_, older)) = open.iter().find(|(k, _)| *k == key) {
                        let newer = self.remove_normal(i);
                        self.normal_q[older].msg = newer.msg;
                        continue;
                    }
                    open.push((key, i));
                }
                _ => match channel {
                    Some(ch) => open.retain(|(k, _)| k.1 != ch),
                    None => open.clear(),
                },
            }
            i += 1;
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

    fn scheduler(config: &PhyConfig) -> TxScheduler {
        let msg_samples = repetition_samples(config) * config.repetition_k.max(1) as u64;
        let hb = config.sample_rate_hz as u64 * config.heartbeat_interval_ms as u64 / 1000;
        TxScheduler::new(config.sample_rate_hz, hb, msg_samples)
    }

    fn fast_scheduler() -> TxScheduler {
        scheduler(&PhyConfig::fast())
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

    #[test]
    fn msc_goes_with_note_offs_ahead_of_notes() {
        let mut s = fast_scheduler();
        s.push(on(60), 0);
        s.push(
            ProgramChange {
                channel: 0,
                program: 3,
            },
            0,
        );
        s.push(go(), 0);
        s.push(off(50), 0);
        assert_eq!(
            drain(&mut s),
            vec![
                go(),
                off(50),
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

    #[test]
    fn refresh_right_after_note_off() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        let t = play_and_release(&mut s, 60, 0);
        assert_eq!(s.poll(t), None);
        assert_eq!(s.poll(t + msg), Some(off(60)));
        assert_eq!(s.poll(t + msg), None);
        assert_eq!(s.poll(t + 2 * msg), Some(off(60)));
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
        assert_eq!(s.poll(2 * msg), Some(release));
    }

    #[test]
    fn two_notes_refresh_round_robin() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        play_and_release(&mut s, 10, 0);
        let t = play_and_release(&mut s, 20, 2 * msg);
        assert_eq!(s.poll(t + msg), Some(off(10)));
        assert_eq!(s.poll(t + 2 * msg), Some(off(20)));
        assert_eq!(s.poll(t + 3 * msg), Some(off(10)));
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
        assert_eq!(s.poll(t + s.message_samples), Some(off(1)));
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
        let msg = s.message_samples;
        let mut t = play_and_release(&mut s, 60, 0);
        for _ in 0..REFRESH_MAX_REPEATS {
            t += msg;
            assert_eq!(s.poll(t), Some(off(60)));
        }
        assert!(refresh_notes(&s).is_empty());
        assert_eq!(s.poll(t + msg), None);
        assert_eq!(s.poll(t + s.heartbeat_interval_samples), Some(Heartbeat));
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

    #[test]
    fn noteon_budget_drops_oldest_not_panic() {
        let mut s = fast_scheduler();
        let max_noteons = (s.noteon_budget_samples / s.message_samples).max(1) as usize;
        for i in 0..max_noteons + 2 {
            s.push(on(i as u8), 0);
        }
        assert!(s.note_on_dropped);
        s.push(Panic, 0);
        assert_eq!(s.poll(0), Some(Panic));
    }

    /// A Robust repeat message (K = 2) is longer than the airtime budget; a chord pushed at
    /// once must still be sent whole.
    #[test]
    fn robust_chord_is_not_dropped() {
        let mut s = scheduler(&PhyConfig::robust_repeat());
        assert!(s.message_samples > s.noteon_budget_samples);
        for n in [60u8, 64, 67, 72] {
            s.push(on(n), 0);
        }
        assert!(!s.note_on_dropped);
        let sent: Vec<_> = (0..4).map(|i| s.poll(i * s.message_samples)).collect();
        assert_eq!(
            sent,
            vec![Some(on(60)), Some(on(64)), Some(on(67)), Some(on(72))]
        );
    }

    /// Dropping a queued re-strike must not lose the release of the note
    /// that is still sounding from its earlier strike.
    #[test]
    fn dropped_restrike_keeps_release_of_sounding_note() {
        let mut s = fast_scheduler();
        s.push(on(60), 0);
        assert_eq!(s.poll(0), Some(on(60)));
        s.push(on(60), 1);
        s.push(off(60), 2);
        for n in 70..80u8 {
            s.push(on(n), 3);
        }
        assert!(s.note_on_dropped);
        assert_eq!(s.poll(4), Some(off(60)));
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
        let budget = s.noteon_budget_samples;
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
        let held_note_on: usize = s
            .normal_q
            .iter()
            .filter(|q| is_note_on(&q.msg))
            .map(|q| q.held.len())
            .sum();
        assert_eq!(
            (s.note_ons, s.held_total, s.held_note_on),
            (note_ons, held_total, held_note_on)
        );
        assert_eq!(
            s.queued(),
            s.panic_q.len() + s.urgent_q.len() + s.normal_q.len() + held_total
        );
    }

    /// Dropping a NoteOn lets the controller values it kept apart merge,
    /// latest value wins, at the older value's place.
    #[test]
    fn dropped_note_on_lets_controller_values_merge() {
        let mut s = fast_scheduler();
        s.push(cc(0, 7, 1), 0);
        s.push(on(60), 0);
        s.push(cc(0, 7, 2), 0); // a new entry: on(60) is between them
        s.push(cc(1, 7, 5), 0); // another channel, untouched
        assert_eq!(s.queued(), 4);
        check_counts(&s);
        for n in 61..70u8 {
            s.push(on(n), 0); // on(60) is dropped for the NoteOn budget
        }
        assert!(s.note_on_dropped);
        assert_eq!(s.queue_dropped, 0);
        check_counts(&s);
        let sent = drain(&mut s);
        assert_eq!(sent[0], cc(0, 7, 2));
        assert_eq!(
            sent.iter().filter(|m| **m == cc(0, 7, 1)).count(),
            0,
            "{sent:?}"
        );
        assert_eq!(sent.iter().filter(|m| **m == cc(1, 7, 5)).count(), 1);
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
        assert!(!s.note_on_dropped);
        check_counts(&s);
        assert_eq!(
            drain(&mut s),
            vec![cc(0, 64, 127), off(60), pc(3), pc(4), on(70)]
        );
        check_counts(&s);
    }

    /// The protected NoteOns are kept even when they alone exceed the
    /// bound; beyond them NoteOns are droppable for the bound too, and a
    /// dropped NoteOn counts in both flags.
    #[test]
    fn queue_bound_keeps_protected_note_ons() {
        let mut s = fast_scheduler();
        let msg = s.message_samples;
        s = s.with_queue_budget(2 * msg);
        for n in 60..64u8 {
            s.push(on(n), 0);
        }
        assert_eq!((s.queued(), s.queue_dropped), (4, 0));
        s.push(pc(1), 1); // nothing droppable: appended beyond the bound
        assert_eq!((s.queued(), s.queue_dropped), (5, 0));
        // A fifth NoteOn: on(60) goes for the NoteOn budget (the 5th would
        // exceed 250 ms), then pc(1) for the bound; the 4 NoteOns stay.
        s.push(on(64), 2);
        assert!(s.note_on_dropped);
        assert_eq!(s.queue_dropped, 1);
        check_counts(&s);
        assert_eq!(drain(&mut s), vec![on(61), on(62), on(63), on(64)]);
        // With a bound far below the NoteOn budget a NoteOn is still only
        // dropped beyond the protected four: here for the bound, not the
        // NoteOn budget, so both flags report it.
        let mut s = scheduler(&PhyConfig::digital()).with_queue_budget(3 * 42);
        s.note_on_dropped = false;
        for n in 0..5u8 {
            s.push(on(n), 0);
        }
        assert_eq!((s.queued(), s.queue_dropped), (4, 1));
        assert!(s.note_on_dropped);
        assert_eq!(drain(&mut s), vec![on(1), on(2), on(3), on(4)]);
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

    /// Spec §5.5: at most max(4, floor(250 ms / airtime)) NoteOns are ever
    /// queued, per profile.
    #[test]
    fn max_queued_note_ons_per_profile() {
        for (name, config, want) in [
            ("robust", PhyConfig::robust(), 4usize),
            ("robust_repeat", PhyConfig::robust_repeat(), 4),
            ("fast", PhyConfig::fast(), 4),
            ("ltc", PhyConfig::ltc(), 14),
            ("ltc_fec", PhyConfig::ltc_fec(), 11),
            ("ltc9600", PhyConfig::ltc_with_bit_rate(9600), 29),
            ("ltc14400", PhyConfig::from_name("ltc14400").unwrap(), 46),
            ("digital", PhyConfig::digital(), 285),
        ] {
            let mut s = scheduler(&config);
            let by_budget = (s.noteon_budget_samples / s.message_samples) as usize;
            assert_eq!(MIN_KEPT_NOTE_ONS.max(by_budget), want, "{name}");
            for n in 0..=127u8 {
                s.push(on_ch(n & 1, n), 0);
                s.push(on_ch(2 + (n & 1), n), 0);
                s.push(on_ch(4, n), 0);
            }
            assert_eq!(s.note_ons, want, "{name}");
            assert!(s.note_on_dropped, "{name}");
            check_counts(&s);
        }
    }
}
