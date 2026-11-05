# Proposal: bit-transparent digital phy ("digital" profile)

Status: **experimental.** Not part of the AML v1 spec; a candidate for a future §6.6. The code is in
`cricket_core/src/phy/digital.rs` (phy), `cricket_core/src/pathcheck.rs` (path check),
`cricket_engine/src/pathcheck.rs` (path check on devices) and is selected with `PhyConfig::digital()` /
`--profile digital`. Every other profile is unchanged on the wire. The packets are the AML v1 link-layer
packets (spec §4.1); only the phy is new.

## 1. Motivation and scope

Many show chains carry audio as untouched PCM: a Dante or AES67 flow routed device to device, a MADI or
AES3 link, a USB interface in a bit-exact mode. On such a path the receiver gets the very sample values
the transmitter wrote. The other profiles spend their airtime on surviving an analog channel (tones,
preambles, biphase-mark cells); a bit-transparent path needs none of that. Like SMPTE ST 337 (data in
AES3 words), the `digital` profile writes the packet straight into sample values:

* a packet is **21 samples** (0.44 ms at 48 kHz) and is sent twice back to back: **0.88 ms on air**;
* measured MIDI-in → MIDI-out NoteOn latency through the engine **0.5–0.7 ms mean** (ltc14400: 5.5 ms,
  Fast: ~80 ms; §9), **~1,150 messages/s**, a 4-note chord complete after **3.3 ms**;
* the signal peaks at **−57.4 dBFS**, so a misrouted channel is a faint tick, not a blast;
* on a path that changes the samples the frames simply fail: **no wrong MIDI** in any simulated
  condition (§5), and a path check tells the operator before the show (§8).

It is not for analog lines, lossy codecs, or anything with gain, EQ, dynamics, sample-rate conversion
or mixing in the path: use `ltc9600_fec`, Fast or Robust there.

## 2. Sample layout

### 2.1 Codes, symbols and the bit diagram

Samples are read as 24-bit two's complement **codes**: `code = x · 2²³` for an `f32` sample `x`. Each
sample carries one **symbol** `v`, a small signed integer, as `code = v · 1024`, i.e. the symbol sits in
bit 10 and up and bits 0–9 are zero:

```text
 bit of the 24-bit code   23 ............ 14 | 13 12 11 10 |  9  8 |  7 ........ 0
 content                  sign extension of v |  symbol v   | guard  | zero
 same bits in a 16-bit word 15 ............ 6 |  5  4  3  2 |  1  0 |  (not carried)
```

Example, symbol −3: code −3072 = `0xFFF400` (24-bit) = `0xFFF4` (16-bit) = `-0.0003662109375f32`.

| symbols | meaning |
|---|---|
| −8 … +7 | data: one packet nibble as 4-bit two's complement (`0x0..0x7` → 0..7, `0x8..0xF` → −8..−1) |
| ±9, ±10, ±11 | control: sync and END. No nibble maps to them in either polarity |
| ±12 and beyond, anything off the grid | invalid (aborts a frame) |

### 2.2 Decision rule and tolerance

The receiver decides `v = round(code / 1024)` and accepts the sample only if
`|code − v · 1024| ≤ 320` (1.25 LSB of a 16-bit word). Between the windows of two neighbouring symbols
lies a **guard band** of 384 codes (`320 < |residual| < 704`): an error must exceed **704 codes
(2.75 LSB16, a −81.5 dBFS step)** before a sample can read as a *different* symbol; anything less either
reads correctly or fails the frame.

| what the path does | error per sample (codes) | result |
|---|---|---|
| nothing (bit-exact, any word length ≥ 16 bits, float with power-of-two scaling) | 0 | exact (margin 54 dB, see `digital::margin_db`) |
| 16-bit truncation or rounding | 0 (bits 0–7 are zero already) | exact |
| float stage scaled by 2²³ − 1 with truncation | ±1 | passes |
| 16-bit conversion scaled by 32767 with truncation (old drivers, Cricket before 1.1) | up to 256 | passes |
| ±1 LSB16 of TPDF dither | up to 256 | passes (margin ~6 dB) |
| DC offset of 1 LSB16 | 256 | passes |
| noise at −120 / −100 dBFS RMS (σ = 8 / 84 codes) | 3.8 σ = 320 at −100 dBFS | passes / marginal (0.32 % of frames lost) |
| gain ±0.1 dB | ≤ 131 at the peak symbol | passes |
| gain beyond ±0.24 dB, SRC, drift, EQ, 8-bit word | > 320 | frames dropped |

