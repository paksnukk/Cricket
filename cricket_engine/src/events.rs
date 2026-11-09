//! Events workers report to their owner (GUI, CLI, tests).
//!
//! # For integrators
//!
//! Every event is one [`EngineEvent`]; the CLI's `--json` mode prints them
//! one per line. Kinds and when they are sent:
//!
//! | Event | Sent by | When | Rate |
//! |---|---|---|---|
//! | `Log(text)` | both | something worth a line in the log (devices opened, errors, TX of a user message, …) | as it happens |
//! | `Level(rms)` | decoder | input level, RMS 0–1 ([`rms_to_dbfs`] converts) | every 1/20 s of audio |
//! | `Decoded(msg)` | decoder | a valid packet, heartbeats included | per packet |
//! | `Stats(LinkStats)` | decoder | link statistics (state, CRC pass/fail, wrong-key suspects, FEC corrections, confidence, drops) | at most every 200 ms while audio flows |
//! | `Alarm { kind, message }` | both | a condition the operator must see (table below) | once per occurrence |
//! | `AlarmCleared { kind, message }` | decoder | the condition behind an earlier alarm of `kind` has ended | once |
//! | `TxStatus { queued, latency_ms, dropped }` | encoder | TX queue depth, how late the oldest queued message is, and how many messages the scheduler has dropped since start | every 200 ms |
//! | `Stopped` | both | the worker has finished (device error, or `stop()`); always the last event | once |
//!
//! Alarm kinds ([`AlarmKind`], stable string in [`AlarmKind::as_str`]):
//!
//! | Kind | `as_str` | Meaning | Cleared |
//! |---|---|---|---|
//! | `LinkDead` | `link_dead` | no valid packet for `link_dead_ms`; Sustain off + All Notes Off were sent on all 16 channels | by the next valid packet |
//! | `AudioInputStalled` | `audio_input_stalled` | the audio input delivered no samples for `link_dead_ms` (if the link was up, the fail-safe burst was sent) | when samples resume |
//! | `AudioDeviceError` | `audio_device_error` | the audio stream reported an error (device unplugged, driver stall) | never (reopen) |
//! | `MessageRejected` | `message_rejected` | the encoder got a message it cannot carry exactly (e.g. an MSC cue with two decimals); it was **not** sent | never (per message) |
//! | `MessageDropped` | `message_dropped` | the TX scheduler dropped queued messages (NoteOns beyond the profile's chord limit, or tier-3 messages beyond 1 s of airtime): the link is overloaded | never (per batch of drops) |
//! | `MidiOutputError` | `midi_output_error` | sending to the MIDI output device failed; decoded MIDI is being lost | when a send succeeds again |

use cricket_core::phy::LinkState;
use cricket_core::transport::{TransportMessage, PITCH_BEND_CENTER};

/// Something a decoder or encoder worker reports. Delivered in order on the
/// handle's event channel; [`EngineEvent::Stopped`] is always the last one.
/// See the [module documentation](self) for the full table.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// Human-readable status line.
    Log(String),
    /// Decoder input RMS level (0–1), reported every 1/20 s of audio.
    /// [`rms_to_dbfs`] converts it for a meter.
    Level(f32),
    /// A decoded message (including heartbeats).
    Decoded(TransportMessage),
    /// Decoder link statistics, at most every 200 ms.
    Stats(LinkStats),
    /// A condition the operator should see.
    Alarm { kind: AlarmKind, message: String },
    /// The condition behind an earlier [`EngineEvent::Alarm`] of `kind` has
    /// ended. Sent for [`AlarmKind::AudioInputStalled`] (samples resumed),
    /// [`AlarmKind::LinkDead`] (the next valid packet) and
    /// [`AlarmKind::MidiOutputError`] (a send succeeded again).
    AlarmCleared { kind: AlarmKind, message: String },
    /// Encoder queue status, about every 200 ms. `queued` = messages waiting
    /// in the TX scheduler, `latency_ms` = how long the oldest of them has
    /// already waited plus the audio buffered ahead of the output,
    /// `dropped` = messages the scheduler has dropped since the encoder
    /// started (spec §5.5: NoteOns beyond the chord limit, tier-3 messages
    /// beyond the queue bound).
    TxStatus {
        queued: usize,
        latency_ms: f32,
        dropped: u64,
    },
    /// The worker has finished; no further events follow.
    Stopped,
}

/// Decoder link statistics (same fields the GUI's `link_stats` event carries).
#[derive(Debug, Clone, PartialEq)]
pub struct LinkStats {
    pub state: LinkState,
    pub crc_pass: u64,
    pub crc_fail: u64,
    /// Of `crc_fail`: bodies with a packet's structure whose CRC failed,
    /// almost always packets from a Cricket link with another link key
    /// (crosstalk from a neighbouring link, or the two ends set
    /// differently). Random noise passes the structure with probability
    /// about 2⁻¹⁷.
    pub wrong_key_suspects: u64,
    /// Body symbols repaired by the error-correcting code (0 on uncoded
    /// profiles). A rising count on a working link means the line is
    /// eating symbols: the margin is shrinking.
    pub fec_corrected: u64,
    pub confidence_db: f32,
    pub last_seq: Option<u8>,
    /// Preamble locks lost before the body was complete.
    pub sync_lost: u64,
    /// Percentage 0–100 (100 before any packet).
    pub crc_pass_rate: f32,
    /// First-copy airtime of a full packet (not a measured latency).
    pub est_latency_ms: f32,
    pub link_alive: bool,
    /// Input samples lost because the capture buffer was full.
    pub buffer_drops: u64,
}

