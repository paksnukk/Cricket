use super::crc::{crc16_keyed, DEFAULT_LINK_KEY};
use crate::transport::MessageType;
use alloc::vec::Vec;

/// Cricket wire-protocol version carried in the header's high nibble.
/// AML v1 is `0x1`; packets with any other version are rejected.
pub const PROTOCOL_VERSION: u8 = 0x1;

/// SEQ values: a 2-bit counter (0–3, wrapping). The upper six bits of the
/// SEQ byte are reserved and must be zero (spec §4.1): duplicate suppression
/// only needs SEQ to differ between consecutive messages (§4.4), and the
/// reserved bits keep random data from passing as a packet
/// (`docs/proposals/fec.md` §9.7).
pub const SEQ_MODULUS: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub msg_type: MessageType,
    pub seq: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketDecodeError {
    TooShort,
    BadCrc,
    UnknownType(u8),
    BadPayloadLen {
        expected: usize,
        got: usize,
    },
    BadVersion(u8),
    /// SEQ outside 0–3 (reserved bits set).
    BadSeq(u8),
}

pub fn bytes_to_nibbles(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(b >> 4);
        out.push(b & 0x0F);
    }
    out
}

pub fn nibbles_to_bytes(nibbles: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nibbles.len().div_ceil(2));
    let mut i = 0;
    while i + 1 < nibbles.len() {
        out.push(((nibbles[i] & 0x0F) << 4) | (nibbles[i + 1] & 0x0F));
        i += 2;
    }
    out
}

/// Packet bytes with the CRC-16 of the unkeyed link ([`DEFAULT_LINK_KEY`]).
pub fn encode_packet_bytes(packet: &Packet) -> Result<Vec<u8>, PacketDecodeError> {
    encode_packet_bytes_keyed(packet, DEFAULT_LINK_KEY)
}

/// Packet bytes (HDR, SEQ, payload, CRC-16) with `link_key` as the CRC init
/// value (spec §4.1).
pub fn encode_packet_bytes_keyed(
    packet: &Packet,
    link_key: u16,
) -> Result<Vec<u8>, PacketDecodeError> {
    let expected = packet.msg_type.payload_len();
    if packet.payload.len() != expected {
        return Err(PacketDecodeError::BadPayloadLen {
            expected,
            got: packet.payload.len(),
        });
    }
    if packet.seq >= SEQ_MODULUS {
        return Err(PacketDecodeError::BadSeq(packet.seq));
    }
    let hdr = (PROTOCOL_VERSION << 4) | (packet.msg_type as u8 & 0x0F);
    let mut body = Vec::with_capacity(2 + expected + 2);
    body.push(hdr);
    body.push(packet.seq);
    body.extend_from_slice(&packet.payload);
    let crc = crc16_keyed(&body, link_key);
    body.push((crc >> 8) as u8);
    body.push((crc & 0xFF) as u8);
    Ok(body)
}

/// Decode exact packet bytes of the unkeyed link ([`DEFAULT_LINK_KEY`]).
pub fn decode_packet_bytes(bytes: &[u8]) -> Result<Packet, PacketDecodeError> {
    decode_packet_bytes_keyed(bytes, DEFAULT_LINK_KEY)
}