Pure gain cannot produce a wrong frame: a sync is recognised only if all of +9, −11, +10, −9, +11, −10
land in their windows, which needs |gain − 1| ≤ 320 / 11264 (±0.24 dB); within that range every data
symbol (|v| ≤ 8) still rounds to itself.

### 2.3 Level

The largest symbol is 11 · 1024 = 11,264 codes: a frame peaks at **−57.4 dBFS** (data alone −60.2 dBFS,
about −61 dBFS RMS over a frame). Between messages the channel carries digital silence (zeros) except a
Heartbeat every 500 ms (30 samples). The path-check pattern (§8) peaks at −48.2 dBFS.

### 2.4 f32, integer formats and scaling in Cricket

Workers see `f32`. A 24-bit code is exact in `f32` (24-bit mantissa), so the layout survives any number
of `f32` stages that scale by a power of two. The engine (`cricket_engine/src/audio.rs`) now:

* converts device integers itself with **power-of-two scaling and rounding**: `i16` ÷ 2¹⁵, `i32` ÷ 2³¹,
  `u16` offset by 2¹⁵; `f32`/`f64` pass through. The 24-bit value of an `i32` stream round-trips
  exactly (`integer_conversions_are_exact_for_24_bit_values`). Before, `i16` was scaled by 32767 and
  truncated, `I32` streams were not supported, and the callbacks allocated;
* opens the device for the `digital` profile in **`I32` if offered, else `F32`, else `I16`**
  (`--sample-format auto`), to keep conversions in the driver to a minimum; `--sample-format
  f32|i16|i32` forces one. cpal 0.15 has no packed 24-bit format; 24-bit devices are offered as `I32`
  (left-justified) or `F32` by the host (on ALSA, a device that only offers packed 24-bit needs its
  `plughw:` name).

What happens below cpal is the host's business (§7). `cricket pathcheck` measures exactly that
end-to-end, through the same open path as the workers.

## 3. Framing

### 3.1 Frame

```text
| SYNC: +9 −11 +10 −9 +11 −10 | BODY: packet nibbles, high nibble first | END: −11 |
       6 samples                 14 samples (Midi, Msc) / 8 (Heartbeat, Panic)    1 sample
```

The body is the link-layer packet itself (spec §4.1/§4.2: 7 or 4 bytes) without padding,
interleaving, FEC or whitening. Its length follows from the type nibble (second body sample); the END
symbol must follow the last nibble. A frame is **21 samples** (Heartbeat / Panic: 15). The
transmitter sends each packet **K = 2** times back to back and nothing between frames
(`repetition_gap_samples` = 0); the idle channel is digital silence.

### 3.2 Sync and alignment

* The sync uses only control symbols, which never occur in a body: **a sync cannot start inside a frame**
  (a code violation, as in AES3 and the BMC phy), so the receiver needs no rewind or resync logic.
* Alignment is per sample: the receiver checks the last six symbols at every sample, so a frame is found
  at any offset and any chunking (`roundtrip_all_kinds_chunks_offsets_polarity`).
* **Polarity:** the sync is also accepted negated; the frame is then negated. A polarity-inverting path
  works unchanged (`polarity` row of §5).
* Every frame sample must pass the tolerance; one failed sample drops the frame.

### 3.3 Validation and duplicates

A structurally complete frame (sync, in-range body of the right length, END) is handed to the modem,
which applies the full packet validation of spec §4.7: CRC-16/CCITT, version `1`, known type, `SEQ`,
reserved zero bits and payload ranges. Duplicate suppression follows spec §4.4 with the digital frame
as the repetition: window (2 + 0.5) × 21 = **52 samples (1.1 ms)**. Identical packet bytes recur only
after 4 messages (`SEQ` is 2 bits), at least 4 × 30 = 120 samples later, so the window never hides a
genuine message.

### 3.4 Repetition: K = 2, no FEC

FEC would need a longer body for little gain: on a bit-exact path symbols do not get *slightly* wrong,
a frame is either exact or hit by an event (a slipped, corrupted or muted sample) that usually takes out
more than a nibble. A second copy is cheap (0.44 ms) and covers those events. Measured
(`digital_report`, 5000 random messages, events at 1 per 10 ms, i.e. ~100× worse than a sick network):

| event | K = 1 delivered | K = 2 delivered | false messages |
|---|---|---|---|
| 1 corrupted sample / 10 ms | 96.36 % | 99.84 % | 0 |
| 1 dropped sample (clock slip) / 10 ms | 96.08 % | 99.84 % | 0 |
| 1 repeated sample (clock slip) / 10 ms | 96.44 % | 99.88 % | 0 |
| 1 ms muted (lost packet) / 100 ms | 98.78 % | 99.14 % | 0 |

