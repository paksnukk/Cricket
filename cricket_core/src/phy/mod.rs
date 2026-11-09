//! AML v1 physical layer: MFSK-16 synthesis, Goertzel demod, preamble sync.
//! Plus the biphase-mark (LTC-style) phy of the provisional LTC profiles in [`bmc`]
//! and the bit-transparent phy of the experimental `digital` profile in [`digital`].

pub mod bmc;
mod config;
mod demod;
pub mod digital;
mod filter;
mod goertzel;
pub mod pack;
mod select;
mod synth;

pub use bmc::{BmcParams, Modulation};
pub use config::{
    amplitude_from_dbfs, dbfs_from_amplitude, orthogonal_step_hz, profile_info, Acquisition,
    PhyConfig, ProfileInfo, ProfileTier, DEFAULT_AMPLITUDE, DEFAULT_PROFILE, DEFAULT_TX_LEVEL_DBFS,
    HF_AMPLITUDE, HF_BASE_96K_HZ, HF_BASE_HZ, HF_RX_BAND_HZ, HF_RX_BAND_ORDER, HF_TX_LEVEL_DBFS,
    PROFILES,
};
pub use demod::{DemodResult, LinkState, ModemRx, RxEvent, LOCK_HOLD_MS};
pub use filter::{butterworth_q, BandPass, Biquad};
pub use goertzel::{best_tone, goertzel_power, tone_purity, ToneDetection};
pub use select::{PhyRx, PhyTx};
pub use synth::ModemTx;