/// Decode exact packet bytes whose CRC-16 was computed with `link_key`
/// (spec §4.1, §4.7 rules 1–2 and the payload length).
pub fn decode_packet_bytes_keyed(bytes: &[u8], link_key: u16) -> Result<Packet, PacketDecodeError> {
    if bytes.len() < 4 {
        return Err(PacketDecodeError::TooShort);
    }
    let data_end = bytes.len() - 2;
    let crc_hi = bytes[data_end];
    let crc_lo = bytes[data_end + 1];
    let data = &bytes[..data_end];
    let expected_crc = crc16_keyed(data, link_key);
    let got_crc = ((crc_hi as u16) << 8) | (crc_lo as u16);
    if expected_crc != got_crc {
        return Err(PacketDecodeError::BadCrc);
    }

    let hdr = data[0];
    let version = hdr >> 4;
    if version != PROTOCOL_VERSION {
        return Err(PacketDecodeError::BadVersion(version));
    }
    let type_nibble = hdr & 0x0F;
    let msg_type =
        MessageType::from_u8(type_nibble).ok_or(PacketDecodeError::UnknownType(type_nibble))?;
    let seq = data[1];
    if seq >= SEQ_MODULUS {
        return Err(PacketDecodeError::BadSeq(seq));
    }
    let payload = data[2..].to_vec();
    let expected = msg_type.payload_len();
    if payload.len() != expected {
        return Err(PacketDecodeError::BadPayloadLen {
            expected,
            got: payload.len(),
        });
    }
    Ok(Packet {
        msg_type,
        seq,
        payload,
    })
}

/// Whether exact packet bytes `slice` have the structure of a packet
/// regardless of the CRC (spec §4.7 rules 2–3: version, type, SEQ, length
/// and a canonical payload). A structurally valid body whose CRC fails is
/// most likely a packet from a link with another key (`docs/aml-v1-spec.md`
/// §4.1); random data passes this with probability about 2⁻¹⁷.
pub fn packet_structure_is_valid(slice: &[u8]) -> bool {
    let Some(&hdr) = slice.first() else {
        return false;
    };
    let Some(msg_type) = MessageType::from_u8(hdr & 0x0F) else {
        return false;
    };
    let needed = 2 + msg_type.payload_len() + 2;
    hdr >> 4 == PROTOCOL_VERSION
        && slice.len() == needed
        && slice[1] < SEQ_MODULUS
        && msg_type.payload_in_range(&slice[2..needed - 2])
}

/// [`fec_candidate_is_valid_keyed`] for the unkeyed link.
pub fn fec_candidate_is_valid(nibbles: &[u8]) -> bool {
    fec_candidate_is_valid_keyed(nibbles, DEFAULT_LINK_KEY)
}

/// Whether an FEC-decoded data block (packet nibbles + zero pad) holds a
/// valid packet of the link with `link_key`: CRC-16, version, type and SEQ
/// checks pass, the pad is zero and the payload is canonical
/// ([`MessageType::payload_in_range`]).
///
/// Besides the CRC-16 this leaves about 17 bits of structure a random block
/// must also match (`docs/proposals/fec.md` §9.7), which is what keeps the
/// erasure-tolerant FEC decoder from turning noise into messages.
pub fn fec_candidate_is_valid_keyed(nibbles: &[u8], link_key: u16) -> bool {
    let bytes = nibbles_to_bytes(nibbles);
    let Some(&hdr) = bytes.first() else {
        return false;
    };
    let Some(msg_type) = MessageType::from_u8(hdr & 0x0F) else {
        return false;
    };
    let needed = 2 + msg_type.payload_len() + 2;
    // Cheapest checks first: version, SEQ, pad, range, then the CRC.
    hdr >> 4 == PROTOCOL_VERSION
        && bytes.len() >= needed
        && bytes[1] < SEQ_MODULUS
        && bytes[needed..].iter().all(|&b| b == 0)
        && msg_type.payload_in_range(&bytes[2..needed - 2])
        && decode_packet_bytes_keyed(&bytes[..needed], link_key).is_ok()
}