A gap between the copies would help against a lost 1 ms network packet (both copies fit into one
packet now), but costs ~1 ms per message in a chord and only matters on a network that is already
losing packets. Lost releases are covered by the NoteOff refresh instead (§3.5).

### 3.5 Scheduler pacing

* **Chords:** the encoder encodes every queued user message whose audio fits below the 10 ms refill
  threshold at once (spec §5.5), so the notes of a chord leave back to back in the next audio callback:
  0.88 ms per note, the last of 4 notes 2.6 ms after the first (MIDI DIN: ~1 ms per note).
* **Idle traffic** (heartbeats, NoteOff refreshes) is encoded only when less than one message is
  buffered, so a note never waits behind more than one of them (≤ 0.88 ms).
* **NoteOff refresh spacing (new):** spec §5.5 sends the 6 refreshes of a release in back-to-back free
  packets. With 0.88 ms messages that is 6 ms in all, and one short dropout loses the release
  completely (a stuck note). For `digital` the scheduler keeps refreshes **20 ms apart**
  (`TxScheduler::with_refresh_spacing`, `modem::refresh_spacing_samples`), so they span ~120 ms
  (Fast: ~350 ms). Test `spaced_refresh_survives_a_dropout`: back to back, a 10 ms dropout from the
  NoteOff on leaves the note stuck; spaced, the note is released after a 100 ms dropout. Every other
  profile keeps spacing 0 (unchanged). Heartbeats still wait for an empty refresh list.
* **Heartbeat** every 500 ms when idle, as in every profile: link-dead (2 s) and the fail-safe burst
  work unchanged.

### 3.6 Running next to another profile: `+digital`

A decoder profile with the suffix `+digital` (e.g. `ltc9600+digital`) runs the digital frame detector
next to that profile's own receiver, with its own duplicate history, so a transmitter can switch
between `ltc9600` and `digital` without touching the receiver (`coexists_with_other_profiles`). The
`digital` decoder itself ignores MFSK and BMC audio (§4.3).

## 4. False accepts

Target as in `fec.md` §9.2: **at most one false message per 10 years of continuous worst-case audio per
receiver.**

### 4.1 Model

A false message needs, at one sample position: a sync (6 samples in their windows, either polarity),
a body whose 8 or 14 nibbles spell a *valid* packet (version, type, `SEQ`, reserved bits, payload
ranges and CRC-16 all correct; 9,158,372 such packets exist), and END. For white Gaussian noise of σ
codes on a 24- or 16-bit grid the probability of each sample landing in a given symbol's window is
exact (`p_symbol`), and the product over the frame summed over all valid packets gives the false-message
probability per sample position (`analytic_white` in `cricket_core/tests/digital.rs`).

### 4.2 Analytic rate, white noise (the worst level for this phy is σ ≈ 8–10 steps, −58 to −60 dBFS)

| grid | σ (steps of 1024) | level dBFS | P(sync) per sample | syncs per 10 years | P(valid body) | years per false message |
|---|---|---|---|---|---|---|
| 24-bit | 5 | −64.3 | 1.8 × 10⁻¹³ | 2.7 | 4.6 × 10⁻¹¹ | 1.8 × 10¹³ |
| 24-bit | 8 | −60.2 | 1.7 × 10⁻¹¹ | 250 | 3.3 × 10⁻¹² | 1.0 × 10¹² |
| 24-bit | 10 | −58.3 | 2.4 × 10⁻¹¹ | 359 | 7.3 × 10⁻¹³ | 2.8 × 10¹² |
| 24-bit | 16 | −54.2 | 8.9 × 10⁻¹² | 135 | 2.3 × 10⁻¹⁴ | 2.6 × 10¹⁴ |
| 16-bit | 5 | −64.3 | 5.3 × 10⁻¹³ | 8.0 | 2.0 × 10⁻¹⁰ | 1.2 × 10¹² |
| 16-bit | 8 | −60.2 | 4.9 × 10⁻¹¹ | 741 | 1.4 × 10⁻¹¹ | **6.7 × 10¹⁰** (worst) |
| 16-bit | 10 | −58.3 | 7.0 × 10⁻¹¹ | 1,061 | 3.1 × 10⁻¹² | 1.9 × 10¹¹ |
| 16-bit | 25 | −50.3 | 3.6 × 10⁻¹² | 55 | 3.1 × 10⁻¹⁵ | 5.4 × 10¹⁵ |

