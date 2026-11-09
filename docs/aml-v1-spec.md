# AML v1 — MIDI over audio (bitstream modem)

| | |
|---|---|
| **Protocol** | AML (AudioMIDI Link) version **1**: header version nibble `1` (§4.1) |
| **Document** | AML v1 — revision **r5**, 2026-11-09 |
| **Status** | Pre-release draft. §3–§5 and §6.1–§6.4 are complete and tested in software; §6.5 is **provisional**. Nothing has been proven on a real show chain yet (see §7). The first tagged release of the reference implementation (Cricket **v0.2.0**) freezes the wire format of the stable sections under the policy below. |
| **Reference implementation** | Cricket (`cricket_core`), <https://github.com/paksnukk/Cricket>; test vectors in §9 are asserted by its test suite |
| **Informative material** | `docs/proposals/fec.md`, `docs/proposals/ltc-bmc.md` (adopted), `docs/proposals/digital-transparent.md` (experimental, not part of v1) |

### Revision log

| Revision | Date | Changes |
|---|---|---|
| r1 | 2026-10-14 | First text under the name AML v1 (the project's "V2" bitstream-modem draft of 2026-07-29, with level-independent detection, rewind-and-resync, single link-dead authority, windowed duplicate suppression and the TX scheduler folded in). MFSK-16 phy, packet format, Robust / Robust repeat / Fast, fail-safe, observability. |
| r2 | 2026-10-15 | **Wire changes:** body FEC (Reed–Solomon over GF(16), whitening) and the packet validation rules (§4.6–§4.7); joint preamble acquisition (§3.2.2); all MIDI 1.0 channel messages and full MSC (§5.1–§5.4); `SEQ` as a 2-bit counter; controller coalescing and scheduler priorities (§5.5); Robust becomes the coded single-copy profile, Robust repeat the two-copy one (§6); LTC biphase-mark profiles as provisional (§6.5); test vectors (§9). |
| r3 | 2026-11-09 | This header (status, revision log, errata, versioning policy); §7 renamed from "Release assumptions (v1.0)" to "Release assumptions"; title aligned with the README. Same day: the transmit amplitude becomes a SHOULD of 0.25 (−12 dBFS peak, was 0.8; §3.1, §6, §6.5.2), the §5.5 queue bounds are rewritten (1 s airtime cap on order-3 messages; the "bounded by coalescing" claim was wrong), an unknown profile name is rejected, and the user-facing profile names are listed (informative). No wire-format change: receivers are level-independent and the packet format is untouched. |
| r4 | 2026-11-09 | **Wire change: orthogonal tone spacing.** The MFSK tone step becomes 1 / symbol duration (§3.1): Robust and Robust repeat 166⅔ Hz (tones 2500 … 5000 Hz, was 100 Hz / 2500 … 4000 Hz), Fast 333⅓ Hz (2500 … 7500 Hz, was 200 Hz / 2500 … 5500 Hz); the line bandwidth each profile needs is now a MUST (§6). Receiver (SHOULD, §3.2): tone-band input filter, purity gate 0.20 for every profile (was 0.25 / 0.30), the joint-acquisition contradiction bar at the gate purity, uncoded bodies keep the best guess instead of aborting, body symbol clock tracking. §9 tone frequencies updated. Both ends must run r4: an r3 receiver hears nothing from an r4 transmitter and vice versa. |
| r5 | 2026-11-09 | **Wire change (policy rule 1):** the CRC-16 init value is the **link key** (§4.1); the default key `0xFFFF` is the CRC-16/CCITT-FALSE init, so an unkeyed link is bit-identical to r3 and interoperates with it. §4.7 adds the wrong-key count (informative). Fail-safe: timers per profile (§5.3, §6 table, §6.5; shorter from 9600 bit/s and on `digital`), the burst releases the receiver's sounding notes explicitly and MAY be disabled for cue-only links, a suppressed duplicate re-arms link-dead (§5.3). MSC: the transmitter MAY filter by device ID / command format for multi-device rigs (§5.4). Scheduler (§5.5, TX policy, no wire effect): MSC ahead of Note Off; the Note On limit per profile with drop-newest; Note Off repeats on a Fibonacci schedule after a quiet line (coded profiles); controller state refreshed in Heartbeat slots; §5.5 "at most 4 Note Ons" statement replaced. |

Revisions are numbered r1, r2, … per document; a revision id and date identify the text an
implementation or a measurement refers to. The crate version of the reference implementation is
independent of the revision number.

### Versioning policy

1. **Before the first tagged release** (Cricket v0.2.0) wire-affecting changes may land with a
   revision bump only; the version nibble stays `1`.
2. **After the first tagged release**, any wire-incompatible change to a stable section (§3–§5,
   §6.1–§6.4) bumps the header version nibble (§4.1) and starts a new document (AML v2); the v1
   document is then frozen except for errata. A receiver MUST NOT deliver packets whose version
   nibble it does not implement (§4.7).
3. **Provisional and experimental sections** (today §6.5; the `ltc14400*` and `digital` profiles are
   not in the spec) may change with a spec revision and a **minor** bump of the crates. Such a change
   is a wire change for those profiles only; both ends must run the same revision.
4. **Errata are appended** to the *Errata* list below with a date and the affected section, never
   silently edited into the text. The next revision that folds an erratum into the text references
   it in the revision log.
5. A revision that changes no wire behaviour (editorial, clarifications, measurements) does not
   require a crate release.

### Errata

None yet. Format: `E1 (YYYY-MM-DD) — §x.y: what the text said, what it must say, which revision
introduced it, which revision fixes it.`

---

## 1. Overview

AML (AudioMIDI Link) is the protocol; Cricket is its reference implementation.

AML v1 transports MIDI 1.0 short messages (all channels) and MIDI Show Control over an audio path. A
message is a small packet (header, sequence number, payload, CRC-16) sent as a burst of audio, with
fail-safe link monitoring on the receiving end. The packet format (§4.1) and the transport layer (§5)
are the same for every **profile**; a profile fixes the physical layer and the body coding (§6):

| Profile | Physical layer | Body | Copies | Status |
|---|---|---|---|---|
| **Robust** (default) | MFSK-16, 6.5 ms per symbol (§3) | Reed–Solomon coded, whitened (§4.6) | 1 | stable |
| Robust repeat | MFSK-16, 6.5 ms per symbol | uncoded, interleaved (§4.3) | 2 | stable |
| Fast | MFSK-16, 3 ms per symbol | uncoded, interleaved | 1 | stable |
| LTC family | biphase-mark code at 2400 / 4800 / 9600 bit/s (§6.5) | uncoded, or coded (`_fec`) | 1 | **provisional** |

Profiles are not signalled on the wire: both ends MUST be configured to the same profile. A receiver
configured for one profile never decodes another profile's audio (the bodies differ in length, coding
or modulation, and every packet is validated, §4.7).

AML v1 is the first official protocol version. The earlier prototype phy (semantic tone symbols, see
`docs/history/aml-prototype-protocol.md`) remains available as a legacy debug phy only; it is not part of AML v1.

## 2. Layering and conventions

| Layer | Responsibility |
|-------|----------------|
| Transport | Message types, MIDI and MSC mapping, TX scheduling, heartbeat / fail-safe policy |
| Link | Packet header, sequence numbers, CRC-16, body coding (interleave or Reed–Solomon FEC + whitening), repetition, validation |
| Physical | MFSK-16 tones or biphase-mark code, preamble/sync, symbol or bit decisions |

Audio I/O always uses **channel 0 only** (mono). Extra interleaved channels are ignored on input and
filled with the mono signal on output. Rates and durations are given at the nominal **48 000 Hz**;
an implementation running at another sample rate keeps the durations (sample counts are
`round(duration × rate)`) and the frequencies.

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT** and **MAY** are used as in RFC 2119.
The rule of thumb: everything a transmitter puts on the wire, and every check that decides whether a
receiver *delivers* a packet (§4.7), is **MUST**: it decides interoperability and the false-message
rate. How a receiver finds and demodulates a packet (acquisition, gates, decoder search) is
**SHOULD**: other strategies interoperate, and the reference strategy is what the published
measurements assume. Nibbles are written in hex (`A` = 10); "symbol period" is symbol + guard gap.

## 3. Physical layer: MFSK-16

Used by Robust, Robust repeat and Fast. Per-profile values are in §6.

### 3.1 Waveform and preamble

