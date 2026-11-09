//! Transport messages and their AML v1 payloads (spec §5).

use super::msc::MscMessage;
use alloc::vec::Vec;

/// The 4-bit message type in the packet header (spec §4.1, §5).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// A MIDI 1.0 short message: payload = status byte + data bytes.
    Midi = 0,
    /// MIDI Show Control, compact form (spec §5.4).
    Msc = 3,
    Heartbeat = 4,
    Panic = 5,
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Midi),
            3 => Some(Self::Msc),
            4 => Some(Self::Heartbeat),
            5 => Some(Self::Panic),
            _ => None,
        }
    }

    pub fn payload_len(self) -> usize {
        match self {
            Self::Midi | Self::Msc => 3,
            Self::Heartbeat | Self::Panic => 0,
        }
    }

    /// Whether `payload` (of [`payload_len`](Self::payload_len) bytes) is a
    /// canonical payload of this type (spec §5.1): for `Midi` a supported
    /// status byte, data bytes < 0x80, unused bytes zero and no NoteOn with
    /// velocity 0; for `Msc` a supported command with fields in range
    /// (spec §5.4). A transmitter never sends anything else; receivers
    /// reject everything else, which is also a check against false FEC
    /// decodes (`docs/proposals/fec.md` §9).
    pub fn payload_in_range(self, payload: &[u8]) -> bool {
        if payload.len() != self.payload_len() {
            return false;
        }
        match self {
            Self::Midi => TransportMessage::from_midi_payload(payload).is_some(),
            Self::Msc => MscMessage::from_compact(payload).is_some(),
            Self::Heartbeat | Self::Panic => true,
        }
    }
}

/// A message carried by AML v1.
///
/// MIDI channels are 0–15 (MIDI channel 1 is `0`). All values are 7-bit
/// (0–127) except `PitchBend::value` and `SongPosition::beats` (14-bit,
/// 0–16383; pitch bend center is 8192). Construct values in range: the
/// encoders mask out-of-range bits.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportMessage {
    NoteOff {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// Velocity 1–127; a NoteOn with velocity 0 is a NoteOff (velocity 64)
    /// and is converted to one on input.
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// Polyphonic key pressure (aftertouch).
    PolyPressure {
        channel: u8,
        note: u8,
        pressure: u8,
    },
    /// Control Change, including the channel mode messages (controllers
    /// 120–127).
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
    /// Channel pressure (aftertouch).
    ChannelPressure {
        channel: u8,
        pressure: u8,
    },
    PitchBend {
        channel: u8,
        value: u16,
    },
    /// System common: Song Position Pointer (MIDI beats since song start).
    SongPosition {
        beats: u16,
    },
    /// System common: Song Select.
    SongSelect {
        song: u8,
    },
    /// System common: Tune Request.
    TuneRequest,
    /// MIDI Show Control (spec §5.4).
    Msc(MscMessage),
    Heartbeat,
    Panic,
}

/// Pitch bend center (no bend).
pub const PITCH_BEND_CENTER: u16 = 0x2000;

/// Number of data bytes after a supported MIDI status byte (spec §5.1), or
/// `None` for status bytes AML v1 does not carry (SysEx, MTC quarter frame,
/// real-time messages, undefined system messages, data bytes).
pub fn midi_data_len(status: u8) -> Option<usize> {
    match status {
        0x80..=0xBF | 0xE0..=0xEF | 0xF2 => Some(2),
        0xC0..=0xDF | 0xF3 => Some(1),
        0xF6 => Some(0),
        _ => None,
    }
}

impl TransportMessage {
    pub fn msg_type(&self) -> MessageType {
        match self {
            Self::Msc(_) => MessageType::Msc,
            Self::Heartbeat => MessageType::Heartbeat,
            Self::Panic => MessageType::Panic,
            _ => MessageType::Midi,
        }
    }

    /// MIDI channel (0–15) of a channel message.
    pub fn channel(&self) -> Option<u8> {
        match *self {
            Self::NoteOff { channel, .. }
            | Self::NoteOn { channel, .. }
            | Self::PolyPressure { channel, .. }
            | Self::ControlChange { channel, .. }
            | Self::ProgramChange { channel, .. }
            | Self::ChannelPressure { channel, .. }
            | Self::PitchBend { channel, .. } => Some(channel & 0x0F),
            _ => None,
        }
    }

