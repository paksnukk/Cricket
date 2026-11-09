//! MIDI Show Control (MSC 1.0) in a compact 3-byte form (spec §5.4).
//!
//! An MSC message on MIDI is a SysEx: `F0 7F <device_id> 02
//! <command_format> <command> <data> F7`. Cricket carries the command and
//! its data only; the device ID and command format are receiver settings
//! ([`MscTarget`]). Cue and cue-list numbers are ASCII digits with at most one
//! `.` (`"14.5"`), separated by `00` (`<cue> 00 <list> 00 <path>`).
//!
//! Compact form (24 bits, big-endian):
//!
//! ```text
//! bits 23..19  command      MSC command byte (0x01–0x1C, supported set only)
//! bits 18..15  list         0 = no cue list, 1–15 = cue list 1–15
//! bits 14..0   value        cue commands: 0 = no cue, n = cue (n − 1) / 10
//!                           (cue 0–999.9, one decimal); FIRE: macro 0–127;
//!                           other commands: 0
//! ```
//!
//! What does not fit is rejected, never approximated ([`MscError`]).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Largest cue number carried, in tenths (999.9).
pub const MSC_MAX_CUE_TENTHS: u16 = 9999;
/// Largest cue list number carried.
pub const MSC_MAX_LIST: u8 = 15;

/// Supported MSC commands (MSC 1.0 command bytes).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MscCommand {
    Go = 0x01,
    Stop = 0x02,
    Resume = 0x03,
    Load = 0x05,
    Fire = 0x07,
    AllOff = 0x08,
    Restore = 0x09,
    Reset = 0x0A,
    GoOff = 0x0B,
    GoJamClock = 0x10,
    StandbyPlus = 0x11,
    StandbyMinus = 0x12,
    SequencePlus = 0x13,
    SequenceMinus = 0x14,
    StartClock = 0x15,
    StopClock = 0x16,
    ZeroClock = 0x17,
    MtcChaseOn = 0x19,
    MtcChaseOff = 0x1A,
    OpenCueList = 0x1B,
    CloseCueList = 0x1C,
}

/// What data a command takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `[cue [00 list]]`.
    OptionalCue,
    /// `cue [00 list]` (LOAD).
    RequiredCue,
    /// One byte, macro number 0–127 (FIRE).
    Macro,
    /// No data.
    Nothing,
    /// `[list]`.
    OptionalList,
    /// `list`.
    RequiredList,
}

impl MscCommand {
    pub const ALL: [MscCommand; 21] = [
        Self::Go,
        Self::Stop,
        Self::Resume,
        Self::Load,
        Self::Fire,
        Self::AllOff,
        Self::Restore,
        Self::Reset,
        Self::GoOff,
        Self::GoJamClock,
        Self::StandbyPlus,
        Self::StandbyMinus,
        Self::SequencePlus,
        Self::SequenceMinus,
        Self::StartClock,
        Self::StopClock,
        Self::ZeroClock,
        Self::MtcChaseOn,
        Self::MtcChaseOff,
        Self::OpenCueList,
        Self::CloseCueList,
    ];

    pub fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| *c as u8 == v)
    }

    /// MSC 1.0 name, e.g. `"GO"`, `"ALL_OFF"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Go => "GO",
            Self::Stop => "STOP",
            Self::Resume => "RESUME",
            Self::Load => "LOAD",
            Self::Fire => "FIRE",
            Self::AllOff => "ALL_OFF",
            Self::Restore => "RESTORE",
            Self::Reset => "RESET",
            Self::GoOff => "GO_OFF",
            Self::GoJamClock => "GO/JAM_CLOCK",
            Self::StandbyPlus => "STANDBY_+",
            Self::StandbyMinus => "STANDBY_-",
            Self::SequencePlus => "SEQUENCE_+",
            Self::SequenceMinus => "SEQUENCE_-",
            Self::StartClock => "START_CLOCK",
            Self::StopClock => "STOP_CLOCK",
            Self::ZeroClock => "ZERO_CLOCK",
            Self::MtcChaseOn => "MTC_CHASE_ON",
            Self::MtcChaseOff => "MTC_CHASE_OFF",
            Self::OpenCueList => "OPEN_CUE_LIST",
            Self::CloseCueList => "CLOSE_CUE_LIST",
        }
    }

    fn shape(self) -> Shape {
        match self {
            Self::Go | Self::Stop | Self::Resume | Self::GoOff | Self::GoJamClock => {
                Shape::OptionalCue
            }
            Self::Load => Shape::RequiredCue,
            Self::Fire => Shape::Macro,
            Self::AllOff | Self::Restore | Self::Reset => Shape::Nothing,
            Self::StandbyPlus
            | Self::StandbyMinus
            | Self::SequencePlus
            | Self::SequenceMinus
            | Self::StartClock
            | Self::StopClock
            | Self::ZeroClock
            | Self::MtcChaseOn
            | Self::MtcChaseOff => Shape::OptionalList,
            Self::OpenCueList | Self::CloseCueList => Shape::RequiredList,
        }
    }
}

