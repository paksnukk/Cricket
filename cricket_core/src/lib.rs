//! Cricket core library — AML v1 modem (default) + legacy prototype phy.
//!
//! The crate is `no_std` + `alloc` so the modem can run on microcontrollers. The
//! default `std` feature adds WAV file I/O (`wav`) and the legacy prototype phy (`phy_legacy`).

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod link;
pub mod modem;
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