    /// The MIDI 1.0 bytes of a MIDI message (`len` of `bytes` are used),
    /// or `None` for MSC, Heartbeat and Panic.
    pub fn to_midi_short(&self) -> Option<([u8; 3], usize)> {
        let ch = |c: u8| c & 0x0F;
        let d = |v: u8| v & 0x7F;
        let lsb = |v: u16| (v & 0x7F) as u8;
        let msb = |v: u16| ((v >> 7) & 0x7F) as u8;
        Some(match *self {
            Self::NoteOff {
                channel,
                note,
                velocity,
            } => ([0x80 | ch(channel), d(note), d(velocity)], 3),
            Self::NoteOn {
                channel,
                note,
                velocity,
            } => {
                if d(velocity) == 0 {
                    ([0x80 | ch(channel), d(note), 0x40], 3)
                } else {
                    ([0x90 | ch(channel), d(note), d(velocity)], 3)
                }
            }
            Self::PolyPressure {
                channel,
                note,
                pressure,
            } => ([0xA0 | ch(channel), d(note), d(pressure)], 3),
            Self::ControlChange {
                channel,
                controller,
                value,
            } => ([0xB0 | ch(channel), d(controller), d(value)], 3),
            Self::ProgramChange { channel, program } => ([0xC0 | ch(channel), d(program), 0], 2),
            Self::ChannelPressure { channel, pressure } => {
                ([0xD0 | ch(channel), d(pressure), 0], 2)
            }
            Self::PitchBend { channel, value } => ([0xE0 | ch(channel), lsb(value), msb(value)], 3),
            Self::SongPosition { beats } => ([0xF2, lsb(beats), msb(beats)], 3),
            Self::SongSelect { song } => ([0xF3, d(song), 0], 2),
            Self::TuneRequest => ([0xF6, 0, 0], 1),
            Self::Msc(_) | Self::Heartbeat | Self::Panic => return None,
        })
    }

    /// Parse one complete MIDI short message (as delivered by a MIDI input
    /// API: no running status). Returns `None` for anything AML v1 does not
    /// carry (SysEx, MTC quarter frame, real-time, undefined) and for
    /// malformed input. A NoteOn with velocity 0 becomes a NoteOff with
    /// velocity 64.
    pub fn from_midi_short(bytes: &[u8]) -> Option<Self> {
        let (&status, data) = bytes.split_first()?;
        let n = midi_data_len(status)?;
        if data.len() != n || data.iter().any(|&b| b >= 0x80) {
            return None;
        }
        let mut payload = [status, 0, 0];
        payload[1..1 + n].copy_from_slice(data);
        if status & 0xF0 == 0x90 && payload[2] == 0 {
            payload = [0x80 | (status & 0x0F), payload[1], 0x40];
        }
        Self::from_midi_payload(&payload)
    }

    /// Decode a canonical `Midi` payload (status, data bytes, zero pad).
    fn from_midi_payload(p: &[u8]) -> Option<Self> {
        let &[status, d1, d2] = p else {
            return None;
        };
        let n = midi_data_len(status)?;
        if d1 >= 0x80 || d2 >= 0x80 || (n < 2 && d2 != 0) || (n < 1 && d1 != 0) {
            return None;
        }
        let channel = status & 0x0F;
        let wide = (d1 as u16) | ((d2 as u16) << 7);
        Some(match status & 0xF0 {
            0x80 => Self::NoteOff {
                channel,
                note: d1,
                velocity: d2,
            },
            0x90 if d2 == 0 => return None,
            0x90 => Self::NoteOn {
                channel,
                note: d1,
                velocity: d2,
            },
            0xA0 => Self::PolyPressure {
                channel,
                note: d1,
                pressure: d2,
            },
            0xB0 => Self::ControlChange {
                channel,
                controller: d1,
                value: d2,
            },
            0xC0 => Self::ProgramChange {
                channel,
                program: d1,
            },
            0xD0 => Self::ChannelPressure {
                channel,
                pressure: d1,
            },
            0xE0 => Self::PitchBend {
                channel,
                value: wide,
            },
            _ => match status {
                0xF2 => Self::SongPosition { beats: wide },
                0xF3 => Self::SongSelect { song: d1 },
                0xF6 => Self::TuneRequest,
                _ => return None,
            },
        })
    }

