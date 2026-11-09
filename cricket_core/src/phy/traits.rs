//! The phy boundary: what [`crate::modem`] needs from a physical layer, and
//! nothing else.
//!
//! A modulation is three things: a [`PhyFactory`] (the parameters of the
//! plan, which build the transmitter and the receiver and declare how the
//! link-layer body is laid out and how the TX scheduler should repeat a
//! released note), a [`PhyTx`] (nibbles in, one burst of samples out) and a
//! [`PhyRx`] (samples in, [`RxEvent`]s out). The built-in modulations
//! implement them on their parameter structs ([`MfskParams`](super::MfskParams),
//! [`BmcParams`](super::BmcParams), [`DigitalParams`](super::DigitalParams));
//! an external crate implements them on its own type and plugs it in with
//! [`Modulation::Custom`](super::Modulation::Custom). `docs/architecture.md`
//! has the walk-through.
//!
//! The traits are object-safe and `Send`, so a modem can own
//! `Box<dyn PhyRx>` and be moved to a worker thread. Everything else of the
//! link — packet format, CRC, FEC, duplicate suppression, fail-safe,
//! scheduling — is the modem's and is the same for every phy.

use super::config::PhyConfig;
use super::demod::{LinkState, RxEvent};
use crate::link::FecMode;
use crate::transport::{RefreshSchedule, REFRESH_SCHEDULE_CONSECUTIVE, REFRESH_SCHEDULE_SPREAD};
use alloc::boxed::Box;
use alloc::vec::Vec;

/// How the link-layer packet bytes are laid out in the body a phy carries,
/// declared by the phy ([`PhyFactory::body_coding`]) so the modem never has
/// to know which modulation it is talking to.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyCoding {
    /// Uncoded body (spec §4.3): the packet nibbles zero-padded to
    /// [`MAX_BODY_NIBBLES`](crate::modem::MAX_BODY_NIBBLES) (16) and
    /// block-interleaved with `PhyConfig::interleave_depth`.
    Padded16,
    /// Coded body (spec §4.6): the packet nibbles RS16-coded and whitened;
    /// the body is `mode.body_nibbles()` nibbles (22 with parity 4). The
    /// receiver's nibble reliabilities and erasures feed the decoder.
    Fec(FecMode),
    /// The packet nibbles as they are: no pad, interleave or code. The phy
    /// frames each packet itself (its length follows from the type nibble)
    /// and its receiver reports whole frames ([`RxEvent::Frame`]) instead of
    /// body nibbles. The modem runs no separate digital-frame detector next
    /// to such a phy (`PhyConfig::digital_detect`): it is its own.
    Raw,
}

impl BodyCoding {
    /// `Fec(mode)` when `mode` is enabled, `Padded16` otherwise: the coding
    /// of the MFSK and BMC phys for a `PhyConfig::fec`.
    pub fn from_fec(mode: FecMode) -> Self {
        if mode.is_enabled() {
            Self::Fec(mode)
        } else {
            Self::Padded16
        }
    }

    /// Nibbles of one body on the wire: 16, the coded length, or for `Raw`
    /// the largest packet (7 bytes).
    pub fn body_nibbles(&self) -> usize {
        match self {
            Self::Padded16 => crate::modem::MAX_BODY_NIBBLES,
            Self::Fec(mode) => mode.body_nibbles(),
            Self::Raw => 2 * super::digital::MAX_PACKET_BYTES,
        }
    }

    /// The FEC mode of a coded body, `FecMode::None` otherwise.
    pub fn fec_mode(&self) -> FecMode {
        match self {
            Self::Fec(mode) => *mode,
            _ => FecMode::None,
        }
    }
}

