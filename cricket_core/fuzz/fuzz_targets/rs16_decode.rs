//! Reed–Solomon over GF(16): decode any word with any erasure list for any
//! (k, nsym); a clean codeword with t errors and e erasures (2t + e ≤ nsym)
//! must come back.
#![no_main]

use arbitrary::Arbitrary;
use cricket_core::link::{Rs16, RS16_MAX_N};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    k: u8,
    nsym: u8,
    word: Vec<u8>,
    erasures: Vec<u8>,
    /// Positions to corrupt in a valid codeword (second half of the test).
    flips: Vec<(u8, u8)>,
}

fuzz_target!(|input: Input| {
    let k = usize::from(input.k % (RS16_MAX_N as u8 + 2));
    let nsym = usize::from(input.nsym % (RS16_MAX_N as u8 + 2));
    let Ok(rs) = Rs16::new(k, nsym) else {
        return;
    };
    // Arbitrary word, arbitrary erasures: no panic whatever the lengths.
    let mut word: Vec<u8> = input.word.iter().map(|b| b & 0x0F).collect();
    let erasures: Vec<usize> = input.erasures.iter().map(|&e| usize::from(e)).collect();
    let _ = rs.decode(&mut word, &erasures);

    // A real codeword with a correctable pattern decodes to the data.
    let data: Vec<u8> = (0..rs.k())
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(input.k) & 0x0F)
        .collect();
    let clean = rs.encode(&data);
    assert_eq!(clean.len(), rs.n());
    let mut damaged = clean.clone();
    let mut erased = Vec::new();
    let mut errors = 0usize;
    for &(pos, val) in input.flips.iter().take(rs.nsym()) {
        let pos = usize::from(pos) % rs.n();
        if erased.contains(&pos) || damaged[pos] != clean[pos] {
            continue;
        }
        if val & 1 == 0 && erased.len() + 2 * errors + 1 <= rs.nsym() {
            erased.push(pos);
            damaged[pos] = (val >> 1) & 0x0F;
        } else if 2 * (errors + 1) + erased.len() <= rs.nsym() {
            damaged[pos] = (clean[pos] + 1 + ((val >> 1) % 15)) & 0x0F;
            errors += 1;
        }
    }
    match rs.decode(&mut damaged, &erased) {
        Ok(_) => assert_eq!(&damaged[..rs.k()], &data[..], "data part restored"),
        Err(e) => panic!("correctable pattern ({errors} errors, {} erasures) failed: {e:?}", erased.len()),
    }
});