| Parameter | Robust, Robust repeat | Fast |
|-----------|-------|------|
| Modulation | MFSK-16, 4 bits per symbol | same |
| Tone of nibble `k` (0–15) | 2500 + 166⅔ `k` Hz (2500 … 5000 Hz) | 2500 + 333⅓ `k` Hz (2500 … 7500 Hz) |
| Tone step | 1 / symbol duration = 1000/6 Hz | 1000/3 Hz |
| Symbol duration | 6.0 ms (288 samples) | 3.0 ms (144 samples) |
| Guard gap (silence after every symbol) | 0.5 ms (24 samples) | 0 |
| Symbol period | 6.5 ms (312 samples) | 3.0 ms (144 samples) |
| Attack / release (linear ramps inside the symbol) | 0.3 ms (14 samples) | 0.15 ms (7 samples) |
| Peak amplitude `A` | 0.25 (−12 dBFS; full scale = 1.0) — SHOULD, see §6 | same |
| Preamble | `A 5 C 3` (4 symbols) | `A 5` (2 symbols) |

Symbol `n` (`N` samples, attack `A` and release `R` samples, tone `f`, sample rate `fs`) is, for
`i = 0 … N−1`:

```
s[i] = A · env[i] · sin(2π f i / fs)            (every symbol starts at phase 0)
env[i] = i / A           for i < A
       = (N − i) / R     for i ≥ N − R
       = 1               otherwise
```

followed by the guard gap of zeros. One **copy** of a packet is the preamble symbols, then the body
symbols (one per body nibble, §4.5), then **one symbol period of silence**. The preamble is not
CRC-covered; it marks the packet start and the symbol clock and replaces a separate sync word.

The tone step is exactly the reciprocal of the symbol duration (166⅔ Hz = 1000/6, 333⅓ Hz =
1000/3; it is a property of the symbol, not of the sample rate), so the 16 tones are **orthogonal**
over a symbol: every tone falls on a zero of every other tone's rectangular-window spectrum, and a
symbol leaks about −26 dB into its neighbours' detector bins instead of the −5.3 dB of the former
0.6-bin spacing. A receiver therefore sees ≳ 20 dB between the sent tone and the runner-up on a
clean line (was ≈ 5 dB), and a sample-rate mismatch of ±1 % between the ends (a 0.2-bin frequency
offset) no longer closes that gap. The price is bandwidth: a line MUST pass 2.5–5.0 kHz for Robust
and Robust repeat, and 2.5–7.5 kHz for Fast (§6).

### 3.2 Receiver: detection and acquisition

This section describes the reference receiver. It is **SHOULD** throughout: the transmitted signal
does not depend on it, and a receiver MAY find and demodulate packets differently, as long as it
applies the validation of §4.7.

**Input filter.** The receiver band-passes its input to the tone band before any measurement: a
second-order Butterworth high-pass at 0.7 × the lowest tone and a second-order Butterworth low-pass
at 1.3 × the highest (Robust 1750–6500 Hz, Fast 1750–9750 Hz). Everything below (window RMS, energy,
purity) is measured on the filtered signal, so hum, rumble, speech or music bleed below the band and
hiss above it do not count against a tone. Non-finite input samples are read as zero.

**Detection gate.** A Goertzel bank over the 16 tone frequencies is evaluated on a symbol window
(`N` = symbol samples). Detection is level-independent. A window shows nibble `k` when all hold:

1. **Silence floor:** window RMS ≥ `min_rms_dbfs` = −60 dBFS (full-scale sine = −3 dBFS). Quieter
   windows are never detected.
2. **Tone purity:** `purity = P_k / (E · N / 2) ≥ min_tone_purity`, where `P_k` is tone `k`'s Goertzel
   power, `k` is the strongest tone, and `E = Σ x²` the energy of the (filtered) window. A steady
   sinusoid filling the window gives purity ≈ 1.0 at any amplitude; ramps, misalignment, other tones
   and in-band noise lower it. Threshold: **0.20** for every MFSK profile.
3. **Confidence:** `10 · log10(P_k / P_runner-up) ≥ c`, with `c = min_confidence_db` (**3.0 dB**
   Robust / Robust repeat, **2.0 dB** Fast) for the first preamble symbol under first-symbol
   acquisition, and `c = 0.5 · min_confidence_db` for every other symbol.

Measured purity of synthesized symbols after the input filter: ≈ 0.97 aligned, ≥ 0.4 at a full
search hop of misalignment, ≈ 0.6 for a symbol in white noise at −6 dB full-band SNR. The threshold
was tuned on the reference implementation's simulator: 0.20 is the lowest value at which white noise
and music-like audio produce no phy lock for any profile (at 0.15, Robust locks twice and Fast 13
times per minute on white noise), and each 0.05 above it costs about 1 dB of noise margin. Packets
decode from 0 dB down to at least −30 dB relative to the transmit level, and in the simulator down
to −10 dB (Robust), −9.5 dB (Robust repeat) and −5 dB (Fast) of full-band white-noise SNR at 95 %
delivery (the per-bin SNR is 21.6 dB (Robust) / 18.6 dB (Fast) higher).

**Search** advances the window by `hop = symbol_samples / 4` (72 / 36 samples). A receiver that
finds a preamble then fine-aligns to the symbol clock, reads the body one symbol period at a time,
and hands the body to the link layer. Two acquisition procedures are defined; the profiles use:

| Profile | Acquisition |
|---|---|
| Robust | joint, 2 misses (§3.2.2) |
| Robust repeat | joint, 1 miss |
| Fast | first symbol (§3.2.1); joint detection is unsuitable for a 2-symbol preamble (one tolerated miss would let a single matching tone lock) |

**Fine alignment** (both procedures) maximizes a Goertzel power over window start positions in
`[t − hop, t + symbol_samples]` around the search position `t` that triggered (the backward range is
clamped to the end of the last decoded packet): a coarse grid of `hop / 2` through `t`, earliest
first (ties keep the earlier), then halving refinement around the best point down to one sample.
Residual error on a clean signal: ≤ 1 sample. The receiver waits until all samples the alignment
can look at have arrived before deciding, so the result does not depend on how input is chunked.

#### 3.2.1 First-symbol acquisition

1. At each search position `t`, the window must show `preamble[0]` and pass the gate with the full
   confidence `min_confidence_db`. (A receiver may first check that tone's purity alone and skip the
   bank when it is below the gate; the result is identical.)
2. Fine alignment maximizes the power of the `preamble[0]` tone; the gate is re-checked on the
   aligned window.
3. The remaining preamble symbols, one symbol period apart, must each show their expected nibble and
   pass the gate (half confidence). Any failure abandons the attempt (§3.2.3).

#### 3.2.2 Joint acquisition (`misses` = M)

At each search position `t`, with `N` preamble symbols at `t + i · period`:

1. **Candidate.** For each preamble window compute the purity of its *expected* tone only (0 below
   the silence floor). The position is a candidate when at most `M` windows have purity below
   `min_tone_purity` and the purities sum to at least `0.8 · N · min_tone_purity`.
2. **Fine alignment** maximizes the expected-tone power **summed over all `N` windows**, so a weak or
   muted symbol does not decide the symbol clock.
3. **Accept.** Run the full bank on each aligned window. A symbol is *good* when it shows the expected
   nibble and passes the gate (half confidence). It is *contradicting* when another tone wins with
   purity ≥ `min_tone_purity` (a tone that would be detected as a symbol; with the input filter the
   best bin of a noise window routinely reaches a third of the gate). The preamble locks when at
   most `M` symbols are not good (anywhere, including the first), at least one is good, and at most
   one is contradicting.
4. On a lock, body reading starts at `aligned + N · period`. A failed attempt resumes search at
   `t + hop`.

