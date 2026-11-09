//! Profile lookup on arbitrary strings: never panics, and the three name
//! functions agree (a name that selects a profile has a canonical name and a
//! catalog entry, and that entry builds the same profile).
#![no_main]

use cricket_core::phy::{profile_info, PhyConfig};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|name: &str| {
    let config = PhyConfig::from_name(name);
    let canonical = PhyConfig::canonical_name(name);
    let info = profile_info(name);
    assert_eq!(config.is_some(), canonical.is_some(), "{name:?}");
    assert_eq!(config.is_some(), info.is_some(), "{name:?}");
    if let (Some(config), Some(canonical), Some(info)) = (config, canonical, info) {
        let again = PhyConfig::from_name(&canonical).expect("canonical name selects");
        assert_eq!(again.fec, config.fec);
        assert_eq!(again.repetition_k, config.repetition_k);
        assert_eq!(again.modulation, config.modulation);
        assert!(canonical.starts_with(info.name));
        // The derived quantities are finite for every profile.
        assert!(config.estimated_first_copy_ms().is_finite());
        assert!(config.symbol_samples() > 0);
        assert!(cricket_core::modem::repetition_samples(&config) > 0);
    }
});