impl LinkStats {
    /// Preamble locks so far: every lock ends in a CRC check (pass or fail)
    /// or in a lost sync.
    pub fn locks(&self) -> u64 {
        self.crc_pass + self.crc_fail + self.sync_lost
    }
}

/// Alarm categories. See the [module documentation](self) for meanings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmKind {
    /// No valid packet for `link_dead_ms`; Sustain off + All Notes Off was
    /// sent on all 16 channels. Cleared by the next valid packet.
    LinkDead,
    /// The audio input delivered no samples for `link_dead_ms` (device
    /// unplugged, driver stall). If the link was alive, All Notes Off was
    /// sent. Cleared by [`EngineEvent::AlarmCleared`] when samples resume.
    AudioInputStalled,
    /// The audio device (input or output stream) reported an error.
    AudioDeviceError,
    /// The encoder received a message it must not send because it cannot
    /// carry it exactly (e.g. an MSC cue with more than one decimal). The
    /// message was dropped.
    MessageRejected,
    /// Sending to the MIDI output device failed: decoded MIDI is not
    /// reaching the device. Raised once per failure streak and cleared when
    /// a send succeeds again.
    MidiOutputError,
    /// The TX scheduler dropped queued messages because the link is
    /// overloaded: NoteOns beyond the profile's chord limit (spec §5.5), or
    /// tier-3 messages beyond 1 s of queued airtime. The message says how
    /// many and why. Never cleared (one alarm per batch of drops).
    MessageDropped,
}

impl AlarmKind {
    /// Stable identifier used by the GUI and `--json` (`"link_dead"`,
    /// `"audio_input_stalled"`, `"audio_device_error"`,
    /// `"message_rejected"`, `"midi_output_error"`, `"message_dropped"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            AlarmKind::LinkDead => "link_dead",
            AlarmKind::AudioInputStalled => "audio_input_stalled",
            AlarmKind::AudioDeviceError => "audio_device_error",
            AlarmKind::MessageRejected => "message_rejected",
            AlarmKind::MidiOutputError => "midi_output_error",
            AlarmKind::MessageDropped => "message_dropped",
        }
    }

    /// Short plain-language title for an alarm list.
    pub fn title(&self) -> &'static str {
        match self {
            AlarmKind::LinkDead => "Link dead",
            AlarmKind::AudioInputStalled => "Audio input stalled",
            AlarmKind::AudioDeviceError => "Audio device error",
            AlarmKind::MessageRejected => "Message not sent",
            AlarmKind::MidiOutputError => "MIDI output failing",
            AlarmKind::MessageDropped => "Messages dropped",
        }
    }
}

/// Display name of a link state ("Search" / "Locked" / "Dead").
pub fn link_state_name(state: LinkState) -> &'static str {
    match state {
        LinkState::Search => "Search",
        LinkState::Locked => "Locked",
        LinkState::Dead => "Dead",
    }
}

/// Input RMS (as in [`EngineEvent::Level`]) as a level in dBFS where a
/// full-scale sine reads 0 dBFS, like a console's peak meter on a steady
/// tone (the default transmit level then reads −12 dBFS). Silence reads
/// −120.
pub fn rms_to_dbfs(rms: f32) -> f32 {
    if rms <= 0.0 {
        return -120.0;
    }
    (20.0 * (rms * core::f32::consts::SQRT_2).log10()).max(-120.0)
}

/// Below this input level (dBFS, see [`rms_to_dbfs`]) the decoder is
/// looking at silence or noise, not at a Cricket signal (the profiles'
/// silence floor is −60 dBFS RMS).
pub const NO_SIGNAL_DBFS: f32 = -50.0;

/// The operator-facing traffic light of a decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkLight {
    /// Valid packets are arriving.
    Ok,
    /// Nothing on the input: no cable, wrong channel, or the far end is off.
    NoSignal,
    /// Audio is present but no packet decodes: profile mismatch, EQ or
    /// dynamics on the bus, or something else is on this channel.
    SignalNoLock,
    /// The link was up and died (no valid packet for `link_dead_ms`); the
    /// fail-safe burst was sent.
    Dead,
}

