# AML v1 Specification (Bitstream Modem)

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
`aml-prototype-protocol.md`) remains available as a legacy debug phy only; it is not part of AML v1.

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
| Tone of nibble `k` (0–15) | 2500 + 100 `k` Hz (2500 … 4000 Hz) | 2500 + 200 `k` Hz (2500 … 5500 Hz) |
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

### 3.2 Receiver: detection and acquisition

This section describes the reference receiver. It is **SHOULD** throughout: the transmitted signal
does not depend on it, and a receiver MAY find and demodulate packets differently, as long as it
applies the validation of §4.7.

**Detection gate.** A Goertzel bank over the 16 tone frequencies is evaluated on a symbol window
(`N` = symbol samples). Detection is level-independent. A window shows nibble `k` when all hold:

1. **Silence floor:** window RMS ≥ `min_rms_dbfs` = −60 dBFS (full-scale sine = −3 dBFS). Quieter
   windows are never detected.
2. **Tone purity:** `purity = P_k / (E · N / 2) ≥ min_tone_purity`, where `P_k` is tone `k`'s Goertzel
   power, `k` is the strongest tone, and `E = Σ x²` the window energy. A steady sinusoid filling the
   window gives purity ≈ 1.0 at any amplitude; ramps, misalignment, other tones and noise lower it
   (white noise ≈ `2 / N` per bin). Thresholds: **0.25** (Robust, Robust repeat), **0.30** (Fast).
3. **Confidence:** `10 · log10(P_k / P_runner-up) ≥ c`, with `c = min_confidence_db` (**3.0 dB**
   Robust / Robust repeat, **2.0 dB** Fast) for the first preamble symbol under first-symbol
   acquisition, and `c = 0.5 · min_confidence_db` for every other symbol.

Measured purity of synthesized symbols (both timings): ≈ 0.97 aligned, ≈ 0.91 aligned at SNR 12 dB,
≥ 0.43 at a full search hop of misalignment; the best bin of white noise peaks at ≈ 0.09 (N = 288) /
≈ 0.18 (N = 144). Packets decode from 0 dB down to at least −30 dB relative to the transmit level.

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
   purity ≥ `0.5 · min_tone_purity`. The preamble locks when at most `M` symbols are not good
   (anywhere, including the first), at least one is good, and at most one is contradicting.
4. On a lock, body reading starts at `aligned + N · period`. A failed attempt resumes search at
   `t + hop`.

