//! Packet parsing (spec §4.1, §4.7) on arbitrary bytes: never panics, and a
//! packet that decodes re-encodes to the same bytes.
#![no_main]

use cricket_core::link::{
    decode_packet_bytes, decode_packet_bytes_keyed, encode_packet_bytes_keyed,
    packet_structure_is_valid,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = packet_structure_is_valid(data);
    if let Ok(packet) = decode_packet_bytes(data) {
        let again = encode_packet_bytes_keyed(&packet, cricket_core::link::DEFAULT_LINK_KEY)
            .expect("a decoded packet re-encodes");
        assert_eq!(again, data, "round trip");
    }
    // A keyed decode with a key taken from the data.
    if data.len() >= 2 {
        let key = u16::from_be_bytes([data[0], data[1]]);
        let _ = decode_packet_bytes_keyed(&data[2..], key);
    }
});
