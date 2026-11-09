//! Link-layer body FEC for AML packet bodies (spec §4.6; design history in
//! docs/proposals/fec.md).
//!
//! The packet bytes (HDR, SEQ, PAYLOAD, CRC-16 — unchanged) are split into
//! nibbles, zero-padded to [`FEC_DATA_NIBBLES`] (14 = the largest packet,
//! MSC, 7 bytes) and protected by two shortened Reed–Solomon codewords
//! RS(7 + p, 7) over GF(16) (`p` = parity nibbles per codeword). The two
//! codewords are interleaved symbol by symbol on the wire
//! (`a0 b0 a1 b1 …`), so a burst of `2p` consecutive bad symbols is still
//! within reach of erasure decoding. The interleaved body is XORed with a
//! fixed whitening sequence ([`WHITENING`]), so the coded body is a coset of
//! the code: an uncoded AML body on the same phy (a misconfigured
//! transmitter) does not line up with the packet checks.
//!
//! The layer works on nibbles only; it does not care how a nibble reached
//! the wire (one MFSK-16 tone, or 4 bits of another modulation).
//!
//! Decoding is errors-and-erasures with a small generalized-minimum-distance
//! (GMD) search: the receiver's per-symbol reliability (confidence in dB)
//! picks extra erasure candidates, and every candidate packet must still
//! pass the unchanged CRC-16 (checked by the caller's `accept`).
//!
//! **False-accept bound.** Every candidate handed to `accept` is one more
//! chance for a random (non-Cricket) body to pass the CRC, and the GMD search
//! can find codewords far beyond the code's radius. A candidate pair is
//! therefore only tried when its errata weight `sum(2 e + f)` over the whole
//! body (`e` = changed symbols that passed the gate, `f` = gate erasures) is
//! at most the body's redundancy `2 * parity` — bounded-distance decoding at
//! body level, which still lets GMD fix an uneven split (3 errors in one
//! codeword, 0–1 in the other) — or when it changes at most one symbol
//! besides the gate erasures (a dropout plus one weak symbol). Pairs are
//! tried lightest first. See `docs/proposals/fec.md` §9.

use super::rs16::{Rs16, RS16_MAX_N};
use alloc::vec::Vec;

/// Data nibbles carried by an FEC-coded body (largest packet: 7 bytes).
pub const FEC_DATA_NIBBLES: usize = 14;
/// Uncoded body length (`modem::MAX_BODY_NIBBLES`).
const UNCODED_BODY_NIBBLES: usize = 16;
const CODEWORDS: usize = 2;
const K: usize = FEC_DATA_NIBBLES / CODEWORDS;

/// Whitening sequence XORed onto the coded body (wire symbol `i` carries
/// `coded[i] ^ WHITENING[i]`), long enough for the longest body (parity 8).
/// Symbols 1.. are the non-zero hex digits of the fraction of π
/// (`243F6A8885A3…`, from the second on); symbol 0 is `0x8`, so the
/// version nibble `0x1` goes on air as `0x9` (`1001`): a non-zero mask
/// there keeps an uncoded body's version nibble from lining up, and a
/// leading `1` bit right after the BMC sync measured best under heavy
/// high-pass filtering (`docs/proposals/fec.md` §9).
pub const WHITENING: [u8; 30] = [
    0x8, 0x4, 0x3, 0xF, 0x6, 0xA, 0x8, 0x8, 0x8, 0x5, 0xA, 0x3, 0x8, 0xD, 0x3, 0x1, 0x3, 0x1, 0x9,
    0x8, 0xA, 0x2, 0xE, 0x3, 0x7, 0x7, 0x3, 0x4, 0x4, 0xA,
];
const _: () = assert!(WHITENING.len() >= CODEWORDS * RS16_MAX_N);