Quieter noise never reaches the control symbols (σ = 3 steps: 10²¹ years); louder noise rarely lands
on the grid. Even the worst case is **~7 × 10⁹ times better than the target**: a sync from noise
happens a few hundred times in 10 years, and then the body has to be one of 9 million valid packets out
of 16¹⁴ nibble strings, with a CRC on top.

### 4.3 Measured: foreign audio (60 s each, `digital_report`)

| signal | sync prefix of 2 / 3 / 4 symbols | syncs | frames | messages |
|---|---|---|---|---|
| white noise −70 / −60 / −58 / −55 / −40 / 0 dBFS, 24- and 16-bit | up to 1,867 / 26 / 2 | 0 | 0 | 0 |
| music-like audio −20 / −40 / −60 dBFS RMS, 24- and 16-bit | up to 110 / 1 / 0 | 0 | 0 | 0 |
| Robust, Fast, `ltc`, `ltc9600_fec`, `ltc14400`, `ltc14400_fec` traffic, back to back | 0 | 0 | 0 | 0 |

The longest accidental sync prefix in 18 minutes of noise and music was 4 of 6 symbols. MFSK and BMC
traffic never even matches the first sync symbol: their samples are far louder than −57 dBFS except near
zero crossings, and the tests (`foreign_audio_never_becomes_a_message`) assert zero syncs for them.
The `fec.md` §9 harness for the other profiles is unaffected (their receivers do not run the digital
detector unless `+digital` is given).

### 4.4 Genuine frames on a damaged path

On a path within tolerance every symbol reads exactly, so there is nothing to mis-deliver. Outside it, a
sample must move ≥ 704 codes and land within ±320 of another symbol; a frame with such a sample still
needs the CRC-16, which detects every error burst of up to 16 bits (so any single wrong nibble, and
any two within four positions) and passes at most 2⁻¹⁶ of the wider corruptions, plus the §4.7 structure
checks where the damage reaches the header. In the 22 simulated paths and the glitch table (§3.4),
**zero false messages** were delivered (~73,000 messages).

## 5. Fail safe on paths that are not bit-exact

`fail_safe_table` (a test) sends 1,500 random messages (all kinds, K = 2) through each simulated path
(`pathcheck::PathImpairment`) and decodes them; `analyze` runs the path check on the same path. No path
may produce a false message; transparent ones must deliver everything.

| path | delivered | false | `cricket pathcheck` verdict |
|---|---|---|---|
| bit-exact | 1500 / 1500 | 0 | BIT-EXACT |
| delay 1 sample (sample offset) | 1500 / 1500 | 0 | BIT-EXACT |
| polarity inverted | 1500 / 1500 | 0 | BIT-EXACT (inverted) |
| 16-bit truncation | 1500 / 1500 | 0 | DigitalOk |
| 16-bit with TPDF dither | 1500 / 1500 | 0 | DigitalOk |
| float round trip scaled by 2²³ − 1, truncated | 1500 / 1500 | 0 | DigitalOk |
| 16-bit scaled by 32767, truncated | 1500 / 1500 | 0 | DigitalOk |
| noise −120 dBFS | 1500 / 1500 | 0 | DigitalOk |
| noise −100 dBFS | 1500 / 1500 | 0 | Marginal |
| DC +1 LSB16 | 1500 / 1500 | 0 | DigitalOk |
| gain +0.1 dB | 1500 / 1500 | 0 | DigitalOk |
| gain −0.1 dB | 1500 / 1500 | 0 | DigitalOk |
| gain +0.5 dB | 0 / 1500 | 0 | Unusable |
| gain −1 dB | 0 / 1500 | 0 | Unusable |
| gain −0.2 dB + 16-bit TPDF | 1137 / 1500 | 0 | Unusable |
| noise −80 dBFS | 0 / 1500 | 0 | Unusable |
| DC +2 LSB16 | 0 / 1500 | 0 | Unusable |
| drift +10 ppm (unlocked clocks) | 0 / 1500 | 0 | Unusable |
| SRC 48 k → 44.1 k → 48 k | 0 / 1500 | 0 | Unusable |
| EQ +3 dB at 10 kHz | 0 / 1500 | 0 | Unusable |
| EQ +1 dB at 1 kHz | 1497 / 1500 | 0 | Unusable |
| 8-bit truncation | 0 / 1500 | 0 | Unusable |

**Pass** (everything delivered): bit-exact, sample offset, polarity, 16-bit truncation with or without
TPDF dither, 2²³ − 1 and 32767 scaling, noise ≤ −100 dBFS, DC ≤ 1 LSB16, gain within ±0.1 dB.
**Fail safely** (nothing or part delivered, never wrong): gain ≥ 0.2 dB, SRC, clock drift, EQ, noise
≥ −80 dBFS, 8-bit paths. The path check is deliberately stricter than the receiver: a path that only
just passes (−0.2 dB with dither, a mild EQ) is reported unusable, because it fails at random.

