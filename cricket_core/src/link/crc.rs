//! CRC-16/CCITT (poly `0x1021`, refin=false, refout=false, xorout=`0x0000`)
//! with the **link key** as its init value (spec §4.1).
//!
//! The link key identifies a link: two Cricket links in one multicore or
//! Dante domain leak into each other at −40 dB or so, and a receiver is
//! level-independent down to −60 dBFS, so without a key it would deliver the
//! other link's (valid) packets. A packet's CRC is computed with the sender's
//! key as init; a receiver with another key computes a different value for
//! the same bytes, so every clean packet from another link fails the check
//! (the CRC is affine in its init: two inits differ by a fixed non-zero
//! amount for a given length). The key costs no wire bits, and with
//! [`DEFAULT_LINK_KEY`] (`0xFFFF`, the CRC-16/CCITT-FALSE init) the wire
//! format is exactly the unkeyed one.

/// The unkeyed link: CRC-16/CCITT-FALSE's init value. Both ends default to
/// it, so links without a configured key interoperate as before.
pub const DEFAULT_LINK_KEY: u16 = 0xFFFF;

/// CRC-16/CCITT with `key` as init value.
pub fn crc16_keyed(data: &[u8], key: u16) -> u16 {
    let mut crc: u16 = key;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF ([`DEFAULT_LINK_KEY`]),
/// refin=false, refout=false, xorout=0x0000.
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    crc16_keyed(data, DEFAULT_LINK_KEY)
}

pub fn verify_crc16(data_without_crc: &[u8], crc_hi: u8, crc_lo: u8) -> bool {
    let expected = crc16_ccitt(data_without_crc);
    expected == ((crc_hi as u16) << 8) | (crc_lo as u16)
}

/// A link key from a passphrase: the CRC-16/CCITT-FALSE of its UTF-8 bytes,
/// so operators can share a word instead of a hex value. The one in 65 536
/// passphrases whose hash is [`DEFAULT_LINK_KEY`] maps to `0x0000`, so a
/// passphrase always keys the link.
pub fn link_key_from_passphrase(passphrase: &str) -> u16 {
    match crc16_ccitt(passphrase.as_bytes()) {
        DEFAULT_LINK_KEY => 0x0000,
        key => key,
    }
}

/// Parse a link key setting: `0x1234` (1–4 hex digits after `0x` / `0X`),
/// or any other non-empty text as a passphrase
/// ([`link_key_from_passphrase`]). Empty or blank text is the unkeyed link.
pub fn parse_link_key(text: &str) -> u16 {
    let text = text.trim();
    if text.is_empty() {
        return DEFAULT_LINK_KEY;
    }
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        if (1..=4).contains(&hex.len()) {
            if let Ok(key) = u16::from_str_radix(hex, 16) {
                return key;
            }
        }
    }
    link_key_from_passphrase(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_vector() {
        // Empty → init value
        assert_eq!(crc16_ccitt(&[]), 0xFFFF);
        let c = crc16_ccitt(&[0x01, 0x02, 0x03]);
        assert_ne!(c, 0);
        assert!(verify_crc16(
            &[0x01, 0x02, 0x03],
            (c >> 8) as u8,
            (c & 0xFF) as u8
        ));
        // CRC-16/CCITT-FALSE check value.
        assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
        assert_eq!(crc16_keyed(b"123456789", DEFAULT_LINK_KEY), 0x29B1);
    }

    /// For a given length, two keys change the CRC by the same non-zero
    /// amount whatever the data (the CRC is affine in its init), so a
    /// clean packet from another key never matches, not even by chance.
    #[test]
    fn keys_shift_the_crc_by_a_data_independent_nonzero_amount() {
        let five = [
            &[0x10u8, 0x00, 0x90, 0x3C, 0x64][..],
            &[0u8; 5],
            &[0xFFu8; 5],
        ];
        for (a, b) in [(0xFFFFu16, 0x1234u16), (0x0000, 0x0001), (0xBEEF, 0xBEEE)] {
            let deltas: Vec<u16> = five
                .iter()
                .map(|d| crc16_keyed(d, a) ^ crc16_keyed(d, b))
                .collect();
            assert_ne!(deltas[0], 0);
            assert!(deltas.iter().all(|d| *d == deltas[0]), "{a:#06x} {b:#06x}");
            // Another length: another (non-zero) shift.
            assert_ne!(
                crc16_keyed(&[0x14, 0x02], a) ^ crc16_keyed(&[0x14, 0x02], b),
                0
            );
        }
        // Exhaustively for the 2-byte (Heartbeat) body: no two keys agree.
        let data = [0x14u8, 0x02];
        let base = crc16_keyed(&data, 0);
        for key in 1..=0xFFFFu32 {
            assert_ne!(crc16_keyed(&data, key as u16), base, "key {key:#06x}");
        }
    }

    #[test]
    fn link_key_parsing() {
        assert_eq!(parse_link_key(""), DEFAULT_LINK_KEY);
        assert_eq!(parse_link_key("  "), DEFAULT_LINK_KEY);
        assert_eq!(parse_link_key("0x1234"), 0x1234);
        assert_eq!(parse_link_key("0XABcd"), 0xABCD);
        assert_eq!(parse_link_key("0x0"), 0);
        assert_eq!(parse_link_key(" 0xFFFF "), DEFAULT_LINK_KEY);
        // Not hex: a passphrase.
        assert_eq!(
            parse_link_key("stage left"),
            link_key_from_passphrase("stage left")
        );
        assert_eq!(parse_link_key("1234"), link_key_from_passphrase("1234"));
        assert_eq!(
            parse_link_key("0x12345"),
            link_key_from_passphrase("0x12345")
        );
        assert_eq!(parse_link_key("0xZZ"), link_key_from_passphrase("0xZZ"));
        assert_ne!(
            link_key_from_passphrase("stage left"),
            link_key_from_passphrase("stage right")
        );
        assert_ne!(link_key_from_passphrase("x"), DEFAULT_LINK_KEY);
    }
}