/// How the TX scheduler repeats a released note on this phy (spec §5.5),
/// declared by [`PhyFactory::refresh_policy`]. The modem turns it into
/// sample counts for [`crate::transport::TxScheduler`]
/// (`refresh_schedule`, `refresh_spacing_samples`, `refresh_quiet_samples`
/// in [`crate::modem`]).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshPolicy {
    /// The slots of the six repeats: the Fibonacci spread (messages of
    /// 50–300 ms, where consecutive copies would fall into one dropout) or
    /// the first six free slots (messages of a few ms).
    pub schedule: RefreshSchedule,
    /// Least spacing between the repeats of one released note, ms (0 =
    /// none); for phys whose message is under a millisecond, so the repeats
    /// do not all fall into one short dropout.
    pub spacing_ms: u32,
    /// With a coded body ([`BodyCoding::Fec`]), a repeat waits for one
    /// message time of idle line, so a note struck shortly after another
    /// never waits behind a repeat; uncoded bodies lose far more repeats
    /// and need them promptly.
    pub quiet_when_coded: bool,
}

impl RefreshPolicy {
    /// The MFSK policy: the spread, no extra spacing, quiet line when coded.
    pub const SPREAD: Self = Self {
        schedule: REFRESH_SCHEDULE_SPREAD,
        spacing_ms: 0,
        quiet_when_coded: true,
    };
    /// The policy of the short-message phys (BMC): the first six free
    /// slots, no spacing, no quiet time.
    pub const CONSECUTIVE: Self = Self {
        schedule: REFRESH_SCHEDULE_CONSECUTIVE,
        spacing_ms: 0,
        quiet_when_coded: false,
    };

    /// `CONSECUTIVE` with the repeats at least `spacing_ms` apart.
    pub const fn consecutive_spaced(spacing_ms: u32) -> Self {
        Self {
            spacing_ms,
            ..Self::CONSECUTIVE
        }
    }
}

/// The parameters of a modulation: build its transmitter and receiver and
/// describe the frame to the modem. Implemented on the parameter struct of
/// each built-in modulation and by external crates for
/// [`Modulation::Custom`](super::Modulation::Custom).
///
/// Every method takes the whole [`PhyConfig`] (whose `modulation` holds
/// `self`), because the frame also depends on the common parameters: sample
/// rate, amplitude and the body length that follows from `config.fec`.
pub trait PhyFactory: core::fmt::Debug + Send + Sync {
    /// Short lower-case name of the modulation (`"mfsk"`, `"bmc"`,
    /// `"digital"`; an external crate uses its own). Two `Custom`
    /// modulations compare equal when their names are equal.
    fn name(&self) -> &str;

    /// A boxed copy (the object-safe `Clone`).
    fn clone_box(&self) -> Box<dyn PhyFactory>;

    /// The transmitter for `config`.
    fn tx(&self, config: &PhyConfig) -> Box<dyn PhyTx>;

    /// The receiver for `config`.
    fn rx(&self, config: &PhyConfig) -> Box<dyn PhyRx>;

    /// How the packet bytes are laid out in the body this phy carries for
    /// `config` (normally [`BodyCoding::from_fec`] of `config.fec`).
    fn body_coding(&self, config: &PhyConfig) -> BodyCoding;

    /// Samples one burst occupies — preamble or sync, a body of
    /// `body_nibbles` nibbles and any postamble or tail — without the gap
    /// that follows it. Must agree with [`PhyTx::synthesize_framed`].
    fn frame_samples(&self, config: &PhyConfig, body_nibbles: usize) -> usize;

    /// Silence the modem appends after each burst, in samples (0 for a phy
    /// whose frames delimit themselves).
    fn gap_samples(&self, config: &PhyConfig) -> usize;

    /// Duration of one body symbol on the wire, in ms: a tone, a bit cell
    /// times four, a sample. The unit of the modem's generic timing helpers
    /// (`PhyConfig::symbol_samples`, the flush and inter-message silences).
    fn symbol_duration_ms(&self, config: &PhyConfig) -> f32;

    /// Silence between body symbols, in ms (0 unless the phy has one).
    fn guard_gap_ms(&self, _config: &PhyConfig) -> f32 {
        0.0
    }