## 6. Failure modes

| failure | what happens | what the operator sees |
|---|---|---|
| any processing in the path (gain, EQ, dynamics, SRC, mixing, a fader not at unity) | frames dropped | no MIDI; link state Search; link-dead after 2 s → fail-safe burst (All Notes Off) |
| marginal path (noise near −100 dBFS, gain ~0.2 dB) | some frames dropped at random | occasional missing messages; path check says Marginal / Unusable |
| unlocked clocks (two word clocks, PTP not converged) | sample slips; between slips frames pass | rare losses with an SRC, steady losses with slips; path check shows drift in ppm |
| packet loss on the network (zero-filled) | messages hit by the gap lost (both copies in one 1 ms packet) | releases still arrive via spaced refresh (§3.5); notes struck during the gap are lost |
| packet-loss concealment by **repeating** audio | a frame repeated > 52 samples later is a second, identical message (a duplicate, never a wrong one) | rare doubled NoteOn / MSC; Dante and most AES67 receivers zero-fill instead |
| other audio mixed in (system sounds, a talkback bus) | frames under the other audio fail | missing messages while the other audio plays |
| the channel is monitored on speakers | a faint tick per message at −57 dBFS | mute the channel on the monitor bus |
| wrong channel or device | nothing decodes | path check: NO SIGNAL |

## 7. Path requirements

The whole chain from Cricket's process to the far Cricket's process must deliver the 16 most
significant bits of each sample unchanged (24 bits are better: they leave the full margin). Check every
new chain with `cricket pathcheck` (§8) before relying on it.

**Network audio (Dante, AES67, Ravenna) and MADI / AES3:** route the channel device to device with no
processing; all devices locked to one clock (PTP leader / word clock), no sample-rate converters (AES3
inputs often have SRC switched on; some AVIO adapters and format bridges convert), sample rate 48 kHz
everywhere (the profile also runs at 96 kHz, but both ends must agree). Software routers such as Dante
Via may mix or resample: check them; a plain Dante Controller route between devices is transparent.
Packet loss on a healthy network should be zero; check the receive-error counters if messages go
missing.

**Consoles:** use a **direct out** or a pure patch, tapped before any processing: digital trim at 0 dB,
no HPF, EQ, dynamics, delay, fader or pan in the path, and no channel or bus that sums other sources. A
floating-point console engine at unity gain is normally transparent; output dither at 24 bits (±1 LSB24)
is harmless.

**USB interfaces:** set the device to 48 kHz / 24-bit, output volume and any software mixer
("DSP mixer", "loopback") to unity / bypass, and the input gain on a digital input to 0 dB. Analog
inputs and outputs are never bit-exact: only digital I/O (AES3, S/PDIF, ADAT, MADI, network) is.

**Windows:** Cricket's audio goes through cpal's WASAPI **shared mode**, i.e. the Windows audio engine:
it mixes every application's sound into the device, applies the device volume and any "audio
enhancements" (APOs), and converts whenever the stream's format differs from the device's shared-mode
format. That can be
bit-transparent (one stream, 100 % volume, enhancements off, the device's default format set to
24-bit 48 kHz in the Sound control panel, system sounds off for that device) but nothing guarantees
it. WASAPI exclusive mode and ASIO would bypass the mixer; this build supports neither (cpal's ASIO
backend needs the ASIO SDK and a build feature). With Dante Virtual Soundcard, its WDM mode goes through
the same mixer. Run the path check, and prefer macOS or Linux for the digital profile until exclusive
mode / ASIO are added.

**macOS (CoreAudio):** CoreAudio works in `f32` and converts to the device's integer format with
power-of-two scaling, so a device at the right rate is normally transparent. Conditions: the device's
format in Audio MIDI Setup is 48 kHz (otherwise CoreAudio resamples), its output volume is at maximum
(software volume is applied digitally) and the device is not an aggregate device with drift correction
(that is an SRC) or the system's alert-sound device. Dante Virtual Soundcard on macOS is a CoreAudio
device and transparent under these conditions.

**Linux:** pick the ALSA hardware device (`hw:` / `plughw:` names from `cricket list-devices`), not
`default`: PulseAudio and PipeWire mix, apply volume and resample. Set the interface's mixer controls to
unity.

