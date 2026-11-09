//! Cricket core library — AML v1 modem (default) + legacy prototype phy.
//!
//! The crate is `no_std` + `alloc` so the modem can run on microcontrollers. The
//! default `std` feature adds WAV file I/O (`wav`), the legacy prototype phy
//! (`phy_legacy`) and the bit-transparency analyzer (`pathcheck`, also its own
//! feature). The layering and the phy boundary are described in
//! `docs/architecture.md`.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod link;
pub mod modem;
#[cfg(feature = "pathcheck")]
pub mod pathcheck;
pub mod phy;
#[cfg(feature = "std")]
pub mod phy_legacy;
pub mod transport;
#[cfg(feature = "std")]
pub mod wav;

// Convenience re-exports for the AML v1 stack
pub use modem::{
    decode_samples, encode_to_samples, CricketModemRx, CricketModemTx, ModemOutput, ModemStats,
    MAX_BODY_NIBBLES,
};
pub use phy::{LinkState, PhyConfig};
pub use transport::{MessageType, MscCommand, TransportMessage};
