//! The RS16 body decoder (spec §4.6) on arbitrary received symbols, with
//! every parity: never panics, a decoded body passes the candidate check
//! and its errata weight stays within the code's redundancy, and a wrong
//! body length is refused.
#![no_main]

use arbitrary::Arbitrary;
use cricket_core::link::{fec_candidate_is_valid, BodySymbol, FecCodec, FecMode, MAX_PARITY};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    parity: u8,
    symbols: Vec<(u8, i8, bool)>,
}

fuzz_target!(|input: Input| {
    let mode = FecMode::Rs16 {
        parity: input.parity % (MAX_PARITY + 2),
    };
    let Some(codec) = FecCodec::new(mode) else {
        return;
    };
    let symbol = |&(n, rel, erased): &(u8, i8, bool)| BodySymbol {
        nibble: n & 0x0F,
        reliability: rel as f32 * 0.5,
        erased,
    };
    // Exactly one body: pad with erasures.
    let mut body: Vec<BodySymbol> = input.symbols.iter().map(symbol).collect();
    body.resize(
        codec.body_nibbles(),
        BodySymbol {
            nibble: 0,
            reliability: 0.0,
            erased: true,
        },
    );
    if let Some(decoded) = codec.decode_body(&body, |nibbles| fec_candidate_is_valid(nibbles)) {
        assert!(decoded.weight <= codec.max_weight());
        assert!(fec_candidate_is_valid(&decoded.nibbles));
    }
    // A wrong length is refused, never a panic.
    assert!(codec
        .decode_body(&body[..body.len() - 1], |_| true)
        .is_none());
    // Encoding any packet-sized input never panics either.
    let packet: Vec<u8> = body.iter().map(|s| s.nibble).take(14).collect();
    let coded = codec.encode_body(&packet);
    assert_eq!(coded.len(), codec.body_nibbles());
});