impl LinkLight {
    /// Derive the light from the latest stats and input level (dBFS).
    pub fn from_stats(stats: Option<&LinkStats>, level_dbfs: f32) -> Self {
        match stats {
            Some(s) if s.link_alive => LinkLight::Ok,
            Some(s) if s.state == LinkState::Dead => LinkLight::Dead,
            _ if level_dbfs < NO_SIGNAL_DBFS => LinkLight::NoSignal,
            _ => LinkLight::SignalNoLock,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            LinkLight::Ok => "LINK OK",
            LinkLight::NoSignal => "NO SIGNAL",
            LinkLight::SignalNoLock => "SIGNAL, NO LOCK",
            LinkLight::Dead => "LINK DEAD",
        }
    }

    /// One-line hint for the operator, if the state calls for one.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            LinkLight::Ok => None,
            LinkLight::NoSignal => Some(
                "No signal: check the cable, the input channel, and that the far end is encoding.",
            ),
            LinkLight::SignalNoLock => Some(
                "Signal present, no lock: profile mismatch, or EQ / dynamics on the bus? Both ends must use the same profile.",
            ),
            LinkLight::Dead => Some(
                "Link dead: Sustain off + All Notes Off were sent. It recovers by itself with the next valid packet.",
            ),
        }
    }
}

/// `(frame_type, payload)` display strings for a message. MIDI channels
/// are shown 1–16.
pub fn format_message(msg: &TransportMessage) -> (String, String) {
    use TransportMessage as M;
    let (kind, payload) = match *msg {
        M::NoteOn {
            channel,
            note,
            velocity,
        } => (
            "NoteOn",
            format!("ch={} note={} vel={}", channel + 1, note, velocity),
        ),
        M::NoteOff {
            channel,
            note,
            velocity,
        } => (
            "NoteOff",
            format!("ch={} note={} vel={}", channel + 1, note, velocity),
        ),
        M::PolyPressure {
            channel,
            note,
            pressure,
        } => (
            "PolyPressure",
            format!("ch={} note={} pressure={}", channel + 1, note, pressure),
        ),
        M::ControlChange {
            channel,
            controller,
            value,
        } => (
            "ControlChange",
            format!("ch={} cc={} value={}", channel + 1, controller, value),
        ),
        M::ProgramChange { channel, program } => (
            "ProgramChange",
            format!("ch={} program={}", channel + 1, program),
        ),
        M::ChannelPressure { channel, pressure } => (
            "ChannelPressure",
            format!("ch={} pressure={}", channel + 1, pressure),
        ),
        M::PitchBend { channel, value } => (
            "PitchBend",
            format!(
                "ch={} value={:+}",
                channel + 1,
                i32::from(value) - i32::from(PITCH_BEND_CENTER)
            ),
        ),
        M::SongPosition { beats } => ("SongPosition", format!("beats={}", beats)),
        M::SongSelect { song } => ("SongSelect", format!("song={}", song)),
        M::TuneRequest => ("TuneRequest", String::new()),
        M::Msc(m) => ("MSC", m.to_string()),
        M::Heartbeat => ("Heartbeat", String::new()),
        M::Panic => ("Panic", String::new()),
    };
    (kind.into(), payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_in_dbfs_reads_like_a_peak_meter() {
        // Full-scale sine: RMS 1/sqrt(2) reads 0 dBFS; the default TX level
        // (peak 0.25) reads -12 dBFS; silence is clamped.
        assert!(rms_to_dbfs(core::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        assert!((rms_to_dbfs(0.25 * core::f32::consts::FRAC_1_SQRT_2) + 12.04).abs() < 0.05);
        assert_eq!(rms_to_dbfs(0.0), -120.0);
        assert_eq!(rms_to_dbfs(1e-9), -120.0);
    }

    #[test]
    fn traffic_light() {
        let mut s = LinkStats {
            state: LinkState::Search,
            crc_pass: 0,
            crc_fail: 0,
            wrong_key_suspects: 0,
            fec_corrected: 0,
            confidence_db: 0.0,
            last_seq: None,
            sync_lost: 0,
            crc_pass_rate: 100.0,
            est_latency_ms: 0.0,
            link_alive: false,
            buffer_drops: 0,
        };
        assert_eq!(LinkLight::from_stats(None, -120.0), LinkLight::NoSignal);
        assert_eq!(LinkLight::from_stats(None, -20.0), LinkLight::SignalNoLock);
        assert_eq!(LinkLight::from_stats(Some(&s), -80.0), LinkLight::NoSignal);
        assert_eq!(
            LinkLight::from_stats(Some(&s), -12.0),
            LinkLight::SignalNoLock
        );
        s.link_alive = true;
        s.state = LinkState::Locked;
        assert_eq!(LinkLight::from_stats(Some(&s), -12.0), LinkLight::Ok);
        s.link_alive = false;
        s.state = LinkState::Dead;
        assert_eq!(LinkLight::from_stats(Some(&s), -120.0), LinkLight::Dead);
        assert!(LinkLight::Dead.hint().is_some());
        assert!(LinkLight::Ok.hint().is_none());
        s.crc_pass = 3;
        s.crc_fail = 1;
        s.sync_lost = 2;
        assert_eq!(s.locks(), 6);
    }
}
