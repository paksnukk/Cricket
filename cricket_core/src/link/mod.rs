//! Link layer: CRC-16, packet framing, interleave, repetition, optional FEC.

mod crc;
mod fec;
mod interleave;
mod packet;
mod rs16;

pub use crc::{
    crc16_ccitt, crc16_keyed, link_key_from_passphrase, parse_link_key, verify_crc16,
    DEFAULT_LINK_KEY,
};
pub use fec::{BodySymbol, FecCodec, FecDecoded, FecMode, FEC_DATA_NIBBLES, MAX_PARITY, WHITENING};
pub use interleave::{deinterleave, interleave};
pub use packet::{
    body_nibble_count, bytes_to_nibbles, decode_packet_bytes, decode_packet_bytes_keyed,
    encode_packet_bytes, encode_packet_bytes_keyed, fec_candidate_is_valid,
    fec_candidate_is_valid_keyed, nibbles_to_bytes, packet_structure_is_valid, Packet,
    PacketDecodeError, PROTOCOL_VERSION, SEQ_MODULUS,
};
pub use rs16::{Rs16, Rs16Error, RS16_MAX_N};
