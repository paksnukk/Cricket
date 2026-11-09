# Proposal: Reed–Solomon FEC for the Robust profile

Status: **Adopted in AML v1** — see spec (`docs/aml-v1-spec.md`) §4.6 (body FEC), §4.7 (validation
rules), §3.2.2 (joint acquisition) and §6.1 (Robust). The profile this document calls `robust_fec` is
now **`robust`**, the default (`robust_fec` remains an accepted alias); the K = 2 profile it calls
`robust` is now **`robust_repeat`**, which also uses joint acquisition with one miss (§10.4). This
document is kept as design history: names and numbers are as they were when measured; where it differs
from the spec, the spec applies.

**2026-11 update (spec r4, "MFSK receiver and tone plan" in the CHANGELOG).** The MFSK tone step is
now 1 / symbol duration (orthogonal tones) and the receiver band-passes its input to the tone band,
keeps the best guess in uncoded bodies and tracks the body symbol clock (spec §3.1–§3.2). The
acquisition thresholds this document records moved: purity gate 0.20 for every MFSK profile (§3.2
had 0.25 / 0.30), the "contradicting symbol" bar of §10.1 is the full gate purity (was half). Random
MFSK tones now cause ~4 phy locks per minute for `robust` and `robust_repeat` (§10.2 measured 13 /
10). False accepts (§9, `FALSE_SECONDS=120`, canonical-payload estimate): `robust` fed uncoded
Robust repeat / Fast traffic 9 candidates per minute, one false message per ~1,660 years (§9.7:
58 / 260); MFSK traffic into `ltc_fec` 55 candidates per minute, 272 years (37 / 404), into
`ltc2400_fec` 37 per minute, 404 years (77 / 196). The 10-year target holds with margin in every
measured case. The numbers below are otherwise as they were when measured.

