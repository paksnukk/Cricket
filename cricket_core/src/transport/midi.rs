//! MIDI mapping (spec §5.2) and the fail-safe burst (spec §5.3).

use super::message::{TransportMessage, PITCH_BEND_CENTER};
use super::msc::{MscError, MscFilter, MscMessage, MscTarget};
use super::note_set::NoteSet;
use alloc::vec;
use alloc::vec::Vec;

/// CC123 (All Notes Off) on `channel` (0–15).
pub fn all_notes_off(channel: u8) -> Vec<u8> {
    vec![0xB0 | (channel & 0x0F), 0x7B, 0x00]
}

/// Note Off (release velocity 64) for `note` on `channel` (0–15).
pub fn note_off(channel: u8, note: u8) -> Vec<u8> {
    vec![0x80 | (channel & 0x0F), note & 0x7F, 0x40]
}

/// CC64 = 0 (sustain pedal off) on `channel` (0–15).
pub fn sustain_off(channel: u8) -> Vec<u8> {
    vec![0xB0 | (channel & 0x0F), 0x40, 0x00]
}

/// Pitch bend to center on `channel` (0–15).
pub fn pitch_bend_center(channel: u8) -> Vec<u8> {
    vec![0xE0 | (channel & 0x0F), 0x00, 0x40]
}

/// Fail-safe burst without receiver state: CC64 = 0 then CC123 on every
/// channel 1–16 (spec §5.3). [`MidiOutput::fail_safe`] adds Note Offs for
/// the notes it turned on and pitch-bend resets for channels it saw bent.
pub fn fail_safe_burst() -> Vec<Vec<u8>> {
    (0..16u8)
        .flat_map(|ch| [sustain_off(ch), all_notes_off(ch)])
        .collect()
}

/// Decoder-side MIDI output: turns received messages into MIDI and owns
/// the state the fail-safe burst needs (which notes this receiver turned
/// on and has not released, which channels have a pitch bend away from
/// center).
#[derive(Debug, Clone)]
pub struct MidiOutput {
    msc: MscTarget,
    /// Notes turned on by this receiver and not yet turned off.
    sounding: NoteSet,
    /// Bit `ch` set: the last pitch bend sent on that channel was not center.
    bent: u16,
    /// Whether link-dead and Panic produce the fail-safe burst at all
    /// (off for cue-only links, where a stray All Notes Off is unwanted).
    burst_enabled: bool,
}

impl Default for MidiOutput {
    fn default() -> Self {
        Self::new(MscTarget::default())
    }
}

impl MidiOutput {
    pub fn new(msc: MscTarget) -> Self {
        Self {
            msc,
            sounding: NoteSet::default(),
            bent: 0,
            burst_enabled: true,
        }
    }

    pub fn msc_target(&self) -> MscTarget {
        self.msc
    }

    pub fn set_msc_target(&mut self, msc: MscTarget) {
        self.msc = msc;
    }

    /// Enable or disable the fail-safe burst (default: enabled). Disabled,
    /// [`fail_safe`](Self::fail_safe) returns nothing and a Panic outputs
    /// nothing; link-dead itself is still detected and reported.
    pub fn set_fail_safe_burst(&mut self, enabled: bool) {
        self.burst_enabled = enabled;
    }

    pub fn fail_safe_burst_enabled(&self) -> bool {
        self.burst_enabled
    }

    /// Notes this receiver has turned on and not yet released (per the
    /// messages it output), which the fail-safe burst releases explicitly.
    pub fn sounding(&self) -> &NoteSet {
        &self.sounding
    }

