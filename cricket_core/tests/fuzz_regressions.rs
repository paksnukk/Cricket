//! Inputs the fuzz targets (`cricket_core/fuzz`) found: each one used to
//! panic or to make two functions disagree. They stay here so the fix is
//! pinned without a nightly toolchain.

use cricket_core::phy::{profile_info, PhyConfig};
use cricket_core::transport::{MscError, MscMessage};

/// `msc_parse_sysex`: a 6-byte SysEx with the MSC header has no command
/// byte; `parse_sysex` indexed past the end of it.
#[test]
fn msc_sysex_without_a_command_byte_is_malformed() {
    assert_eq!(
        MscMessage::parse_sysex(&[0xF0, 0x7F, 0x01, 0x02, 0x01, 0xF7]),
        Err(MscError::Malformed)
    );
    // Shorter ones are not MSC at all (as before); the shortest real
    // message parses.
    assert_eq!(
        MscMessage::parse_sysex(&[0xF0, 0x7F, 0x01, 0x02, 0xF7]),
        Err(MscError::NotMsc)
    );
    assert!(MscMessage::parse_sysex(&[0xF0, 0x7F, 0x01, 0x02, 0x01, 0x08, 0xF7]).is_ok());
}

/// `from_name`: whitespace between the base name and the `+digital`
/// suffix selected a profile in `from_name` but not in `canonical_name`
/// or `profile_info`.
#[test]
fn profile_name_functions_agree_on_odd_whitespace() {
    for name in [
        "digital\u{a0}\n+digital",
        "digital \n+digital",
        " ltc +digital",
        "  clean-line+digital  ",
        "ROBUST",
        "+digital",
    ] {
        let found = PhyConfig::from_name(name).is_some();
        assert_eq!(PhyConfig::canonical_name(name).is_some(), found, "{name:?}");
        assert_eq!(profile_info(name).is_some(), found, "{name:?}");
    }
}