Noise or a dropout can take out `M` symbols, while a stream of unrelated tones still has to match
`N − M` preamble symbols in sequence. Measured on random MFSK symbols (other packets' bodies): ~4
phy locks per minute for Robust and for Robust repeat, each rejected by the link layer (~13 / ~10
with the former 0.6-bin tone spacing).

#### 3.2.3 Body symbols, erasures and resync

- After a lock, each body symbol is read one symbol period after the previous one and is checked
  against the gate (half confidence).
- **Uncoded body:** every symbol is kept as its best guess (the strongest tone; `0` below the
  silence floor) whether or not it passes the gate. The CRC-16 and the packet checks of §4.7 decide,
  and a rejected body costs nothing an abort would not, since both rewind the search (below). Before
  r4 the first failing symbol abandoned the attempt; keeping the guess moved Robust repeat's
  simulated noise cliff by 2.5 dB and Fast's by 1 dB, and lets Fast through a noise gate whose attack
  clips a symbol.
- **Coded body (§4.6):** a body symbol that fails the gate is kept as an **erasure** (its best guess
  and confidence are passed on), up to `2 · parity` = 8 per body; the next failure abandons the
  attempt. Every body symbol carries its confidence (dB over the runner-up) as the reliability for
  the decoder.
- **Symbol clock tracking (SHOULD):** for every body symbol that passed the gate, the receiver
  measures its tone's Goertzel power in windows `hop / 2` samples early and late and moves the next
  symbol's window by `0.75 · (N / 4) · (P_late − P_early) / P` samples (an early-late gate; at most
  `N / 32` per symbol), so a sample-rate mismatch between the ends does not walk the windows off the
  symbols. Two percent of mismatch moves a 6.5 ms period by 6 samples per symbol, within the ±9 the
  gate can follow; the reference receiver delivers 100 % at ±1 % (was 0–16 %).
- **Resync:** an abandoned attempt, and a complete body that the link layer rejects (§4.7), rewind
  the search to `t + hop` (as if the window that triggered the attempt had been rejected). Samples
  read during the attempt are kept until it resolves (at most one packet), so a real preamble that
  starts inside a false lock is still found. After a valid packet, search continues after its body.

## 4. Link layer

### 4.1 Packet bytes (compact)

```
[0]     HDR         = (VERSION << 4) | MSG_TYPE
                      VERSION = 0x1 (AML v1; any other value is rejected)
[1]     SEQ         = 0..3 (2-bit counter, wrapping 0, 1, 2, 3, 0, …);
                      bits 7..2 are reserved and MUST be 0
[2..]   PAYLOAD     (length implied by MSG_TYPE, §5)
[…]     CRC16_HI
[…]     CRC16_LO
```

Payload lengths by type:

| MSG_TYPE | Payload bytes | Packet bytes |
|----------|---------------|--------------|
| Midi (0x0) | 3 (`status`, `data1`, `data2`) | 7 |
| Msc (0x3) | 3 (compact MSC, §5.4) | 7 |
| Heartbeat (0x4) | 0 | 4 |
| Panic (0x5) | 0 | 4 |

Type values 0x1, 0x2 and 0x6–0xF are reserved. A transmitter increments SEQ by one (mod 4) per
message (not per copy).

SEQ only has to tell consecutive packets apart for duplicate suppression (§4.4): message `n` and
`n + 4` are always more than one dedup window apart, because messages are sent one after another and
the window is shorter than two messages. The six reserved bits are part of the structure a random
or corrupted block must match (§4.7).

CRC-16/CCITT (poly `0x1021`, refin=false, refout=false, xorout=`0x0000`) covers all bytes from
`HDR` through end of `PAYLOAD` (excludes the CRC itself), transmitted high byte first. Its **init
value is the link key**, a 16-bit value configured identically on both ends; the default key is
**`0xFFFF`** (the CRC-16/CCITT-FALSE init), which is the unkeyed link.