/// Name of an MSC command byte this implementation does not carry.
fn unsupported_name(cmd: u8) -> Option<&'static str> {
    Some(match cmd {
        0x04 => "TIMED_GO",
        0x06 => "SET",
        0x18 => "SET_CLOCK",
        0x1D => "OPEN_CUE_PATH",
        0x1E => "CLOSE_CUE_PATH",
        _ => return None,
    })
}

/// A cue number with at most one decimal, 0–999.9, stored in tenths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MscCue(u16);

impl MscCue {
    /// Cue from tenths (`145` = cue 14.5); `None` above 999.9.
    pub fn from_tenths(tenths: u16) -> Option<Self> {
        (tenths <= MSC_MAX_CUE_TENTHS).then_some(Self(tenths))
    }

    pub fn tenths(self) -> u16 {
        self.0
    }

    /// MSC ASCII form: `"14"`, `"14.5"`.
    pub fn to_ascii(self) -> Vec<u8> {
        let mut out = Vec::new();
        push_decimal(&mut out, u32::from(self.0 / 10));
        if !self.0.is_multiple_of(10) {
            out.push(b'.');
            out.push(b'0' + (self.0 % 10) as u8);
        }
        out
    }
}

impl fmt::Display for MscCue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_multiple_of(10) {
            write!(f, "{}", self.0 / 10)
        } else {
            write!(f, "{}.{}", self.0 / 10, self.0 % 10)
        }
    }
}

fn push_decimal(out: &mut Vec<u8>, v: u32) {
    let start = out.len();
    let mut v = v;
    loop {
        out.push(b'0' + (v % 10) as u8);
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out[start..].reverse();
}

/// An MSC message Cricket can carry. Build it with the constructors; they
/// enforce the per-command data rules ([`is_valid`](Self::is_valid)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MscMessage {
    pub command: MscCommand,
    /// Cue number (cue commands only).
    pub cue: Option<MscCue>,
    /// Cue list 1–15 (cue commands with a cue, and list commands).
    pub list: Option<u8>,
    /// Macro number 0–127 (FIRE only).
    pub macro_number: Option<u8>,
}

impl MscMessage {
    /// A command without data (also GO / STOP / … without a cue, and the
    /// list commands without a list).
    pub fn new(command: MscCommand) -> Self {
        Self {
            command,
            cue: None,
            list: None,
            macro_number: None,
        }
    }

    /// A cue command (`GO 14.5`, list optional).
    pub fn cue(command: MscCommand, cue: MscCue, list: Option<u8>) -> Self {
        Self {
            cue: Some(cue),
            list,
            ..Self::new(command)
        }
    }

    /// A list command (`STANDBY_+ 2`, `OPEN_CUE_LIST 3`).
    pub fn list(command: MscCommand, list: u8) -> Self {
        Self {
            list: Some(list),
            ..Self::new(command)
        }
    }

    /// `FIRE <macro>`.
    pub fn fire(macro_number: u8) -> Self {
        Self {
            macro_number: Some(macro_number),
            ..Self::new(MscCommand::Fire)
        }
    }

    /// Whether the fields match the command and are in range.
    pub fn is_valid(&self) -> bool {
        let list_ok = self.list.is_none_or(|l| (1..=MSC_MAX_LIST).contains(&l));
        let cue_ok = self.cue.is_none_or(|c| c.0 <= MSC_MAX_CUE_TENTHS);
        list_ok
            && cue_ok
            && match self.command.shape() {
                Shape::OptionalCue | Shape::RequiredCue => {
                    self.macro_number.is_none()
                        && (self.cue.is_some()
                            || (self.command.shape() == Shape::OptionalCue && self.list.is_none()))
                }
                Shape::Macro => {
                    self.cue.is_none()
                        && self.list.is_none()
                        && self.macro_number.is_some_and(|m| m < 0x80)
                }
                Shape::Nothing => {
                    self.cue.is_none() && self.list.is_none() && self.macro_number.is_none()
                }
                Shape::OptionalList | Shape::RequiredList => {
                    self.cue.is_none()
                        && self.macro_number.is_none()
                        && (self.list.is_some() || self.command.shape() == Shape::OptionalList)
                }
            }
    }

