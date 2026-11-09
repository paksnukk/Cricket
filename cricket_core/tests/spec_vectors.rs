//! Test vectors of `docs/aml-v1-spec.md` §9. The spec quotes these exact
//! values; this test keeps spec and code from drifting apart. If it fails,
//! the wire format changed: update the spec (and think twice).

use cricket_core::link::{
    bytes_to_nibbles, crc16_ccitt, encode_packet_bytes, interleave, FecCodec, FecMode, Packet,
    Rs16, WHITENING,
};
use cricket_core::modem::{decode_samples, repetition_samples, CricketModemTx};
use cricket_core::phy::{best_tone, PhyConfig, RxEvent};
use cricket_core::transport::{MscCommand, MscCue, MscMessage, TransportMessage};

/// Parse `"1 0 9 …"` (hex nibbles) or `"10 00 90 …"` (hex bytes).
fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|t| u8::from_str_radix(t, 16).unwrap())
        .collect()
}

struct Vector {
    msg: TransportMessage,
    seq: u8,
    bytes: &'static str,
    crc: u16,
    nibbles: &'static str,
    codeword_a: &'static str,
    codeword_b: &'static str,
    /// Robust / `*_fec` body on the wire (codewords interleaved, whitened).
    coded_body: &'static str,
    /// Robust repeat / Fast / LTC body on the wire (padded, interleaved).
    uncoded_body: &'static str,
}

fn vectors() -> Vec<Vector> {
    vec![
        // Note On, channel 1, note 60 (middle C), velocity 100, SEQ 0.
        Vector {
            msg: TransportMessage::NoteOn {
                channel: 0,
                note: 60,
                velocity: 100,
            },
            seq: 0,
            bytes: "10 00 90 3C 64 01 B5",
            crc: 0x01B5,
            nibbles: "1 0 0 0 9 0 3 C 6 4 0 1 B 5",
            codeword_a: "1 0 0 0 9 0 3 2 9 7 B",
            codeword_b: "C 6 4 0 1 B 5 7 2 4 D",
            coded_body: "9 8 3 9 6 E 8 8 1 4 A 8 B 8 1 6 A 3 E C 1 F",
            uncoded_body: "1 9 6 B 0 0 4 5 0 3 0 0 0 C 1 0",
        },
        // MSC GO cue 14.5 list 2, SEQ 1 (compact payload 0x090092).
        Vector {
            msg: TransportMessage::Msc(MscMessage::cue(
                MscCommand::Go,
                MscCue::from_tenths(145).unwrap(),
                Some(2),
            )),
            seq: 1,
            bytes: "13 01 09 00 92 B0 5A",
            crc: 0xB05A,
            nibbles: "1 3 0 1 0 9 0 0 9 2 B 0 5 A",
            codeword_a: "1 3 0 1 0 9 0 5 0 4 4",
            codeword_b: "0 9 2 B 0 5 A 9 6 8 A",
            coded_body: "9 4 0 6 6 8 9 3 8 5 3 6 8 7 6 8 3 7 D 0 E 8",
            uncoded_body: "1 0 9 5 3 9 2 A 0 0 B 0 1 0 0 0",
        },
        // Heartbeat, SEQ 2 (shortest packet: pad nibbles in both bodies).
        Vector {
            msg: TransportMessage::Heartbeat,
            seq: 2,
            bytes: "14 02 F2 FA",
            crc: 0xF2FA,
            nibbles: "1 4 0 2 F 2 F A",
            codeword_a: "1 4 0 2 F 2 F 6 A 7 F",
            codeword_b: "A 0 0 0 0 0 0 3 5 C 4",
            coded_body: "9 E 7 F 6 A A 8 7 5 8 3 7 D 5 2 9 4 E 4 5 6",
            uncoded_body: "1 F 0 0 4 2 0 0 0 F 0 0 2 A 0 0",
        },
    ]
}

#[test]
fn field_generator_and_whitening() {
    // g(x) = (x + α)(x + α²)(x + α³)(x + α⁴) over GF(16) / x⁴ + x + 1:
    // the codeword of data x⁰…0 1 is g itself, = x⁴ + Dx³ + Cx² + 8x + 7.
    let rs = Rs16::new(7, 4).unwrap();
    assert_eq!(
        rs.encode(&hex("0 0 0 0 0 0 1")),
        hex("0 0 0 0 0 0 1 D C 8 7")
    );
    assert_eq!(
        &WHITENING[..22],
        hex("8 4 3 F 6 A 8 8 8 5 A 3 8 D 3 1 3 1 9 8 A 2").as_slice()
    );
}

#[test]
fn link_layer_vectors() {
    let rs = Rs16::new(7, 4).unwrap();
    let codec = FecCodec::new(FecMode::Rs16 { parity: 4 }).unwrap();
    for v in vectors() {
        let packet = Packet {
            msg_type: v.msg.msg_type(),
            seq: v.seq,
            payload: v.msg.to_payload(),
        };
        let bytes = encode_packet_bytes(&packet).unwrap();
        assert_eq!(bytes, hex(v.bytes), "{:?}", v.msg);
        assert_eq!(crc16_ccitt(&bytes[..bytes.len() - 2]), v.crc);
        let nibbles = bytes_to_nibbles(&bytes);
        assert_eq!(nibbles, hex(v.nibbles));

        let mut data = nibbles.clone();
        data.resize(14, 0);
        assert_eq!(rs.encode(&data[..7]), hex(v.codeword_a));
        assert_eq!(rs.encode(&data[7..]), hex(v.codeword_b));
        assert_eq!(codec.encode_body(&nibbles), hex(v.coded_body));

        let mut padded = nibbles;
        padded.resize(16, 0);
        assert_eq!(interleave(&padded, 4), hex(v.uncoded_body));
    }
}