/// Largest `parity` of [`FecMode::Rs16`]: a codeword over GF(16) holds at
/// most [`RS16_MAX_N`] = 15 nibbles, 7 of them data.
pub const MAX_PARITY: u8 = (RS16_MAX_N - K) as u8;

/// Forward-error-correction scheme applied to the packet body.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FecMode {
    /// Uncoded body: 16 nibbles, block-interleaved (spec §4.3).
    #[default]
    None,
    /// Two interleaved RS(7 + parity, 7) codewords over GF(16).
    /// `parity` is 1..=[`MAX_PARITY`] (8); a value outside that range is
    /// treated as the nearest valid one (see [`FecMode::parity`]), so no
    /// constructor panics on it. Body = `2 * (7 + parity)` nibbles.
    Rs16 { parity: u8 },
}

impl FecMode {
    pub fn is_enabled(&self) -> bool {
        !matches!(self, FecMode::None)
    }

    /// Parity nibbles per codeword: `Rs16`'s `parity` clamped to
    /// 1..=[`MAX_PARITY`], 0 for `None`. Every size and codec derived from
    /// the mode uses this value, so an out-of-range `parity` is consistent
    /// between transmitter and receiver (but not a wire format of its own).
    pub fn parity(&self) -> usize {
        match *self {
            FecMode::None => 0,
            FecMode::Rs16 { parity } => usize::from(parity.clamp(1, MAX_PARITY)),
        }
    }

    /// Body symbols per repetition (excluding the preamble).
    pub fn body_nibbles(&self) -> usize {
        match *self {
            FecMode::None => UNCODED_BODY_NIBBLES,
            FecMode::Rs16 { .. } => CODEWORDS * (K + self.parity()),
        }
    }

    /// Preamble symbols after the first that the receiver may miss and
    /// still lock: the coded body and CRC-16 verify the lock instead.
    /// The first symbol (which triggers fine alignment) must always match.
    pub fn preamble_misses(&self) -> usize {
        match *self {
            FecMode::None => 0,
            FecMode::Rs16 { .. } => 1,
        }
    }

    /// Body symbols the receiver may accept as erasures (failed detection
    /// gate) before abandoning a lock: the most the code could fill in.
    pub fn max_body_erasures(&self) -> usize {
        match *self {
            FecMode::None => 0,
            FecMode::Rs16 { .. } => CODEWORDS * self.parity(),
        }
    }
}

/// One received body symbol, as handed to [`FecCodec::decode_body`].
#[derive(Debug, Clone, Copy)]
pub struct BodySymbol {
    pub nibble: u8,
    /// Higher = more trustworthy (the demodulator's confidence in dB).
    pub reliability: f32,
    /// The symbol failed the detection gate; its value is a guess at best.
    pub erased: bool,
}

/// Result of a successful body decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FecDecoded {
    /// The [`FEC_DATA_NIBBLES`] data nibbles (packet nibbles + zero pad).
    pub nibbles: Vec<u8>,
    /// Data symbols the decoder changed (0 = received clean).
    pub corrected: usize,
    /// Errata weight `sum(2 e + f)` of the decoded body: 2 per changed
    /// symbol that passed the gate, 1 per gate erasure (at most
    /// [`FecCodec::max_weight`]).
    pub weight: usize,
}

/// One decoding of one codeword.
#[derive(Debug, Clone)]
struct Candidate {
    data: Vec<u8>,
    /// Data symbols changed.
    corrected: usize,
    /// Symbols that passed the gate but were changed (`e`).
    errors: usize,
    /// `2 e + f` over the whole codeword (see [`FecDecoded::weight`]).
    weight: usize,
}

/// Encoder/decoder for [`FecMode::Rs16`].
#[derive(Debug, Clone)]
pub struct FecCodec {
    rs: Rs16,
}

