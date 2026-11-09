//! The modulation of a [`PhyConfig`](super::PhyConfig): which phy carries
//! the body, with that phy's own parameters.

use super::bmc::BmcParams;
use super::digital::DigitalParams;
use super::mfsk::MfskParams;
use super::traits::PhyFactory;
use alloc::boxed::Box;

/// Physical-layer modulation selected by [`PhyConfig::modulation`](super::PhyConfig::modulation),
/// each variant carrying the parameters of its phy. The built-in phys are
/// the AML v1 MFSK-16 tones, the biphase-mark (LTC-style) line code and the
/// bit-transparent digital frames; [`Modulation::Custom`] plugs in a phy
/// from another crate (see `docs/architecture.md`, "Adding a modulation").
///
/// More variants may follow: match with a wildcard arm.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Modulation {
    /// AML v1 MFSK (spec §3): robust, robust_repeat, fast and the HF
    /// (instrument-audio) profiles.
    Mfsk(MfskParams),
    /// Biphase-mark code (LTC-style, provisional, spec §6.5), see
    /// [`super::bmc`].
    Bmc(BmcParams),
    /// Packet nibbles as PCM sample values (bit-transparent paths only,
    /// experimental), see [`super::digital`].
    Digital(DigitalParams),
    /// A phy from another crate. The factory builds the transmitter and
    /// receiver and describes its frame; the modem treats it like any
    /// built-in phy.
    Custom(Box<dyn PhyFactory>),
}

impl Modulation {
    /// The phy's factory (the parameters themselves for the built-in
    /// variants): what the modem dispatches through.
    pub fn phy(&self) -> &dyn PhyFactory {
        match self {
            Self::Mfsk(p) => p,
            Self::Bmc(p) => p,
            Self::Digital(p) => p,
            Self::Custom(f) => f.as_ref(),
        }
    }

    /// Short lower-case name: `"mfsk"`, `"bmc"`, `"digital"` or the custom
    /// phy's own.
    pub fn name(&self) -> &str {
        self.phy().name()
    }

    /// The MFSK parameters, if this is the MFSK phy.
    pub fn mfsk(&self) -> Option<&MfskParams> {
        match self {
            Self::Mfsk(p) => Some(p),
            _ => None,
        }
    }

    pub fn mfsk_mut(&mut self) -> Option<&mut MfskParams> {
        match self {
            Self::Mfsk(p) => Some(p),
            _ => None,
        }
    }

    /// The BMC parameters, if this is the BMC phy.
    pub fn bmc(&self) -> Option<&BmcParams> {
        match self {
            Self::Bmc(p) => Some(p),
            _ => None,
        }
    }

    pub fn bmc_mut(&mut self) -> Option<&mut BmcParams> {
        match self {
            Self::Bmc(p) => Some(p),
            _ => None,
        }
    }

    /// The digital phy's parameters, if this is the digital phy.
    pub fn digital(&self) -> Option<&DigitalParams> {
        match self {
            Self::Digital(p) => Some(p),
            _ => None,
        }
    }
}

/// Built-in variants compare by value; two `Custom` modulations are equal
/// when their factories report the same [`PhyFactory::name`].
impl PartialEq for Modulation {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Mfsk(a), Self::Mfsk(b)) => a == b,
            (Self::Bmc(a), Self::Bmc(b)) => a == b,
            (Self::Digital(a), Self::Digital(b)) => a == b,
            (Self::Custom(a), Self::Custom(b)) => a.name() == b.name(),
            _ => false,
        }
    }
}
