//! Body nibbles ↔ MFSK symbols for alphabets smaller than 16 tones.
//!
//! The link layer works in nibbles (4 bits). An MFSK plan with 8 tones
//! carries 3 bits per symbol (the `hf_fast` profile, spec §6.6, whose band
//! has no room for 16 tones at 3 ms), so the body nibbles are sent as a bit
//! stream, MSB first, cut into 3-bit symbols and zero-padded at the end:
//! 22 coded nibbles (88 bits) become 30 symbols (90 bits). A 16-tone plan
//! maps one nibble to one symbol and is untouched by this module.

use alloc::vec::Vec;

/// Bits per symbol of an alphabet of `n_tones` (16 → 4, 8 → 3, 4 → 2).
pub fn bits_per_symbol(n_tones: usize) -> u32 {
    (usize::BITS - n_tones.max(2).leading_zeros() - 1).min(4)
}

/// Symbols needed for `nibbles` nibbles at `bits` bits per symbol.
pub fn symbols_for_nibbles(nibbles: usize, bits: u32) -> usize {
    (nibbles * 4).div_ceil(bits.clamp(1, 4) as usize)
}

/// Nibbles → symbols of `bits` bits, MSB first, zero-padded.
pub fn pack_nibbles(nibbles: &[u8], bits: u32) -> Vec<u8> {
    let bits = bits.clamp(1, 4);
    if bits == 4 {
        return nibbles.iter().map(|n| n & 0x0F).collect();
    }
    let total = symbols_for_nibbles(nibbles.len(), bits);
    let mut out = Vec::with_capacity(total);
    let mut acc: u32 = 0;
    let mut have = 0u32;
    for &n in nibbles {
        acc = (acc << 4) | u32::from(n & 0x0F);
        have += 4;
        while have >= bits {
            have -= bits;
            out.push(((acc >> have) & ((1 << bits) - 1)) as u8);
        }
    }
    if have > 0 {
        out.push(((acc << (bits - have)) & ((1 << bits) - 1)) as u8);
    }
    debug_assert_eq!(out.len(), total);
    out
}

/// Symbols of `bits` bits → the first `nibbles` nibbles of the bit stream.
/// Missing symbols read as zero.
pub fn unpack_symbols(symbols: &[u8], bits: u32, nibbles: usize) -> Vec<u8> {
    let bits = bits.clamp(1, 4);
    if bits == 4 {
        return (0..nibbles)
            .map(|i| symbols.get(i).map_or(0, |s| s & 0x0F))
            .collect();
    }
    let mask = (1u32 << bits) - 1;
    let mut out = Vec::with_capacity(nibbles);
    let mut acc: u32 = 0;
    let mut have = 0u32;
    let mut i = 0;
    while out.len() < nibbles {
        let s = symbols.get(i).map_or(0, |s| u32::from(*s) & mask);
        i += 1;
        acc = (acc << bits) | s;
        have += bits;
        if have >= 4 {
            have -= 4;
            out.push(((acc >> have) & 0x0F) as u8);
        }
    }
    out
}

/// The symbols (indices into the symbol stream) that carry bits of nibble
/// `nibble` at `bits` bits per symbol: `first..=last`.
pub fn symbols_of_nibble(nibble: usize, bits: u32) -> (usize, usize) {
    let bits = bits.clamp(1, 4) as usize;
    let first_bit = nibble * 4;
    let last_bit = first_bit + 3;
    (first_bit / bits, last_bit / bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn alphabet_sizes() {
        assert_eq!(bits_per_symbol(16), 4);
        assert_eq!(bits_per_symbol(8), 3);
        assert_eq!(bits_per_symbol(4), 2);
        assert_eq!(bits_per_symbol(32), 4);
        assert_eq!(symbols_for_nibbles(22, 3), 30);
        assert_eq!(symbols_for_nibbles(16, 3), 22);
        assert_eq!(symbols_for_nibbles(22, 4), 22);
    }

    #[test]
    fn pack_round_trips_and_pads_with_zeros() {
        let nibbles: Vec<u8> = (0..22u8).map(|i| (i * 7 + 3) & 0x0F).collect();
        let symbols = pack_nibbles(&nibbles, 3);
        assert_eq!(symbols.len(), 30);
        assert!(symbols.iter().all(|&s| s < 8));
        assert_eq!(unpack_symbols(&symbols, 3, 22), nibbles);
        // 88 bits in 30 × 3 = 90: the last symbol ends in two zero bits.
        assert_eq!(symbols[29] & 0b11, 0);
        // The MSB-first stream: nibbles 0x1, 0x0 → bits 0001 0000 → 000 100 00…
        assert_eq!(pack_nibbles(&[0x1, 0x0], 3), vec![0, 4, 0]);
        assert_eq!(pack_nibbles(&[0xA, 0x5], 4), vec![0xA, 0x5]);
        assert_eq!(unpack_symbols(&[0xA, 0x5], 4, 2), vec![0xA, 0x5]);
    }

    #[test]
    fn nibble_to_symbol_map() {
        assert_eq!(symbols_of_nibble(0, 3), (0, 1));
        assert_eq!(symbols_of_nibble(1, 3), (1, 2));
        assert_eq!(symbols_of_nibble(2, 3), (2, 3));
        assert_eq!(symbols_of_nibble(3, 3), (4, 5));
        assert_eq!(symbols_of_nibble(21, 3), (28, 29));
        assert_eq!(symbols_of_nibble(7, 4), (7, 7));
    }
}
