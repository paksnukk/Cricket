//! Modulation-agnostic phy front ends used by [`crate::modem`]: dispatch to
//! the MFSK, the BMC (LTC) or the digital implementation per
//! [`PhyConfig::modulation`].

use super::bmc::{BmcRx, BmcTx, Modulation};
use super::digital::{DigitalRx, DigitalTx};
use super::{LinkState, ModemRx, ModemTx, PhyConfig, RxEvent};
use alloc::boxed::Box;
use alloc::vec::Vec;

/// Transmit phy selected by `config.modulation`.
pub enum PhyTx {
    Mfsk(ModemTx),
    Bmc(BmcTx),
    Digital(DigitalTx),
}

impl PhyTx {
    pub fn new(config: PhyConfig) -> Self {
        match config.modulation {
            Modulation::Mfsk => Self::Mfsk(ModemTx::new(config)),
            Modulation::Bmc(_) => Self::Bmc(BmcTx::new(config)),
            Modulation::Digital => Self::Digital(DigitalTx::new(config)),
        }
    }

    /// One on-wire burst (preamble/sync + body) for `body_nibbles`.
    pub fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32> {
        match self {
            Self::Mfsk(tx) => tx.synthesize_framed(body_nibbles),
            Self::Bmc(tx) => tx.synthesize_framed(body_nibbles),
            Self::Digital(tx) => tx.synthesize_framed(body_nibbles),
        }
    }
}

/// Receive phy selected by `config.modulation`; all emit [`RxEvent`]s.
/// Boxed: the receivers differ a lot in size and live for the whole stream.
pub enum PhyRx {
    Mfsk(Box<ModemRx>),
    Bmc(Box<BmcRx>),
    Digital(Box<DigitalRx>),
}

impl PhyRx {
    pub fn new(config: PhyConfig) -> Self {
        match config.modulation {
            Modulation::Mfsk => Self::Mfsk(Box::new(ModemRx::new(config))),
            Modulation::Bmc(_) => Self::Bmc(Box::new(BmcRx::new(config))),
            Modulation::Digital => Self::Digital(Box::new(DigitalRx::new(config))),
        }
    }

    pub fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent> {
        match self {
            Self::Mfsk(rx) => rx.push_samples(samples),
            Self::Bmc(rx) => rx.push_samples(samples),
            Self::Digital(rx) => rx.push_samples(samples),
        }
    }

    pub fn link_state(&self) -> LinkState {
        match self {
            Self::Mfsk(rx) => rx.link_state(),
            Self::Bmc(rx) => rx.link_state(),
            Self::Digital(rx) => rx.link_state(),
        }
    }

    pub fn last_confidence_db(&self) -> f32 {
        match self {
            Self::Mfsk(rx) => rx.last_confidence_db(),
            Self::Bmc(rx) => rx.last_confidence_db(),
            Self::Digital(rx) => rx.last_confidence_db(),
        }
    }

    pub fn set_expected_body_nibbles(&mut self, n: usize) {
        match self {
            Self::Mfsk(rx) => rx.set_expected_body_nibbles(n),
            Self::Bmc(rx) => rx.set_expected_body_nibbles(n),
            // Digital frames carry their length (type nibble).
            Self::Digital(_) => {}
        }
    }

    pub fn samples_pushed(&self) -> u64 {
        match self {
            Self::Mfsk(rx) => rx.samples_pushed(),
            Self::Bmc(rx) => rx.samples_pushed(),
            Self::Digital(rx) => rx.samples_pushed(),
        }
    }

    /// The completed body decoded; resume (see [`ModemRx::confirm`]).
    pub fn confirm(&mut self) -> Vec<RxEvent> {
        match self {
            Self::Mfsk(rx) => rx.confirm(),
            // BMC does not pause after a body: its sync search never stops,
            // so there is nothing to rewind (see `BmcRx`). Neither does the
            // digital phy (a sync can never occur inside a valid body).
            Self::Bmc(_) | Self::Digital(_) => Vec::new(),
        }
    }

    /// The completed body failed to decode; rewind and resume (see
    /// [`ModemRx::reject`]). No-op for BMC: its sync correlator keeps running
    /// during a body and a real sync replaces a false lock directly.
    pub fn reject(&mut self) -> Vec<RxEvent> {
        match self {
            Self::Mfsk(rx) => rx.reject(),
            Self::Bmc(_) | Self::Digital(_) => Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        match self {
            Self::Mfsk(rx) => rx.reset(),
            Self::Bmc(rx) => rx.reset(),
            Self::Digital(rx) => rx.reset(),
        }
    }
}