    /// The 3-byte compact payload. Panics if the message is not valid.
    pub fn to_compact(&self) -> [u8; 3] {
        assert!(self.is_valid(), "invalid MSC message {self:?}");
        let value = match (self.cue, self.macro_number) {
            (Some(c), _) => u32::from(c.0) + 1,
            (None, Some(m)) => u32::from(m),
            (None, None) => 0,
        };
        let bits = (u32::from(self.command as u8) << 19)
            | (u32::from(self.list.unwrap_or(0)) << 15)
            | value;
        [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8]
    }

    /// Decode a compact payload; `None` unless it is canonical.
    pub fn from_compact(p: &[u8]) -> Option<Self> {
        let &[b0, b1, b2] = p else {
            return None;
        };
        let bits = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        let command = MscCommand::from_u8((bits >> 19) as u8)?;
        let list = ((bits >> 15) & 0x0F) as u8;
        let value = (bits & 0x7FFF) as u16;
        let list = (list != 0).then_some(list);
        let mut msg = Self {
            list,
            ..Self::new(command)
        };
        match command.shape() {
            Shape::OptionalCue | Shape::RequiredCue => {
                if value != 0 {
                    msg.cue = Some(MscCue::from_tenths(value - 1)?);
                }
            }
            Shape::Macro => msg.macro_number = Some(u8::try_from(value).ok()?),
            _ if value != 0 => return None,
            _ => {}
        }
        msg.is_valid().then_some(msg)
    }

    /// The MSC SysEx: `F0 7F <device_id> 02 <command_format> <command>
    /// <data> F7`.
    pub fn to_sysex(&self, target: &MscTarget) -> Vec<u8> {
        let mut out = alloc::vec![
            0xF0,
            0x7F,
            target.device_id & 0x7F,
            0x02,
            target.command_format & 0x7F,
            self.command as u8,
        ];
        if let Some(cue) = self.cue {
            out.extend(cue.to_ascii());
            if let Some(list) = self.list {
                out.push(0x00);
                push_decimal(&mut out, u32::from(list));
            }
        } else if let Some(list) = self.list {
            push_decimal(&mut out, u32::from(list));
        } else if let Some(m) = self.macro_number {
            out.push(m & 0x7F);
        }
        out.push(0xF7);
        out
    }

    /// Parse an MSC SysEx (any device ID and command format).
    ///
    /// `Err(MscError::NotMsc)` for SysEx that is not MSC (ignore it); any
    /// other error is an MSC message Cricket cannot carry exactly and must
    /// not send (report it). Cue and list numbers are compared as numbers:
    /// `"05"`, `"5.0"` and `"5"` are the same cue and arrive as `"5"`.
    pub fn parse_sysex(bytes: &[u8]) -> Result<Self, MscError> {
        if bytes.len() < 6 || bytes[0] != 0xF0 || bytes[1] != 0x7F || bytes[3] != 0x02 {
            return Err(MscError::NotMsc);
        }
        if bytes[bytes.len() - 1] != 0xF7 || bytes[1..bytes.len() - 1].iter().any(|&b| b >= 0x80) {
            return Err(MscError::Malformed);
        }
        let cmd = bytes[5];
        let data = &bytes[6..bytes.len() - 1];
        let Some(command) = MscCommand::from_u8(cmd) else {
            return Err(match unsupported_name(cmd) {
                Some(name) => MscError::UnsupportedCommand(name),
                None => MscError::UnknownCommand(cmd),
            });
        };
        let fields: Vec<&[u8]> = if data.is_empty() {
            Vec::new()
        } else {
            data.split(|&b| b == 0).collect()
        };
        let mut msg = Self::new(command);
        match command.shape() {
            Shape::OptionalCue | Shape::RequiredCue => {
                if fields.len() > 2 {
                    return Err(MscError::CuePath(command));
                }
                if let Some(text) = fields.first() {
                    msg.cue = Some(parse_cue(text)?);
                }
                if let Some(text) = fields.get(1) {
                    msg.list = Some(parse_list(text)?);
                }
                if msg.cue.is_none() && command.shape() == Shape::RequiredCue {
                    return Err(MscError::Malformed);
                }
            }
            Shape::Macro => match data {
                [m] => msg.macro_number = Some(*m),
                _ => return Err(MscError::Malformed),
            },
            Shape::Nothing => {
                if !data.is_empty() {
                    return Err(MscError::Malformed);
                }
            }
            Shape::OptionalList | Shape::RequiredList => {
                match fields.as_slice() {
                    [] if command.shape() == Shape::OptionalList => {}
                    [text] => msg.list = Some(parse_list(text)?),
                    _ => return Err(MscError::Malformed),
                };
            }
        }
        debug_assert!(msg.is_valid());
        Ok(msg)
    }
}

