//! Link layer: CRC-16, packet framing, interleave, repetition, optional FEC.

mod crc;
mod fec;
mod interleave;
mod packet;
mod rs16;

pub use crc::{crc16_ccitt, verify_crc16};
pub use fec::{BodySymbol, FecCodec, FecDecoded, FecMode, FEC_DATA_NIBBLES, MAX_PARITY, WHITENING};
pub use interleave::{deinterleave, interleave};
pub use packet::{
    body_nibble_count, bytes_to_nibbles, decode_packet_bytes, encode_packet_bytes,
    fec_candidate_is_valid, nibbles_to_bytes, Packet, PacketDecodeError, PROTOCOL_VERSION,
    SEQ_MODULUS,
};
pub use rs16::{Rs16, Rs16Error, RS16_MAX_N};