    /// MIDI messages for a received message, in order. Panic gives the
    /// fail-safe burst; Heartbeat gives nothing.
    pub fn message(&mut self, msg: &TransportMessage) -> Vec<Vec<u8>> {
        match msg {
            TransportMessage::Heartbeat => Vec::new(),
            TransportMessage::Panic => self.fail_safe(),
            TransportMessage::Msc(m) => vec![m.to_sysex(&self.msc)],
            other => {
                match *other {
                    TransportMessage::NoteOn { channel, note, .. } => {
                        self.sounding.insert(channel, note);
                    }
                    TransportMessage::NoteOff { channel, note, .. } => {
                        self.sounding.remove(channel, note);
                    }
                    TransportMessage::PitchBend { channel, value } => {
                        let bit = 1u16 << (channel & 0x0F);
                        if value == PITCH_BEND_CENTER {
                            self.bent &= !bit;
                        } else {
                            self.bent |= bit;
                        }
                    }
                    // Reset All Controllers centers pitch bend (RP-015).
                    TransportMessage::ControlChange {
                        channel,
                        controller: 121,
                        ..
                    } => self.bent &= !(1u16 << (channel & 0x0F)),
                    // All Sound Off, All Notes Off and the mode messages
                    // that imply it end the channel's notes.
                    TransportMessage::ControlChange {
                        channel,
                        controller: 120 | 123..=127,
                        ..
                    } => self.sounding.clear_channel(channel),
                    _ => {}
                }
                other
                    .to_midi_short()
                    .map(|(bytes, len)| vec![bytes[..len].to_vec()])
                    .unwrap_or_default()
            }
        }
    }

    /// The fail-safe burst sent on Panic and link-dead (spec §5.3): on each
    /// channel 1–16, a Note Off for every note this receiver turned on and
    /// has not released (synths and DAWs that ignore CC123 still release
    /// them), then CC64 = 0, then CC123 = 0, and pitch bend to center on
    /// channels whose last pitch bend was off center. Empty when the burst
    /// is disabled ([`set_fail_safe_burst`](Self::set_fail_safe_burst)).
    pub fn fail_safe(&mut self) -> Vec<Vec<u8>> {
        if !self.burst_enabled {
            self.sounding.clear();
            self.bent = 0;
            return Vec::new();
        }
        let mut out =
            Vec::with_capacity(32 + self.sounding.len() + self.bent.count_ones() as usize);
        for ch in 0..16u8 {
            out.extend(self.sounding.notes(ch).map(|note| note_off(ch, note)));
            out.push(sustain_off(ch));
            out.push(all_notes_off(ch));
            if self.bent & (1 << ch) != 0 {
                out.push(pitch_bend_center(ch));
            }
        }
        self.sounding.clear();
        self.bent = 0;
        out
    }
}

/// Stateless MIDI mapping of one message (MSC with the default
/// [`MscTarget`]; Panic without pitch-bend resets). The decoder uses
/// [`MidiOutput`].
pub fn message_to_midi(msg: &TransportMessage) -> Vec<Vec<u8>> {
    MidiOutput::default().message(msg)
}

/// What a MIDI input message becomes on the transmitter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiInput {
    /// Carried: queue it for transmission.
    Message(TransportMessage),
    /// An MSC message that cannot be carried exactly; it is not sent and
    /// the operator should be told why.
    Rejected(MscError),
    /// A well-formed MSC message addressed to another device ID or command
    /// format than the transmitter's [`MscFilter`] accepts (spec §5.4,
    /// multi-device rigs): not sent.
    NotAddressed { device_id: u8, command_format: u8 },
    /// Not carried by design (other SysEx, MIDI clock and other real-time
    /// messages, MTC quarter frames, undefined bytes).
    Ignored,
}

/// Classify one complete MIDI input message (short message or SysEx),
/// accepting MSC for any device ID and command format.
pub fn midi_input(bytes: &[u8]) -> MidiInput {
    midi_input_filtered(bytes, &MscFilter::default())
}

/// Classify one complete MIDI input message (short message or SysEx),
/// accepting only the MSC that `filter` addresses to this transmitter.
pub fn midi_input_filtered(bytes: &[u8], filter: &MscFilter) -> MidiInput {
    if bytes.first() == Some(&0xF0) {
        return match MscMessage::parse_sysex(bytes) {
            Ok(m) => {
                let (device_id, command_format) = (bytes[2], bytes[4]);
                if filter.accepts(device_id, command_format) {
                    MidiInput::Message(TransportMessage::Msc(m))
                } else {
                    MidiInput::NotAddressed {
                        device_id,
                        command_format,
                    }
                }
            }
            Err(MscError::NotMsc) => MidiInput::Ignored,
            Err(e) => MidiInput::Rejected(e),
        };
    }
    match TransportMessage::from_midi_short(bytes) {
        Some(m) => MidiInput::Message(m),
        None => MidiInput::Ignored,
    }
}