impl FecCodec {
    /// `None` for [`FecMode::None`]; always `Some` for `Rs16` (its parity
    /// is clamped to a valid range, see [`FecMode::parity`]).
    pub fn new(mode: FecMode) -> Option<Self> {
        match mode {
            FecMode::None => None,
            FecMode::Rs16 { .. } => Rs16::new(K, mode.parity()).ok().map(|rs| Self { rs }),
        }
    }

    pub fn body_nibbles(&self) -> usize {
        CODEWORDS * self.rs.n()
    }

    /// Packet nibbles (at most [`FEC_DATA_NIBBLES`]) → interleaved coded body.
    pub fn encode_body(&self, packet_nibbles: &[u8]) -> Vec<u8> {
        assert!(packet_nibbles.len() <= FEC_DATA_NIBBLES, "packet too long");
        let mut data = [0u8; FEC_DATA_NIBBLES];
        data[..packet_nibbles.len()].copy_from_slice(packet_nibbles);
        let a = self.rs.encode(&data[..K]);
        let b = self.rs.encode(&data[K..]);
        let mut body = Vec::with_capacity(self.body_nibbles());
        for (x, y) in a.into_iter().zip(b) {
            body.push(x);
            body.push(y);
        }
        for (s, w) in body.iter_mut().zip(WHITENING) {
            *s ^= w;
        }
        body
    }

    /// Largest errata weight a decoded body may have: the body's total
    /// redundancy (`2 * parity` symbols).
    pub fn max_weight(&self) -> usize {
        CODEWORDS * self.rs.nsym()
    }

    /// Decode a received body. `accept` is called with each candidate
    /// [`FEC_DATA_NIBBLES`]-nibble data block (lightest errata weight first,
    /// never above [`max_weight`](Self::max_weight)) and must return true
    /// only for a valid packet (CRC-16 etc.).
    pub fn decode_body(
        &self,
        symbols: &[BodySymbol],
        mut accept: impl FnMut(&[u8]) -> bool,
    ) -> Option<FecDecoded> {
        if symbols.len() != self.body_nibbles() {
            return None;
        }
        let cw: [Vec<BodySymbol>; CODEWORDS] = core::array::from_fn(|c| {
            symbols
                .iter()
                .zip(WHITENING)
                .skip(c)
                .step_by(CODEWORDS)
                .map(|(s, w)| BodySymbol {
                    nibble: (s.nibble ^ w) & 0x0F,
                    ..*s
                })
                .collect()
        });
        let a = self.candidates(&cw[0]);
        let b = self.candidates(&cw[1]);
        let max = self.max_weight();
        // At most 5 x 5 pairs; stable sort keeps the GMD order within a weight.
        let mut pairs: Vec<(usize, usize)> = (0..a.len())
            .flat_map(|i| (0..b.len()).map(move |j| (i, j)))
            .filter(|&(i, j)| a[i].weight + b[j].weight <= max || a[i].errors + b[j].errors <= 1)
            .collect();
        pairs.sort_by_key(|&(i, j)| a[i].weight + b[j].weight);
        let mut data = Vec::with_capacity(FEC_DATA_NIBBLES);
        for (i, j) in pairs {
            let (ca, cb) = (&a[i], &b[j]);
            data.clear();
            data.extend_from_slice(&ca.data);
            data.extend_from_slice(&cb.data);
            if accept(&data) {
                return Some(FecDecoded {
                    nibbles: data,
                    corrected: ca.corrected + cb.corrected,
                    weight: ca.weight + cb.weight,
                });
            }
        }
        None
    }