**2026-11 update (spec r5, "Link key, scheduler and fail-safe" in the CHANGELOG).** The CRC-16 init
value is now the link key (spec §4.1); the default `0xFFFF` keeps the wire format of this document. A
neighbouring link of the same profile with another key is a new foreign signal for §9: its clean
bodies decode to exactly one candidate each (the neighbour's packet), which is structurally valid
and fails the CRC **deterministically** (two keys shift the CRC by a fixed non-zero amount for a
given length), so it cannot become a false message however often it arrives; only the GMD
alternatives of such a body are random candidates in the sense of §9.1. Measured
(`false_accept_report`, `FALSE_SECONDS=120`, "other-key traffic" row, back-to-back traffic at SNR
20 dB): `robust` 652 false bodies and 636 candidates per minute, `ltc_fec` 2,550 and 2,550, i.e.
~1 candidate per body and no random candidate beyond the neighbour's own packet in 2 minutes
(`other_key_traffic_never_decodes` guards < 1.3 candidates per body). The report excludes the
deterministically rejected candidates from its estimate for this row. Keying the whitening sequence
as well (making the neighbour's body a non-codeword, ~0.04 random candidates per body as in §9.1)
was therefore not adopted: it would add a key-dependent wire transform for no measurable gain over
the ~0 random candidates per body measured.

Original status: **Phase 1, prototype + measurements.** Not part of AML v1. The `robust` and `fast` profiles are
unchanged on the wire; the prototype is the opt-in profile `robust_fec` (`PhyConfig::robust_fec()`).
Since the acquisition hardening (§10), `robust_fec` matches or beats `robust` in every measured channel
except dropouts of ≥ ~50 ms, which are a single-copy body limit.

## 1. Problem

Robust sends every packet twice (K = 2). That doubles airtime: 273 ms per message instead of 136.5 ms.
On a single message the first good copy arrives after ~131 ms either way. Under real traffic, though, a
NoteOn waits behind whatever is on the air (often a NoteOff refresh), and that wait is a full K = 2
transmission. This is most of the measured NoteOn latency (mean ~360 ms, p95 ~500 ms in `cricket loopback`).

Repetition is also a weak code. A copy is lost when any one of its 20 symbols is bad, and the receiver
abandons a lock on the first body symbol that fails the detection gate.

## 2. Chosen code: RS over GF(16), two interleaved RS(11,7) codewords

- **Symbol-matched.** One MFSK-16 tone is one nibble is one GF(16) code symbol. A wrong tone is exactly
  one symbol error, whatever its bit pattern. Bit-oriented codes are a poor fit: one wrong tone flips
  1–4 bits, so Hamming(7,4) fails and a convolutional/Viterbi code needs bit interleaving and soft bits
  that the phy does not have. Byte-wise RS over GF(256) works, but a codeword needs at least ~11 bytes
  (22 symbols) for the same correction, and a single tone error costs one whole byte.
- **Erasures are cheap and accurate.** The demodulator already has a reliable gate (purity, confidence).
  In the simulator, under AWGN, **every** body symbol that passed the gate was correct, and every bad
  symbol failed the gate. RS corrects `2e + f <= parity`, so an erasure costs half as much as an error.
- **Layout.** RS over GF(16) has `n <= 15`, and the largest packet (MscLite) is 7 bytes = 14 nibbles.
  The 14 data nibbles (packet bytes zero-padded, **CRC-16 unchanged**) are split into two halves, and
  each half is encoded as a shortened RS(11,7) (4 parity nibbles). The codewords go out alternately
  (`a0 b0 a1 b1 …`; this replaces the depth-4 block interleaver). Body: 22 symbols instead of 16.
  Per codeword the code corrects 2 errors, or 4 erasures, or a mix. On the wire that covers any burst
  of 4 wrong tones or 8 erased symbols.
- **Decoder.** Errors-and-erasures (Forney syndromes, Berlekamp–Massey, Chien, Forney). A small
  GMD search adds 0..4 extra erasures on the lowest-`confidence_db` symbols and finally tries
  errors-only. Each codeword yields at most 5 distinct candidates. A candidate pair is tried only if
  its errata weight `Σ(2e + f)` is at most 8 (or it changes at most one symbol besides the gate
  erasures), lightest first, and accepted only if the CRC-16, version, type, zero padding and payload
  ranges all check out (§9).
- **Whitening.** The 22 body nibbles are XORed with a fixed sequence (`fec::WHITENING`), so a body
  without FEC on the same phy does not line up with the packet checks (§9).
- **RX changes (only with FEC):** a body symbol that fails the gate is kept as an erasure, up to 8 per
  body (above that the attempt is abandoned and rewinds as today). Preamble symbols may also fail,
  because the coded body + CRC now verify the lock. Without this change, the 4-symbol preamble
  becomes the main loss mechanism at K = 1 (see §5). In the first prototype that meant one symbol after
  the first. `robust_fec` now uses joint preamble detection, which allows two misses anywhere (§10).

Code: `cricket_core/src/link/rs16.rs` (codec, ~300 lines, `no_std` + `alloc`, no dependencies),
`cricket_core/src/link/fec.rs` (`FecMode`, `FecCodec`), `PhyConfig::fec`, and erasure and
preamble-miss handling in `phy/demod.rs`. `ModemStats::fec_corrected` counts the repaired symbols.

## 3. Wire-format impact

| | AML v1 Robust | `robust_fec` |
|---|---|---|
| Preamble | `A 5 C 3` | unchanged (RX: joint detection, tolerates 2 misses anywhere, §10) |
| Packet bytes (HDR, SEQ, PAYLOAD, CRC-16) | — | unchanged |
| Body | 16 nibbles, block-interleaved (depth 4) | 22 nibbles: 2 × RS(11,7), symbol-interleaved, XOR whitening |
| Copies K | 2 | 1 |
| On-wire per message | 2 × 21 periods = 273 ms | 27 periods = 175.5 ms |

- **No header flag is possible.** The header sits inside the codeword, and the receiver has to know
  the body length before it can decode. The FEC mode must be a profile setting on both ends, the
  same way Robust and Fast are today (profiles are not signalled on the wire).
- **Coexistence.** A mismatched receiver locks the preamble, reads the wrong body length and
  normally drops the packet. Before §9 this was not safe enough: AML v1 Robust/Fast traffic into a
  `robust_fec` receiver reached the CRC with ~1000 candidates per minute, about one false message
  per day of continuous traffic. With the measures of §9 it is ≳ 4,000 years.
- **Spec changes if adopted:** a new §4.6 "Body FEC" (field polynomial `x^4+x+1`, generator roots
  α^1..α^4, systematic, wire order data-first/highest-degree-first, two codewords over
  data nibbles 0–6 and 7–13, alternating on the wire, XORed with the whitening sequence, and the
  receiver acceptance rules of §9: weight bound and payload ranges). §4.3 interleave applies to uncoded profiles
  only. In §4.4 the dedup window uses the profile's body length. §3.2 gets the erasure and
  preamble-miss rules. §6 gets a new profile entry. `PROTOCOL_VERSION` can stay `0x1`: the packet
  format is identical, only the body coding differs per profile.

## 4. Measurements

*The FEC columns in this section and in §5 were measured with the first-prototype acquisition. §10.2 has the
numbers with joint preamble detection.*

Channel simulator (`cricket_core/tests/support`), one message per trial (¾ NoteOn, ¼ MscLite), random
offset and seed, 400 trials per cell. Delivery % and mean time from message start to delivery.
Reproduce with `cargo test --release -p cricket_core --test fec -- --ignored --nocapture`.
SNR is relative to the tone RMS over the full 24 kHz band. "Wrong tone" replaces a symbol period
with another tone (plus SNR 20 dB). A dropout mutes one stretch at a random position (plus SNR 20 dB).

| channel | robust K2 (today) | robust K1 | **fec p4 K1** | fec p2 K1 | fec p4 K2 |
|---|---|---|---|---|---|
| airtime / message | 273 ms | 136.5 ms | **175.5 ms** | 149.5 ms | 351 ms |
| clean | 100 (131 ms) | 100 (131) | **100 (170)** | 100 (144) | 100 (170) |
| AWGN 6 dB | 100 (131) | 100 (131) | **100 (170)** | 100 (144) | 100 (170) |
| AWGN 0 dB | 100 (134) | 97.5 | **97.8** | 97.8 | 100 |
| AWGN −2 dB | 97.0 (147) | 85.8 | **93.8** | 93.8 | 99.5 |
| AWGN −3 dB | 33.0 | 19.5 | **77.2** | 76.2 | 94.5 |
| AWGN −4 dB | 0 | 0 | **18.2** | 3.5 | 32.5 |
| wrong tone 1 % | 97.0 (152) | 82.0 | **99.0 (170)** | 98.2 | 100 |
| wrong tone 2 % | 94.8 (164) | 71.5 | **98.5 (170)** | 96.0 | 100 |
| wrong tone 5 % | 64.0 | 41.0 | **88.5** | 79.5 | 99.0 |
| wrong tone 10 % | 32.2 | 17.5 | **73.0** | 54.0 | 91.8 |
| dropout 10 ms | 100 (163) | 65.5 | **95.8** | 96.2 | 100 |
| dropout 30 ms | 96.5 | 63.0 | **96.2** | 79.2 | 100 |
| dropout 60 ms | 88.8 | 58.2 | **70.2** | 63.5 | 100 |
| gain −40 dB, SNR 0 | 100 | 96.2 | **97.5** | 97.5 | 99.8 |
| low-pass 2.5 kHz, SNR 6 | 100 | 98.5 | **99.0** | 98.8 | 100 |
| drift 500 ppm, SNR 0 | 100 | 98.5 | **98.8** | 98.8 | 100 |

No cell produced a false message. (Measured before the false-accept measures of §9; they move the
`fec p4 K1` cells by at most 1.1 points, see §9.6.) The no-preamble-tolerance variant (first prototype) scored worse
at K = 1, because the preamble was then the main loss: wrong tone 1/2/5 % = 96.8/92.8/77.0,
AWGN −3 dB = 62.5, dropout 10 ms = 93.0.

**Engine latency** (`cricket loopback --profiles robust,robust_fec --notes 60`, MIDI in → MIDI out,
notes held and released, 3 runs, consistent within ±1 ms):

| profile | mean | p95 | jitter |
|---|---|---|---|
| robust (K=2) | 360 ms | 500 ms | 93 ms |
| **robust_fec (K=1)** | **269 ms (−25 %)** | **348 ms (−31 %)** | **55 ms** |

**Stuck-note run** (`tests/stuck_notes.rs`, back-to-back traffic, SNR 20 dB + wrong tones). Messages
lost, robust vs robust_fec: 0.5 % → 2/788 vs 6/918; 1 % → 13/819 vs 10/1046; 2 % → 47/752 vs 26/1048.
robust_fec sends ~30 % more messages in the same time (more NoteOff refreshes fit). No false
messages and no stuck notes in either profile.

## 5. Reading the results

- Single-message latency goes **up** by 39 ms (body 22 vs 16 symbols, and K=2's first copy was
  already a full message). Latency under traffic goes **down** by ~25 % (mean) and ~31 % (p95),
  because each message holds the channel for 175.5 ms instead of 273 ms.
- Against symbol errors (the dominant MFSK failure), FEC K=1 beats K=2 at every level we tested,
  and its advantage grows with the error rate.
- Under AWGN, K=1 loses ~2 % at 0 dB. In those trials the first preamble symbol fails its 3 dB
  gate, and only a second copy helps. Below −2 dB FEC is far better (erasure decoding).
- **Dropouts longer than the preamble** (≳ 30 ms) are where repetition still wins. FEC K=1 cannot
  recover a lost preamble. A dropout that falls inside the body and covers at most 8 symbols
  (~45 ms) is fully recovered.
- p = 2 (RS(9,7)) saves only 26 ms of airtime and is clearly weaker. p = 4 is the better trade.
- FEC with K=2 is the most robust option measured. It costs 351 ms of airtime, so it is an option for
  cue-only links (MSC) where latency doesn't matter.

## 6. Recommendation

1. Adopt **RS(11,7) × 2 over GF(16), with erasure decoding and K = 1**, as a new profile
   ("Robust-FEC", or as the next Robust after a field trial). It cuts airtime by 36 % and loaded
   NoteOn latency by ~25–30 %. It is at least as robust as today's K=2 against symbol errors and
   deep noise. It is slightly worse at the AWGN edge and for long dropouts.
2. Keep K=2 + FEC as an option for latency-insensitive cue links.
3. ~~Before replacing Robust, harden the first preamble symbol.~~ Done: joint preamble detection
   (§10). The residual K=1 weakness is now dropouts longer than the body's erasure capacity (≳ 50 ms).
4. Do not touch Fast yet. The same layer works there (`fast` + FEC: 72 ms vs 54 ms per message),
   but Fast exists for clean chains, where FEC buys little.

## 7. Reuse by other modulations (LTC/BMC branch)

`FecCodec` is modulation-agnostic. It takes packet nibbles and returns coded nibbles
(`encode_body`), and it takes `BodySymbol { nibble, reliability, erased }` and returns packet nibbles
(`decode_body`). A biphase-mark mode can transmit the 22 coded nibbles as 88 bits. It can mark a
nibble erased when any of its 4 bit cells is ambiguous (e.g. a transition missing or out of place),
and use the weakest cell margin as `reliability`. For bursty bit errors, the BMC mode should keep
each nibble's 4 bits adjacent, so that one burst damages few symbols. The codeword alternation
already spreads bursts across both codewords. Nothing in `rs16.rs`/`fec.rs` depends on MFSK.

This is now implemented as the experimental `ltc_fec` profiles. The BMC receiver uses the weakest bit
margin as `reliability` and erases nibbles that contain a dropout. Marking ambiguous nibbles as erased
did not help: the reliability-ordered candidates already cover them. See `docs/proposals/ltc-bmc.md` §9.

## 8. Open questions

- **False lock swallowing a real packet.** After a false lock whose body completes (≤ 8 erasures)
  but fails RS/CRC, the phy does not rewind, so a real packet that starts inside it is lost. This
  already happens with uncoded bodies. Tolerating erasures and preamble misses makes it somewhat more
  likely, though the stuck-note run shows a net gain. The fix is a "decode failed → rewind" hook
  from the modem to the phy.
- **Miscorrection budget.** Analysed, measured and bounded in §9.
- **Variable body length.** NoteOn needs only 12 data nibbles, so 2 × RS(10,6) = 20 symbols would
  save 13 ms. It would require the type to be known early, e.g. a separately protected first codeword.
- **Parity per profile.** Is 4 right for real converter chains? A field trial with
  `fec_corrected` / CRC stats on real Dante/MADI/analog paths should decide it.
- **Should the preamble-miss rule be part of the FEC mode or a separate phy knob?** In the prototype
  it is tied to `FecMode` to keep `PhyConfig` changes minimal.

## 9. False accepts: analysis, target, measures

A false message needs three things: a false lock whose body passes the phy gates (a **false body**), a
pair of RS decodings of it (a **candidate**: both decoders report success, the pre-CRC event), and a
candidate that passes every packet check. The erasure-tolerant decoder makes the second step easy, so
the question is how often the third one happens. The soft-decision LTC receiver made it urgent: back-to-back
MFSK audio into an `ltc_fec` receiver produces ~110 false bodies per minute (`ltc2400_fec`: ~310),
where the old receiver produced none.

### 9.1 Analytic model

(This is the AML v1.0 message set; §9.7 redoes the count for v1.1, which carries all MIDI channels and full MSC.)

Treat a candidate as a uniformly random 14-nibble data block. It must have version `1` (4 bits), a
known type (6 of 16), zero bytes after the packet (pad) and, new, a payload within the ranges of spec §5
(MIDI data bytes < 0x80; MscLite `cmd` 0–3, `cuelist` 0–15, `cue` 0–127). Then the CRC-16.

| type | pad bits | range bits | pass, v1 checks | pass, + range checks |
|---|---|---|---|---|
| NoteOn | 8 | 2 | 2⁻⁸ | 2⁻¹⁰ |
| NoteOff, ProgramChange | 16 | 1 | 2⁻¹⁶ | 2⁻¹⁷ |
| MscLite | 0 | 11 | 1 | 2⁻¹¹ |
| Heartbeat, Panic | 24 | 0 | 2⁻²⁴ | 2⁻²⁴ |

Per candidate, structure (version, type, pad, ranges) passes with probability 2⁻⁸ × Σ = **3.9 × 10⁻³**
with the v1 checks (8.0 bits: MscLite has no pad and dominated) and **5.8 × 10⁻⁶** with the range
checks (17.4 bits). With the CRC (2⁻¹⁶) a candidate becomes a message with probability
**6.0 × 10⁻⁸** (24 bits) before and **8.8 × 10⁻¹¹** (33.4 bits) after. Of the residual false messages
66 % would be NoteOn and 33 % MscLite (8 % MSC GO).

Candidates per false body: per codeword the decoder tries the gate erasures plus 0–4 extra erasures on
the weakest symbols and errors-only, so at most 5 distinct decodings, at most **25 pairs** (not 36 as
§8 and `ltc-bmc.md` §9.6 first estimated). A random word lies within errors-only radius 2 of an RS(11,7)
codeword with probability 12541/16⁴ = 0.19, and the 4-erasure attempt always "succeeds" (it has no
redundancy left to fail), so every random body offers candidates. Simulated (`random_bodies`, uniformly
random nibbles and reliabilities):

| gate erasures in the body | 0 | 2 | 4 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|
| candidates per body, before | 2.35 | 2.34 | 2.02 | 1.64 | 1.52 | 1.45 |
| candidates per body, after (§9.3) | 0.04 | 0.08 | 0.18 | 0.31 | 0.85 | 1.02 |

Per false body: before 2.35 × 6.0 × 10⁻⁸ ≈ **1.4 × 10⁻⁷**; after 0.04–1.02 × 8.8 × 10⁻¹¹ ≈
**3.5 × 10⁻¹² – 9 × 10⁻¹¹**, 1,500–40,000 times less.

### 9.2 Target

**At most one false message per 10 years of continuous worst-case audio, per receiver.** Ten years is
about the service life of a show rig, and continuous worst-case audio is pessimistic (a receiver's input
is mostly Cricket traffic or silence), so a rig should never see one. A false NoteOn is audible but
recoverable (NoteOff refreshes, link-dead); a false MSC GO fires a cue, which is worse, and is 8 % of the
residual, i.e. ≥ 125 years at the target. "Worst-case audio" covers what a receiver can plausibly be fed:
other Cricket profiles (MFSK traffic, and a transmitter on the same phy without FEC, i.e. a
misconfigured end), white noise, clicks, music, speech and SMPTE timecode.

### 9.3 Measures

1. **Payload range checks** (`MessageType::payload_in_range`, used by `link::fec_candidate_is_valid`):
   ×680 on random candidates, free (a transmitter never sends out-of-range values). Applied to FEC
   candidates only; uncoded AML v1 decoding is unchanged.
2. **Body-level errata-weight bound.** A candidate pair is tried only if `Σ(2e + f) ≤ 2 × parity` (8) —
   `e` = changed symbols that passed the gate, `f` = gate erasures, counted over the whole body however
   GMD found the codeword — or if it changes at most one symbol besides the gate erasures (a dropout
   plus one weak symbol). Pairs are tried lightest first. GMD can still fix an uneven split (3 errors in
   one codeword, ≤ 1 in the other) but no longer offers codewords far beyond the code's radius, which are
   what random bodies decode to. Candidates per random body 2.35 → 0.04; per MFSK false body 1.7–1.9
   → 0.4–0.5.
3. **Whitening.** The coded body is XORed with a fixed sequence (`fec::WHITENING`), making it a coset
   of the code. Without it, an uncoded body on the same phy (an `ltc` transmitter into an `ltc_fec`
   receiver, or Robust into `robust_fec`) lines up with the checks: its first nibble is the version, its
   pad nibbles are zero, its payload is in range: a third of its candidates had a valid header and 1 in 19
   passed every check but the CRC (random: 1 in 255), one false message every 2–22 hours of traffic
   (table below). The first mask
   nibble is `0x8`, so the version nibble goes on air as `1001`; with `0x2` (`0011`) `ltc2400_fec`
   behind a 1 kHz high-pass lost 6 points (the bits right after the BMC sync matter there), with `0x8`
   it gains 2.

Considered and not chosen: a tighter bound (`Σ ≤ 7` or `≤ 6`, another 10–100× fewer candidates, but
−1 to −3 points at SNR ≤ 0 dB and under clicks, where the GMD wins live); the bound without the
one-error exception (−7.5 points for `ltc9600_fec` with a 3 ms dropout); skipping the GMD search on
low-quality bodies (the bound already removes what matters; true GMD wins are within it); stricter
phy gates for bodies that need correction (MFSK false bodies have the same mean reliability as true
bodies at 0 dB SNR, so there is no clean separation).

Cost: per body the same ≤ 10 RS decodes as before, plus an 11-symbol compare per candidate, a sort of
≤ 25 pairs and 22 XORs; fewer CRC checks. `no_std` + `alloc` unchanged.

### 9.4 Measurements

`cricket_core/tests/fec_false_accept.rs`, `false_accept_report` (10 minutes of each signal per profile,
`FALSE_SECONDS=600`). It runs the modem's receive flow, counts false bodies, candidates (pre-CRC) and
candidates passing the structure, and estimates years per false message as
`1 / (max(observed structural passes, candidates × P(structure)) × 2⁻¹⁶)`. Signals: back-to-back MFSK
traffic (Robust, Fast, Robust FEC at SNR 20 dB), white noise (σ = 0.003, 0.05, 0.5), clicks (100/s and
1000/s, 1–4 samples, random level and sign), music-like (1–3 voices of harmonic and square-ish tones with
envelopes, drum hits, pink noise), speech-like (pitch-varying pulse train through three formant
resonators, syllable envelope, pauses, fricative noise), SMPTE LTC at 25 and 30 fps, and transmitters
without FEC on the same phy (uncoded LTC at the profile's rate into the LTC FEC profiles; AML v1 Robust
and Fast into `robust_fec`). "Before" is main at 7bcc900 (the same harness with that decoder).

| profile | signal | false bodies / min | candidates / min, before → after | years per false message, before → after |
|---|---|---|---|---|
| ltc_fec | MFSK traffic | 110 | 207 → 43 | 0.15 (1,300 h) → **510** |
| ltc_fec | uncoded LTC (misconfigured TX) | 3,240 | 5,815 → 124 | 0.0004 (3.5 h) → **175** |
| ltc2400_fec | MFSK traffic | 312 → 237¹ | 522 → 116 | 0.061 (530 h) → **185** |
| ltc2400_fec | music-like | 19 | 42 → 0 | 0.75 → > 70,000² |
| ltc2400_fec | speech-like | 16 | 31 → 4 | 1.0 → 5,400 |
| ltc2400_fec | uncoded LTC (misconfigured TX) | 1,620 | 2,950 → 82 | 0.0008 (7 h) → **260** |
| ltc9600_fec | uncoded LTC (misconfigured TX) | 6,470 | 11,600 → 241 | 0.0002 (1.7 h) → **89** |
| robust_fec | AML v1 Robust / Fast traffic | 441 | 1,014 → 4.6 | 0.0025 (22 h) → **4,700** |

¹ The MFSK signal contains `robust_fec` bursts, whose whitened bodies differ. ² No candidate in 10 min.

No false body at all in 10 minutes (before or after) for white noise, clicks and SMPTE LTC on every
profile, music and speech on `ltc_fec` and `ltc9600_fec`, MFSK on `ltc9600_fec`, and every signal except
AML v1 traffic on `robust_fec`. Even at the 95 % bound for zero (3 bodies per 10 minutes, each with
1.02 candidates) that is > 70,000 years. No false message was observed in any run.

The physical ceiling is one false body per body length (the misconfigured-transmitter rows are close
to it). If every slot held a body with 8 erasures, the rate would be one false message per ~6.5 years at
4800 bit/s (13 at 2400, 3 at 9600). No natural signal comes near that: it needs a valid sync and a
dropout in every body.

**Result: the worst measured case is 185 years for natural audio (MFSK traffic into `ltc2400_fec`) and
89 years for a misconfigured transmitter (`ltc` into `ltc9600_fec`), 18× and 9× better than the
target; before, it was 530 hours and 1.7 hours.**

### 9.5 Regression tests

Fast (`cargo test`): `link::fec` unit tests check that a body GMD could decode beyond the bound is not
offered to the CRC, that a dropout plus one weak symbol still decodes and two do not, and that the
whitening is applied and required; `link::packet` checks the payload, version, SEQ and pad rejection;
`fec_false_accept.rs` checks that random bodies (0/4/8 erasures) never decode and offer < 0.1 / 0.35 /
1.3 candidates per body, and that MFSK traffic into `ltc2400_fec`, uncoded LTC into `ltc_fec` and AML v1
traffic into `robust_fec` never decode and stay below 0.9 / 0.15 / 0.15 candidates per false body (the
old decoder: 2.35 per random body, 1.8 per MFSK body). The full measurement is the ignored
`false_accept_report`:

```sh
FALSE_SECONDS=600 cargo test --release -p cricket_core --test fec_false_accept -- --ignored --nocapture
```

### 9.6 Delivery before and after

Same harnesses and seeds as §4 and `ltc-bmc.md` §9.3, 1000 trials per cell. "Before" is main at 7bcc900.

| channel | ltc_fec | ltc2400_fec | ltc9600_fec |
|---|---|---|---|
| SNR 4 dB | 99.8 → 99.9 | 100 → 100 | 97.3 → 97.3 |
| SNR 2 dB | 99.4 → 99.5 | 100 → 100 | 86.1 → 84.0 |
| SNR 1 dB | 98.9 → 98.8 | 99.8 → 99.9 | 63.2 → 60.2 |
| SNR 0 dB | 96.2 → 96.3 | 99.7 → 99.8 | 28.6 → 26.1 |
| clicks 200/s @ +6 dB | 99.4 → 98.9 | 99.8 → 99.8 | 92.8 → 91.8 |
| clicks 100/s @ +12 dB | 92.9 → 92.4 | 98.5 → 98.1 | 91.5 → 91.8 |
| dropout 1 ms | 95.8 → 95.6 | 100 → 100 | 91.9 → 91.9 |
| dropout 3 ms | 92.3 → 92.2 | 92.6 → 92.6 | 62.3 → 64.7 |
| dropout 10 ms | 0 → 0 | 82.2 → 81.5 | 0 → 0 |
| EQ +12 dB @ 3 kHz | 2.6 → 55.2 | 100 → 100 | 27.7 → 27.6 |
| EQ −12 dB @ 3 kHz | 99.6 → 98.7 | 99.8 → 99.8 | 100 → 100 |
| HP 1000 Hz | 100 → 100 | 79.3 → 81.5 | 100 → 100 |
| drift +1 % + SNR 6 | 100 → 100 | 100 → 100 | 97.8 → 96.9 |

All other LTC cells moved by ≤ 0.6 points. `robust_fec` (`fec p4 K1` in §4, 1000 trials): AWGN
0 / −2 / −3 / −4 dB 98.3 / 92.0 / 75.3 / 17.8 → 98.4 / 92.2 / 75.6 / 16.7; wrong tone 1 / 2 / 5 / 10 %
98.8 / 98.1 / 91.1 / 74.9 → 98.8 / 98.1 / 90.9 / 74.9; dropout 10 / 30 / 60 ms 96.2 / 95.3 / 70.5 →
96.2 / 95.3 / 70.7; the rest unchanged.

The key cells (SNR, clicks, dropouts on `ltc_fec`, `ltc2400_fec`, `robust_fec`) are within 0.5 points,
except `ltc2400_fec` with a 10 ms dropout (−0.7) and `robust_fec` at −4 dB (−1.1). The trade-offs: `ltc9600_fec` at SNR ≤ 2 dB loses 2–3 points (GMD
wins far beyond the code's radius, in a region where that profile is not usable anyway), and the
whitening changes the bit patterns on air, which moves pattern-sensitive cells both ways (`ltc_fec`
behind +12 dB of EQ at 3 kHz: +53; −12 dB: −0.9).

### 9.7 Message set of AML v1.1: all MIDI channels and MSC

AML v1.1 (spec §5) carries every MIDI 1.0 short message on all 16 channels (one `Midi` type whose
payload is the status byte and the data bytes) and MIDI Show Control with cue numbers 0–999.9 and
cue lists 1–15 (`Msc`). That is more information per packet than the six v1.0 types, and every valid
encoding is one more block a random candidate can match: the structure check (§9.1) must still leave
enough redundancy. Counted over all byte values (`canonical_payloads` in `fec_false_accept.rs`):

| type | canonical payloads (of 2²⁴) |
|---|---|
| Midi: 80 three-byte channel statuses × 2¹⁴ (minus NoteOn velocity 0), 32 two-byte × 2⁷, Song Position 2¹⁴, Song Select 2⁷, Tune Request 1 | 1,329,281 |
| Msc: 5 cue commands × (1 + 10,000 cues × 16 list values), LOAD 160,000, FIRE 128, 3 without data, 11 list commands 174 | 960,310 |
| Heartbeat, Panic (24 pad bits each) | 1 + 1 |

Carrying the channel alone costs 4 bits, so at the v1.0 header layout the structure would have
dropped from 17.4 to 10.9 bits (90× more false accepts). The SEQ byte pays for it: SEQ is now a 2-bit
counter and its six upper bits are reserved zero (spec §4.1). Duplicate suppression only needs message
`n` and `n + 4` to differ, and they are always more than a dedup window apart (the window is shorter
than two messages). Structure per random candidate: version 2⁻⁴ × SEQ 2⁻⁶ × Σ over types of
2⁻⁴ × (canonical payloads / 2²⁴) = **8.3 × 10⁻⁶ = 2⁻¹⁶·⁸⁷** (v1.0: 5.8 × 10⁻⁶ = 2⁻¹⁷·⁴), i.e.
**1.44× more** false accepts per candidate. The simulated random bodies agree (7 / 8 gate erasures:
5.9 / 9.8 × 10⁻⁶ per candidate). Of the residual false messages 58 % would be MIDI and 42 % MSC; a false
MSC GO is 7 % of them, 1.22× as likely per candidate as with v1.0 MscLite.

Measured (`false_accept_report`, `FALSE_SECONDS=120`, same signals as §9.4):

| profile | signal | candidates / min | years per false message |
|---|---|---|---|
| ltc_fec | MFSK traffic | 37 | 404 |
| ltc_fec | uncoded LTC (misconfigured TX) | 123 | 122 |
| ltc2400_fec | MFSK traffic | 77 | **196** |
| ltc2400_fec | music-like / speech-like | 2.0 / 1.5 | 7,500 / 10,000 |
| ltc2400_fec | uncoded LTC (misconfigured TX) | 89 | 169 |
| ltc9600_fec | uncoded LTC (misconfigured TX) | 241 | **62** |
| robust_fec | AML v1 Robust / Fast traffic | 58¹ | 260 |

¹ With the joint preamble acquisition of §10 (1,040 false bodies / min); 4 candidates / min before it.

No candidate passed the structure check and no false message was observed; white noise, clicks and
SMPTE LTC produced no candidate on any profile. The worst cases are 196 years for natural audio and
62 years for a misconfigured transmitter, still 20× and 6× better than the 10-year target. Keeping 17.4
bits would have meant cue lists 1–3 or no cue decimals, or a 1-bit SEQ whose dedup margin is half a
repetition; the 0.5 bit was judged the better trade.

## 10. Acquisition hardening

With a single copy, a packet whose preamble is missed is lost, and FEC cannot help. Before this change
`robust_fec` used the AML v1 acquisition: gate on `preamble[0]` alone (purity ≥ 0.25 **and** ≥ 3 dB over
the runner-up tone), fine-align on that one tone, then confirm the rest in order (with one tolerated miss
after the first). That rule caused most of the cells where `robust_fec` lost to `robust`. At 0 dB the
first symbol often fails its 3 dB confidence gate: adjacent tones are 100 Hz apart and leak about
−6 dB into each other's bins, so noise easily closes the gap. A 10 ms dropout on symbol 0 also costs the
packet.

### 10.1 Joint preamble detection (`Acquisition::Joint`, receiver only)

`PhyConfig::acquisition` selects the receiver's preamble search. `robust_fec` now uses
`Acquisition::Joint { misses: 2 }`. `robust` and `fast` keep `FirstSymbol`, which is exactly the
AML v1 §3.2 behaviour. The wire format does not change: same preamble `A 5 C 3`, same timing.

At each search hop (`symbol/4`) at position `t`:

1. **Candidate (cheap).** One Goertzel plus the window energy for each preamble window
   `t + i·period`, at that window's expected tone only. It is a candidate when at most `misses` windows
   have expected-tone purity < `min_tone_purity`, and the purities sum to at least
   `0.8 · N · min_tone_purity`. The loop stops at the first excess miss, so silence and noise usually
   cost 3 single-tone passes per hop, against the 16-tone bank used before.
2. **Fine alignment** maximizes the expected-tone power **summed over all N windows**, over
   `[t − hop, t + symbol]`, using the same grid and refinement as today. A weak or muted symbol no
   longer decides the symbol clock.
3. **Accept** runs the full 16-tone bank on each aligned window. A symbol is good when it shows the
   expected nibble, has purity ≥ the gate and confidence ≥ `0.5 · min_confidence_db`. The lock needs
   at most `misses` (2) failed symbols, **anywhere** including the first. At most one of those may be
   *contradicting*, meaning another tone wins it with ≥ half the gate purity. Noise or a dropout can
   therefore take out two symbols, but a stream of unrelated tones still has to match three preamble
   symbols in sequence.
4. On a lock, the receiver goes straight to Body (no ConfirmPreamble). A failed body (erasure cap,
   CRC/FEC reject) rewinds to `t + hop`, as before. Delivery time does not change: the decision needs
   no samples beyond the first body symbol.

`FirstSymbol` also gets an **exact** shortcut. The gate requires the `preamble[0]` tone to win with
purity ≥ the gate, so the receiver first computes that single tone's purity, with bit-identical power and
energy (`goertzel_power_energy`). It skips the 16-tone bank when that purity is below the gate.
Measured results for `robust` and `fast` are identical to before in every cell (same seeds), and the
search costs about 15× less CPU.

### 10.2 Results

`cargo test --release -p cricket_core --test fec acquisition_ab_table -- --ignored --nocapture`,
`FEC_AB_TRIALS=1000`, same seeds for every column. "before" = `robust_fec@first` (the previous
acquisition). The `robust@joint1` column is for information only; see §10.4. Delivery %:

| channel | robust (K2) | robust_fec before | **robust_fec after** | fast | robust@joint1 |
|---|---|---|---|---|---|
| clean | 100 | 100 | **100** | 100 | 100 |
| AWGN 2 dB | 100 | 99.4 | **100** | 99.3 | 100 |
| AWGN 0 dB | 100 | 99.0 | **100** | 92.6 | 100 |
| AWGN −2 dB | 97.7 | 92.1 | **100** | 5.5 | 99.9 |
| AWGN −3 dB | 32.8 | 75.6 | **97.0** | 0 | 45.7 |
| AWGN −4 dB | 0 | 17.3 | **39.6** | 0 | 0.1 |
| wrong tone 1 % | 98.1 | 99.2 | **100** | 87.7 | 99.0 |
| wrong tone 2 % | 90.9 | 97.6 | **99.6** | 73.6 | 94.8 |
| wrong tone 5 % | 65.4 | 93.8 | **97.0** | 45.8 | 75.3 |
| wrong tone 10 % | 31.4 | 72.1 | **79.3** | 21.4 | 42.6 |
| dropout 10 ms | 100 | 95.7 | **99.8** | 81.3 | 100 |
| dropout 30 ms | 95.7 | 96.1 | **97.2** | 81.5 | 97.8 |
| dropout 60 ms | **90.1** | 70.7 | 72.2 | 80.0 | 91.9 |
| gain −40 dB, SNR 0 | 99.9 | 99.0 | **100** | 92.0 | 100 |
| LP 2.5 kHz, SNR 6 | 100 | 98.9 | **100** | 85.9 | 100 |
| drift 500 ppm, SNR 0 | 100 | 97.8 | **100** | 91.8 | 100 |

The §4 harness (`fec_ab_table`, 400 trials, the same seeds as the §4 table) gives the same picture for
`fec p4 K1` = `robust_fec`. Before → after:
0 dB 97.8 → 100, −2 dB 93.8 → 100, −3 dB 77.2 → 98.8, −4 dB 18.2 → 42.8;
wrong tone 1/2/5/10 % 99.0/98.5/88.5/73.0 → 100/99.8/94.5/78.8;
dropout 10/30/60 ms 95.8/96.2/70.2 → 99.8/98.5/71.8; gain −40 dB 97.5 → 100; LP 99.0 → 100; drift 98.8 → 100.
These "after" figures include the false-accept measures of §9.
(`fec_parity()` now also uses the joint acquisition, so the p2 and K2 columns there moved as well.)
No cell in either table produced a false message.

**Where the 60 ms dropout losses come from** (1000 trials). The split is by where the dropout starts:
in the first copy's preamble, or in its body.

| | robust | robust_fec before | robust_fec after |
|---|---|---|---|
| dropout 10 ms | 0 / 0 | 43 / 0 | 2 / 0 |
| dropout 30 ms | 0 / 43 | 39 / 0 | 28 / 0 |
| dropout 60 ms | 0 / 99 | 61 / 232 | 46 / 232 |

The body losses at 60 ms are a hard limit of a single 22-symbol RS(11,7)×2 copy, not an
acquisition problem. 60 ms is 9.2 symbol periods, so a dropout inside the body erases ≥ 9 symbols.
The 14 data nibbles cannot be recovered from the ≤ 13 symbols left. Even a perfect acquisition would
reach only ~77 %, against Robust's 90 %.

**False triggers and CPU.** `false_trigger_report` measures phy locks that reach the body, per minute,
on 60 s each of white noise, music-like audio and random MFSK symbols with no preamble (that is, other
packets' bodies). It also times receiver CPU in host ns per input sample (best of 3, release build;
"before" is the full 16-tone bank at every hop):

| profile | locks/min noise / music / random tones | ns/sample noise | music | random tones | 200 packets @ 0 dB (delivered) |
|---|---|---|---|---|---|
| robust before | 0 / 0 / 0 | 224 | 217 | 249 | 68 (200/200) |
| **robust after** | 0 / 0 / 0 | **14.3** | **14.6** | **64** | **53** (200/200) |
| robust_fec before | 0 / 0 / 2 | 221 | 211 | 242 | 64 (194/200) |
| **robust_fec after** | 0 / 0 / 13 | **44** | **39** | **156** | **60** (200/200) |
| fast before | 0 / 0 / 225 | 204 | 198 | 256 | 69 (194/200) |
| **fast after** | 0 / 0 / 225 | **14.6** | **15.9** | **70** | **53** (194/200) |

No packet-free input produced a message. The existing false-alarm tests (noise, random tone streams
with sprinkled preambles, `stuck_notes::no_false_messages`) pass. On random tone streams,
`robust_fec` locks 13 times a minute instead of 2. Each lock is one rejected body decode, which counts
toward the false-accept budget (§9). The worst-case CPU of every profile is now below the old
common case (silence).

**Engine latency** (`cricket loopback --profiles robust,robust_fec,fast --notes 60`): robust 360 ms
mean / 503 ms p95; robust_fec 267 / 340 (with first-symbol acquisition: 266 / 344); fast 81 / 120. Acquisition
does not move the delivery point.

**Stuck-note run** (`tests/stuck_notes.rs`, back-to-back traffic, wrong tones). Messages lost,
before → after for robust_fec: 0.5 % 6/918 → **0/918**, 1 % 10/1046 → **0/1046**, 2 % 26/1048 →
**5/1048** (robust, unchanged: 2/788, 13/819, 47/752). No false messages, no stuck notes.

### 10.3 Options measured and rejected

- **Three tolerated misses** (`Joint { misses: 3 }`): one matching symbol is then enough to lock, and
  music-like audio produced 1105 locks/min. Rejected.
- **Contradiction rule.** The first version counted a failed symbol as contradicting only when another
  tone won with the full 3 dB confidence. Windows straddling two symbols of an unrelated tone stream
  then passed as "weak" misses: 122 locks/min on random tones. The purity-based rule (another tone
  with ≥ half the gate purity) gives 13/min, with identical delivery in every cell. Its threshold
  between 0.5× and 0.8× the gate purity made no difference.
- **Candidate sum threshold.** At 0.8 × gate purity × N, CPU on random tones drops from 255 to ~170 ns/sample
  (156 in the final build) at no delivery cost. At 1.0× AWGN −3/−4 dB fall to 94.8/27.0 %, and at 1.2× to 39.8/2.5 %.
- **Longer preamble for robust_fec** (6 symbols `A 5 C 3 8 E`, +13 ms per message, harness name
  `robust_fec+pre6`). With 2 misses: −3 dB 88.8 %, −4 dB 19.8 %, wrong tone 10 % 76.8 %, dropout 30 ms
  94.0 %. With 3 misses: 97.4 / 36.4 / 76.8 / 96.3 %. Both are no better than 4 symbols with 2 misses
  (97.0 / 38.5 / 80.4 / 97.2 % in the same run) and cost 13 ms. The only gain is 0 instead of 13 random-tone locks/min.
  Rejected.
- **Mid-body resync marker or postamble.** After joint detection, preamble-caused losses are 0.2 %
  (10 ms), 2.8 % (30 ms) and 4.6 % (60 ms). At 30 ms robust_fec already beats robust. At 60 ms the
  body limit above caps it at ~77 % whatever the sync does. A marker costs ≥ 1–2 symbols (6.5–13 ms,
  +4–7 % airtime) for at most a few percent in one cell. Not worth it.

### 10.4 Robust and Fast

Both keep the AML v1 acquisition. Their decoding results are bit-for-bit unchanged; they gain only
the exact CPU shortcut. Joint detection would also help Robust. With `Joint { misses: 1 }`
(`robust@joint1` above): −3 dB 32.8 → 45.7 %, wrong tone 2/5/10 % 90.9/65.4/31.4 → 94.8/75.3/42.6 %,
dropout 30/60 ms 95.7/90.1 → 97.8/91.9 %, with no cell worse. The costs are 10 locks/min on random tones
and 29 ns/sample in search. That is a change to the receiver procedure in spec §3.2, so it is left to
a spec decision. It is not useful for Fast: with a 2-symbol preamble, one tolerated miss means one
matching tone locks, which gave 9700 locks/min on random tones.

### 10.5 Verdict

`robust_fec` now matches or beats `robust` in every measured cell except **long dropouts (60 ms:
72 % vs 90 %)**. That gap is the single-copy body limit, not acquisition. At dropout 10 ms it is
99.8 % vs 100 % (2 losses in 1000, within sampling noise). Everywhere else it is better, often by a
wide margin: −3 dB 97 % vs 33 %, wrong tone 5 % 97 % vs 65 %. It does this with 36 % less airtime and
~25 % lower loaded latency. If links with ≥ 50 ms dropouts matter, Robust (K = 2) or FEC with K = 2 is
still the safer choice.