impl fmt::Display for MscMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.command.name())?;
        if let Some(cue) = self.cue {
            write!(f, " cue {}", cue)?;
        }
        if let Some(list) = self.list {
            write!(f, " list {}", list)?;
        }
        if let Some(m) = self.macro_number {
            write!(f, " macro {}", m)?;
        }
        Ok(())
    }
}

/// An ASCII MSC number: digits with at most one `.`. Returns (integer
/// part, fraction digits without trailing zeros), or `None` if malformed.
fn parse_number(text: &[u8]) -> Option<(u32, &[u8])> {
    if text.is_empty() || !text.iter().any(u8::is_ascii_digit) {
        return None;
    }
    let (int, frac) = match text.iter().position(|&b| b == b'.') {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => (text, &text[..0]),
    };
    if !int.iter().chain(frac).all(u8::is_ascii_digit) {
        return None;
    }
    let mut value = 0u32;
    for &d in int {
        value = value.saturating_mul(10).saturating_add(u32::from(d - b'0'));
    }
    let frac_len = frac.iter().rposition(|&b| b != b'0').map_or(0, |i| i + 1);
    Some((value, &frac[..frac_len]))
}

fn parse_cue(text: &[u8]) -> Result<MscCue, MscError> {
    let unrepresentable = || MscError::CueNotRepresentable(ascii(text));
    let (int, frac) = parse_number(text).ok_or(MscError::Malformed)?;
    if frac.len() > 1 || int > u32::from(MSC_MAX_CUE_TENTHS / 10) {
        return Err(unrepresentable());
    }
    let tenths = int * 10 + frac.first().map_or(0, |d| u32::from(d - b'0'));
    MscCue::from_tenths(tenths as u16).ok_or_else(unrepresentable)
}

fn parse_list(text: &[u8]) -> Result<u8, MscError> {
    let (int, frac) = parse_number(text).ok_or(MscError::Malformed)?;
    if !frac.is_empty() || !(1..=u32::from(MSC_MAX_LIST)).contains(&int) {
        return Err(MscError::ListNotRepresentable(ascii(text)));
    }
    Ok(int as u8)
}

fn ascii(text: &[u8]) -> String {
    text.iter()
        .map(|&b| {
            if b.is_ascii_graphic() {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

/// Why an MSC SysEx was not converted.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MscError {
    /// Not an MSC SysEx (other SysEx): ignore it.
    NotMsc,
    /// MSC, but not well-formed (bad data for the command, bytes ≥ 0x80).
    Malformed,
    /// An MSC command Cricket does not carry (TIMED_GO, SET, SET_CLOCK,
    /// OPEN_CUE_PATH, CLOSE_CUE_PATH).
    UnsupportedCommand(&'static str),
    /// A command byte that is not in MSC 1.0.
    UnknownCommand(u8),
    /// The cue number does not fit (more than one decimal, above 999.9).
    CueNotRepresentable(String),
    /// The cue list is not 1–15.
    ListNotRepresentable(String),
    /// The message has a cue path.
    CuePath(MscCommand),
}

impl fmt::Display for MscError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMsc => write!(f, "not an MSC message"),
            Self::Malformed => write!(f, "malformed MSC message"),
            Self::UnsupportedCommand(name) => write!(f, "MSC {name} is not supported"),
            Self::UnknownCommand(c) => write!(f, "unknown MSC command {c:#04x}"),
            Self::CueNotRepresentable(t) => write!(
                f,
                "MSC cue {t} not representable (cues 0–999.9 with at most one decimal)"
            ),
            Self::ListNotRepresentable(t) => {
                write!(f, "MSC cue list {t} not representable (lists 1–15)")
            }
            Self::CuePath(c) => write!(f, "MSC {} with a cue path is not supported", c.name()),
        }
    }
}

/// Receiver-side MSC settings: the device ID and command format put on
/// every MSC SysEx the decoder outputs (spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MscTarget {
    /// 0x00–0x6F individual device, 0x70–0x7E group, 0x7F all-call.
    pub device_id: u8,
    /// 0x01 Lighting (General), 0x10 Sound (General), 0x7F All-types, …
    pub command_format: u8,
}