    /// Distinct decodings of one codeword, most plausible first: gate
    /// erasures plus 0, 1, … extra erasures on the least reliable symbols,
    /// then errors-only.
    fn candidates(&self, word: &[BodySymbol]) -> Vec<Candidate> {
        let nsym = self.rs.nsym();
        let k = self.rs.k();
        let received: Vec<u8> = word.iter().map(|s| s.nibble & 0x0F).collect();

        // Positions ordered least reliable first; gate erasures before all others.
        let mut order: Vec<usize> = (0..word.len()).collect();
        order.sort_by(|&i, &j| {
            let key = |s: &BodySymbol| (!s.erased, s.reliability);
            let (ei, ri) = key(&word[i]);
            let (ej, rj) = key(&word[j]);
            ei.cmp(&ej)
                .then(ri.partial_cmp(&rj).unwrap_or(core::cmp::Ordering::Equal))
        });
        let gate_erasures = word.iter().filter(|s| s.erased).count();
        let hard = gate_erasures.min(nsym);

        let mut out: Vec<Candidate> = Vec::new();
        let attempt = |erasures: &[usize], out: &mut Vec<Candidate>| {
            let mut w = received.clone();
            if self.rs.decode(&mut w, erasures).is_ok() {
                let data = &w[..k];
                if !out.iter().any(|c| c.data == data) {
                    let corrected = data
                        .iter()
                        .zip(&received[..k])
                        .filter(|(x, y)| x != y)
                        .count();
                    // Intrinsic distance to the received word, however the
                    // codeword was found (GMD erasures that turned out to
                    // need a change count as errors).
                    let errors = word
                        .iter()
                        .zip(w.iter().zip(&received))
                        .filter(|(s, (x, y))| !s.erased && x != y)
                        .count();
                    out.push(Candidate {
                        data: data.to_vec(),
                        corrected,
                        errors,
                        weight: 2 * errors + gate_erasures,
                    });
                }
            }
        };
        for n_erase in hard..=nsym {
            attempt(&order[..n_erase], &mut out);
        }
        if hard > 0 {
            attempt(&[], &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(body: &[u8]) -> Vec<BodySymbol> {
        body.iter()
            .map(|&nibble| BodySymbol {
                nibble,
                reliability: 20.0,
                erased: false,
            })
            .collect()
    }

    #[test]
    fn body_sizes() {
        assert_eq!(FecMode::None.body_nibbles(), 16);
        assert_eq!(FecMode::Rs16 { parity: 4 }.body_nibbles(), 22);
        assert_eq!(FecMode::Rs16 { parity: 2 }.body_nibbles(), 18);
        assert_eq!(FecMode::Rs16 { parity: 4 }.max_body_erasures(), 8);
    }

    #[test]
    fn roundtrip_and_burst_recovery() {
        let codec = FecCodec::new(FecMode::Rs16 { parity: 4 }).unwrap();
        let data: Vec<u8> = (0..12).map(|i| (i * 5 + 3) as u8 & 0x0F).collect();
        let body = codec.encode_body(&data);
        assert_eq!(body.len(), 22);
        let mut want = data.clone();
        want.resize(FEC_DATA_NIBBLES, 0);
        let expect = |d: &[u8]| d == want.as_slice();

        let dec = codec.decode_body(&clean(&body), expect).unwrap();
        assert_eq!(dec.corrected, 0);

        // A burst of 4 wrong symbols (2 per codeword) decodes errors-only.
        let mut rx = clean(&body);
        for s in rx.iter_mut().skip(6).take(4) {
            s.nibble ^= 0x9;
        }
        assert!(codec.decode_body(&rx, expect).is_some());

        // A burst of 8 gate-erased symbols (4 per codeword) is filled in.
        let mut rx = clean(&body);
        for s in rx.iter_mut().skip(10).take(8) {
            s.nibble = 0;
            s.erased = true;
            s.reliability = 0.0;
        }
        assert!(codec.decode_body(&rx, expect).is_some());

        // 3 wrong symbols in one codeword exceed errors-only capacity, but
        // GMD recovers them when they are the least reliable ones.
        let mut rx = clean(&body);
        for i in [0usize, 2, 4] {
            rx[i].nibble ^= 0x3;
            rx[i].reliability = 1.0;
        }
        assert!(codec.decode_body(&rx, expect).is_some());
    }

    fn setup() -> (FecCodec, Vec<u8>, Vec<u8>) {
        let codec = FecCodec::new(FecMode::Rs16 { parity: 4 }).unwrap();
        let data: Vec<u8> = (0..14).map(|i| (i * 7 + 1) as u8 & 0x0F).collect();
        let body = codec.encode_body(&data);
        (codec, data, body)
    }

    /// GMD could reach this body (3 weak errors in codeword a, 2 in b), but
    /// its errata weight 10 exceeds the body redundancy 8 with more than one
    /// error: it must not reach `accept`, although it is the right one.
    #[test]
    fn candidates_beyond_weight_bound_never_reach_accept() {
        let (codec, data, body) = setup();
        assert_eq!(codec.max_weight(), 8);
        let mut rx = clean(&body);
        // Even wire positions are codeword a, odd ones codeword b.
        for i in [0usize, 4, 8, 3, 7] {
            rx[i].nibble ^= 0x5;
            rx[i].reliability = 1.0;
        }
        // (Wrong codewords within the bound may still be offered; the
        // right one, at weight 10, must not be.)
        assert!(codec.decode_body(&rx, |d| d == data.as_slice()).is_none());

        // One error fewer (weight 8) is within the bound and decodes.
        rx[7] = clean(&body)[7];
        let dec = codec.decode_body(&rx, |d| d == data.as_slice()).unwrap();
        assert_eq!(dec.weight, 8);
        assert!(dec.weight <= codec.max_weight());
    }

    /// A dropout (7 gate erasures) plus one weak wrong symbol is beyond the
    /// bound (weight 9) but allowed: at most one changed symbol. With 6
    /// erasures and two weak wrong symbols (weight 10) each codeword is
    /// still GMD-decodable, but the pair is a guess and is not tried.
    #[test]
    fn dropout_plus_one_error_is_allowed() {
        let (codec, data, body) = setup();
        let dropout = |rx: &mut Vec<BodySymbol>, n: usize| {
            for s in rx.iter_mut().skip(6).take(n) {
                s.nibble = 0;
                s.erased = true;
                s.reliability = 0.0;
            }
        };
        let mut rx = clean(&body);
        dropout(&mut rx, 7);
        rx[21].nibble ^= 0x3;
        rx[21].reliability = 0.5;
        let dec = codec.decode_body(&rx, |d| d == data.as_slice()).unwrap();
        assert_eq!(dec.weight, 9);

        let mut rx = clean(&body);
        dropout(&mut rx, 6);
        for i in [20usize, 21] {
            rx[i].nibble ^= 0x3;
            rx[i].reliability = 0.5;
        }
        assert!(codec.decode_body(&rx, |d| d == data.as_slice()).is_none());
        // Either error alone: weight 8, decoded.
        rx[21] = clean(&body)[21];
        assert_eq!(
            codec
                .decode_body(&rx, |d| d == data.as_slice())
                .unwrap()
                .weight,
            8
        );
    }

    /// The body on air is the interleaved codewords XOR [`WHITENING`]; the
    /// same body without whitening (an uncoded-style or foreign body) does
    /// not decode to the packet.
    #[test]
    fn whitening_is_applied_and_required() {
        let (codec, data, body) = setup();
        assert_eq!(WHITENING[0], 0x8);
        assert!(WHITENING.iter().all(|&w| w != 0 && w < 16));
        let plain: Vec<u8> = body.iter().zip(WHITENING).map(|(b, w)| b ^ w).collect();
        let rs = Rs16::new(K, 4).unwrap();
        let a = rs.encode(&data[..K]);
        let b = rs.encode(&data[K..]);
        let interleaved: Vec<u8> = a.iter().zip(&b).flat_map(|(x, y)| [*x, *y]).collect();
        assert_eq!(plain, interleaved);
        assert!(codec
            .decode_body(&clean(&plain), |d| d == data.as_slice())
            .is_none());
    }
}