    pub fn to_payload(&self) -> Vec<u8> {
        match self {
            Self::Msc(m) => m.to_compact().to_vec(),
            Self::Heartbeat | Self::Panic => Vec::new(),
            other => {
                let (mut bytes, len) = other.to_midi_short().expect("MIDI message");
                bytes[len..].fill(0);
                bytes.to_vec()
            }
        }
    }

    /// Decode a payload; `None` unless it is canonical for `msg_type`
    /// ([`MessageType::payload_in_range`]).
    pub fn from_type_payload(msg_type: MessageType, payload: &[u8]) -> Option<Self> {
        if payload.len() != msg_type.payload_len() {
            return None;
        }
        match msg_type {
            MessageType::Midi => Self::from_midi_payload(payload),
            MessageType::Msc => MscMessage::from_compact(payload).map(Self::Msc),
            MessageType::Heartbeat => Some(Self::Heartbeat),
            MessageType::Panic => Some(Self::Panic),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn every_supported_status_round_trips() {
        let mut count = 0usize;
        for status in 0x80..=0xFFu8 {
            let Some(n) = midi_data_len(status) else {
                continue;
            };
            for (d1, d2) in [(0u8, 0u8), (1, 1), (60, 100), (127, 127), (5, 0)] {
                let data = [d1, d2];
                let mut bytes = vec![status];
                bytes.extend_from_slice(&data[..n]);
                let msg = TransportMessage::from_midi_short(&bytes).expect("supported");
                let (out, len) = msg.to_midi_short().unwrap();
                if status & 0xF0 == 0x90 && d2 == 0 {
                    assert_eq!(&out[..len], &[0x80 | (status & 0x0F), d1, 0x40]);
                } else {
                    assert_eq!(&out[..len], &bytes[..], "{status:#04x}");
                }
                let payload = msg.to_payload();
                assert!(MessageType::Midi.payload_in_range(&payload));
                assert_eq!(
                    TransportMessage::from_type_payload(MessageType::Midi, &payload),
                    Some(msg)
                );
                count += 1;
            }
        }
        // 7 channel voice kinds x 16 channels + 3 system common, 5 cases each.
        assert_eq!(count, (7 * 16 + 3) * 5);
    }

    #[test]
    fn unsupported_and_malformed_midi_is_rejected() {
        for bytes in [
            &[0xF0, 0x7E, 0x7F, 0xF7][..], // SysEx
            &[0xF1, 0x10],                 // MTC quarter frame
            &[0xF8],                       // clock
            &[0xFA],                       // start
            &[0xFE],                       // active sensing
            &[0xF4],                       // undefined
            &[0x90, 60],                   // short
            &[0x90, 60, 100, 1],           // long
            &[0x90, 0x80, 100],            // data >= 0x80
            &[0x3C, 0x40],                 // running status
            &[],
        ] {
            assert_eq!(
                TransportMessage::from_midi_short(bytes),
                None,
                "{bytes:02x?}"
            );
        }
    }

    #[test]
    fn canonical_payload_checks() {
        let ok = |p: [u8; 3]| MessageType::Midi.payload_in_range(&p);
        assert!(ok([0x93, 60, 1]));
        assert!(ok([0xC5, 7, 0]));
        assert!(ok([0xF6, 0, 0]));
        assert!(!ok([0x93, 60, 0]), "NoteOn velocity 0 is sent as NoteOff");
        assert!(!ok([0xC5, 7, 1]), "unused byte must be zero");
        assert!(!ok([0xF6, 1, 0]));
        assert!(!ok([0xF0, 0, 0]));
        assert!(!ok([0xF8, 0, 0]));
        assert!(!ok([0x7F, 0, 0]));
        assert!(!ok([0xB0, 0x80, 0]));
        assert!(!ok([0xE0, 0, 0x80]));
    }
}