impl MscTarget {
    /// All-call device ID: every MSC device responds.
    pub const DEFAULT_DEVICE_ID: u8 = 0x7F;
    /// Lighting (General).
    pub const DEFAULT_COMMAND_FORMAT: u8 = 0x01;
}

impl Default for MscTarget {
    fn default() -> Self {
        Self {
            device_id: Self::DEFAULT_DEVICE_ID,
            command_format: Self::DEFAULT_COMMAND_FORMAT,
        }
    }
}

/// Transmitter-side MSC filter (spec §5.4, multi-device rigs): which
/// device ID and command format an incoming MSC SysEx must address for the
/// transmitter to carry it. `None` accepts any value; the default accepts
/// everything, so a single-device rig needs no setting. An all-call device
/// ID (`0x7F`) and the all-types command format (`0x7F`) always pass: they
/// address every device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MscFilter {
    /// Accept only MSC addressed to this device ID (or to all-call).
    pub device_id: Option<u8>,
    /// Accept only MSC in this command format (or all-types).
    pub command_format: Option<u8>,
}

impl MscFilter {
    /// All-call device ID and all-types command format.
    pub const ALL: u8 = 0x7F;

    /// Whether an MSC SysEx with `device_id` and `command_format` passes.
    pub fn accepts(&self, device_id: u8, command_format: u8) -> bool {
        let device_ok = device_id == Self::ALL || self.device_id.is_none_or(|d| d == device_id);
        let format_ok =
            command_format == Self::ALL || self.command_format.is_none_or(|f| f == command_format);
        device_ok && format_ok
    }