**Link key.** A receiver is level-independent down to its silence floor (§3.2), so two Cricket links
in one multicore or Dante domain, leaking into each other at −40 dB or so, would deliver each other's
valid packets. With different keys they do not: the CRC is affine in its init value, so for a given
packet length two keys shift the CRC by the same non-zero amount whatever the data, and every clean
packet from a link with another key fails the check (not merely with probability 2⁻¹⁶). The key costs
no wire bits, changes no other field, and leaves the false-accept arithmetic of §4.7 as it is. A
transmitter MUST compute the CRC with its configured key; a receiver MUST verify with its own. Both
ends of a link MUST use the same key. An implementation SHOULD accept the key as four hex digits and
MAY derive it from a passphrase (the reference implementation uses the CRC-16/CCITT-FALSE of the
passphrase's UTF-8 bytes, mapping the one value that equals `0xFFFF` to `0x0000`). The key is an
identity, not a secret.

### 4.2 Nibble mapping

Each byte becomes two nibbles, high nibble first:

```
byte b → nibbles [b >> 4, b & 0x0F]
```

A 7-byte packet is 14 nibbles, a 4-byte packet 8. Every body has the same length whatever the type,
so a receiver never needs the type before the body is complete.

### 4.3 Uncoded body (Robust repeat, Fast, `ltc`, `ltc2400`, `ltc9600`)

The packet nibbles are padded with `0` to **16** nibbles and block-interleaved with depth **4**:
written column-major into 4 rows, read row-major:

```
body[r · 4 + c] = padded[c · 4 + r]        r, c = 0 … 3
```

i.e. the wire order is padded nibbles 0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15.

### 4.4 Repetition and duplicate suppression

Each packet is transmitted **K** times (Robust repeat: K = 2; every other profile: K = 1), each copy
a full preamble/sync + body. The receiver delivers the first valid copy and suppresses later copies:
a valid packet is dropped only if its packet bytes (same `SEQ`, type and payload) equal a packet
accepted within the dedup window before it, measured in received samples:

```
repetition = samples of one copy including the silence after it
             (MFSK: (len(preamble) + body nibbles + 1) symbol periods; LTC: §6.5)
window     = K × repetition + repetition / 2
```

Robust ≈ 263 ms (12 636 samples), Robust repeat ≈ 341 ms (16 380), Fast ≈ 86 ms (4 104). A packet
with the same `SEQ` but a different payload, or one arriving after the window (TX restart, `SEQ`
wrap), is delivered. The dedup history is cleared on link-dead. A receiver MUST NOT deliver the second
copy of a K = 2 transmission as a second message.

### 4.5 On-wire order (one copy)

```
uncoded:  PREAMBLE || INTERLEAVE4(PAD16(NIBBLES(packet)))          || silence
coded:    PREAMBLE || WHITEN(ALTERNATE(RS(d0..d6), RS(d7..d13)))    || silence
```

MFSK sends one tone per nibble (§3.1). The LTC profiles send the same nibble sequence as 4 bits per
nibble behind their own sync (§6.5). Copy lengths: Robust 4 + 22 = 26 symbols (169 ms), Robust repeat
4 + 16 = 20 symbols (130 ms), Fast 2 + 16 = 18 symbols (54 ms), each followed by one symbol period of
silence.

### 4.6 Body FEC: Reed–Solomon over GF(16) (Robust, `ltc_fec`, `ltc2400_fec`, `ltc9600_fec`)

One nibble is one code symbol, so one wrong MFSK tone is exactly one symbol error.

#### 4.6.1 Field and code

- **Field:** GF(16) = GF(2)[x] / (x⁴ + x + 1) (`0x13`). A nibble's bit `i` is the coefficient of
  `xⁱ`. Primitive element α = x (`0x2`).
- **Code:** narrow-sense Reed–Solomon with 4 parity symbols, generator

  ```
  g(x) = (x − α)(x − α²)(x − α³)(x − α⁴) = x⁴ + D·x³ + C·x² + 8·x + 7
  ```

  shortened to RS(11, 7): 7 data + 4 parity nibbles, systematic. It corrects `e` errors and `f`
  erasures per codeword whenever `2e + f ≤ 4`.
- **Codeword order:** a codeword is its 7 data nibbles followed by its 4 parity nibbles; codeword
  position `w` (0 … 10) is the coefficient of `x^(10 − w)`, i.e. the first nibble sent is the
  highest-degree coefficient. With `m(x) = Σ d_w x^(6 − w)`, the codeword is
  `c(x) = m(x) · x⁴ + (m(x) · x⁴ mod g(x))`, so `c(α^j) = 0` for `j = 1 … 4`.

#### 4.6.2 Body layout

1. **Data:** the packet nibbles (§4.2), zero-padded to **14** nibbles `d0 … d13` (the largest packet
   is exactly 14).
2. **Codewords:** `A = RS(d0 … d6)`, `B = RS(d7 … d13)`.
3. **Alternate:** `A0 B0 A1 B1 … A10 B10` (22 nibbles). This replaces the interleaver of §4.3; a burst
   of 8 consecutive bad symbols is at most 4 per codeword.
4. **Whiten:** wire nibble `i` = body nibble `i` XOR `W[i]`, with

   ```
   W = 8 4 3 F 6 A 8 8 8 5 A 3 8 D 3 1 3 1 9 8 A 2
   ```

   (`W[1…]` are the non-zero hex digits of the fraction of π from the second digit on,
   `243F6A8885A308D3…`; `W[0]` = 8 makes the version nibble `1` go on air as `9`.) Whitening makes
   the coded body a coset of the code, so an uncoded body on the same phy (a misconfigured
   transmitter) does not line up with the checks of §4.7.

The coded body is 22 nibbles (uncoded: 16). The CRC-16, header and payload are unchanged; the
protocol version stays `0x1`.

#### 4.6.3 Decoding (SHOULD)

The reference decoder un-whitens the body, splits it into the two codewords, and for each codeword
computes up to five distinct candidate decodings (errors-and-erasures: Forney syndromes,
Berlekamp–Massey, Chien search, Forney's formula):

- with the gate erasures (§3.2.3) plus 0, 1, … extra erasures on the least reliable remaining symbols,
  up to 4 erasures in total (generalized minimum distance), then
- errors-only (when there were gate erasures).

Candidate pairs (one decoding per codeword) are tried in increasing errata weight (§4.7) and the first
pair that passes §4.7 is delivered. Erasures cost half as much as errors, which is why the receiver
keeps failed symbols as erasures instead of abandoning the packet.

### 4.7 Packet validation (MUST)

A receiver MUST deliver a packet only if **all** of the following hold, and MUST otherwise discard the
body (and SHOULD rewind its search, §3.2.3):

1. **CRC-16** (§4.1) matches.
2. **Version** nibble is `0x1`; **type** is 0x0, 0x3, 0x4 or 0x5; **SEQ** < 4 (reserved bits zero).
3. **Payload is canonical** (§5.1 for Midi, §5.4 for Msc). A CRC-valid packet with a non-canonical
   payload is a false decode, not a message.
4. **Coded bodies only — zero pad:** every data nibble after the packet (`d8 … d13` for Heartbeat and
   Panic) is `0`.
5. **Coded bodies only — errata-weight bound:** let `e` be the number of body symbols the decoding
   changes that passed the detection gate, and `f` the number of gate erasures (counted over the whole
   22-nibble body, however the decoding was found). The decoding MUST satisfy
   `Σ (2e + f) ≤ 2 · parity = 8`, **or** change at most one gate-passed symbol in total (`e ≤ 1`:
   a dropout plus one weak symbol). A receiver that does not mark erasures has `f = 0`, i.e. at most 4
   corrected symbols (any split between the codewords), or 1.

Rules 3–5 exist because the erasure-tolerant decoder offers the CRC many candidates per false lock:
without them, foreign audio (other profiles' traffic, a misconfigured transmitter) produced a false
message every few hours; with them, the worst measured case is one per ≥ 62 years of continuous
worst-case input per receiver (target: one per 10 years; see `docs/proposals/fec.md` §9). Uncoded
bodies MAY ignore their pad nibbles (the reference receiver does). A neighbouring link of the same
profile with another link key (§4.1) is the cleanest foreign input: its bodies decode to one
candidate each, which fails the CRC deterministically; measured, such traffic offers the CRC fewer
candidates per body than a misconfigured uncoded transmitter.

**Wrong-key count (SHOULD, informative).** A body that passes rules 2–3 (version, type, SEQ,
canonical payload) but fails the CRC is almost always a packet from a link with another key: random
data passes rules 2–3 with probability about 2⁻¹⁷. A receiver SHOULD count these separately from
other CRC failures and show the count to the operator ("packets from another link key"), so a wrong
key on one end, or crosstalk from a neighbouring link, is diagnosed in seconds instead of looking
like noise. The reference implementation counts them per body (each of K copies, and a body read
again after a rejected lock, counts).

## 5. Transport messages

| MSG_TYPE | Name | Payload |
|----------|------|---------|
| 0x0 | Midi | One MIDI 1.0 short message: `status`, `data1`, `data2` (§5.1) |
| 0x3 | Msc | MIDI Show Control command, compact form (§5.4) |
| 0x4 | Heartbeat | empty |
| 0x5 | Panic | empty |

### 5.1 Midi payload and validation

The payload is the MIDI message itself, without running status, with unused bytes zero:

| Status | Message | Data bytes |
|--------|---------|------------|
| `0x8n` | Note Off (with release velocity) | note, velocity |
| `0x9n` | Note On, velocity 1–127 | note, velocity |
| `0xAn` | Polyphonic key pressure | note, pressure |
| `0xBn` | Control Change, incl. channel mode messages (controllers 120–127) | controller, value |
| `0xCn` | Program Change | program, `0` |
| `0xDn` | Channel pressure | pressure, `0` |
| `0xEn` | Pitch Bend | LSB, MSB |
| `0xF2` | Song Position Pointer | LSB, MSB |
| `0xF3` | Song Select | song, `0` |
| `0xF6` | Tune Request | `0`, `0` |

`n` is the MIDI channel (0–15 = channels 1–16); all 16 channels are carried. A payload is
**canonical** if its status byte is in the table, its data bytes are `< 0x80`, its unused bytes are
`0`, and it is not a Note On with velocity 0. A transmitter MUST send canonical payloads only: a Note
On with velocity 0 is sent as Note Off with velocity 64 (`0x40`). A receiver MUST reject a packet
whose payload is not canonical (§4.7).

Not carried, by design: System Exclusive other than MSC (§5.4), MIDI Time Code quarter frames,
and all System Real-Time messages (Timing Clock, Start, Continue, Stop, Active Sensing, Reset).
Their value depends on timing: a clock or timecode stream needs a steady rate with sub-millisecond
jitter, and a packet takes 8–273 ms of airtime depending on the profile, so these streams would
both arrive useless and delay every other message. A transmitter drops them at its MIDI input.

### 5.2 MIDI mapping

The receiver outputs each Midi payload as the MIDI message it holds (2 or 3 bytes per the table in
§5.1; `0xF6` alone), on its channel. Msc is output as an MSC SysEx (§5.4); Heartbeat has no MIDI
output; Panic outputs the fail-safe burst (§5.3).

### 5.3 Fail-safe

- Decoder marks link **dead** if no CRC-valid packet for **`link_dead_ms`** after the first valid
  packet. `link_dead_ms` is a profile parameter (§6 table, §6.5): **2000 ms** on the MFSK profiles
  and the LTC profiles up to 4800 bit/s, **1000 ms** from 9600 bit/s, **500 ms** on `digital`; an
  implementation MAY let the operator override it. The Heartbeat interval (§5.5) is a quarter to
  a fifth of it, so three to four heartbeats in a row must be lost before a link that is merely poor is
  declared dead: at 97 % packet delivery that is one false link-dead per 5–170 h on Robust (per
  500 ms slot, p³–p⁴), per 2.6–86 h on `ltc9600_fec` (per 250 ms slot), and at 99 % per 70 h–290 d
  on `ltc9600_fec`; a line that bad is unusable for notes anyway, and a false link-dead costs one
  burst. The interval is also at least 20 message airtimes, so a heartbeat occupies at most 5 % of
  the idle line and one in flight stays out of the p95 note latency (hence 500 ms on `ltc_fec`,
  whose 21.8 ms message would be 17 % of a 125 ms interval).
  This **sample-clock** link-dead detector is timed in received
  samples: it fires at sample `end of last valid packet + link_dead_ms`, independent of how audio
  is chunked, and is reported in the processing call whose input contains that sample. It fires
  once per event; the next valid packet re-arms it. Link state reads **Dead** from that moment
  until the next valid packet. Every CRC-valid packet re-arms it, **a suppressed duplicate
  (§4.4) included**: the second copy of a K = 2 transmission is proof of life even though it is
  not delivered.
- **Wall-clock input watchdog** (engine RX, distinct from sample-clock link-dead): if the capture
  device delivers no audio samples for **`link_dead_ms`** while the link has been alive (at least
  one valid packet received and sample-clock link-dead not yet fired), the worker emits the
  fail-safe burst exactly once (when the modem still considers the link alive), raises an
  **audio-input-stalled** alarm, and disarms sample-clock link-dead for that outage. When samples
  resume, the stall alarm clears; link state stays **Dead** until the next valid packet, as usual.
  Sample-clock link-dead is unchanged when samples keep arriving but packets do not.
- On link-dead (either detector), or on Panic: emit the **fail-safe burst** exactly once. The
  demodulator is already back in search after each packet / during silence; its buffer is not
  flushed. Link-dead also clears the dedup history (§4.4). A Panic is a valid packet, so a link
  that goes silent after it later produces its own link-dead burst.
- Bad-CRC / incomplete frames are discarded; return to search
- Idle **Heartbeat** timing and keepalive airtime are defined by the TX scheduler (§5.5).
- An implementation MAY let the operator **disable the burst** for cue-only (MSC) links, where a
  stray All Notes Off on a shared MIDI line is unwanted. Link-dead MUST still be detected and
  reported; only the MIDI output of the burst is omitted (on Panic too).

**Fail-safe burst.** For each MIDI channel 1–16 in order: a Note Off (`0x8n note 0x40`) for every
note this receiver has output a Note On for and no Note Off since (nor All Notes Off / All Sound
Off on its channel), in ascending note order; then Sustain off (`0xBn 0x40 0x00`), then All Notes
Off (`0xBn 0x7B 0x00`), then — only on channels whose last Pitch Bend output by this receiver was
not center and not followed by Reset All Controllers — Pitch Bend center (`0xEn 0x00 0x40`). That is
32–48 short messages plus one per sounding note (~31–46 ms plus ~1 ms per note on a 31.25 kbit/s
MIDI cable).

- *Explicit Note Offs first*: several soft synths and DAWs ignore CC123 (All Notes Off); a Note Off
  per note the receiver itself turned on releases them too. They come before Sustain off so a note
  held by the pedal is released by the pedal going up right after.
- *All 16 channels*, not only channels seen since start: the receiver may have restarted while
  notes were sounding, and a stateless burst cannot miss a channel. It is the usual "MIDI panic"
  convention and harmless on channels without notes.
- *Sustain off first* (of the channel messages), because All Notes Off does not release notes held
  by the pedal.
- *Pitch Bend center* only where this receiver left a bend: a pitch wheel springs back to center, so
  a lost "back to center" would detune every later note; other controllers have no rest position
  and are left alone.
- *Not sent:* Reset All Controllers (CC121; it also resets expression, modulation and pressure
  that a show may rely on, and its exact effect varies between devices), All Sound Off (CC120;
  it cuts release tails), and Note Offs for notes this receiver did not turn on.

A receiver that restarted, or ran its fail-safe, has lost the pedal and bend state the transmitter
still holds; the transmitter's controller refresh (§5.5) restores it within a few Heartbeat
intervals.

### 5.4 MIDI Show Control

MSC 1.0 messages are SysEx: `F0 7F <device_id> 02 <command_format> <command> <data> F7`, where cue
numbers, lists and paths are ASCII digits with an optional `.` (`"14.5"` = `31 34 2E 35`), separated
by `00` (`<cue> 00 <list> 00 <path>`).

AML carries the **command and its data only**. The device ID and command format are **receiver
settings** (default device ID **`0x7F`** all-call, so every MSC device responds; default command format
**`0x01`** Lighting (General), the format lighting consoles listen for). By default a transmitter
accepts MSC from any device ID and in any command format.

**Multi-device rigs: filter at the transmitter.** With several MSC devices on the transmitter's MIDI
line (a sound console on device 3 / format Sound, a lighting console on device 1 / format Lighting)
every cue would otherwise cross the link and be re-addressed to the receiver's single target: a
sound GO would come out as a lighting all-call GO. A transmitter MAY therefore be configured to
carry only MSC addressed to one device ID and/or one command format; MSC addressed to the all-call
device ID `0x7F` or in the all-types format `0x7F` always passes, as MSC semantics require. Filtered
messages are not sent and are not errors (the transmitter SHOULD log the first one). The default,
no filter, keeps the single-device case as it was.

**Compact form** (payload bytes `b0 b1 b2`, big-endian 24 bits):

```
bits 23..19  command   MSC command byte (supported commands only, table below)
bits 18..15  list      0 = no cue list; 1–15 = cue list 1–15
bits 14..0   value     cue commands: 0 = no cue; v = 1–10000 → cue (v − 1) / 10  (0.0–999.9)
                       FIRE: macro number 0–127
                       other commands: 0
```

| Command (byte) | Data | Compact |
|---|---|---|
| GO (01), STOP (02), RESUME (03), GO_OFF (0B), GO/JAM_CLOCK (10) | `[cue [00 list]]` | cue optional; list only with a cue |
| LOAD (05) | `cue [00 list]` | cue required; list optional |
| FIRE (07) | macro number | value = macro, list 0 |
| ALL_OFF (08), RESTORE (09), RESET (0A) | none | list 0, value 0 |
| STANDBY_+ (11), STANDBY_- (12), SEQUENCE_+ (13), SEQUENCE_- (14), START_CLOCK (15), STOP_CLOCK (16), ZERO_CLOCK (17), MTC_CHASE_ON (19), MTC_CHASE_OFF (1A) | `[list]` | list optional, value 0 |
| OPEN_CUE_LIST (1B), CLOSE_CUE_LIST (1C) | `list` | list required, value 0 |

A compact payload is **canonical** only as in this table (a receiver rejects anything else, §4.7).

**Numbers.** Cue and list numbers are carried as numbers: `"05"`, `"5.0"`, `"5."` and `"5"` are the
same cue and are output as `"5"`; `"12.50"` is output as `"12.5"`. Cues 0–999.9 with at most one
decimal (after dropping trailing zeros) and lists 1–15 are carried.

**Not carried** (the transmitter drops the message and reports it, e.g. "MSC cue 12.345 not
representable"; it never sends an approximation): cues with more than one decimal or above 999.9,
lists 0 or above 15, lists with decimals, cue paths, TIMED_GO, SET, SET_CLOCK, OPEN_CUE_PATH,
CLOSE_CUE_PATH, and malformed MSC. Other SysEx is ignored silently (§5.1).

**Output.** The receiver outputs `F0 7F <device_id> 02 <command_format> <command> <data> F7` with its
configured device ID and command format; `<data>` is the cue in ASCII without trailing zeros (`14`,
`14.5`, `0.5`), then `00` and the list in ASCII if there is one; the list alone for list commands;
the macro byte for FIRE; nothing otherwise.

### 5.5 TX scheduler

The encoder schedules transport messages before modem encoding. **Heartbeat** is generated by the
scheduler when due; it is not queued with user traffic.

**Priority** (next message to encode):

| Order | Messages |
|-------|----------|
| 1 | Panic; channel-mode messages that end notes: CC 120 (All Sound Off), 123 (All Notes Off), 124–127 (Omni Off/On, Mono, Poly) |
| 2 | MSC, in arrival order |
| 3 | Note Off (any channel), in arrival order |
| 4 | Everything else: Note On, Program Change, Control Change, Pitch Bend, pressure, system common |
| 5 | NoteOff refresh (idle; see below) |
| 6 | Heartbeat slot: a pending NoteOff refresh, else a controller refresh, else Heartbeat |

Messages of orders 1–3 are never coalesced or dropped. MSC goes ahead of Note Off (r5; before, both
shared one tier in arrival order) because a cue must not wait behind a chord release: on Robust, four
Note Offs ahead of a GO delayed it by up to 0.7 s.

**Order within tier 4.** *Continuous* messages — Control Change except Bank Select (CC 0, 32), Data
Entry (6, 38), Data Increment/Decrement (96, 97), NRPN/RPN select (98–101) and the channel mode
messages (120–127); Pitch Bend; channel pressure; polyphonic key pressure — are latest-value-wins.
All other tier-4 messages are *discrete* and keep their arrival order. The next tier-4 message is:
the oldest discrete message, unless a continuous message **of its channel** was queued before it,
in which case that continuous message goes first; with no discrete message queued, the oldest
continuous message. A note therefore waits for at most the controller values of its own channel that
were set before it (it is struck with them), never for those of other channels. System common
messages have no channel and keep their place relative to everything. ("Tier 3" in older text and
in the reference implementation's `tier3_*` names is this tier.)

**Coalescing (latest value wins).** A continuous message replaces, in place, the newest queued
continuous message with the same channel, kind and controller (or note, for polyphonic pressure),
unless a discrete message of the same channel or a system common message is queued after it, or
Note Offs are held behind it (below); then it is queued as a new message. So a controller value set
before a Note On stays before it and the newer value follows it; Bank Select, RPN/NRPN and Data Entry
are never merged or reordered, and stay in order with the Program Change after them. Program Change
is never coalesced: each one can be a deliberate action (a scene or cue on the receiving device), and
a skipped program can be audible. A fader streaming faster than the link carries thus costs one queued
message, and its final value goes out within the message in flight plus one.

**Held NoteOff.** A Note Off that arrives while a Note On for the same channel and note is still
queued is held with the latest such Note On and sent immediately after it, so the note is not
released before it is struck; this also keeps off / on / off on a note that is already sounding (a
trill or roll) in order. Likewise a Note Off that arrives while a pedal controller of its channel —
Sustain (CC 64), Sostenuto (66) or Hold 2 (69) — is queued is held behind the latest such pedal
message: pedal down then key up must not become key up then pedal down (the note would stop instead
of ringing). A pedal message with held Note Offs is not coalesced.

**Panic and channel mode.** Panic removes queued Note Ons on every channel; a Panic-class channel
mode message (order 1) removes queued Note Ons of its channel (they would otherwise sound after it).
Reset All Controllers (CC 121) and Local Control (CC 122) are discrete tier-4 messages: moving them
ahead of queued controller values would change the resulting state.

**Heartbeat slot:** one **Heartbeat interval** (§6 table: 500 ms on the MFSK profiles and the LTC
profiles up to 4800 bit/s, 250 ms from 9600 bit/s, 100 ms on `digital`) after the last transmission of any
kind, when nothing is queued, the transmitter sends one message: a pending Note Off refresh if there
is one (whether or not its schedule below says it is due), else a **controller refresh** (below),
else a **Heartbeat**. Any of them is a valid packet and keeps the link alive (§5.3). While user
messages are pending, no Heartbeat airtime is used.

**NoteOff refresh:** after a user Note Off is transmitted, it is repeated (same channel, note and
velocity) up to **6** times within **10 s** (max **32** most recent releases). On the **MFSK**
profiles (messages of 57–273 ms) repeat *k* (1–6) is sent no earlier than **1, 2, 3, 5, 8, 13
message times** after the user Note Off: a Fibonacci spread, so consecutive copies do not fall into
one dropout and a release costs a few idle slots spread over seconds instead of a burst, while the
six repeats still fit within link-dead time (0.74 s on Fast, 2.3 s on Robust). On the **LTC**
profiles and `digital` (messages of a few ms) the repeats take the **first six free slots**: they
are over before the next note is likely to come, and a dropout of a few ms covers at most one of
them; `digital` keeps at least 20 ms between copies. On **Robust** (coded MFSK: few Note Offs lost,
0.3 % at 2 % symbol errors, and a message long compared with the time between notes) a due repeat
also waits for a **quiet line**: one message time of idle line after the message in flight, so a
note struck shortly after another never waits behind a repeat; under sustained playing the repeats
wait for a pause or the Heartbeat slot. On the uncoded MFSK profiles (Robust repeat, Fast), which
lose far more Note Offs (Fast: 23 % at 2 %), a due repeat takes the first free slot. Among due
repeats the note with the **fewest** repeats so far goes first, then the **oldest** release. A
**Note On** for that channel and note removes it from the refresh list. A refresh never runs ahead
of a queued message. Measured against the r4 policy (six repeats back to back in the first free
slots), legato playing on Robust (a note every 300–600 ms, each released 60 ms after the next
strike) gains 74 ms of mean Note On latency in the scheduler harness (324 → 250 ms, one on-wire
copy included) and 290 ms in the in-process loopback (485 → 199 ms); Fast gains 8 ms (89 → 81);
the LTC profiles are unchanged (with the spread schedule `ltc_fec`'s p95 rose from 21 to 38 ms,
which is why they keep the first free slots). No note stayed stuck past link-dead time in the
stuck-note harness up to 2 % symbol errors on Robust, Robust repeat and Fast.

**Controller refresh:** the transmitter remembers, per channel, the last value it sent for the
pedals (CC 64, 66, 67, 69), Modulation (CC 1) and Pitch Bend, until a Reset All Controllers (CC 121)
on that channel. In a Heartbeat slot with no Note Off refresh pending, instead of a Heartbeat it
re-sends one state that differs from the default — a pedal at 64 or more, Modulation not 0, a bend
off center — round robin over channels and controllers. It costs no airtime beyond the Heartbeat it
replaces; a receiver that restarted or ran its fail-safe (§5.3) gets its pedal and bend state back
within a few Heartbeat intervals, and a receiver that did not sees an idempotent repeat.

**Note On limit:** at most **N** Note Ons are ever queued, where N = max(⌊250 ms / airtime per
message⌋, min(10, max(4, ⌊1 s / airtime per message⌋))): **5** on Robust, **4** on Robust repeat,
**10** on Fast, **14** on `ltc`, **11** on `ltc_fec`, **29** on `ltc9600`, **46** on `ltc14400`.
A Note On arriving beyond N is **refused** (drop-newest) and counted; the Note Ons already queued
stay. For a chord struck at once this keeps the notes struck first — the bass, the hand that leads —
and drops the last; a note that would only have gone out N message times late is reported instead
(the reference implementation raises an alarm with the count). A Note Off for a refused note is
not sent either, unless the note is sounding from an earlier strike. Before r5 the oldest queued Note
On was dropped and at most 4 were queued on the MFSK profiles, so the 6th note of a chord dropped
the 2nd struck. The limit means a 10-note chord goes out whole on Fast (0.57 s) and a 5-note chord on
Robust (0.88 s); a run beyond the link's capacity loses notes on either policy, and the alarm tells
the operator to use a faster profile.

The airtime of **everything queued in order 4** — Note On, Program Change, controllers, pitch bend,
pressure, system common, plus the Note Offs held behind them — is capped at **1 s**. Before another
order-4 message is queued, the oldest queued order-4 messages are dropped until it fits, whatever
their kind, except Note Ons (bounded by their own limit) and pedal messages with Note Offs held behind
them; the queue exceeds the cap only by those. Coalescing alone does not bound the queue: every
discrete message pins the controller values of its channel queued before it, and discrete messages
other than Note On had no budget of their own. Dropping strictly oldest first means a controller value
never outlives a discrete message that kept it apart from a newer one, so nothing needs merging
afterwards. Under such overload a Bank Select may be separated from its Program Change, or an
RPN/NRPN select from its Data Entry; every drop is counted and reported. Orders 1–3 (Panic-class,
MSC, Note Off) have no bound: dropping one would leave a note stuck or skip a cue, so their queues
grow with the input. A source that sustains more Note Offs than the link carries is a fault of the
source; the scheduler keeps them all and reports the queue depth.

**Audio output ring:** sized to about **two** on-wire messages. The next message is chosen and
encoded only when buffered output has drained below **10 ms**, so a Note On that arrives while a
message is playing waits for that message only, not for one already encoded behind it.

## 6. Profiles

Profile identifiers (case-insensitive; `-` and `_` are equivalent): `robust`, `robust_repeat`,
`fast`, `ltc2400`, `ltc` (= `ltc4800`), `ltc9600`, `ltc2400_fec`, `ltc_fec` (= `ltc4800_fec`),
`ltc9600_fec`. The reference implementation also accepts `robust_fec`, the former name of `robust`,
and rejects an unknown name (it MUST NOT substitute another profile: both ends have to match).

User-facing names (Cricket; informative, equivalent to the identifiers above): `analog-safe` =
`robust`, `analog-safe-repeat` = `robust_repeat`, `analog-fast` = `fast`, `clean-line` = `ltc_fec`,
`clean-line-fast` = `ltc9600_fec`, `digital-line` = `ltc14400_fec` (not in this spec),
`digital-bit-exact` = `digital` (not in this spec), `analog-safe-fast` = `robust_fast` (experimental,
not in this spec: Robust's preamble and coded body on 4 ms symbols with 250 Hz orthogonal tones,
2.5–6.25 kHz, 108 ms per message). The uncoded LTC profiles keep their identifiers.

| | Robust (default) | Robust repeat | Fast | LTC family (§6.5) |
|---|---|---|---|---|
| User-facing name | `analog-safe` | `analog-safe-repeat` | `analog-fast` | `clean-line` (`ltc_fec`), `clean-line-fast` (`ltc9600_fec`); uncoded: `ltc*` |
| Phy | MFSK-16 (§3) | MFSK-16 | MFSK-16 | biphase mark, 2400 / 4800 / 9600 bit/s |
| Tones | 2500 + 166⅔ k Hz (2.5–5.0 kHz) | 2500 + 166⅔ k Hz | 2500 + 333⅓ k Hz (2.5–7.5 kHz) | — |
| Line bandwidth (MUST pass) | 2.5–5.0 kHz | 2.5–5.0 kHz | 2.5–7.5 kHz | §6.5 |
| Symbol + guard | 6.0 + 0.5 ms | 6.0 + 0.5 ms | 3.0 + 0 ms | 4 bits per nibble |
| Ramps | 0.3 ms | 0.3 ms | 0.15 ms | raised-cosine edges, ½ half cell |
| Amplitude `A` (peak; SHOULD) | 0.25 (−12 dBFS) | 0.25 | 0.25 | `A` / √2 ≈ 0.177 |
| Preamble / sync | `A 5 C 3` | `A 5 C 3` | `A 5` | 8 × `1` + 2 code violations |
| Body | 22, RS(11,7) × 2, whitened (§4.6) | 16, interleaved (§4.3) | 16, interleaved | 16 (`ltc*`) or 22 coded (`ltc*_fec`) |
| Copies K | 1 | 2 | 1 | 1 |
| Silence after each copy | 1 symbol period | 1 symbol period | 1 symbol period | 4 bit cells |
| One copy on air | 169 ms | 130 ms | 54 ms | 4800: 15.9 ms / 20.9 ms coded |
| Airtime per message | 175.5 ms | 273 ms | 57 ms | 4800: 16.8 ms / 21.8 ms coded |
| Dedup window (§4.4) | 263 ms | 341 ms | 86 ms | one repetition × 1.5 |
| Acquisition (RX, §3.2) | joint, 2 misses | joint, 1 miss | first symbol | sync correlator (§6.5.3) |
| Input filter (RX, §3.2) | 1750–6500 Hz | 1750–6500 Hz | 1750–9750 Hz | — |
| Purity / confidence gate (RX) | 0.20 / 3.0 dB | 0.20 / 3.0 dB | 0.20 / 2.0 dB | — |
| Body erasures kept (RX) | ≤ 8 | best guess kept (§3.2.3) | best guess kept | ≤ 8 coded, unlimited uncoded |
| Heartbeat / link-dead (§5.3, §5.5) | 500 ms / 2000 ms | same | same | 2400, 4800: same; 9600: 250 / 1000 ms |

Rows marked RX are reference receiver settings (§3.2, SHOULD); everything else is wire format (MUST),
except the amplitude: receivers are level-independent (§3.2, silence floor −60 dBFS), so a
transmitter SHOULD send at `A` = 0.25 (−12 dBFS peak, headroom on the bus) and MAY use another level
between −30 dB below that and full scale. (Before v1.1 the reference transmitter used 0.8.)

### 6.1 Robust (default)

MFSK-16 at 6.5 ms per symbol period, preamble `A 5 C 3`, the coded 22-nibble body of §4.6, sent
**once**. One copy is 26 symbols (169 ms); with the trailing silence a message holds the line for
175.5 ms. Reference receiver: joint acquisition with 2 misses (§3.2.2), up to 8 body erasures.

Use it when the chain is unknown or may include converters, patchbays, mild distortion or noise. The
line must pass 2.5–5.0 kHz. In the channel simulator it matches or beats Robust repeat in every
condition except dropouts of ≳ 50 ms (e.g. 95 % delivery down to −10 dB vs −9.5 dB of white-noise
SNR; 5 % wrong tones: 96 % vs 72 %; 60 ms dropout: 75 % vs 93 %), with 36 % less airtime and ~25 %
lower NoteOn latency under load (`docs/proposals/fec.md` §10).

### 6.2 Robust repeat

Robust's tones, timing and preamble with the **uncoded** 16-nibble body (§4.3), sent **twice** (K = 2).
One copy is 20 symbols (130 ms); a message holds the line for 2 × 136.5 = 273 ms. Reference receiver:
joint acquisition with 1 miss.

Use it on lines with glitches or dropouts of about 50 ms or more: a dropout that long erases more of a
single coded body than the code can fill, while a second copy survives it (60 ms dropouts: 93 %
delivered vs 75 % for Robust). The line must pass 2.5–5.0 kHz.

### 6.3 Fast

For clean dedicated chains only (no EQ / compression / limiter on the AML bus) that pass 2.5–7.5 kHz.

| Parameter | Value |
|-----------|-------|
| Symbol duration | 3.0 ms |
| Guard gap | 0 |
| Attack / release | 0.15 ms |
| Tone spacing | 333⅓ Hz = 1 / symbol duration (orthogonal tones, 2500 … 7500 Hz) |
| Line bandwidth | MUST pass 2.5–7.5 kHz |
| Preamble | `A 5` (2 symbols) |
| Body | 16 nibbles, uncoded (§4.3) |
| Repetition K | 1 |
| Confidence (RX) | 2.0 dB |
| Min tone purity (RX) | 0.20 |
| Acquisition (RX) | first symbol (§3.2.1) |

One copy for any packet is 18 symbols (54 ms).

### 6.4 Throughput (airtime budgets)

Airtime per message sets a practical sustained rate on a dedicated chain: Robust (**175.5 ms**)
about **5 messages/s**, Robust repeat (**273 ms**) about **3–4 messages/s**, Fast (**57 ms**) about
**17 messages/s**, `ltc_fec` (**21.8 ms**) about **45 messages/s**. A chord of up to 5 notes (Robust),
4 (Robust repeat) or 10 (Fast) is sent whole (later notes wait about one message each); Note Ons
beyond that are refused and reported (§5.5). These are on-wire airtime budgets
for a clean dedicated path, not guarantees through EQ, dynamics, or other processing on the AML bus.

### 6.5 LTC (biphase-mark) profiles — status: provisional

A biphase-mark line code like SMPTE LTC, carrying the same packets and bodies as the MFSK profiles
in a fraction of the airtime (16.8 ms per message at 4800 bit/s, against 57 ms for Fast). **Provisional** means: the wire format below is complete and tested, and an
independent implementation built from it interoperates with the reference implementation; but it has
not been validated on real show chains yet, and a later revision of AML v1 MAY change these
parameters (it would then also change the profile names). They need a **dedicated, clean line**: no
EQ (an in-band boost of +12 dB at 3 kHz breaks 4800 bit/s), no compression, gate or limiter, and no
dropouts for the uncoded profiles. 9600 bit/s needs more than ~6 kHz of bandwidth (digital lines);
2400 bit/s does not survive a 1 kHz high-pass.

| Profile | User-facing name | Bit rate | Half cell @ 48 kHz | Body | Burst | Burst + gap | Heartbeat / link-dead |
|---|---|---|---|---|---|---|---|
| `ltc2400` | — | 2400 bit/s | 10 samples | 16 nibbles (§4.3) | 31.9 ms | 33.5 ms | 500 / 2000 ms |
| `ltc` = `ltc4800` | — | 4800 bit/s | 5 samples | 16 nibbles | 15.9 ms | 16.8 ms | 500 / 2000 ms |
| `ltc9600` | — | 9600 bit/s | 2.5 samples | 16 nibbles | 8.0 ms | 8.4 ms | 250 / 1000 ms |
| `ltc2400_fec` | — | 2400 bit/s | 10 samples | 22 nibbles coded (§4.6) | 41.9 ms | 43.5 ms | 500 / 2000 ms |
| `ltc_fec` = `ltc4800_fec` | `clean-line` | 4800 bit/s | 5 samples | 22 nibbles coded | 20.9 ms | 21.8 ms | 500 / 2000 ms |
| `ltc9600_fec` | `clean-line-fast` | 9600 bit/s | 2.5 samples | 22 nibbles coded | 10.5 ms | 10.9 ms | 250 / 1000 ms |

The fail-safe timers (§5.3) follow the bit rate: a heartbeat costs at most 5 % of the idle line
(coded 9600: 10.9 ms per 250 ms), and from 9600 bit/s a dead line is declared in 1 s instead of 2 s
(`ltc14400*`, not in this spec: 250 / 1000 ms too; `digital`: 100 / 500 ms).

#### 6.5.1 Line code and frame

Biphase mark (as SMPTE LTC and AES3): every bit cell starts with a level transition; a `1` has a
second transition in the middle of the cell, a `0` has none. Information is in the transition times
only, so the code is polarity-free and DC-free. Between transitions there are only two intervals in
data: `H` (half a cell, half of a `1`) and `F` (a full cell, a `0`).

One burst, as a sequence of intervals between transitions, in half cells (the level alternates at
every transition; the first level is arbitrary, the reference transmitter starts positive):

| Part | Content | Intervals (half cells) |
|---|---|---|
| Preamble | 8 `1` bits | 16 × `1` |
| Sync | two code violations `V` of 1.5 cells, at opposite levels | `3`, `3` |
| Body | body nibbles (§4.3 or §4.6), each **MSB first**: 4 bits per nibble | `1`, `1` per `1` bit; `2` per `0` bit |
| Postamble | one `1` bit (terminates the last body bit with a transition) | `1`, `1` |
| Tail | one more half cell at the last level, then a fade to 0 | `1` |

The burst is `16 + 6 + 8 · nibbles + 2 + 1` half cells: 153 for 16 nibbles, 201 for 22. Valid data
only contains `H` and `F`, so `V V` cannot occur inside a body whatever the payload: the sync needs no
reserved pattern or bit stuffing. The transition that ends the second `V` is the start of the first
body bit. After each burst the transmitter sends **4 bit cells of silence** (`round(8 · h)` samples,
`h` = half-cell samples = `fs / (2 · bit_rate)`).

#### 6.5.2 Waveform

The signal is `± A / √2` (the same RMS as an MFSK tone of amplitude `A`; `A` = 0.25 by default, a
SHOULD as in §6). Each transition, including
the onset from 0 at the start and the fade to 0 after the tail, is a raised-cosine step of width
`w = 0.5` half cells centred on the nominal transition time. With time measured in half cells,
`t = (n − 0.25 · h) / h` for sample `n` (the first ramp starts at sample 0), `b = round(t)`,
`d = t − b`, and `L(j)` the level of half cell `j` (0 before the first and after the last):

```
s[n] = A/√2 · ( L(b−1) + (L(b) − L(b−1)) · (½ + ½ · sin(π d / w)) )     if |d| < w / 2
       A/√2 · L(floor(t))                                                 otherwise
```

for `n = 0 … ceil((H + w) · h)`, `H` = burst length in half cells. Half cells need not be whole
samples (2.5 at 9600 bit/s, 4.59 at 4800 bit/s and 44.1 kHz): transitions are placed at their exact
fractional positions. The repetition length for §4.4 is the burst samples plus the gap samples.

#### 6.5.3 Receiver (requirements and reference)

A receiver MUST accept either polarity and SHOULD decode bit rates within ±1 % of nominal (sample-rate
mismatch), signal levels from 0 dB down to −30 dB relative to the transmit level, and SHOULD ignore
input below the −60 dBFS silence floor. It MUST apply §4.7. The rest is the reference receiver
(SHOULD), whose parameters are relative to the bit rate:

- DC blocker (1-pole high-pass at 0.05 × bit rate), then a 2-pole Butterworth low-pass at
  1.5 × bit rate.
- **Sync:** half-cell box sums correlated, sign-free and energy-normalized, against the last 16
  preamble half cells plus the two violations; the best peak above **0.75** starts a body (snapped to
  the transition that ends the second violation when one lies within ½ half cell). The correlator keeps
  running during a body: a sync stronger by 0.05 (squared correlation), or one ≥ 0.95, replaces the
  lock, so a false lock never hides a real packet.
- **Bits:** soft decisions on a half-cell grid: a bit is `1` when its two half-cell integrals differ in
  sign; the normalized difference is the bit margin, a nibble's reliability is its weakest bit, and a
  nibble containing a dropout (bit energy below 0.25 × the running bit energy) is an erasure. The grid
  is a second-order timing loop pulled by transitions within 0.45 half cells of a grid point.
- **False-lock gates:** a completed body whose mean nibble margin is below 0.3, or in which fewer than
  60 % of transitions fall near the grid, is dropped before the link layer sees it.
- With a coded body, a body with more than 8 erased nibbles is abandoned.

Measured with the reference receiver (`docs/proposals/ltc-bmc.md` §4, §9): ≥ 99.8 % of packets at
4 dB SNR (`ltc`, `ltc_fec`), 1 ms / 3 ms dropouts survived by 96 % / 92 % with `ltc_fec` (uncoded:
22 % / 0 %); MIDI-in → MIDI-out NoteOn latency in loopback ~22 ms (`ltc`) and ~26 ms (`ltc_fec`).
The MFSK and BMC receivers never decode each other's audio, so a receiver MAY run both in parallel.

## 7. Release assumptions

- Dedicated mono path (analog XLR, Dante, MADI, virtual loop) treated like LTC
- Operators do not EQ/compress/limit the AML channel
- Gain staging is set once and left alone during the show
- GUI is the primary product (MIDI in/out + audio modem); CLI remains a lab tool

## 8. Observability metrics

Implementations should expose:

- Link state: `Search` | `Locked` | `Dead`
- CRC pass count / fail count / pass rate; of the failures, the wrong-key suspects (§4.7)
- Transmitter: queued messages, wait of the oldest, messages dropped (§5.5)
- Sync-lost count
- Last symbol confidence (dB over runner-up)
- Last sequence number
- Body symbols repaired by FEC (coded profiles)
- Estimated first-copy latency
- Audio ring-buffer drop count

## 9. Test vectors

Generated by the reference implementation and asserted by `cricket_core/tests/spec_vectors.rs`, so
this section and the code cannot drift apart. All values hex; nibbles are listed in wire order.

**Generator check:** RS(11,7) encoding of data `0 0 0 0 0 0 1` is `0 0 0 0 0 0 1 D C 8 7` (= g(x)).

| | NoteOn | MSC GO | Heartbeat |
|---|---|---|---|
| Message | Note On, channel 1, note 60, velocity 100 | MSC GO cue 14.5, list 2 | Heartbeat |
| SEQ | 0 | 1 | 2 |
| Packet bytes | `10 00 90 3C 64 01 B5` | `13 01 09 00 92 B0 5A` | `14 02 F2 FA` |
| CRC-16 | `01B5` | `B05A` | `F2FA` |
| Packet nibbles | `1 0 0 0 9 0 3 C 6 4 0 1 B 5` | `1 3 0 1 0 9 0 0 9 2 B 0 5 A` | `1 4 0 2 F 2 F A` |
| Codeword A | `1 0 0 0 9 0 3 · 2 9 7 B` | `1 3 0 1 0 9 0 · 5 0 4 4` | `1 4 0 2 F 2 F · 6 A 7 F` |
| Codeword B | `C 6 4 0 1 B 5 · 7 2 4 D` | `0 9 2 B 0 5 A · 9 6 8 A` | `A 0 0 0 0 0 0 · 3 5 C 4` |
| Coded body on air (Robust, `ltc*_fec`) | `9 8 3 9 6 E 8 8 1 4 A 8 B 8 1 6 A 3 E C 1 F` | `9 4 0 6 6 8 9 3 8 5 3 6 8 7 6 8 3 7 D 0 E 8` | `9 E 7 F 6 A A 8 7 5 8 3 7 D 5 2 9 4 E 4 5 6` |
| Uncoded body on air (Robust repeat, Fast, `ltc*`) | `1 9 6 B 0 0 4 5 0 3 0 0 0 C 1 0` | `1 0 9 5 3 9 2 A 0 0 B 0 1 0 0 0` | `1 F 0 0 4 2 0 0 0 F 0 0 2 A 0 0` |

(`·` separates data and parity.) The MSC payload `09 00 92` is command GO (`01`) in bits 23–19,
list 2 in bits 18–15 and value 146 (= cue 14.5 × 10 + 1) in bits 14–0 (§5.4).

On air, Robust sends the NoteOn as the 26 tones of `A 5 C 3 9 8 3 9 6 E …` (preamble, then the coded
body): 4166⅔, 3333⅓, 4500, 3000, 4000, 3833⅓, 3000, 4000, 3500, 4833⅓ Hz, … (2500 + 166⅔ k) then one
symbol period of silence. Robust repeat sends `A 5 C 3 1 9 6 B …` twice, Fast `A 5 1 9 6 B …`
(333⅓ Hz steps: 5833⅓, 4166⅔, 2833⅓, 5500, 4500, 6166⅔ Hz, …) once. The
LTC profiles send the same body nibbles, MSB first, after their sync: `ltc_fec`'s first body bits for
the NoteOn are `1001 1000 0011 1001 …`.

## 10. Design history

The rationale and measurements behind §3.2.2, §4.6, §4.7 and §6.5 are in
`docs/proposals/fec.md` (body FEC, false-accept analysis, joint acquisition) and
`docs/proposals/ltc-bmc.md` (biphase-mark phy). Those documents are informative; where they differ
from this specification, this specification applies.