/// The MFSK transmitters put exactly preamble + body on the air, one tone
/// per nibble: Robust sends the coded body once, Robust repeat the uncoded
/// body twice, Fast the uncoded body once. Each decodes back.
#[test]
fn mfsk_on_air_symbols() {
    for v in vectors() {
        for (config, body, copies) in [
            (PhyConfig::robust(), v.coded_body, 1),
            (PhyConfig::robust_repeat(), v.uncoded_body, 2),
            (PhyConfig::fast(), v.uncoded_body, 1),
        ] {
            let audio = CricketModemTx::new(config.clone()).encode_message_with_seq(&v.msg, v.seq);
            let rep = repetition_samples(&config) as usize;
            assert_eq!(audio.len(), copies * rep);
            let mut symbols = config.preamble().to_vec();
            symbols.extend(hex(body));
            let period = config.samples_per_symbol_period();
            let sym = config.symbol_samples();
            let tones = config.tone_table();
            for copy in 0..copies {
                for (i, &want) in symbols.iter().enumerate() {
                    let at = copy * rep + i * period;
                    let det =
                        best_tone(&audio[at..at + sym], &tones, config.sample_rate_hz).unwrap();
                    assert_eq!(det.index, want, "{:?} copy {copy} symbol {i}", v.msg);
                }
                // One silent symbol period after each copy.
                let gap = &audio[copy * rep + symbols.len() * period..(copy + 1) * rep];
                assert!(gap.iter().all(|&s| s == 0.0));
            }
            assert_eq!(decode_samples(config, &audio), vec![v.msg.clone()]);
        }
    }
}

/// The HF profiles (spec §6.6): `hf_robust` sends Robust's symbols on
/// 19 000 + 166⅔ k Hz; `hf_fast` repacks the coded body into 3-bit symbols
/// on 8 tones (19 000 + 333⅓ k Hz) after the preamble `5 2 6 1`. The Note
/// On vector's 30 body symbols are quoted in §9.
#[test]
fn hf_on_air_symbols() {
    let v = &vectors()[0];
    let robust = PhyConfig::hf_robust();
    assert_eq!(robust.tone_table()[0], 19_000.0);
    assert!((robust.tone_table()[15] - 21_500.0).abs() < 0.01);
    let fast = PhyConfig::hf_fast();
    assert_eq!(fast.preamble(), [5, 2, 6, 1]);
    assert!((fast.tone_table()[7] - 21_333.3).abs() < 0.1);
    let fast_body: Vec<u8> = "4 6 0 3 4 5 5 6 4 2 0 1 2 2 5 0 5 6 0 1 3 2 4 3 7 3 0 1 7 4"
        .split_whitespace()
        .map(|t| t.parse().unwrap())
        .collect();
    assert_eq!(
        cricket_core::phy::pack::pack_nibbles(&hex(v.coded_body), 3),
        fast_body
    );
    for (config, body) in [(robust, hex(v.coded_body)), (fast, fast_body)] {
        let audio = CricketModemTx::new(config.clone()).encode_message_with_seq(&v.msg, v.seq);
        let rep = repetition_samples(&config) as usize;
        assert_eq!(audio.len(), rep);
        let mut symbols = config.preamble().to_vec();
        symbols.extend(body);
        let period = config.samples_per_symbol_period();
        let sym = config.symbol_samples();
        let tones = config.tone_table();
        for (i, &want) in symbols.iter().enumerate() {
            let at = i * period;
            let det = best_tone(&audio[at..at + sym], &tones, config.sample_rate_hz).unwrap();
            assert_eq!(det.index, want, "symbol {i}");
        }
        assert!(audio[symbols.len() * period..].iter().all(|&s| s == 0.0));
        assert_eq!(decode_samples(config, &audio), vec![v.msg.clone()]);
    }
}

/// The LTC profiles carry the same bodies (`ltc_fec`: the coded body,
/// `ltc`: the uncoded one) as 4 bits per nibble. (The BMC receiver can
/// start a second, false lock on the burst's tail, whose erased nibbles the
/// modem discards; hence `take`.)
#[test]
fn ltc_on_air_nibbles() {
    for v in vectors() {
        for (config, body) in [
            (PhyConfig::ltc_fec(), v.coded_body),
            (PhyConfig::ltc(), v.uncoded_body),
        ] {
            let mut audio = vec![0.0; 100];
            audio
                .extend(CricketModemTx::new(config.clone()).encode_message_with_seq(&v.msg, v.seq));
            audio.extend(vec![0.0; 500]);
            let mut rx = config.modulation.phy().rx(&config);
            let want = hex(body);
            rx.set_expected_body_nibbles(want.len());
            let got: Vec<u8> = rx
                .push_samples(&audio)
                .into_iter()
                .filter_map(|ev| match ev {
                    RxEvent::BodyNibble { result, .. } => Some(result.nibble),
                    _ => None,
                })
                .take(want.len())
                .collect();
            assert_eq!(got, want, "{:?}", v.msg);
            assert_eq!(decode_samples(config, &audio), vec![v.msg.clone()]);
        }
    }
}