/// Parse a MIDI input message into a transport message (short messages and
/// MSC SysEx); `None` for anything not carried or not representable.
pub fn midi_to_message(bytes: &[u8]) -> Option<TransportMessage> {
    match midi_input(bytes) {
        MidiInput::Message(m) => Some(m),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::msc::{MscCommand, MscCue};

    #[test]
    fn burst_covers_every_channel_and_bent_pitch() {
        let mut out = MidiOutput::default();
        let burst = out.fail_safe();
        assert_eq!(burst.len(), 32);
        assert_eq!(burst[0], vec![0xB0, 64, 0]);
        assert_eq!(burst[1], vec![0xB0, 123, 0]);
        assert_eq!(burst[31], vec![0xBF, 123, 0]);
        assert_eq!(burst, fail_safe_burst());

        let bend = |channel, value| TransportMessage::PitchBend { channel, value };
        assert_eq!(out.message(&bend(2, 0x3FFF)), vec![vec![0xE2, 0x7F, 0x7F]]);
        out.message(&bend(5, 100));
        out.message(&bend(9, 1));
        out.message(&bend(9, PITCH_BEND_CENTER)); // returned to center
        out.message(&TransportMessage::ControlChange {
            channel: 5,
            controller: 121,
            value: 0,
        });
        let burst = out.message(&TransportMessage::Panic);
        assert_eq!(burst.len(), 33);
        assert_eq!(
            burst[4..7],
            [vec![0xB2, 64, 0], vec![0xB2, 123, 0], vec![0xE2, 0, 0x40]]
        );
        assert_eq!(out.fail_safe().len(), 32, "bend state cleared");
    }

    /// The burst releases the notes this receiver turned on explicitly,
    /// per channel before that channel's CC64 = 0 and CC123 (for synths
    /// that ignore CC123); released notes and channels cleared by CC123 /
    /// CC120 are not in it.
    #[test]
    fn burst_releases_sounding_notes_first() {
        let mut out = MidiOutput::default();
        let on = |channel, note| TransportMessage::NoteOn {
            channel,
            note,
            velocity: 100,
        };
        let off = |channel, note| TransportMessage::NoteOff {
            channel,
            note,
            velocity: 0,
        };
        for m in [
            on(0, 60),
            on(0, 64),
            on(0, 67),
            off(0, 64),
            on(3, 40),
            on(3, 41),
            on(9, 36),
        ] {
            out.message(&m);
        }
        out.message(&TransportMessage::ControlChange {
            channel: 3,
            controller: 123,
            value: 0,
        });
        assert_eq!(out.sounding().len(), 3);
        let burst = out.fail_safe();
        assert_eq!(burst.len(), 32 + 3);
        assert_eq!(
            burst[..4],
            [
                vec![0x80, 60, 0x40],
                vec![0x80, 67, 0x40],
                vec![0xB0, 64, 0],
                vec![0xB0, 123, 0]
            ]
        );
        let ch9 = burst.iter().position(|m| m[0] == 0x89).unwrap();
        assert_eq!(burst[ch9], vec![0x89, 36, 0x40]);
        assert_eq!(burst[ch9 + 1], vec![0xB9, 64, 0]);
        assert!(out.sounding().is_empty());
        assert_eq!(out.fail_safe().len(), 32);
        // A disabled burst outputs nothing, on Panic too, but still
        // forgets the notes.
        out.message(&on(0, 60));
        out.set_fail_safe_burst(false);
        assert!(!out.fail_safe_burst_enabled());
        assert!(out.message(&TransportMessage::Panic).is_empty());
        assert!(out.fail_safe().is_empty());
        assert!(out.sounding().is_empty());
        out.set_fail_safe_burst(true);
        assert_eq!(out.fail_safe().len(), 32);
    }

    /// The transmitter's MSC filter: by default every device ID and
    /// command format is accepted; with a filter only the configured ones,
    /// plus all-call / all-types.
    #[test]
    fn msc_filter_at_the_transmitter() {
        let go = |device_id: u8, format: u8| -> Vec<u8> {
            vec![0xF0, 0x7F, device_id, 0x02, format, 0x01, b'3', 0xF7]
        };
        let any = MscFilter::default();
        assert!(matches!(
            midi_input_filtered(&go(5, 0x10), &any),
            MidiInput::Message(_)
        ));
        let sound_3 = MscFilter {
            device_id: Some(3),
            command_format: Some(0x10),
        };
        assert!(matches!(
            midi_input_filtered(&go(3, 0x10), &sound_3),
            MidiInput::Message(_)
        ));
        assert!(matches!(
            midi_input_filtered(&go(0x7F, 0x7F), &sound_3),
            MidiInput::Message(_)
        ));
        assert_eq!(
            midi_input_filtered(&go(1, 0x10), &sound_3),
            MidiInput::NotAddressed {
                device_id: 1,
                command_format: 0x10
            }
        );
        assert_eq!(
            midi_input_filtered(&go(3, 0x01), &sound_3),
            MidiInput::NotAddressed {
                device_id: 3,
                command_format: 0x01
            }
        );
        let device_only = MscFilter {
            device_id: Some(3),
            command_format: None,
        };
        assert!(matches!(
            midi_input_filtered(&go(3, 0x01), &device_only),
            MidiInput::Message(_)
        ));
        // Malformed MSC is still rejected, other SysEx ignored.
        assert!(matches!(
            midi_input_filtered(
                &[0xF0, 0x7F, 0x01, 0x02, 0x01, 0x01, b'1', b'.', b'2', b'5', 0xF7],
                &sound_3
            ),
            MidiInput::Rejected(_)
        ));
        assert_eq!(
            midi_input_filtered(&[0xF0, 0x41, 0x10, 0xF7], &sound_3),
            MidiInput::Ignored
        );
    }

    #[test]
    fn msc_goes_out_as_sysex_with_receiver_settings() {
        let go = TransportMessage::Msc(MscMessage::cue(
            MscCommand::Go,
            MscCue::from_tenths(15).unwrap(),
            None,
        ));
        assert_eq!(
            message_to_midi(&go),
            vec![vec![
                0xF0, 0x7F, 0x7F, 0x02, 0x01, 0x01, b'1', b'.', b'5', 0xF7
            ]]
        );
        let mut out = MidiOutput::new(MscTarget {
            device_id: 3,
            command_format: 0x10,
        });
        assert_eq!(out.message(&go)[0][2..5], [3, 0x02, 0x10]);
    }

    #[test]
    fn midi_input_classification() {
        let msc = [0xF0, 0x7F, 0x00, 0x02, 0x01, 0x01, b'3', 0xF7];
        assert!(matches!(
            midi_input(&msc),
            MidiInput::Message(TransportMessage::Msc(_))
        ));
        let bad = [
            0xF0, 0x7F, 0x00, 0x02, 0x01, 0x01, b'3', b'.', b'2', b'5', 0xF7,
        ];
        assert!(matches!(midi_input(&bad), MidiInput::Rejected(_)));
        assert_eq!(midi_input(&[0xF0, 0x41, 0x10, 0xF7]), MidiInput::Ignored);
        assert_eq!(midi_input(&[0xF8]), MidiInput::Ignored);
        assert_eq!(midi_input(&[0xF1, 0x20]), MidiInput::Ignored);
        assert_eq!(
            midi_input(&[0x95, 60, 0]),
            MidiInput::Message(TransportMessage::NoteOff {
                channel: 5,
                note: 60,
                velocity: 64
            })
        );
        assert_eq!(
            midi_to_message(&[0xBF, 123, 0]),
            Some(TransportMessage::ControlChange {
                channel: 15,
                controller: 123,
                value: 0
            })
        );
    }
}