**Latency:** `--buffer-frames N` sets the device buffer (cpal `BufferSize::Fixed`, clamped to what the
device allows; on ALSA the buffer is split into 4 periods). The host default can be long (ALSA in cpal:
100 ms buffer, 25 ms periods). 128–256 frames is a good start; go lower until the decoder reports
dropped input samples or the output crackles.

## 8. Path check: `cricket pathcheck`

The transmitter plays a periodic test pattern in the digital layout (period 100 ms: a probe frame no
receiver delivers — version `0xF` —, silence, random profile symbols on even samples and random 16-bit
values with all low bits on odd samples; peak −48.2 dBFS). The receiver records the far end, aligns to
the pattern (either polarity) and reports: bit-exact samples, digital symbols decoded with their worst
error against the 320-code tolerance, probe frames decoded by the real receiver, and findings: gain,
word length and requantization (truncation, rounding, dither or a 32767 conversion), DC offset, clock
drift / SRC in ppm, waveform changes (EQ, filters), polarity, residual noise.

```sh
# far end of the path, two machines:
cricket pathcheck send --audio-out-name "Dante TX 1-2"                   # until Ctrl-C
cricket pathcheck receive --audio-in-name "Dante RX 1-2" --seconds 5
# both ends on one machine (output -> path -> input):
cricket pathcheck loop --audio-out-name "MADI 1" --audio-in-name "MADI 1" --seconds 5
# a recorder or DAW in the path: write the pattern, play it through, analyse the recording
cricket pathcheck send --wav-out pattern.wav --seconds 10
cricket pathcheck receive --wav-in recording.wav
# no hardware: simulated paths
cricket pathcheck sim            # the table
cricket pathcheck sim 16bit-tpdf # one report
```

`--sample-format` and `--buffer-frames` apply as for `encode`/`decode`; the devices are opened the way
the `digital` profile opens them. The exit status of `receive` and `loop` is 1 when there is no signal
or the path is unusable, so it can gate a pre-show script. Example reports (simulated):

```text
Cricket path check: analysing pattern.wav (144000 samples at 48000 Hz, channel 1)
  Test signal: found (correlation 1.0000, polarity normal)
  Compared: 143936 samples (3.0 s)
  Bit-exact samples: 143936 of 143936 (100.000 %)
  Digital profile symbols decoded: 71504 of 71504 (worst error 0 of 320 codes allowed)
  Digital probe frames decoded: 29 of 29
  Verdict: BIT-EXACT. The `digital` profile will work on this path.

Cricket path check, simulated path `16bit-tpdf` ([WordLength { bits: 16, mode: Tpdf }]):
  ...
  Bit-exact samples: 54637 of 143936 (37.959 %)
  Digital profile symbols decoded: 71504 of 71504 (worst error 256 of 320 codes allowed)
  Digital probe frames decoded: 29 of 29
    - Word length: 16 bit, reduced by ±1 LSB requantization error (dither, or a 32767-scaled conversion) (the digital profile only needs 16 bits)
  Verdict: NOT BIT-EXACT, but the `digital` profile survives it (margin: worst error 256 of 320 codes). Fix the cause above if you can; any further change will break the link (safely: no wrong MIDI, just no MIDI).

Cricket path check, simulated path `drift+10ppm` ([Resample(1.00001)]):
  ...
  Digital profile symbols decoded: 1538 of 71504 (worst error 320 of 320 codes allowed)
  Digital probe frames decoded: 0 of 29
    - Sample clock drifts by +9.9 ppm: sample-rate conversion or unlocked clocks (sync both ends to one word clock / PTP leader, disable SRC)
    - Residual after gain and offset: -56.4 dBFS RMS (noise, dither or processing)
  Verdict: NOT USABLE for the `digital` profile (its receiver would drop frames). Fix the cause above or use ltc9600_fec / robust on this path.
```

Verdicts: **BIT-EXACT**; **DigitalOk** (not bit-exact, every symbol and probe within tolerance);
**Marginal** (< 0.1 % symbol errors, ≥ 95 % probes; the report estimates frame and message losses);
**Unusable**; **NO SIGNAL**. The gain fit is joint with the DC offset and is reported only while the
alignment holds (no drift, correlation ≥ 0.99) and no requantization was found (whose bias would
otherwise read as a small gain).

## 9. Latency

### 9.1 Engine audit (`cricket_engine`)