/// Nibble count for a packet body after interleave padding.
pub fn body_nibble_count(msg_type: MessageType, interleave_depth: usize) -> usize {
    let raw_bytes = 2 + msg_type.payload_len() + 2; // hdr+seq+payload+crc
    let raw_nibbles = raw_bytes * 2;
    let depth = interleave_depth.max(1);
    raw_nibbles.div_ceil(depth) * depth
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::crc16_ccitt;
    use crate::transport::MessageType;

    /// The default key gives the CRC-16/CCITT-FALSE bytes of spec §9; a
    /// keyed packet differs only in its CRC and decodes only with its key,
    /// while its structure is still recognisable (the wrong-key counter).
    #[test]
    fn link_key_is_the_crc_init() {
        let p = Packet {
            msg_type: MessageType::Midi,
            seq: 0,
            payload: vec![0x90, 60, 100],
        };
        let plain = encode_packet_bytes(&p).unwrap();
        assert_eq!(plain, [0x10, 0x00, 0x90, 0x3C, 0x64, 0x01, 0xB5]);
        assert_eq!(
            encode_packet_bytes_keyed(&p, DEFAULT_LINK_KEY).unwrap(),
            plain
        );
        let keyed = encode_packet_bytes_keyed(&p, 0x1234).unwrap();
        assert_eq!(keyed[..5], plain[..5]);
        assert_ne!(keyed[5..], plain[5..]);
        assert_eq!(decode_packet_bytes_keyed(&keyed, 0x1234), Ok(p.clone()));
        assert_eq!(
            decode_packet_bytes_keyed(&keyed, DEFAULT_LINK_KEY),
            Err(PacketDecodeError::BadCrc)
        );
        assert_eq!(decode_packet_bytes(&keyed), Err(PacketDecodeError::BadCrc));
        assert_eq!(
            decode_packet_bytes_keyed(&plain, 0x1234),
            Err(PacketDecodeError::BadCrc)
        );
        assert!(packet_structure_is_valid(&keyed));
        assert!(packet_structure_is_valid(&plain));
        assert!(!packet_structure_is_valid(&plain[..6]));
        assert!(!packet_structure_is_valid(&[
            0x20, 0x00, 0x90, 0x3C, 0x64, 0, 0
        ]));
        assert!(!packet_structure_is_valid(&[
            0x10, 0x04, 0x90, 0x3C, 0x64, 0, 0
        ]));
        assert!(!packet_structure_is_valid(&[
            0x10, 0x00, 0x90, 0x3C, 0x00, 0, 0
        ]));
        assert!(packet_structure_is_valid(&[0x14, 0x02, 0xF2, 0xFA]));
        assert!(packet_structure_is_valid(&[0x14, 0x02, 0x00, 0x00]));
        // FEC candidates likewise.
        let mut nibbles = bytes_to_nibbles(&keyed);
        nibbles.resize(14, 0);
        assert!(fec_candidate_is_valid_keyed(&nibbles, 0x1234));
        assert!(!fec_candidate_is_valid_keyed(&nibbles, DEFAULT_LINK_KEY));
        assert!(!fec_candidate_is_valid(&nibbles));
        let mut nibbles = bytes_to_nibbles(&plain);
        nibbles.resize(14, 0);
        assert!(fec_candidate_is_valid(&nibbles));
        assert!(!fec_candidate_is_valid_keyed(&nibbles, 0x1234));
    }

    #[test]
    fn packet_roundtrip() {
        let p = Packet {
            msg_type: MessageType::Midi,
            seq: 3,
            payload: vec![0x93, 60, 100],
        };
        let bytes = encode_packet_bytes(&p).unwrap();
        assert_eq!(bytes.len(), 7);
        let decoded = decode_packet_bytes(&bytes).unwrap();
        assert_eq!(decoded, p);
    }

    #[test]
    fn seq_is_two_bits_with_reserved_zero_bits() {
        let mut p = Packet {
            msg_type: MessageType::Heartbeat,
            seq: 4,
            payload: vec![],
        };
        assert_eq!(encode_packet_bytes(&p), Err(PacketDecodeError::BadSeq(4)));
        p.seq = 2;
        let mut bytes = encode_packet_bytes(&p).unwrap();
        bytes[1] = 0x42;
        let n = bytes.len();
        let crc = crc16_ccitt(&bytes[..n - 2]);
        bytes[n - 2] = (crc >> 8) as u8;
        bytes[n - 1] = crc as u8;
        assert_eq!(
            decode_packet_bytes(&bytes),
            Err(PacketDecodeError::BadSeq(0x42))
        );
    }

    #[test]
    fn header_carries_version_and_rejects_others() {
        let p = Packet {
            msg_type: MessageType::Midi,
            seq: 1,
            payload: vec![0x90, 60, 100],
        };
        let mut bytes = encode_packet_bytes(&p).unwrap();
        assert_eq!(bytes[0] >> 4, PROTOCOL_VERSION);
        assert_eq!(PROTOCOL_VERSION, 0x1);

        // A packet from another protocol version (e.g. an older 0x2 build)
        // with a valid CRC must still be rejected.
        bytes[0] = (0x2 << 4) | (bytes[0] & 0x0F);
        let n = bytes.len();
        let crc = crc16_ccitt(&bytes[..n - 2]);
        bytes[n - 2] = (crc >> 8) as u8;
        bytes[n - 1] = (crc & 0xFF) as u8;
        assert_eq!(
            decode_packet_bytes(&bytes),
            Err(PacketDecodeError::BadVersion(0x2))
        );
    }

    /// The FEC acceptance check: CRC-valid blocks are still rejected when the
    /// version, SEQ, pad or payload is not canonical (spec §5.1).
    #[test]
    fn fec_candidate_checks_version_seq_pad_and_payload() {
        let block = |hdr: u8, seq: u8, payload: &[u8], pad: u8| -> Vec<u8> {
            let mut b = vec![hdr, seq];
            b.extend_from_slice(payload);
            let crc = crc16_ccitt(&b);
            b.push((crc >> 8) as u8);
            b.push(crc as u8);
            b.resize(7, 0);
            if 2 + payload.len() + 2 < 7 {
                b[6] = pad;
            }
            bytes_to_nibbles(&b)
        };
        let ok = |hdr: u8, payload: &[u8]| fec_candidate_is_valid(&block(hdr, 2, payload, 0));
        assert!(ok(0x10, &[0x90, 60, 100]));
        assert!(ok(0x10, &[0x9F, 0x7F, 0x7F]));
        assert!(ok(0x10, &[0xC3, 5, 0]));
        assert!(ok(0x10, &[0xF6, 0, 0]));
        assert!(ok(0x13, &[0x08, 0x80, 0x01])); // GO 0.0 list 1
        assert!(ok(0x14, &[]));
        assert!(ok(0x15, &[]));
        // Not canonical: data >= 0x80, unsupported status, unused byte set,
        // NoteOn velocity 0, MSC fields out of range.
        assert!(!ok(0x10, &[0x90, 0x80, 100]));
        assert!(!ok(0x10, &[0x90, 60, 0xFF]));
        assert!(!ok(0x10, &[0x90, 60, 0]));
        assert!(!ok(0x10, &[0xF0, 0, 0]));
        assert!(!ok(0x10, &[0xF8, 0, 0]));
        assert!(!ok(0x10, &[0x60, 0, 0]));
        assert!(!ok(0x10, &[0xC0, 5, 1]));
        assert!(!ok(0x13, &[0x00, 0, 0]));
        assert!(!ok(0x13, &[0x08, 0x80, 0x00])); // list without cue
                                                 // Wrong version, unknown types, SEQ reserved bits, non-zero pad.
        assert!(!ok(0x20, &[0x90, 60, 100]));
        for t in [1u8, 2, 6, 7, 15] {
            assert!(!ok(0x10 | t, &[]));
            assert!(!ok(0x10 | t, &[0x90, 60, 100]));
        }
        assert!(!fec_candidate_is_valid(&block(
            0x10,
            4,
            &[0x90, 60, 100],
            0
        )));
        assert!(!fec_candidate_is_valid(&block(0x14, 0x80, &[], 0)));
        assert!(!fec_candidate_is_valid(&block(0x14, 0, &[], 0x01)));
        // A corrupted CRC.
        let mut b = block(0x10, 0, &[0x90, 60, 100], 0);
        b[11] ^= 1;
        assert!(!fec_candidate_is_valid(&b));
    }
}