    /// Whether anything is filtered at all.
    pub fn is_filtering(&self) -> bool {
        self.device_id.is_some() || self.command_format.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn cue(t: u16) -> MscCue {
        MscCue::from_tenths(t).unwrap()
    }

    fn sysex(cmd: u8, data: &[u8]) -> Vec<u8> {
        let mut v = vec![0xF0, 0x7F, 0x01, 0x02, 0x01, cmd];
        v.extend_from_slice(data);
        v.push(0xF7);
        v
    }

    /// Every supported command, with each data form it takes.
    fn samples() -> Vec<MscMessage> {
        let mut v = Vec::new();
        for c in MscCommand::ALL {
            match c.shape() {
                Shape::OptionalCue | Shape::RequiredCue => {
                    if c.shape() == Shape::OptionalCue {
                        v.push(MscMessage::new(c));
                    }
                    for t in [0, 1, 10, 145, 9999] {
                        v.push(MscMessage::cue(c, cue(t), None));
                        v.push(MscMessage::cue(c, cue(t), Some(1)));
                        v.push(MscMessage::cue(c, cue(t), Some(15)));
                    }
                }
                Shape::Macro => {
                    for m in [0, 1, 127] {
                        v.push(MscMessage::fire(m));
                    }
                }
                Shape::Nothing => v.push(MscMessage::new(c)),
                Shape::OptionalList | Shape::RequiredList => {
                    if c.shape() == Shape::OptionalList {
                        v.push(MscMessage::new(c));
                    }
                    v.push(MscMessage::list(c, 1));
                    v.push(MscMessage::list(c, 15));
                }
            }
        }
        v
    }

    #[test]
    fn compact_and_sysex_round_trips_for_every_command() {
        let target = MscTarget::default();
        for m in samples() {
            assert!(m.is_valid(), "{m:?}");
            let compact = m.to_compact();
            assert_eq!(MscMessage::from_compact(&compact), Some(m), "{m}");
            let sx = m.to_sysex(&target);
            assert_eq!(&sx[..5], &[0xF0, 0x7F, 0x7F, 0x02, 0x01]);
            assert_eq!(MscMessage::parse_sysex(&sx), Ok(m), "{m} {sx:02x?}");
        }
    }

    #[test]
    fn sysex_byte_format() {
        let t = MscTarget {
            device_id: 0x01,
            command_format: 0x10,
        };
        // GO 14.5 list 2: F0 7F 01 02 10 01 "14.5" 00 "2" F7.
        assert_eq!(
            MscMessage::cue(MscCommand::Go, cue(145), Some(2)).to_sysex(&t),
            vec![0xF0, 0x7F, 0x01, 0x02, 0x10, 0x01, 0x31, 0x34, 0x2E, 0x35, 0x00, 0x32, 0xF7]
        );
        // GO 100 (no list, no decimal point).
        assert_eq!(
            MscMessage::cue(MscCommand::Go, cue(1000), None).to_sysex(&t),
            vec![0xF0, 0x7F, 0x01, 0x02, 0x10, 0x01, 0x31, 0x30, 0x30, 0xF7]
        );
        // GO 0.5.
        assert_eq!(cue(5).to_ascii(), b"0.5".to_vec());
        // GO without cue, ALL_OFF, FIRE 12, STANDBY_+ list 3.
        assert_eq!(
            MscMessage::new(MscCommand::Go).to_sysex(&t),
            vec![0xF0, 0x7F, 0x01, 0x02, 0x10, 0x01, 0xF7]
        );
        assert_eq!(
            MscMessage::new(MscCommand::AllOff).to_sysex(&t)[5..],
            [0x08, 0xF7]
        );
        assert_eq!(MscMessage::fire(12).to_sysex(&t)[5..], [0x07, 12, 0xF7]);
        assert_eq!(
            MscMessage::list(MscCommand::StandbyPlus, 3).to_sysex(&t)[5..],
            [0x11, 0x33, 0xF7]
        );
    }

    #[test]
    fn numbers_are_parsed_as_numbers() {
        let go = |data: &[u8]| MscMessage::parse_sysex(&sysex(0x01, data));
        let expect = |t: u16, l: Option<u8>| Ok(MscMessage::cue(MscCommand::Go, cue(t), l));
        assert_eq!(go(b"5"), expect(50, None));
        assert_eq!(go(b"05"), expect(50, None));
        assert_eq!(go(b"5.0"), expect(50, None));
        assert_eq!(go(b"5."), expect(50, None));
        assert_eq!(go(b".5"), expect(5, None));
        assert_eq!(go(b"12.50"), expect(125, None));
        assert_eq!(go(b"999.9"), expect(9999, None));
        assert_eq!(go(b"1\x002"), expect(10, Some(2)));
        assert_eq!(go(b"1\x0002"), expect(10, Some(2)));
        assert_eq!(go(b"1\x0015.0"), expect(10, Some(15)));
        // Device ID and command format are not carried: any are accepted.
        let mut sx = sysex(0x01, b"7");
        sx[2] = 0x7F;
        sx[4] = 0x7F;
        assert_eq!(MscMessage::parse_sysex(&sx), expect(70, None));
    }

    #[test]
    fn unrepresentable_messages_are_rejected_not_mangled() {
        let parse = |cmd: u8, data: &[u8]| MscMessage::parse_sysex(&sysex(cmd, data));
        let cue_err = |t: &str| Err(MscError::CueNotRepresentable(t.into()));
        assert_eq!(parse(0x01, b"12.345"), cue_err("12.345"));
        assert_eq!(parse(0x01, b"12.25"), cue_err("12.25"));
        assert_eq!(parse(0x01, b"1000"), cue_err("1000"));
        assert_eq!(parse(0x01, b"99999999999"), cue_err("99999999999"));
        assert_eq!(
            parse(0x01, b"1\x0016"),
            Err(MscError::ListNotRepresentable("16".into()))
        );
        assert_eq!(
            parse(0x01, b"1\x000"),
            Err(MscError::ListNotRepresentable("0".into()))
        );
        assert_eq!(
            parse(0x01, b"1\x001.5"),
            Err(MscError::ListNotRepresentable("1.5".into()))
        );
        assert_eq!(
            parse(0x01, b"1\x001\x003"),
            Err(MscError::CuePath(MscCommand::Go))
        );
        assert_eq!(
            parse(0x04, &[0, 0, 0, 0, 0]),
            Err(MscError::UnsupportedCommand("TIMED_GO"))
        );
        assert_eq!(
            parse(0x06, &[1, 0, 2, 0]),
            Err(MscError::UnsupportedCommand("SET"))
        );
        assert_eq!(parse(0x1F, b""), Err(MscError::UnknownCommand(0x1F)));
        for (cmd, data) in [
            (0x01, &b"1.2.3"[..]),
            (0x01, b"a"),
            (0x01, b"."),
            (0x01, b"\x001"),
            (0x05, b""), // LOAD needs a cue
            (0x07, b""),
            (0x07, b"\x01\x02"),
            (0x08, b"1"),
            (0x1B, b""),
            (0x11, b"1\x002"),
        ] {
            assert_eq!(
                parse(cmd, data),
                Err(MscError::Malformed),
                "{cmd:#x} {data:?}"
            );
        }
        assert_eq!(
            MscMessage::parse_sysex(&[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7]),
            Err(MscError::NotMsc)
        );
        assert_eq!(
            MscMessage::parse_sysex(&[0xF0, 0x43, 0x10, 0x4C, 0, 0, 0x7E, 0, 0xF7]),
            Err(MscError::NotMsc)
        );
        assert_eq!(
            MscMessage::parse_sysex(&[0xF0, 0x7F, 0x01, 0x02, 0x01, 0x01, 0x31]),
            Err(MscError::Malformed),
            "missing F7"
        );
        assert_eq!(
            MscError::CueNotRepresentable("12.345".into()).to_string(),
            "MSC cue 12.345 not representable (cues 0–999.9 with at most one decimal)"
        );
    }

    #[test]
    fn filter_accepts_configured_and_all_call() {
        let any = MscFilter::default();
        assert!(!any.is_filtering());
        assert!(any.accepts(0, 0) && any.accepts(0x7F, 0x7F) && any.accepts(5, 0x10));
        let f = MscFilter {
            device_id: Some(5),
            command_format: Some(0x10),
        };
        assert!(f.is_filtering());
        assert!(f.accepts(5, 0x10));
        assert!(f.accepts(0x7F, 0x10) && f.accepts(5, 0x7F) && f.accepts(0x7F, 0x7F));
        assert!(!f.accepts(4, 0x10) && !f.accepts(5, 0x01) && !f.accepts(0x70, 0x10));
        let group = MscFilter {
            device_id: Some(0x70),
            command_format: None,
        };
        assert!(group.accepts(0x70, 0x01) && group.accepts(0x7F, 0x30) && !group.accepts(0, 0x01));
    }

    #[test]
    fn non_canonical_compact_payloads_are_rejected() {
        let enc = |cmd: u32, list: u32, value: u32| {
            let bits = (cmd << 19) | (list << 15) | value;
            [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8]
        };
        let ok = |p: [u8; 3]| MscMessage::from_compact(&p).is_some();
        assert!(ok(enc(0x01, 0, 0)));
        assert!(ok(enc(0x01, 15, 10_000)));
        assert!(!ok(enc(0x01, 0, 10_001)), "cue above 999.9");
        assert!(!ok(enc(0x01, 1, 0)), "list without cue");
        assert!(!ok(enc(0x05, 0, 0)), "LOAD without cue");
        assert!(!ok(enc(0x04, 0, 0)), "TIMED_GO");
        assert!(!ok(enc(0x00, 0, 0)));
        assert!(!ok(enc(0x07, 0, 128)), "macro above 127");
        assert!(!ok(enc(0x07, 1, 1)), "FIRE with list");
        assert!(!ok(enc(0x08, 0, 1)), "ALL_OFF with data");
        assert!(!ok(enc(0x11, 0, 1)), "STANDBY_+ with value");
        assert!(!ok(enc(0x1B, 0, 0)), "OPEN_CUE_LIST needs a list");
    }

    /// Valid compact payloads per command class, for the FEC analysis
    /// (`docs/proposals/fec.md` §9.7).
    #[test]
    fn valid_compact_payload_count() {
        let mut n = 0u32;
        // Other command fields never decode (`non_canonical_…` above).
        for c in MscCommand::ALL {
            for rest in 0..(1u32 << 19) {
                let bits = (u32::from(c as u8) << 19) | rest;
                let p = [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8];
                n += MscMessage::from_compact(&p).is_some() as u32;
            }
        }
        // 5 optional-cue commands × (1 + 10000 × 16), LOAD 10000 × 16,
        // FIRE 128, 3 without data, 9 × 16 optional list, 2 × 15 list.
        assert_eq!(n, 5 * 160_001 + 160_000 + 128 + 3 + 9 * 16 + 2 * 15);
    }
}