Noise or a dropout can take out `M` symbols, while a stream of unrelated tones still has to match
`N − M` preamble symbols in sequence. Measured on random MFSK symbols (other packets' bodies): ~13
(Robust) and ~10 (Robust repeat) phy locks per minute, each rejected by the link layer.

#### 3.2.3 Body symbols, erasures and resync

- After a lock, each body symbol is read one symbol period after the previous one and must pass the
  gate (half confidence).
- **Uncoded body:** the first body symbol that fails the gate abandons the attempt.
- **Coded body (§4.6):** a body symbol that fails the gate is kept as an **erasure** (its best guess
  and confidence are passed on), up to `2 · parity` = 8 per body; the next failure abandons the
  attempt. Every body symbol carries its confidence (dB over the runner-up) as the reliability for
  the decoder.
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

CRC-16/CCITT (poly `0x1021`, init `0xFFFF`, refin=false, refout=false, xorout=`0x0000`)
covers all bytes from `HDR` through end of `PAYLOAD` (excludes the CRC itself), transmitted high byte
first.

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
bodies MAY ignore their pad nibbles (the reference receiver does).

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

- Decoder marks link **dead** if no CRC-valid packet for **`link_dead_ms`** (default **2000 ms**)
  after the first valid packet. This **sample-clock** link-dead detector is timed in received
  samples: it fires at sample `end of last valid packet + link_dead_ms`, independent of how audio
  is chunked, and is reported in the processing call whose input contains that sample. It fires
  once per event; the next valid packet re-arms it. Link state reads **Dead** from that moment
  until the next valid packet.
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

**Fail-safe burst.** For each MIDI channel 1–16 in order: Sustain off (`0xBn 0x40 0x00`), then
All Notes Off (`0xBn 0x7B 0x00`), then — only on channels whose last Pitch Bend output by this
receiver was not center and not followed by Reset All Controllers — Pitch Bend center
(`0xEn 0x00 0x40`). That is 32–48 short messages (~31–46 ms on a 31.25 kbit/s MIDI cable).

- *All 16 channels*, not only channels seen since start: the receiver may have restarted while
  notes were sounding, and a stateless burst cannot miss a channel. It is the usual "MIDI panic"
  convention and harmless on channels without notes.
- *Sustain off first*, because All Notes Off does not release notes held by the pedal.
- *Pitch Bend center* only where this receiver left a bend: a pitch wheel springs back to center, so
  a lost "back to center" would detune every later note; other controllers have no rest position
  and are left alone.
- *Not sent:* Reset All Controllers (CC121; it also resets expression, modulation and pressure
  that a show may rely on, and its exact effect varies between devices), All Sound Off (CC120;
  it cuts release tails), and Note Offs per note.

### 5.4 MIDI Show Control

MSC 1.0 messages are SysEx: `F0 7F <device_id> 02 <command_format> <command> <data> F7`, where cue
numbers, lists and paths are ASCII digits with an optional `.` (`"14.5"` = `31 34 2E 35`), separated
by `00` (`<cue> 00 <list> 00 <path>`).

AML carries the **command and its data only**. The device ID and command format are **receiver
settings** (default device ID **`0x7F`** all-call, so every MSC device responds; default command format
**`0x01`** Lighting (General), the format lighting consoles listen for). A transmitter accepts MSC from
any device ID and in any command format.

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
| 2 | Note Off (any channel) and MSC, in arrival order |
| 3 | Everything else: Note On, Program Change, Control Change, Pitch Bend, pressure, system common |
| 4 | NoteOff refresh (idle keepalive; see below) |
| 5 | Heartbeat |

Messages of order 1 and order 2 are never coalesced or dropped.

**Order within tier 3.** *Continuous* messages — Control Change except Bank Select (CC 0, 32), Data
Entry (6, 38), Data Increment/Decrement (96, 97), NRPN/RPN select (98–101) and the channel mode
messages (120–127); Pitch Bend; channel pressure; polyphonic key pressure — are latest-value-wins.
All other tier-3 messages are *discrete* and keep their arrival order. The next tier-3 message is:
the oldest discrete message, unless a continuous message **of its channel** was queued before it,
in which case that continuous message goes first; with no discrete message queued, the oldest
continuous message. A note therefore waits for at most the controller values of its own channel that
were set before it (it is struck with them), never for those of other channels. System common
messages have no channel and keep their place relative to everything.

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
Reset All Controllers (CC 121) and Local Control (CC 122) are discrete tier-3 messages: moving them
ahead of queued controller values would change the resulting state.

**Heartbeat:** every **500 ms** only when fully idle: no message queued, no NoteOff held, and the
NoteOff refresh list is empty. While user messages or refreshes are pending, Heartbeat uses no
airtime.

**NoteOff refresh:** after a user Note Off is transmitted, it is repeated (same channel, note and
velocity) up to **6** times within **10 s** (max **32** most recent releases). When nothing higher in
the table is pending, each free packet sends a **Note Off** for one refresh entry instead of waiting
for Heartbeat: the note with the **fewest** refresh transmissions so far, then the **oldest**
original release. A **Note On** for that channel and note removes it from the refresh list. Refresh
never runs ahead of a queued message. Each release therefore costs a bounded burst of airtime (6
messages: about 0.35 s on Fast, 1.05 s on Robust, 1.6 s on Robust repeat, 0.13 s on `ltc_fec`) that a Note On arriving meanwhile may wait behind for
up to one message.

**Queue bounds:** queued **Note On** airtime is capped at **250 ms** (one on-wire message period per
queued Note On, plus one per Note Off held with it). Before accepting another Note On, the oldest
queued Note On is dropped until the budget fits, but at least **4** queued Note Ons are always kept,
so a chord survives even when one message is longer than the budget (Robust repeat, 273 ms). A Note Off held with a
dropped Note On is dropped too, unless the note is still sounding from an earlier strike. A Note Off
for a note already on the air, MSC, and Panic-class messages are never dropped. Continuous messages
are bounded by coalescing.

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
`digital-bit-exact` = `digital` (not in this spec). The uncoded LTC profiles keep their identifiers.

| | Robust (default) | Robust repeat | Fast | LTC family (§6.5) |
|---|---|---|---|---|
| User-facing name | `analog-safe` | `analog-safe-repeat` | `analog-fast` | `clean-line` (`ltc_fec`), `clean-line-fast` (`ltc9600_fec`); uncoded: `ltc*` |
| Phy | MFSK-16 (§3) | MFSK-16 | MFSK-16 | biphase mark, 2400 / 4800 / 9600 bit/s |
| Tones | 2500 + 100 k Hz | 2500 + 100 k Hz | 2500 + 200 k Hz | — |
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
| Purity / confidence gate (RX) | 0.25 / 3.0 dB | 0.25 / 3.0 dB | 0.30 / 2.0 dB | — |
| Body erasures kept (RX) | ≤ 8 | 0 | 0 | ≤ 8 coded, unlimited uncoded |
| Heartbeat / link-dead | 500 ms / 2000 ms | same | same | same |

Rows marked RX are reference receiver settings (§3.2, SHOULD); everything else is wire format (MUST),
except the amplitude: receivers are level-independent (§3.2, silence floor −60 dBFS), so a
transmitter SHOULD send at `A` = 0.25 (−12 dBFS peak, headroom on the bus) and MAY use another level
between −30 dB below that and full scale. (Before v1.1 the reference transmitter used 0.8.)

### 6.1 Robust (default)

MFSK-16 at 6.5 ms per symbol period, preamble `A 5 C 3`, the coded 22-nibble body of §4.6, sent
**once**. One copy is 26 symbols (169 ms); with the trailing silence a message holds the line for
175.5 ms. Reference receiver: joint acquisition with 2 misses (§3.2.2), up to 8 body erasures.

Use it when the chain is unknown or may include converters, patchbays, mild distortion or noise. In the
channel simulator it matches or beats Robust repeat in every condition except dropouts of ≳ 50 ms
(e.g. AWGN −3 dB: 97 % vs 46 %; 5 % wrong tones: 97 % vs 75 %; 60 ms dropout: 72 % vs 92 %), with
36 % less airtime and ~25 % lower NoteOn latency under load (`docs/proposals/fec.md` §10).

### 6.2 Robust repeat

Robust's tones, timing and preamble with the **uncoded** 16-nibble body (§4.3), sent **twice** (K = 2).
One copy is 20 symbols (130 ms); a message holds the line for 2 × 136.5 = 273 ms. Reference receiver:
joint acquisition with 1 miss.

Use it on lines with glitches or dropouts of about 50 ms or more: a dropout that long erases more of a
single coded body than the code can fill, while a second copy survives it (60 ms dropouts: 92 %
delivered vs 72 % for Robust).

### 6.3 Fast

For clean dedicated chains only (no EQ / compression / limiter on the AML bus).

| Parameter | Value |
|-----------|-------|
| Symbol duration | 3.0 ms |
| Guard gap | 0 |
| Attack / release | 0.15 ms |
| Tone spacing | 200 Hz (wider for the short Goertzel window) |
| Preamble | `A 5` (2 symbols) |
| Body | 16 nibbles, uncoded (§4.3) |
| Repetition K | 1 |
| Confidence (RX) | 2.0 dB |
| Min tone purity (RX) | 0.30 |
| Acquisition (RX) | first symbol (§3.2.1) |

One copy for any packet is 18 symbols (54 ms).

### 6.4 Throughput (airtime budgets)

Airtime per message sets a practical sustained rate on a dedicated chain: Robust (**175.5 ms**)
about **5 messages/s**, Robust repeat (**273 ms**) about **3–4 messages/s**, Fast (**57 ms**) about
**17 messages/s**, `ltc_fec` (**21.8 ms**) about **45 messages/s**. A chord of up to 4 notes is sent
whole (later notes wait about one message each). The TX scheduler keeps **queue** latency under
**250 ms** by dropping queued NoteOns beyond that budget (§5.5). These are on-wire airtime budgets
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

| Profile | User-facing name | Bit rate | Half cell @ 48 kHz | Body | Burst | Burst + gap |
|---|---|---|---|---|---|---|
| `ltc2400` | — | 2400 bit/s | 10 samples | 16 nibbles (§4.3) | 31.9 ms | 33.5 ms |
| `ltc` = `ltc4800` | — | 4800 bit/s | 5 samples | 16 nibbles | 15.9 ms | 16.8 ms |
| `ltc9600` | — | 9600 bit/s | 2.5 samples | 16 nibbles | 8.0 ms | 8.4 ms |
| `ltc2400_fec` | — | 2400 bit/s | 10 samples | 22 nibbles coded (§4.6) | 41.9 ms | 43.5 ms |
| `ltc_fec` = `ltc4800_fec` | `clean-line` | 4800 bit/s | 5 samples | 22 nibbles coded | 20.9 ms | 21.8 ms |
| `ltc9600_fec` | `clean-line-fast` | 9600 bit/s | 2.5 samples | 22 nibbles coded | 10.5 ms | 10.9 ms |

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

## 7. Release assumptions (v1.0)

- Dedicated mono path (analog XLR, Dante, MADI, virtual loop) treated like LTC
- Operators do not EQ/compress/limit the AML channel
- Gain staging is set once and left alone during the show
- GUI is the primary product (MIDI in/out + audio modem); CLI remains a lab tool

## 8. Observability metrics

Implementations should expose:

- Link state: `Search` | `Locked` | `Dead`
- CRC pass count / fail count / pass rate
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
body): 3500, 3000, 3700, 2800, 3400, 3300, 2800, 3400, 3100, 3900 Hz, … then one symbol period of
silence. Robust repeat sends `A 5 C 3 1 9 6 B …` twice, Fast `A 5 1 9 6 B …` (200 Hz steps) once. The
LTC profiles send the same body nibbles, MSB first, after their sync: `ltc_fec`'s first body bits for
the NoteOn are `1001 1000 0011 1001 …`.

## 10. Design history

The rationale and measurements behind §3.2.2, §4.6, §4.7 and §6.5 are in
`docs/proposals/fec.md` (body FEC, false-accept analysis, joint acquisition) and
`docs/proposals/ltc-bmc.md` (biphase-mark phy). Those documents are informative; where they differ
from this specification, this specification applies.