| before | after |
|---|---|
| Encoder polled its queue every 5 ms (`sleep(TICK)`): 0–5 ms before a MIDI message was even looked at | MIDI input and `EncoderHandle::send` wake the worker through a condition variable |
| One message encoded per 5 ms loop: a 4-note chord on a sub-millisecond profile took 15 ms+ | every queued user message that fits below the 10 ms refill threshold is encoded at once: the chord leaves in the next audio callback |
| Idle refreshes / heartbeats encoded whenever < 10 ms was buffered: a note could wait behind two of them on LTC profiles | idle messages only when less than one message is buffered |
| Audio pushed sample by sample, with a 1 ms sleep whenever the ring was full | slices; the wait is computed from the buffered audio (0.5–5 ms) |
| Decoder slept 5 ms whenever no input was buffered: 0–5 ms after the callback before decoding | the cpal input callback unparks the decoder (`AudioSource::wait`); decoding starts within microseconds |
| cpal default buffer (ALSA: 100 ms, 25 ms periods) | `--buffer-frames N` (cpal `BufferSize::Fixed`, clamped; falls back to the default if refused) |
| `i16` ↔ `f32` scaled by 32767 and truncated, `I32` unsupported, a `Vec` allocated per callback | power-of-two scaling with rounding for `i16`/`i32`/`u16`/`f64`, no allocation in callbacks; `I32` preferred for `digital`; `--sample-format` |
| Hardware-free loopback cable credited up to 5 ms of idle time to the next packet (it arrived faster than its airtime) | an ideal device: a burst plays from the moment it is queued (stricter, so "after" numbers are if anything pessimistic) |

`cricket loopback --zero-airtime` (audio delivered as soon as written) measures the engine's own
overhead: **0.23–0.41 ms mean** (digital 0.23–0.25, ltc14400 0.25, ltc14400_fec 0.30, ltc9600 0.30,
ltc_fec 0.33, Fast 0.41). Before, each side's 5 ms polling added 0–5 ms (≈ 2.5 ms mean) on top.

### 9.2 Loopback (release build, in-process, 60 NoteOns held 150 ms and released, 48 kHz)

`cricket loopback --profiles digital,ltc14400,ltc14400_fec,ltc,ltc9600,ltc_fec,fast,robust --notes 60`,
before = `main` at `0c79dc7` (with the `ltc14400` profiles), after = with this work on top, same
machine, one run each:

| profile | airtime per message | before: mean / p95 / jitter | after: mean / p95 / jitter |
|---|---|---|---|
| **digital** | 0.88 ms (first copy 0.44 ms) | — | **0.66 / 0.79 / 0.09 ms** (other runs: 0.53–0.72 ms mean) |
| ltc14400 | 5.4 ms | 11.3 / 15.4 / 2.6 ms | **5.53 / 5.63 / 0.08 ms** |
| ltc14400_fec | 7.0 ms | 12.1 / 15.3 / 2.2 ms | **7.20 / 7.32 / 0.07 ms** |
| ltc9600 | 8.4 ms | 13.1 / 15.6 / 2.3 ms | **8.16 / 8.33 / 0.09 ms** |
| ltc (4800) | 16.9 ms | 21.8 / 25.6 / 2.2 ms | **16.1 / 16.2 / 0.06 ms** |
| ltc_fec | 21.7 ms | 26.2 / 30.3 / 3.0 ms | **21.3 / 22.3 / 0.69 ms** |
| fast | 57 ms | 81.9 / 116.5 / 20.5 ms | 81.7 / 118.8 / 21.2 ms (unchanged: preamble timing dominates) |
| robust | 175.5 ms | 263.6 / 339.4 / 54.4 ms | 269.1 / 347.0 / 54.6 ms (unchanged within run-to-run spread) |

The polling removed ~4–6 ms from every LTC profile and almost all of their jitter: they now arrive
one airtime after the note was played. (An earlier run on `c9bd812`, before the `ltc14400` profiles:
ltc9600 12.5 → 8.08 ms, ltc_fec 26.7 → 21.2 ms.)

`--chords 20 --chord-size 4 --throughput 200` (after):

| profile | 4-note chord: first note / last note (mean) | last note p95 | throughput |
|---|---|---|---|
| digital | 0.66 / **3.26 ms** | 3.32 ms | **1,145 messages/s** |
| ltc14400 | 5.52 / 21.7 ms | 22.0 ms | 186 messages/s |
| ltc14400_fec | 8.76 / 29.9 ms | 39.9 ms | 142 messages/s |
| ltc9600 | 11.7 / 37.0 ms | 48.9 ms | 119 messages/s |
| ltc_fec | 40.0 / 105.6 ms | 115.8 ms | 46 messages/s |
| fast | 97.5 / 268.5 ms | 303.2 ms | 18 messages/s |
| robust | 578.8 / 1105.3 ms | 1247.6 ms | 6 messages/s |