    /// Bits one wire symbol carries (4 = one nibble per symbol).
    fn bits_per_symbol(&self, _config: &PhyConfig) -> u32 {
        4
    }

    /// Duration of one burst with a body of `body_nibbles` nibbles, in ms,
    /// as shown to users as the first-copy airtime. The default is
    /// [`frame_samples`](Self::frame_samples) at the sample rate.
    fn first_copy_ms(&self, config: &PhyConfig, body_nibbles: usize) -> f32 {
        self.frame_samples(config, body_nibbles) as f32 * 1000.0 / config.sample_rate_hz as f32
    }

    /// How the TX scheduler repeats a released note on this phy.
    fn refresh_policy(&self, config: &PhyConfig) -> RefreshPolicy;

    /// The transmit level is part of the wire format:
    /// `PhyConfig::with_tx_level_dbfs` leaves `amplitude` unchanged.
    fn fixed_tx_level(&self) -> bool {
        false
    }

    /// The phy needs a bit-transparent audio path (samples arrive
    /// unchanged); the engine then prefers integer sample formats and warns
    /// about mixers in the path.
    fn needs_bit_exact_path(&self) -> bool {
        false
    }
}

impl Clone for Box<dyn PhyFactory> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Transmit side of a phy: one on-wire burst per body.
pub trait PhyTx: Send {
    /// One burst — preamble or sync, the body `body_nibbles` (already padded,
    /// interleaved or coded per [`BodyCoding`]), any postamble and tail — as
    /// mono samples at the configured sample rate. The modem repeats it
    /// `repetition_k` times with [`PhyFactory::gap_samples`] of silence
    /// after each copy.
    fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32>;
}

/// Receive side of a phy: a stream of samples in, [`RxEvent`]s out.
///
/// A body-nibble phy reports [`RxEvent::PreambleLocked`], then one
/// [`RxEvent::BodyNibble`] per nibble (with its reliability and erasure
/// flag for the FEC decoder), or [`RxEvent::SyncLost`]. After the last
/// nibble of a body the modem calls [`confirm`](Self::confirm) or
/// [`reject`](Self::reject) with the packet decode result; a phy that
/// pauses until then (the MFSK receiver, so a packet starting inside a
/// false lock is not lost) resumes there and returns the events of the
/// buffered input. A frame phy ([`BodyCoding::Raw`]) reports
/// [`RxEvent::Frame`]s instead and needs none of that.
pub trait PhyRx: Send {
    /// Feed samples (any chunking; a NaN or infinite sample must be read as
    /// silence, never poison the state) and return the events they
    /// complete. Buffers must stay bounded by one packet plus the chunk.
    fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent>;

    /// Phy lock state (`Search` / `Locked`; never `Dead`, which the modem
    /// owns).
    fn link_state(&self) -> LinkState;

    /// Confidence of the last completed body, in dB (the phy's own scale;
    /// shown to users, never compared across phys).
    fn last_confidence_db(&self) -> f32;

    /// Total samples pushed since creation (absolute stream position).
    fn samples_pushed(&self) -> u64;

    /// Body nibbles to read per burst (16, or the coded length). A frame
    /// phy, whose frames carry their length, ignores it.
    fn set_expected_body_nibbles(&mut self, _n: usize) {}

    /// The completed body held a valid packet: keep it and go on. Returns
    /// the events of input processed on resuming (empty for a phy that does
    /// not pause).
    fn confirm(&mut self) -> Vec<RxEvent> {
        Vec::new()
    }

    /// The completed body failed to decode: the lock was false. A phy that
    /// paused rewinds its search into the body's samples and returns the
    /// events found there; one whose sync search never stops has nothing to
    /// do.
    fn reject(&mut self) -> Vec<RxEvent> {
        Vec::new()
    }

    /// Back to search with the front end and any lock cleared; the sample
    /// count goes on.
    fn reset(&mut self);
}
