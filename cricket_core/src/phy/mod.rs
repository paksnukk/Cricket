//! AML v1 physical layer: MFSK-16 synthesis, Goertzel demod, preamble sync.
//! Plus the biphase-mark (LTC-style) phy of the provisional LTC profiles in
//! [`bmc`] and the bit-transparent phy of the experimental `digital` profile
//! in [`digital`].
//!
//! The modem talks to a phy only through the traits of [`traits`]
//! ([`PhyFactory`], [`PhyTx`], [`PhyRx`]) and the [`Modulation`] that
//! selects it; a phy from another crate plugs in through
//! [`Modulation::Custom`] (see `docs/architecture.md`).

pub mod bmc;
mod config;
mod demod;
pub mod digital;
mod filter;
mod goertzel;
mod mfsk;
mod modulation;
pub mod pack;
mod synth;
pub mod traits;

pub use bmc::BmcParams;
pub use config::{
    amplitude_from_dbfs, dbfs_from_amplitude, profile_info, PhyConfig, PhyConfigBuilder,
    ProfileInfo, ProfileTier, DEFAULT_AMPLITUDE, DEFAULT_PROFILE, DEFAULT_TX_LEVEL_DBFS,
    HF_AMPLITUDE, HF_BASE_96K_HZ, HF_BASE_HZ, HF_RX_BAND_HZ, HF_RX_BAND_ORDER, HF_TX_LEVEL_DBFS,
    PROFILES,
};
pub use demod::{DemodResult, LinkState, ModemRx, RxEvent, LOCK_HOLD_MS};
pub use digital::DigitalParams;
pub use filter::{butterworth_q, BandPass, Biquad};
pub use goertzel::{best_tone, goertzel_power, tone_purity, ToneDetection};
pub use mfsk::{orthogonal_step_hz, Acquisition, MfskParams, PURITY_MFSK};
pub use modulation::Modulation;
pub use synth::ModemTx;
pub use traits::{BodyCoding, PhyFactory, PhyRx, PhyTx, RefreshPolicy};