(Fast and Robust from a run before the rebase, `--throughput 100`. A chord's first note can wait behind
the NoteOff refreshes of the previous chord's release.)

### 9.3 With real devices

End to end ≈ output buffer + input period + airtime (0.44 ms) + engine (~0.1 ms) + the path (Dante:
its latency setting, 0.25–5 ms; MADI/AES3: a few samples). With `--buffer-frames 64`–`128` at 48 kHz
the device buffers add ~1.5–3 ms per side; with the ALSA default, ~100 ms. No audio hardware was
available while this was written, so these are estimates; see the field-test checklist in §11.

## 10. CPU and MCU cost

Host (`digital_report`, release, x86-64):

| input | `DigitalRx` | modem incl. validation |
|---|---|---|
| silence | 9.0 ns / sample | 6.5 ns / sample |
| white noise at the worst level | 15.1 ns / sample | 15.3 ns / sample |
| back-to-back digital traffic | 11.3 ns / sample | 16.9 ns / sample |

TX: 167 ns per message (42 samples). For comparison, the MFSK search costs ~14–64 ns per sample and the
BMC receiver ~60.

MCU: `DigitalRx::push` compiles for `thumbv8m.main-none-eabihf` (Cortex-M33 with FPU) to about 80–100
instructions on the common path (one `f32` multiply, a `roundf` call, a shift, a few compares and the
6-entry ring update), more only on a sample that is a control symbol. At ~1.2 cycles per instruction
that is ~100–130 cycles per sample, **~5–6 M cycles/s at 48 kHz: about 4 % of a 150 MHz Cortex-M33**
(RP2350, STM32U5). An I2S receiver gets integers, so an MCU port can skip the float conversion
(`classify(code)` is integer only) and drop the multiply and the `roundf` call. RAM: ~150 bytes of
state.

## 11. Open questions and field test

* Real chains: Dante (DVS and hardware), AES67, MADI, AES3, class-compliant USB on macOS / Linux,
  Windows shared mode. Which pass `pathcheck` as bit-exact, which are DigitalOk, which fail?
* WASAPI exclusive mode and ASIO on Windows.
* Copies spaced by one network packet (§3.4) if field tests show isolated packet loss.
* Spec §6.6: the frame (§3.1), the decision rule and tolerance (§2.2), K = 2 and dedup window (§3.3),
  refresh spacing (§3.5) and the path requirement (§7) would be normative; the path check informative.

Field-test checklist:

1. `cricket list-devices`; pick the digital I/O (on Linux the `hw:` device).
2. `cricket pathcheck send` on the sender, `cricket pathcheck receive --seconds 10` on the receiver
   (or `pathcheck loop`); note the verdict and any findings. Try `--sample-format i32`, `f32`, `i16`.
3. If BIT-EXACT or DigitalOk: `cricket --profile digital --buffer-frames 128 decode ...` and
   `encode ...`; play notes, chords and fast runs; watch link stats (confidence, CRC failures, drops).
4. Measure real end-to-end latency (e.g. MIDI in and MIDI out into one interface, or a scope on MIDI
   DIN) at `--buffer-frames` 512, 256, 128, 64; note dropouts.
5. Break the path on purpose (fader −0.5 dB, EQ, SRC on, unplug the clock): no wrong notes, link-dead
   and the fail-safe burst after 2 s, recovery when restored.
6. Leave it running overnight with `decode` on an idle or music-carrying channel: zero false messages.

## 12. Files

* `cricket_core/src/phy/digital.rs`: layout, `DigitalTx`, `DigitalRx`.
* `cricket_core/src/modem.rs`: digital frames through §4.7 validation and dedup; `+digital` detector;
  `refresh_spacing_samples`.
* `cricket_core/src/transport/scheduler.rs`: `TxScheduler::with_refresh_spacing`.
* `cricket_core/src/pathcheck.rs`: pattern, path simulator, analyzer and report.
* `cricket_core/tests/digital.rs`: round trips, fail-safe table, foreign audio, coexistence, refresh
  under dropouts; the ignored `digital_report` prints the tables of §3.4, §4, §5 and §10:
  `cargo test --release -p cricket_core --test digital -- --ignored --nocapture`.
* `cricket_engine/src/audio.rs`, `encoder.rs`, `decoder.rs`, `memory.rs`, `loopback.rs`,
  `pathcheck.rs`: buffer size and sample format, wake-ups instead of polling, chord and throughput
  measurements, path check on devices.
* `cricket_cli/src/main.rs`: `--buffer-frames`, `--sample-format`, `loopback --chords/--throughput/
  --zero-airtime`, `pathcheck send|receive|loop|sim`.
