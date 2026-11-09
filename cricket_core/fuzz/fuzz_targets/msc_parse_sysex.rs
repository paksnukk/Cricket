//! MSC SysEx parsing (spec §5.4) on arbitrary bytes: never panics, and a
//! message that parses survives the compact form and the SysEx round trip.
#![no_main]

use cricket_core::transport::{MscFilter, MscMessage, MscTarget};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(msg) = MscMessage::parse_sysex(data) {
        let compact = msg.to_compact();
        let back = MscMessage::from_compact(&compact).expect("compact form parses");
        assert_eq!(back, msg, "compact round trip");
        let sysex = back.to_sysex(&MscTarget::default());
        let again = MscMessage::parse_sysex(&sysex).expect("own SysEx parses");
        assert_eq!(again, msg, "SysEx round trip");
    }
    // The transmit-side filter on the same bytes.
    let filter = MscFilter {
        device_id: data.first().map(|b| b & 0x7F),
        command_format: data.get(1).map(|b| b & 0x7F),
    };
    let _ = cricket_core::transport::midi_input_filtered(data, &filter);
});
