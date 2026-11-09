# Proposal: LTC-style biphase-mark phy ("ltc" profile)

Status: **Adopted in AML v1 as provisional profiles** — see spec (`docs/aml-v1-spec.md`) §6.5 (frame,
waveform, receiver requirements) and §4.6 (body FEC of the `_fec` profiles). This document is kept as
design history; where it differs from the spec, the spec applies. The CPU comparisons with the MFSK
receivers in §3.2 and §5 predate the MFSK search shortcut (`fec.md` §10.1): MFSK search now costs
~14–64 ns per sample on the host (idle to worst case), about the same as the BMC receiver (~60).

Original status: **phase 1: prototype, measurements, design proposal.** Not part of the AML v1 spec. The
prototype is in `cricket_core/src/phy/bmc/` and is selected with `PhyConfig::ltc()` / `--profile ltc`.
The robust and fast profiles behave exactly as before. Since the first prototype the receiver makes
soft decisions and produces erasures, and `ltc_fec` adds Reed–Solomon body FEC (§9).

§10 adds the experimental fast profiles `ltc14400` and `ltc14400_fec` for digital lines (5.4 / 7.0 ms
per message); they are not in the spec yet (candidate for §6.5).

## 1. Motivation

On a clean, dedicated line (Dante/MADI channel, analog XLR tie line) the MFSK profiles spend most of
their airtime on a robustness that the line does not need. Fast needs 54 ms of tones per message and
Robust needs 2 × 130 ms. Audio engineers already route **LTC** over these lines and trust it. LTC is a
biphase-mark (BMC) square wave: it is self-clocking, does not care about polarity, gain staging or AC
coupling, and fits several kbit/s into an audio channel. A BMC phy that carries the existing AML packets
gives:

* **~3.4× less airtime than Fast and ~17× less than Robust** at 4800 bit/s (16.9 ms per message, see §5).
* **Measured MIDI-in → MIDI-out latency of 22 ms mean** through the engine, against 78 ms for Fast and
  343 ms for Robust.
* A receiver that costs **about 1/3 of the CPU** of the Goertzel receivers and keeps only a few bit
  cells of samples (§5).

## 2. Design

### 2.1 Line code

Biphase mark, the same code SMPTE LTC and AES3 use. Every bit cell starts with a transition. A `1` has
an extra transition in the middle of the cell and a `0` has none. The data is carried only by *when*
transitions happen, never by the level. The receiver therefore sees just two transition intervals:
**H** (half a cell, half of a `1`) and **F** (a full cell, a `0`).

### 2.2 Frame (one burst)

```
| preamble: 8 x '1' | sync: V V | body: 16 nibbles = 64 bits | postamble: '1' | tail ½ cell + fade |
```

| Part | Content | Purpose |
|---|---|---|
| Preamble | 8 `1` bits = 16 H intervals, a square wave at the bit rate | Sync detection and envelope settling. The RX correlates all 16 half cells, together with the sync, against the expected pattern (§2.5). |
| Sync | Two **code violations** V, each 1.5 cells (3 half cells), at opposite levels | Valid BMC data contains only H and F intervals, so `V V` can never appear inside a body, whatever the payload. Unlike the LTC sync word, it needs no reserved data pattern and no bit stuffing (AES3 preambles use violations for the same reason). It is DC-balanced and has no polarity. The edge that ends the second V marks the start of the first body bit. |
| Body | The link-layer nibbles, MSB first: exactly the 16 interleaved nibbles that MFSK sends as tones | Reuses the link layer unchanged (§3). |
| Postamble | One `1` bit | Gives the last body bit a terminating transition, so its length can be measured. |
| Tail | ½ cell at the last level, then a shaped fade to 0 | Clean end of burst. |

After each burst comes `gap_bits` = 4 cells of silence (0.8 ms at 4800 bit/s). The burst is 76.5 bit
cells, so it lasts 15.9 ms at 4800 bit/s.

### 2.3 Bit rate

All parameters scale with the bit rate. The three candidates at 48 kHz:

| bit/s | half cell | burst | signal band (fundamentals) | notes |
|---|---|---|---|---|
| 2400 | 10 samples | 31.9 ms | 1.2–2.4 kHz | LTC-like rate (30 fps LTC = 2400 bit/s); most noise margin |
| **4800** | **5 samples** | **15.9 ms** | **2.4–4.8 kHz** | **recommended default**; survives a 3.4 kHz 2-pole low-pass |
| 9600 | 2.5 samples | 8.0 ms | 4.8–9.6 kHz | digital lines only; fails with < ~6 kHz bandwidth, ≥ 97 % down to 5 dB SNR |

The half cell does not have to be a whole number of samples. TX and RX work in fractional samples, and
the tests pass at 44.1 kHz (4.59 samples per half cell at 4800) and at 96 kHz. Rates above 9600 bit/s
for digital lines (14400 bit/s, 1.67 samples per half cell) are in §10.

### 2.4 Waveform shaping

Each transition is a raised-cosine step `edge_frac` = 0.5 half cells wide (52 µs at 4800 bit/s), placed
at its exact fractional sample position. This is the same idea as LTC's 25 ± 5 µs rise time: almost no
energy above ~2.5× the bit rate, so the signal passes analog stages, transformers and SRC anti-alias
filters without the aliasing or ringing a hard-edged digital square would cause. The level is
`amplitude / √2` (0.57 peak), the **same RMS as an MFSK tone of the same `amplitude`**. All SNR figures
below therefore compare like with like. The peak is 3 dB lower than MFSK's, which leaves headroom.

### 2.5 Receiver and clock recovery

```
x → DC blocker (1-pole HP, 0.05 × bit rate) → 2-pole Butterworth LP (1.5 × bit rate) = y
y → half-cell box sums → sync correlator (16 preamble + 6 sync half cells) → body start
y → peak envelope → Schmitt trigger (±0.3 × envelope) → interpolated zero-crossing times
  → timing loop (half-cell grid)
y → per bit: integrals over 4 half cells on the grid → bit + margin → nibble (+ reliability, erased)
```

The first prototype classified the intervals between transitions (H / F / V) and aborted on the first
invalid one. It is replaced by a soft-decision receiver (details and the before/after numbers in §9):

* **Sync correlator.** Every sample, the box sums of the last 22 half cells are correlated with the
  expected `… + − + − − − − + + +` pattern (sign-free, normalized by the energy, so level-independent).
  The best peak above 0.75 is the sync; the transition that ends the second violation, when one is
  close, times it exactly. Body data can score at most 0.82 on a clean line, so a lock is replaced
  only by a clearly stronger sync, or by one ≥ 0.95. The search never stops during a body: a false lock
  cannot hide a real packet, which is what the MFSK receiver needs its "rewind on failed decode" for.
* **Bits.** On a half-cell grid anchored at the sync, each bit compares the integrals of its two
  halves (`1` when their signs differ). Every cell starts with a transition, so the half cell before
  and the half cell after are inverted second looks at the first and second half; averaging them in
  halves the noise of both estimates. The normalized difference is the bit margin (1 ideal, 0 coin
  toss). A nibble reports its weakest bit as `confidence_db` and is flagged **erased** when a bit has
  almost no signal (dropout). With FEC the body is dropped once more nibbles are erased than the code
  can fill (8 for parity 4); without FEC the CRC decides.
* **Timing loop.** The Schmitt trigger decides *that* a transition happened and the interpolated zero
  crossing *when*. Each transition within 0.45 half cells of a grid point pulls the grid (phase gain
  0.3, rate gain 0.02, less weight towards the edge of the window). A body starts at the nominal rate;
  the loop pulls in more than the ±1 % requirement (at SNR 12 dB: 100 % at ±3 %, ≥ 98 % at +5 % for
  2400 and 4800 bit/s). A spurious or
  missing transition only costs the bits it touches.
* **False-lock gates.** A completed body whose mean nibble margin is below 0.3, or in which fewer than
  60 % of the transitions fall near the grid, is dropped before the CRC / FEC sees it (white noise
  scores 0.1–0.25, a real packet at 0 dB SNR ~0.5).
* **Level independence and polarity**: the correlator and the bit margins are ratios, and the Schmitt
  threshold follows the envelope; 0 dB down to −50 dB and inverted polarity decode at 100 %. A silence
  floor (`min_rms_dbfs`, −60 dBFS) disables the Schmitt trigger and the correlator.
* **DC / high-pass.** BMC is DC-free, so AC coupling and transformers are fine. The RX DC blocker also
  removes hum: 50 Hz hum at the same level as the signal passes at all three rates.
* The receiver emits the **same `RxEvent`s as the MFSK receiver** (`PreambleLocked`, one `BodyNibble`
  per 4 bits, `SyncLost`, `Metrics`). `last_confidence_db` is the mean nibble margin of the last packet
  as `20·log10((1 + m) / (1 − m))`, capped at 40 dB (0 dB = coin toss).

### 2.6 Idle behaviour: burst (prototyped) vs continuous (option)

| | **Burst** (prototype) | **Continuous carrier** (option) |
|---|---|---|
| Idle signal | silence + one 17 ms heartbeat burst every 500 ms (3.4 % duty) | constant BMC idle fill (`1`s), like LTC |
| Link-dead detection | heartbeat timeout (2 s, as now). With 17 ms heartbeats it could drop to ~250 ms / 750 ms at 7 % duty | **instant**: no transitions for ~1 ms = dead. The fail-safe can fire within a few ms |
| Latency | preamble (8 bits) + sync + body | the clock is always locked, so no preamble: sync + body, saving 1.7 ms at 4800 |
| Listener annoyance | a short buzz on each message; quiet when idle | permanent timecode-like buzz. Accepted on LTC lines, unpleasant if the channel is ever monitored |
| Crosstalk into neighbours | only while sending | permanent and broadband (square-wave harmonics) |
| Compressors / gates on the path | a downstream gate can chop the start of a burst; the 8-bit preamble absorbs ~1.5 ms of it | a constant level keeps compressors still and never triggers a gate |
| Fits existing AML semantics | yes: heartbeat, link-dead and fail-safe stay unchanged | needs a new link-dead source (carrier loss) next to the heartbeat |

**Recommendation:** keep burst mode as the default because it matches the existing heartbeat and
fail-safe model. Offer continuous mode later as an opt-in for show-critical lines where sub-10 ms
link-dead detection matters. Its receiver is the same one: the idle fill is one long preamble.

## 3. Fit in the code and in AML

### 3.1 Code (additive)

* `PhyConfig` gained one field, `modulation: Modulation`, which is `Mfsk` or `Bmc(BmcParams)`. `robust()`
  and `fast()` set `Mfsk`, so their behaviour is unchanged. New constructors are `ltc()` (4800),
  `ltc_with_bit_rate(n)`, `ltc_fec()` and `ltc_fec_with_bit_rate(n)` (§9). `from_name` accepts `ltc`,
  `ltc2400`, `ltc4800`, `ltc9600`, `ltc_fec` (= `ltc4800_fec`), `ltc2400_fec` and `ltc9600_fec`.
* `phy::PhyTx` / `phy::PhyRx` are small enums that dispatch to `ModemTx`/`ModemRx` (MFSK) or
  `BmcTx`/`BmcRx`. In `modem.rs`, `CricketModemTx`/`CricketModemRx` hold these instead of the MFSK
  types. `repetition_samples` and the new `repetition_gap_samples` know the BMC burst length. This keeps
  the TX scheduler airtime budget, the dedup window and the engine ring sizes correct without touching
  them.
* The packet format, CRC-16, SEQ/dedup, nibble interleave, repetition K, scheduler, heartbeat, link-dead
  and fail-safe are **reused unchanged**. The engine and GUI select the phy by profile name. The CLI has
  a global `--profile` option (`cricket --profile ltc encode|decode|test`) and
  `cricket loopback --profiles ltc,fast,robust`.
* Everything is `no_std` + `alloc` and builds for `thumbv8m.main-none-eabihf`.

### 3.2 Wire format and spec implications

* **No header change is needed.** The packet bytes, including version nibble `0x1`, are the same on both
  phys. The phy is a property of the *line*, the way the MFSK profile already is.
* **Telling MFSK from BMC: auto-detect is feasible and cheap.** The two signals cannot be confused. A BMC
  sync needs a strong correlation with the 22-half-cell preamble + violation pattern and a body that
  sits on the bit grid with clear margins; MFSK needs Goertzel-pure tones on its preamble frequencies;
  and both are CRC-checked. MFSK audio does trigger some BMC bodies (~70–100 per minute of back-to-back
  MFSK traffic, all rejected; §9.6). A test feeds Robust
  and Fast audio to all three BMC receivers and BMC audio to both MFSK receivers, and gets **zero
  cross-decodes** (`mfsk_and_bmc_do_not_cross_decode`). The same test shows that two receivers running
  in parallel on one stream that mixes Fast and LTC bursts each get exactly their own packets. Because
  the BMC receiver adds only ~25–40 % to the CPU cost of an MFSK receiver, a v1.x receiver can **always
  run BMC alongside its MFSK profile** and report which phy the link is on. The operator then picks the
  profile only on the TX side. Auto-detecting the BMC *bit rate* also works: run one BMC receiver per
  rate (each about 60 ns per sample on the host).
* A profile identifier in the header would cost header bits for information the receiver already has.
  If the TX should advertise its profile (for example for GUI display), a later heartbeat payload is
  the right place.
* For the spec: a new §6.x "LTC (BMC) profile". Frame (§2.2), shaping, and RX requirements: ±1 % rate
  tolerance, the silence floor, the rule that the RX must accept either polarity, and minimum
  performance figures rather than a receiver algorithm.

### 3.3 Combining with link-layer FEC

The BMC phy carries an **opaque nibble body** of any length. The TX sends whatever nibbles it is given,
and the RX collects `set_expected_body_nibbles(n)` nibbles. An RS(GF(16)) codeword produced by the link
layer therefore goes on the wire unchanged, at 4 bits per nibble (each extra parity nibble costs 0.83 ms
at 4800 bit/s). Points to note:

* BMC errors are of two kinds. A missed or spurious mid-cell transition gives a **single-bit substitution**
  (H+H read as F, or the reverse), and RS corrects these. A shifted transition gave an **invalid
  interval sequence**, and the first prototype then aborted the packet, so FEC could not help (with that
  receiver FEC made things *worse*, §9.3). The soft-decision receiver (§2.5) never aborts on a bad
  interval: it keeps the bit grid, reports a reliability per nibble and erasures for dropouts, which is
  what RS needs. **Done: see §9.**
* The nibble interleaver is still useful against impulse noise, which corrupts a short run of
  consecutive bits.
* With FEC, the default K stays 1 on BMC. FEC helps more than a second copy, which would double the
  airtime.

## 4. Measurements

Channel simulator: `cricket_core/tests/support`. New impairments are a 2-pole low-pass, 1-pole high-pass,
peaking EQ, soft clip, polarity inversion and mains hum. The existing ones are AWGN, gain, drift
(resampling) and 1-pole low-pass. SNR is relative to the RMS of the signal (equal for MFSK and BMC, see
§2.4), with white noise over the full 0–24 kHz band. Each cell is the success rate of 200 single NoteOns
with random start offsets and random input chunk sizes (64–4096); success means the exact message and
nothing else. The tables are for the current (soft-decision) receiver; §9 compares it with the first
prototype at 1000 trials per cell and adds clicks, dropouts and the FEC profiles.

Reproduce with:

```sh
cargo test --release -p cricket_core --test phy_ab_measurements -- --ignored --nocapture
cargo test --release -p cricket_core --test ltc_fec_ab -- --ignored --nocapture
cargo run --release -p cricket_cli -- loopback --profiles ltc,ltc_fec,ltc2400,ltc9600,fast,robust
```

### 4.1 Packet success (%)

| condition | robust | fast | ltc2400 | ltc4800 | ltc9600 | ltc_fec |
|---|---|---|---|---|---|---|
| clean | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 30 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 20 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 15 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 12 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 10 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 8 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 6 dB | 100 | 100 | 100 | 100 | 99.5 | 100 |
| SNR 4 dB | 100 | 100 | 100 | 100 | 92.5 | 100 |
| SNR 2 dB | 100 | 99.5 | 100 | 97.5 | 53.5 | 100 |
| SNR 0 dB | 100 | 94.0 | 99.0 | 76.5 | 9.0 | 96.5 |
| level -10 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| level -30 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| level -50 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| level -30 dB + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 |
| polarity inverted | 100 | 100 | 100 | 100 | 100 | 100 |
| inverted + SNR 12 | 100 | 100 | 100 | 100 | 100 | 100 |
| drift +1000 ppm | 100 | 100 | 100 | 100 | 100 | 100 |
| drift -1000 ppm | 100 | 100 | 100 | 100 | 100 | 100 |
| drift +1000 ppm + SNR 12 | 100 | 100 | 100 | 100 | 100 | 100 |
| drift +1 % (SRC misconfig) | 0 | 65.5 | 100 | 100 | 100 | 100 |
| drift -1 % | 0 | 73.5 | 100 | 100 | 100 | 100 |
| 1-pole LP 15 kHz | 100 | 100 | 100 | 100 | 100 | 100 |
| 1-pole LP 8 kHz | 100 | 100 | 100 | 100 | 100 | 100 |
| 1-pole LP 4 kHz | 100 | 100 | 100 | 100 | 100 | 100 |
| 2-pole LP 12 kHz | 100 | 100 | 100 | 100 | 100 | 100 |
| 2-pole LP 6 kHz | 100 | 100 | 100 | 100 | 100 | 100 |
| 2-pole LP 3.4 kHz (telephone) | 100 | 100 | 100 | 100 | 0 | 100 |
| HP 20 Hz | 100 | 100 | 100 | 100 | 100 | 100 |
| HP 300 Hz (transformer) | 100 | 100 | 100 | 100 | 100 | 100 |
| HP 1 kHz | 100 | 100 | 0 | 100 | 100 | 100 |
| HP 300 Hz + LP 8 kHz + SNR 15 | 100 | 100 | 100 | 100 | 100 | 100 |
| EQ +12 dB @ 3 kHz Q1 | 100 | 100 | 100 | 0 | 0 | 0 |
| EQ -12 dB @ 3 kHz Q1 | 100 | 100 | 100 | 100 | 100 | 99.5 |
| EQ -12 dB @ 6 kHz Q1 | 100 | 100 | 100 | 100 | 100 | 100 |
| soft clip (drive 4) | 100 | 100 | 100 | 100 | 100 | 100 |
| hum 50 Hz @ -6 dB | 100 | 100 | 100 | 100 | 100 | 100 |
| hum 50 Hz @ 0 dB | 100 | 65.0 | 100 | 100 | 100 | 100 |
| false messages from 60 s of white noise | 0 | 0 | 0 | 0 | 0 | 0 |

### 4.2 Airtime, latency, throughput (NoteOn, 48 kHz)

| profile | burst (one copy) | K | airtime / msg | decode delay¹ | max msgs/s | engine loopback² mean / p95 / jitter |
|---|---|---|---|---|---|---|
| robust | 130.0 ms | 2 | 273.0 ms | 130.0 ms | 4 | 360 / 499 / 93 ms |
| fast | 54.0 ms | 1 | 57.0 ms | 54.0 ms | 18 | 81 / 116 / 19 ms |
| ltc2400 | 31.9 ms | 1 | 33.7 ms | 32.0 ms | 30 | 43 / 65 / 9 ms |
| **ltc4800** | **15.9 ms** | **1** | **16.9 ms** | **16.0 ms** | **59** | **22 / 26 / 3 ms** |
| ltc9600 | 8.0 ms | 1 | 8.4 ms | 8.0 ms | 119 | 12 / 15 / 3 ms |
| **ltc_fec** (4800 + RS p4) | **20.9 ms** | **1** | **21.9 ms** | **21.0 ms** | **46** | **26 / 30 / 3 ms** |

¹ From the first sample of the message's audio to `ModemOutput::Message`, on a clean channel (16-sample
chunks). For BMC, the packet is out 1.5 half cells after the end of its last bit (the decision waits
for the first half of the next cell).
² `cricket loopback`: paced MIDI in → engine encoder → in-memory audio → engine decoder → MIDI out, 60
notes, while notes are also released (so NoteOffs compete for airtime). This includes the engine's
buffering and polling ticks, about 4–6 ms. Real interfaces add their own buffers.

Burst airtime per message is 3.4× lower than Fast and 16× lower than Robust. A 4-note chord (4 NoteOns)
takes 68 ms on air at 4800 bit/s, against 228 ms on Fast.

### 4.3 Robustness weak spots

1. **Noise.** With the soft-decision receiver, 4800 bit/s delivers ≥ 99.8 % down to 4 dB full-band SNR
   and 97 % at 2 dB (the first prototype needed ~10 dB). Fast works down to ~2 dB and Robust below 0 dB,
   so BMC is now close to Fast on white noise, but it is still not a replacement for Robust on unknown
   chains: see 2–4.
2. **In-band EQ boost.** A +12 dB peaking EQ at 3 kHz (between the 2.4 kHz and 4.8 kHz fundamentals)
   distorts the waveform so much that 4800 fails outright (FEC does not help: ~3 %). 2400 passes. A cut
   instead of a boost, and EQ outside the band, are harmless. LTC has the same weakness, and the same
   rule applies: **no EQ on the line**.
3. **Bandwidth.** 9600 bit/s needs more than ~6 kHz of bandwidth (87 % at a 6 kHz 2-pole low-pass, 0 %
   at 4 kHz), so it is for digital lines only. 2400 bit/s partly fails a 1 kHz high-pass (0–80 %
   depending on noise) because its sync violations have their fundamental at 800 Hz.
4. **Impulses and dropouts** are what the uncoded profiles cannot handle: a 1 ms dropout loses 78 % of
   the packets at 4800, 200 clicks/s at +6 dB lose 24 %. Body FEC fixes most of it (§9).
5. **Gates / fast compressors** ahead of the receiver can eat the start of the burst. The sync
   correlator uses the whole 8-bit preamble, so a gate that mutes part of it lowers the correlation;
   this has not been simulated yet.
6. **Spectrum and annoyance.** BMC is broadband (square-wave harmonics, softened by the shaping), so it
   sounds like timecode and produces more crosstalk into neighbouring channels than MFSK tones of the
   same RMS. Bursts are short (17–22 ms), which helps.

## 5. MCU cost

Per input sample, the receiver does a DC blocker (1 multiply, 2 adds), a biquad (5 multiply-adds), an
envelope (1 multiply, 1 compare), a zero-crossing test and the Schmitt compares (1 multiply, 2
compares), as in the first prototype. The soft-decision receiver adds:

* one half-cell box sum (half-cell length adds: 5 at 4800 bit/s / 48 kHz) and
* the sync correlator: 22 multiply-adds for the correlation and 22 for its energy, plus a compare,
* per bit (every 10 samples at 4800): four half-cell integrals, ~20 multiply-adds, i.e. ~2 per sample,
* per transition: one timing-loop update (a few multiplies).

That is **≈ 60 multiply-adds per sample instead of ≈ 8**, about **3 MHz of a Cortex-M33 with FPU at
48 kHz (~2 % of 150 MHz)**. RAM: a history of the filtered signal (64 floats at 4800 / 48 kHz, 256 at
2400 / 96 kHz) and a ring of box sums (128–512 floats), so **≤ 3 KB**, against ~25 KB for Robust's
packet buffer. If the correlator matters on a small MCU it can run every second sample: the peak is
refined by interpolation and snapped to a transition anyway.

The MFSK receivers run a 16-tone Goertzel bank over a 144–288-sample window every ¼ symbol, which is
≈ 64 Goertzel iterations (≈ 200 FLOPs) per sample. They also run fine alignment and purity/energy
computations, and buffer up to a whole packet. That is **≈ 250–400 cycles per sample, ≈ 12–20 MHz**.

Measured on the host (release build, 10 s of traffic at 30 dB SNR, in ns per input sample):

| robust | fast | ltc2400 | ltc4800 | ltc9600 | ltc_fec |
|---|---|---|---|---|---|
| 155 | 225–270 | 58–74 | 57–65 | 74–82 | 57–63 |

(first prototype: 15.5 / 17.8 / 21.5 ns for ltc2400 / 4800 / 9600). The receiver is still **3–4×
cheaper** than MFSK. The FEC decoder runs once per completed body (at most 36 candidate pairs of two
GF(16) RS decodes), not per sample. The TX is unchanged and cheap: per sample it is a table lookup or a
`sin` only inside the 0.5-half-cell transition windows.

## 6. Recommendation

* **Default bit rate: 4800 bit/s, and with FEC (`ltc_fec`) unless the line is known to be glitch-free.**
  `ltc_fec` costs 5 ms more airtime than `ltc` (26 ms instead of 22 ms end-to-end) and turns the
  failures that the uncoded profile cannot handle, 1–3 ms dropouts and bursts of clicks, into ≥ 92 %
  delivery (§9). On white noise both are ≥ 99 % down to 3 dB SNR.
* `ltc` (uncoded) stays for clean digital lines where every millisecond counts. Offer **9600** (with
  FEC) as "LTC-digital" for Dante/MADI/AES lines (12–16 ms end-to-end) and **2400** where an in-band EQ
  boost cannot be ruled out. Dropping to 2400 is **not** a substitute for FEC: it is ~2–3 dB better on
  white noise but still loses most packets to dropouts, and it costs 11 ms more than `ltc_fec` (§9.4).
* Keep **burst mode with the existing heartbeat**, and consider a shorter heartbeat and link-dead for
  this profile. Make continuous carrier an opt-in later.
* Do **not** change the packet header. Do add **parallel MFSK + BMC reception** (auto-detect) to the
  receiver.
* Before going into the spec: a gate/compressor simulation, a proper bound on FEC miscorrection under
  MFSK cross-traffic (§9.6), and **hardware tests** over a real analog transformer path and Dante with
  SRC.

## 7. Open questions

1. Should the RX also lock to bursts at 2× / ½× the configured rate (rate auto-detect), or should the
   rate be part of the profile name only?
2. What heartbeat and link-dead timing should the ltc profile use? 17 ms heartbeats make 250 ms / 750 ms
   affordable at 7 % duty.
3. Continuous carrier: worth implementing as a "LTC-continuous" mode for instant link-dead?
4. Body length: BMC could send variable-length bodies (12 nibbles for NoteOn instead of the padded 16,
   saving 3.3 ms at 4800). That only works if the RX trusts the header type nibble before the CRC. Is it
   worth diverging from the fixed-length MFSK body, and how does it interact with FEC codeword sizes?
5. Output level: 0.57 peak (equal RMS to MFSK). Should it match house LTC level instead (often
   −18…−10 dBFS)? The decoder does not care.
6. ~~Should we prefer a correlating detector or keep pure zero-crossing?~~ Answered by §9: the
   correlating detector with a transition-driven timing loop gains ~5 dB for ~3× the (still small)
   CPU cost, and it is what makes FEC useful.
7. Could a full sequence (Viterbi) detector over the BMC trellis, or decoding the `0`/`1` decision of
   each half cell jointly with the FEC, gain more? Probably ~1 dB; not tried.

## 8. Files

* `cricket_core/src/phy/bmc/{mod,tx,rx}.rs`: BMC parameters, frame, TX synthesis, streaming RX.
* `cricket_core/src/phy/select.rs`: `PhyTx` / `PhyRx` dispatch.
* `cricket_core/src/phy/config.rs`: `modulation` field, `ltc()`, `ltc_with_bit_rate()`, `from_name`.
* `cricket_core/src/modem.rs`: uses the dispatchers; adds BMC-aware `repetition_samples` and
  `repetition_gap_samples`.
* `cricket_core/tests/support/mod.rs`: new impairments.
* `cricket_core/tests/bmc_phy.rs`: regression tests. `cricket_core/tests/phy_ab_measurements.rs`: the
  A/B report and the RX parameter sweep (ignored tests).
* `cricket_core/tests/ltc_fec.rs`: `ltc_fec` profiles: loopback, sync sweep, dropout repair, false lock
  before a real packet, noise / MFSK rejection.
* `cricket_core/tests/ltc_fec_ab.rs`: the §9 A/B tables and the false-lock report (ignored tests;
  `LTC_AB_TRIALS`, `LTC_AB_PROFILES`, `LTC_AB_SET`, `LTC_AB_PARAMS`).
* `cricket_core/tests/stuck_notes.rs`: `ltc_back_to_back_report` (ignored).
* `cricket_core/tests/support/mod.rs`: `clicks` (impulse noise).
* `cricket_cli/src/main.rs`: global `--profile`, `loopback --profiles`.
* `cricket_core/tests/ltc_fast.rs`, `cricket_core/tests/ltc_fast_ab.rs`: the fast profiles of §10
  (regression tests; A/B tables, chords, phy diagnostics and CPU as ignored tests).

## 9. LTC + FEC

The link-layer FEC of `docs/proposals/fec.md` (two interleaved RS(7 + p, 7) codewords over GF(16),
errors-and-erasures decoding plus low-reliability erasure candidates within an errata-weight bound,
every candidate checked by the unchanged CRC-16 / version / type / pad / payload ranges, body XORed with
a whitening sequence; see `fec.md` §9 for the false-accept measures) works on nibbles and does not care how they reached the
wire. On BMC each extra parity nibble costs 0.83 ms at 4800 bit/s. The question was whether it
improves LTC, and at what cost compared with simply halving the bit rate.

**Short answer:** with the first (hard-decision) receiver, FEC made LTC *worse*. With the
soft-decision receiver it is clearly better, mainly against dropouts and impulse noise, for 5 ms more
airtime. The receiver change alone gains ~5 dB on white noise for every LTC profile. Both are in the
code: the receiver for all `ltc*` profiles, FEC as the experimental `ltc_fec`, `ltc2400_fec` and
`ltc9600_fec` profiles (parity 4, 22 body nibbles).

### 9.1 What the receiver had to change

The first receiver turned transition intervals into bits and **aborted on the first invalid interval**,
so its nibbles were all-or-nothing: no erasures, no reliability, and one bad interval lost the rest of
the body. FEC needs the opposite: keep going, keep the bit timing, and say which nibbles are doubtful.
The new receiver (§2.5) does that:

* bits are soft decisions on a half-cell grid that one bad transition cannot shift; each bit has a
  margin, each nibble reports its weakest bit (`confidence_db`, used to rank the FEC's erasure
  candidates) and is flagged erased when it contains a dropout;
* sync is a correlator over the whole preamble and both violations instead of a run of clean intervals,
  so noise in the preamble no longer prevents the lock (at 4 dB SNR the interval-based sync missed
  ~30 % of the packets; the new one locks on all of them);
* the body is abandoned when it has more erasures than the code can fill (8 at parity 4), and a body
  that looks like noise (mean margin < 0.3, < 60 % of its transitions on the grid) is dropped before
  the FEC tries it.

Things tried and dropped, measured with the same harness: a preamble-based rate estimate (worse than
starting at the nominal rate at low SNR); erasing nibbles below a margin threshold (no gain: the
decoder's reliability-ordered candidates already cover it, and it hurts the uncoded profiles); an input
impulse clipper (+1–10 % with clicks, −1–3 % on white noise); a narrower or untapered timing window
(worse on EQ and drift).

### 9.2 The receiver change alone (no FEC)

Single messages (NoteOn, every 4th an MscLite), random offset, 64-sample chunks, **1000 trials per
cell**, same seeds for every column; "before" is the hard-decision receiver (main at 6427f38). SNR is
full-band (0–24 kHz) relative to the signal RMS. Clicks: 1–4-sample spikes at the given rate and peak
level relative to the signal peak, on top of SNR 20 dB; dropouts, EQ, filters: at SNR 20 dB.

| channel | ltc before | **ltc after** | ltc2400 before | **ltc2400 after** | ltc9600 before | **ltc9600 after** |
|---|---|---|---|---|---|---|
| SNR 12 dB | 100 | 100 | 100 | 100 | 99.3 | 100 |
| SNR 10 dB | 99.9 | 100 | 100 | 100 | 95.7 | 100 |
| SNR 9 dB | 99.9 | 100 | 100 | 100 | 85.4 | 100 |
| SNR 8 dB | 99.1 | 100 | 100 | 100 | 66.6 | 100 |
| SNR 7 dB | 95.7 | 100 | 100 | 100 | 39.2 | 99.8 |
| SNR 6 dB | 89.2 | 100 | 99.8 | 100 | 16.5 | 99.0 |
| SNR 5 dB | 73.9 | 100 | 99.1 | 100 | 3.6 | 97.0 |
| SNR 4 dB | 43.9 | 99.8 | 95.8 | 100 | 0.9 | 90.7 |
| SNR 3 dB | 15.8 | 98.8 | 87.2 | 100 | 0 | 76.6 |
| SNR 2 dB | 4.4 | 97.1 | 70.0 | 100 | 0 | 51.7 |
| SNR 1 dB | 0.4 | 91.7 | 45.8 | 99.8 | 0 | 26.2 |
| SNR 0 dB | 0 | 76.3 | 16.4 | 99.0 | 0 | 7.5 |
| clean | 100 | 100 | 100 | 100 | 100 | 100 |
| clicks 20/s @ +0 dB | 96.7 | 100 | 99.8 | 100 | 95.5 | 100 |
| clicks 50/s @ +6 dB | 77.8 | 93.5 | 76.1 | 99.9 | 80.8 | 91.1 |
| clicks 200/s @ +6 dB | 37.7 | 76.0 | 34.5 | 99.2 | 40.4 | 68.5 |
| clicks 100/s @ +12 dB | 36.1 | 69.2 | 35.8 | 71.2 | 52.9 | 74.5 |
| dropout 1 ms | 1.3 | 22.5 | 4.6 | 55.9 | 0 | 1.9 |
| dropout 3 ms | 0 | 0.2 | 0 | 6.8 | 0 | 0 |
| dropout 10 ms | 0 | 0 | 0 | 0 | 100 | 100 |
| EQ +6 dB @ 3 kHz | 100 | 100 | 100 | 100 | 100 | 100 |
| EQ +12 dB @ 3 kHz | 0 | 0 | 100 | 100 | 0 | 27.7 |
| EQ -12 dB @ 3 kHz | 74.6 | 100 | 99.9 | 99.8 | 47.4 | 100 |
| EQ -12 dB @ 6 kHz | 97.7 | 99.9 | 100 | 100 | 16.0 | 99.0 |
| 2-pole LP 6000 Hz | 100 | 100 | 100 | 100 | 5.3 | 87.3 |
| 2-pole LP 4000 Hz | 100 | 100 | 100 | 100 | 0 | 0 |
| 2-pole LP 3400 Hz | 100 | 100 | 100 | 100 | 0 | 0 |
| HP 300 Hz | 100 | 100 | 100 | 100 | 100 | 100 |
| HP 1000 Hz | 98.7 | 100 | 14.2 | 78.1 | 99.8 | 100 |
| drift +1% + SNR 12 | 100 | 100 | 100 | 100 | 96.3 | 100 |
| drift -1% + SNR 12 | 99.9 | 100 | 99.9 | 100 | 97.8 | 100 |
| drift +1% + SNR 6 | 80.4 | 100 | 99.6 | 100 | 3.4 | 95.0 |
| inverted + SNR 6 | 87.6 | 100 | 99.9 | 100 | 13.3 | 99.6 |
| level -40 dB + SNR 8 | 99.0 | 100 | 100 | 100 | 66.2 | 99.9 |

No cell got worse by more than one trial in 1000 (ltc2400, EQ −12 dB: 999 → 998). The uncoded `ltc`
moves its 99 % point from 8 dB to ~3.5 dB SNR, its 50 % point from ~4 dB to below 0 dB. The ltc9600 "dropout 10 ms" row is not meaningful
(the burst is shorter than the dropout, so the simulator applies none).

### 9.3 FEC on LTC

Same trials. All columns use the new receiver except the second, which is the hard-decision receiver
with the parity-4 body.

| channel | ltc | ltc + FEC p4, hard-decision RX | ltc_fec p2 | **ltc_fec (p4)** | ltc2400 | ltc2400_fec | ltc9600_fec |
|---|---|---|---|---|---|---|---|
| SNR 12 dB | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 10 dB | 100 | 99.9 | 100 | 100 | 100 | 100 | 100 |
| SNR 9 dB | 100 | 99.8 | 100 | 100 | 100 | 100 | 100 |
| SNR 8 dB | 100 | 98.9 | 100 | 100 | 100 | 100 | 100 |
| SNR 7 dB | 100 | 95.5 | 100 | 100 | 100 | 100 | 99.8 |
| SNR 6 dB | 100 | 86.3 | 100 | 100 | 100 | 100 | 99.6 |
| SNR 5 dB | 100 | 66.8 | 100 | 100 | 100 | 100 | 99.3 |
| SNR 4 dB | 99.8 | 33.9 | 99.8 | 99.8 | 100 | 100 | 97.3 |
| SNR 3 dB | 98.8 | 9.4 | 99.4 | 99.4 | 100 | 100 | 94.7 |
| SNR 2 dB | 97.1 | 1.9 | 99.3 | 99.4 | 100 | 100 | 86.1 |
| SNR 1 dB | 91.7 | 0 | 98.7 | 98.9 | 99.8 | 99.8 | 63.2 |
| SNR 0 dB | 76.3 | 0 | 95.2 | 96.2 | 99.0 | 99.7 | 28.6 |
| clean | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| clicks 20/s @ +0 dB | 100 | 95.4 | 100 | 100 | 100 | 100 | 100 |
| clicks 50/s @ +6 dB | 93.5 | 73.2 | 99.9 | 99.9 | 99.9 | 99.9 | 97.9 |
| clicks 200/s @ +6 dB | 76.0 | 27.3 | 98.2 | 99.4 | 99.2 | 99.8 | 92.8 |
| clicks 100/s @ +12 dB | 69.2 | 25.5 | 90.6 | 92.9 | 71.2 | 98.5 | 91.5 |
| dropout 1 ms | 22.5 | 1.2 | 95.2 | 95.8 | 55.9 | 100 | 91.9 |
| dropout 3 ms | 0.2 | 0 | 74.1 | 92.3 | 6.8 | 92.6 | 62.3 |
| dropout 10 ms | 0 | 0 | 0 | 0 | 0 | 82.2 | 0 |
| EQ +6 dB @ 3 kHz | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| EQ +12 dB @ 3 kHz | 0 | 0 | 1.7 | 2.6 | 100 | 100 | 27.7 |
| EQ -12 dB @ 3 kHz | 100 | 63.6 | 100 | 99.6 | 99.8 | 99.8 | 100 |
| EQ -12 dB @ 6 kHz | 99.9 | 96.3 | 99.9 | 99.9 | 100 | 100 | 99.4 |
| 2-pole LP 6000 Hz | 100 | 100 | 100 | 100 | 100 | 100 | 87.2 |
| 2-pole LP 4000 Hz | 100 | 100 | 100 | 100 | 100 | 100 | 0 |
| 2-pole LP 3400 Hz | 100 | 100 | 100 | 100 | 100 | 100 | 0 |
| HP 300 Hz | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| HP 1000 Hz | 100 | 98.7 | 100 | 100 | 78.1 | 79.3 | 100 |
| drift +1% + SNR 12 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| drift -1% + SNR 12 | 100 | 99.9 | 100 | 100 | 100 | 100 | 100 |
| drift +1% + SNR 6 | 100 | 77.3 | 100 | 100 | 100 | 100 | 97.8 |
| inverted + SNR 6 | 100 | 86.2 | 100 | 100 | 100 | 100 | 99.9 |
| level -40 dB + SNR 8 | 100 | 99.1 | 100 | 100 | 100 | 100 | 99.9 |

(Measured before the false-accept measures of `fec.md` §9. They move the FEC columns by ≤ 1 point
except `ltc9600_fec` at SNR 2 / 1 / 0 dB (−2 to −3), `ltc9600_fec` dropout 3 ms (+2.4), `ltc2400_fec`
HP 1 kHz (+2.2), `ltc_fec` EQ ±12 dB at 3 kHz (+53 / −0.9); see `fec.md` §9.6.)

| profile | body nibbles | burst | airtime / msg | decode delay (sim) | engine loopback mean / p95 / jitter |
|---|---|---|---|---|---|
| ltc | 16 | 15.9 ms | 16.9 ms | 16.5 ms | 22.0 / 25.5 / 2.5 ms |
| ltc_fec p2 | 18 | 17.6 ms | 18.5 ms | 18.2 ms | – |
| **ltc_fec (p4)** | **22** | **20.9 ms** | **21.9 ms** | **21.5 ms** | **25.7 / 30.1 / 3.1 ms** |
| ltc2400 | 16 | 31.9 ms | 33.7 ms | 32.4 ms | 42.6 / 65.1 / 9.4 ms |
| ltc2400_fec | 22 | 41.9 ms | 43.7 ms | 42.4 ms | 58.9 / 90.7 / 15.2 ms |
| ltc9600 | 16 | 8.0 ms | 8.4 ms | 8.6 ms | 11.9 / 15.3 / 2.5 ms |
| ltc9600_fec | 22 | 10.5 ms | 10.9 ms | 11.1 ms | 15.6 / 19.8 / 3.0 ms |

Decode delay: message start to delivery in the simulator (64-sample chunks). Loopback: `cricket
loopback --profiles … --notes 60` (engine, paced MIDI in → MIDI out, with releases).

* **With the hard-decision receiver FEC was a net loss**: a longer body is more likely to hit an
  invalid interval and be aborted, and there were no erasures to fill (34 % vs 44 % at 4 dB SNR).
* **With the soft receiver, on white noise** the uncoded profile is already ≥ 97 % down to 2 dB, so FEC
  only shows at the bottom (0 dB: 76 → 96 %), a ~1.5–2 dB gain.
* **Impulses and dropouts are where FEC pays**: 100 clicks/s at +12 dB 69 → 93 %, a 1 ms dropout
  22 → 96 %, 3 ms 0 → 92 %. 10 ms (12 nibbles at 4800) is beyond 8 erasures; only 2400 + FEC (6
  nibbles) survives it.
* **Parity 2 vs 4**: the same on white noise and light clicks; parity 4 is clearly better on 3 ms
  dropouts (92 vs 74 %) and a little on heavy clicks (93 vs 91 %). Over `ltc`, parity 2 costs 1.7 ms of
  airtime and parity 4 costs 5 ms.
  Network audio drops whole packets of 1–5 ms, so `ltc_fec` uses parity 4.
* FEC does not help against what breaks the waveform for every bit (EQ +12 dB at 3 kHz, 9600 through a
  4 kHz low-pass).

### 9.4 FEC at 4800, or 2400 without FEC?

| channel | ltc (16.9 ms) | **ltc_fec (21.9 ms)** | ltc2400 (33.7 ms) | ltc2400_fec (43.7 ms) |
|---|---|---|---|---|
| SNR 2 dB | 97.1 | 99.4 | 100 | 100 |
| SNR 0 dB | 76.3 | 96.2 | 99.0 | 99.7 |
| clicks 200/s @ +6 dB | 76.0 | 99.4 | 99.2 | 99.8 |
| clicks 100/s @ +12 dB | 69.2 | 92.9 | 71.2 | 98.5 |
| dropout 1 ms | 22.5 | 95.8 | 55.9 | 100 |
| dropout 3 ms | 0.2 | 92.3 | 6.8 | 92.6 |
| EQ +12 dB @ 3 kHz | 0 | 2.6 | 100 | 100 |
| HP 1 kHz | 100 | 100 | 78.1 | 79.3 |
| engine loopback mean | 22 ms | 26 ms | 43 ms | 59 ms |

Halving the bit rate buys ~3 dB on white noise (half the receiver bandwidth) and tolerance to an
in-band EQ boost, for **+16 ms** of airtime. Spending **+5 ms** on parity instead handles dropouts and
strong clicks, which 2400 does not, and it is within 3 dB of 2400 on noise. **For the LTC default the
airtime is better spent on FEC.** 2400 remains the answer only for lines with an in-band EQ boost or
extreme noise, and 2400 + FEC is the most robust LTC variant (dropouts up to ~10 ms) at 59 ms.

### 9.5 Back-to-back traffic

`stuck_notes.rs` `ltc_back_to_back_report`: ~1400 messages per run (167 NoteOn/NoteOff pairs played
with chords and runs, NoteOff refreshes, heartbeats, gaps longer than link-dead), random offset and
chunk sizes. "Before" is the hard-decision receiver (for `ltc_fec`: the parity-4 body on that receiver).

| profile | channel | lost / sent, before | lost NoteOffs, before | lost / sent, after | lost NoteOffs, after |
|---|---|---|---|---|---|
| ltc | SNR 6 dB | 164 / 1420 | 141 | 1 / 1420 | 1 |
| ltc | SNR 3 dB | 1200 / 1446 | 953 | 12 / 1446 | 10 |
| ltc | SNR 1 dB | 1434 / 1445 | 1149 | 115 / 1445 | 90 |
| ltc | SNR 20 dB + clicks 200/s @ +6 dB | 932 / 1443 | 747 | 321 / 1443 | 255 |
| ltc_fec | SNR 6 dB | 194 / 1417 | 162 | 0 / 1417 | 0 |
| ltc_fec | SNR 3 dB | 1288 / 1441 | 1038 | 5 / 1441 | 5 |
| ltc_fec | SNR 1 dB | 1439 / 1440 | 1158 | 24 / 1440 | 20 |
| ltc_fec | SNR 20 dB + clicks 200/s @ +6 dB | 1018 / 1438 | 816 | 15 / 1438 | 10 |
| ltc2400 | SNR 6 dB | 2 / 1406 | 2 | 0 / 1406 | 0 |
| ltc2400 | SNR 3 dB | 168 / 1425 | 145 | 0 / 1425 | 0 |
| ltc2400 | SNR 1 dB | 825 / 1423 | 666 | 3 / 1423 | 2 |
| ltc2400 | SNR 20 dB + clicks 200/s @ +6 dB | 914 / 1422 | 756 | 12 / 1422 | 9 |
| ltc2400_fec | SNR 6 dB | 4 / 1393 | 3 | 0 / 1393 | 0 |
| ltc2400_fec | SNR 3 dB | 174 / 1411 | 138 | 0 / 1411 | 0 |
| ltc2400_fec | SNR 1 dB | 931 / 1409 | 753 | 3 / 1409 | 3 |
| ltc2400_fec | SNR 20 dB + clicks 200/s @ +6 dB | 1056 / 1407 | 869 | 1 / 1407 | 1 |

No false messages in any run. Before, several runs had notes stuck for longer than link-dead (up to
40 s); after, none (one 10 ms stuck episode in `ltc2400_fec` at 1 dB). The uncoded profiles lost far
more back to back than in single-message trials, because each aborted false lock could swallow the
next packet; the parallel sync search removes that.

### 9.6 False locks and false messages

`ltc_fec_ab.rs` `ltc_false_message_report`: 60 s of white noise (3 levels) and 60 s of back-to-back
MFSK traffic (Robust, Fast, Robust FEC at SNR 20 dB) per profile. Messages (bodies that reached the
CRC / FEC and failed):

| profile | white noise | MFSK traffic |
|---|---|---|
| ltc | 0 (0) | 0 (67) |
| ltc_fec p2 | 0 (0) | 0 (92) |
| ltc_fec | 0 (0) | 0 (102) |
| ltc2400 | 0 (0) | 0 (39) |
| ltc2400_fec | 0 (0) | 0 (240) |
| ltc9600 / ltc9600_fec | 0 (0) | 0 (0) |

The hard-decision receiver never completed a body on these signals. The correlator is more permissive
(that is where its sensitivity comes from); without the two body gates of §9.1 white noise produced
~40–80 failed bodies per minute and MFSK ~240–320. Each failed body is one CRC check without FEC, and
with it as many checks as the decoder finds candidates.

**Bounded since** (`fec.md` §9): the first estimate (36 candidates per body, one false message per
~80 h of MFSK traffic into `ltc_fec`) was replaced by a measurement of the pre-CRC rate. With the decoder
of this section the real numbers were 1.7–1.9 candidates per MFSK false body and one false message per
~1,300 h (`ltc_fec`) and ~530 h (`ltc2400_fec`) of continuous MFSK traffic, and per 1.7–7 h of traffic
from a misconfigured transmitter (`ltc` into an `ltc*_fec` receiver). Payload range checks, a body-level
errata-weight bound on the candidates and a whitened body bring the worst measured case to 185 years
(MFSK traffic into `ltc2400_fec`) and 89 years (`ltc` into `ltc9600_fec`), against a target of at most
one per 10 years. White noise, clicks, music-like and speech-like audio and SMPTE LTC produce no false
body at all, or (music, speech into `ltc2400_fec`) ≥ 5,000 years. Delivery moved by ≤ 0.5 points in the
key cells (`ltc_fec`, `ltc2400_fec`: SNR, clicks, dropouts) (`fec.md` §9.6). The cross-decode test (`mfsk_and_bmc_do_not_cross_decode`) passes for
all six LTC profiles.

### 9.7 "Decode failed → rewind" for BMC

Not needed: the sync correlator keeps running during a body, and a sync that body data cannot produce
(correlation ≥ 0.95), or one clearly stronger than the current lock, replaces it. A truncated packet
(preamble, sync and 0…n − 6 body nibbles) immediately followed by a real one decodes the real one in
all 234 cases over the six LTC profiles (`ltc_fec.rs`, `false_lock_does_not_hide_packet`); with the
replacement rule disabled all 234 are lost. Replaying logged intervals, as first planned, would not work
with a soft receiver anyway (it needs the samples), and costs a buffer.

### 9.8 Recommendation

* Make **`ltc_fec` (4800 bit/s, parity 4) the recommended LTC profile**; keep `ltc` for glitch-free
  digital lines (4 ms faster), `ltc2400` for lines with EQ or very low SNR, and `ltc2400_fec` when
  dropouts up to ~10 ms are expected. All stay experimental.
* The soft-decision receiver replaces the hard-decision one for every `ltc*` profile: it is better in
  every measured condition and costs ~50 more multiply-adds per sample.
* Open: field data (real converter chains, Dante packet loss) to confirm parity 4. The
  miscorrection bound of §9.6 is done (`fec.md` §9).

Reproduce:

```sh
cargo test --release -p cricket_core --test ltc_fec_ab -- --ignored --nocapture
LTC_AB_TRIALS=1000 LTC_AB_PROFILES=ltc,ltc_fec2,ltc_fec4,ltc2400,ltc2400_fec4,ltc9600,ltc9600_fec4 \
  cargo test --release -p cricket_core --test ltc_fec_ab ltc_fec_ab_table -- --ignored --nocapture
cargo test --release -p cricket_core --test stuck_notes ltc_back_to_back_report -- --ignored --nocapture
cargo run --release -p cricket_cli -- loopback --profiles ltc,ltc_fec,ltc2400,fast --notes 60
```

## 10. Fast profiles for digital lines (`ltc14400`, `ltc14400_fec`)

Status: **experimental profiles in the reference implementation, candidate for spec §6.5** (not in the
spec; the decision is open). The profiles of §2–§9 are unchanged on the wire and in the receiver
(`BmcParams::new`; a regression test checks every parameter).

Goal: the lowest latency a dependable BMC link can reach on a **clean digital path** (Dante, MADI, AES,
a virtual loop), where SNR is large but the band is limited by converter and sample-rate-converter
anti-alias filters (flat to ~20 kHz, ~0.45 fs, at 48 kHz, often several in series), sometimes by an
analog leg, and where the clock may be resampled. Noise tolerance may be traded for speed: ≥ 15–20 dB
SNR is acceptable.

### 10.1 The limit: bandwidth, not noise

An all-ones BMC stretch (the preamble) is a square wave **at the bit rate**; zeros put their fundamental
at half the bit rate. With `edge_frac` = 1 (sinusoidal transitions one half cell wide) the preamble is
an almost pure tone at the bit rate and the body's energy sits between ½ and 1 × the bit rate. At 48 kHz
that leaves little room: 14.4 kHz at 14400 bit/s, 16 kHz at 16000, and 19.2 kHz at 19200, which would
already sit in the converter filter's transition band. What breaks first is not the amplitude response
but the **group delay of minimum-phase converter filters** near their band edge: two cascaded
"low-latency" (minimum-phase) filters skew the 16 kHz components against the 8 kHz ones by a fraction
of a half cell (1.5 samples at 16000 bit/s) and the bit decisions fail, while linear-phase filters with
the same passband pass 16000 bit/s at 100 %.

Rates tested: 12000, 14400 and 16000 bit/s at 48 kHz (2.0, 1.67, 1.5 samples per half cell), and
9600, 14400, 19200, 24000 and 28800 bit/s at 96 kHz.

### 10.2 What the fast profiles change

`BmcParams::fast(bit_rate)`, used by `ltc14400` and `ltc14400_fec` (`PhyConfig::ltc_fast_with_bit_rate`,
`ltc_fast_fec_with_bit_rate`):

| | `ltc9600` and the other LTC profiles | fast profiles | why |
|---|---|---|---|
| `edge_frac` (wire) | 0.5 | **1.0** | Sinusoidal edges keep the burst inside a 20 kHz passband. With 0.5 edges two minimum-phase converter filters cut exact bodies from 100 % to 88 % (§10.4). |
| postamble (wire) | 1 bit | **none** | The transition into the tail half cell already terminates the last body bit. |
| gap (wire) | 4 cells | **1 cell** | Bursts can follow each other almost directly (§10.3, §10.6). |
| preamble (wire) | 8 bits | 8 bits | 4 bits saves 0.27 ms but locks worse (§10.3). |
| sync boxes (RX) | rounded to whole samples | **exact half-cell integrals** at fractional delays (`sync_fractional`) | See below. |
| rate gain (RX) | 0.02 | **0.01** | Less rate jitter from noisy zero-crossing times at 1.67 samples per half cell (sweep below). |
| post-body guard (RX) | off | **on** (`post_body_guard`) | See below. |
| RX low-pass | 1.5 × bit rate | same (21.6 kHz at 14400 bit/s; clamped to 0.45 fs at 48 kHz) | Lower cut-offs (1.0, 1.2 or 1.35 × bit rate) were worse on steep converter filters. |

**Fractional sync boxes.** At 9600 bit/s and below, box sums of a whole number of samples at
whole-sample delays reproduce the template well. At 1.67 samples per half cell, rounding to 2-sample
boxes at rounded delays smears the template: real syncs then score lower than the replacement rule
needs (≥ 0.95 replaces any lock), so a false lock on the end of one burst hid the next note of a chord.
The fast receiver integrates each half cell exactly (2–3 weighted samples per box at fractional
delays) and, at a peak candidate, also evaluates the two half-sample points next to it (the
correlation peak is only ~2 samples wide). Real syncs at 40 dB SNR, random sub-sample phase (`rho` =
normalized correlation):

| receiver | median `rho` | 10th percentile |
|---|---|---|
| 9600 bit/s, rounded boxes (unchanged profiles) | 0.964 | 0.935 |
| 14400 bit/s, rounded boxes | 0.916 | 0.823 |
| 14400 bit/s, linearly interpolated box sums (first attempt) | 0.926 | 0.852 |
| 14400 bit/s, exact fractional boxes + half-sample peak search | 0.965 | 0.954 |

**Post-body guard.** The end of a burst (last body bits, tail, fade into silence) can correlate like a
sync (`rho` up to ~0.85 at 14400 bit/s). With a one-cell gap the next burst's sync follows within a
few half cells, and a false lock taken there must not block it. After a completed body the receiver
therefore holds new syncs to the in-body replacement rule until the earliest time the next burst's
sync can arrive. With the fractional correlator it is mostly a safety margin: chords are unchanged
without it on the uncoded profile, and `ltc14400_fec` delivers 1–1.5 points more of its chords at
±1 % drift with it (§10.6).

**Timing loop at 1.67 samples per half cell.** The grid is continuous (fractional) and transitions
are interpolated zero crossings, so resolution is not limited by the sample grid; noise on the
zero-crossing times is. Sweeps over phase gain (0.2–0.45), rate gain (0.01–0.04), capture window,
window taper, integration trims and Schmitt hysteresis changed nothing except the rate gain: 0.04
raised the bit error rate on two minimum-phase filters from 0 to 7.5 × 10⁻³, 0.01 was best. Bit error
rate in delivered bodies (phy diagnostics, 1000 trials per cell) is ≤ 0.1 × 10⁻³ at 10 dB SNR, on every
converter model and at ±1 % drift, and 1.2 × 10⁻³ through a 2-pole 12 kHz low-pass.

Receiver ablation, `ltc14400`, exact bodies % (phy level, 1000 trials, SNR 20 dB unless noted):

| condition | fast profile | rounded sync boxes | rate gain 0.02 | edges 0.5 |
|---|---|---|---|---|
| SNR 10 dB | 99 | 98 | 100 | 100 |
| AD/DA min-phase ×2 (ellip 10, 0.452 fs) | 100 | 97 | 99 | 88 |
| AD/DA min-phase (Chebyshev 8, flat 20 kHz) | 100 | 98 | 99 | 95 |
| 4-pole LP 16 kHz | 100 | 92 | 100 | 100 |
| 2-pole LP 12 kHz | 74 | 44 | 74 | 84 |
| drift +1 % (linear interpolation) | 100 | 98 | 100 | 100 |

### 10.3 Airtime and overhead

At 14400 bit/s a half cell is 34.7 µs: the 8-bit preamble costs 0.56 ms, the sync 0.21 ms, the body
4.44 ms (16 nibbles) or 6.11 ms (22 coded nibbles), the postamble 0.07 ms, the tail 0.03 ms, the gap
0.28 ms (4 cells) or 0.07 ms (1 cell).

| profile | half cell @ 48 kHz | burst | burst + gap (airtime per message) |
|---|---|---|---|
| `ltc9600` | 2.5 samples | 7.97 ms | 8.44 ms |
| `ltc9600_fec` | 2.5 samples | 10.47 ms | 10.94 ms |
| 12000 bit/s, fast parameters | 2.0 samples | 6.29 ms | 6.44 ms |
| **`ltc14400`** | 1.67 samples | **5.24 ms** | **5.38 ms** (−36 %) |
| **`ltc14400_fec`** | 1.67 samples | **6.91 ms** | **7.04 ms** (−36 %) |
| 16000 bit/s, fast parameters | 1.5 samples | 4.72 ms | 4.83 ms |
| 14400 bit/s, original framing (post 1, gap 4) | 1.67 samples | 5.31 ms | 5.65 ms |
| 14400 bit/s, 4-bit preamble | 1.67 samples | 4.97 ms | 5.08 ms |

The shorter postamble and gap save 0.27 ms per message (5 %) and 0.8 ms on a 4-note chord, with no
measurable cost (delivery equal in every cell of §10.4, better at +1 % drift on chords, §10.6). The
cost is on the false-accept side: a misconfigured uncoded transmitter produces more FEC candidates
(§10.7), still far inside the target.

**Shorter preamble.** A 4-bit preamble saves another 0.27–0.3 ms but costs 0.1–0.7 points of delivery
in most cells (SNR 30 dB: 99.9 %, 2 minimum-phase filters: 99.3 %, 12 dB: 99.6 %, 8 dB: 94.5 % against
96.6 %). The cause is false locks: with an 8 + 6 half-cell template body data correlates much better,
and the receiver completes and gates ~1 false body per message (phy diagnostics: 94–107 false bodies
per 100 trials in every condition, against 0–5 with the 8-bit preamble and 15–20 with 6 bits, i.e.
~40× more false locks). The
gates and the replacement rule catch them, but each one is a chance to lose the real packet and a
candidate for the CRC. Keeping 8 bits is the better trade for 0.27 ms.

### 10.4 Delivery at 48 kHz

`ltc_fast_ab.rs` `ltc_fast_ab_table`: single messages (NoteOn, or MSC GO every 4th), random start
offset, random sub-sample phase on paths with an analog leg or SRC, 64-sample chunks, 1000 trials per
cell; % delivered (exactly the message and nothing else). Filters are applied to the transmitted audio,
then AWGN at the SNR (relative to the signal RMS, full band). Converter models: linear-phase FIR flat
to 20 kHz and ≥ 100 dB down from 23 kHz (delay removed; DA → AD = ×2, plus an SRC pair = ×4); 10th-order
elliptic minimum-phase, 0.01 dB ripple to 0.452 fs, 100 dB stop band (the "low-latency" filter of
many converters), ×1 and ×2; 8th-order Chebyshev I minimum-phase, flat to 20 kHz; and softer
Butterworth low-passes. "SRC" is a band-limited windowed-sinc resampler (a real SRC or wrong clock);
"drift" is the simulator's linear-interpolation resampler of the older tables, which also low-passes the
signal by up to 4.6 dB at 0.3 fs.

| channel | ltc9600 | ltc9600_fec | 12000¹ | **ltc14400** | **ltc14400_fec** | 16000¹ | 16000 FEC¹ |
|---|---|---|---|---|---|---|---|
| SNR 30 / 25 / 20 / 18 / 16 / 14 dB | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 12 dB | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 10 dB | 100 | 100 | 99.8 | 99.7 | 100 | 97.6 | 97.8 |
| SNR 8 dB | 100 | 99.8 | 99.0 | 96.6 | 99.4 | 82.3 | 86.4 |
| SNR 6 dB | 99.0 | 99.5 | 88.8 | 77.0 | 96.5 | 38.8 | 61.3 |
| AD/DA linear-phase ×2 (flat 20 kHz) + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| AD/DA linear-phase ×4 + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| AD/DA min-phase ×1 (ellip, 0.452 fs) + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| AD/DA min-phase ×2 (ellip, 0.452 fs) + SNR 20 | 100 | 100 | 100 | 99.8 | 100 | 37.7 | 25.4 |
| AD/DA min-phase (Chebyshev 8, 20 kHz) + SNR 20 | 100 | 100 | 100 | 100 | 100 | 46.6 | 41.1 |
| 4-pole LP 18 kHz + SNR 20 | 100 | 100 | 100 | 100 | 100 | 98.7 | 99.1 |
| 4-pole LP 16 kHz + SNR 20 | 100 | 100 | 100 | 99.9 | 100 | 45.9 | 53.9 |
| 2-pole LP 16 kHz + SNR 20 | 100 | 100 | 100 | 100 | 100 | 91.5 | 91.7 |
| 2-pole LP 12 kHz + SNR 20 | 100 | 100 | 100 | 76.7 | 76.5 | 10.3 | 10.5 |
| clean | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| SRC ±0.1 % + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| SRC ±1 % + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| drift +1 % (linear interp.) + SNR 20 | 100 | 100 | 100 | 100 | 100 | 99.8 | 99.8 |
| drift −1 % (linear interp.) + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| AD/DA ×2 + SRC +0.1 % / −1 % + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| inverted + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| level −30 dB + SNR 20, −50 dB + SNR 30 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| dropout 1 ms + SNR 30 | 2.6 | 94.3 | 0.2 | 0.1 | 91.0 | 0.0 | 92.0 |
| dropout 3 ms + SNR 30 | 0.0 | 81.8 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| clicks 20/s @ 0 dB + SNR 30 | 100 | 100 | 100 | 99.6 | 100 | 99.1 | 99.5 |
| clicks 200/s @ +6 dB + SNR 30 | 65.7 | 93.2 | 67.4 | 67.3 | 92.1 | 68.8 | 89.6 |
| mean start → delivery | 8.6 ms | 11.1 ms | 7.0 ms | 6.0 ms | 7.6 ms | 5.4 ms | 6.9 ms |

¹ `BmcParams::fast` at that bit rate (no profile name). No false message in any cell.

* **14400 bit/s is the fastest rate that passes every converter model**, including two cascaded
  minimum-phase filters and soft low-passes down to 16 kHz, with 100 % down to 12 dB SNR (uncoded) and
  10 dB (FEC). It needs ~16 kHz of bandwidth: a 2-pole 12 kHz low-pass, which no digital path has,
  delivers 77 %.
* **16000 bit/s is not dependable**: 25–47 % through minimum-phase converter filters and a 4-pole
  16 kHz low-pass. 12000 bit/s passes everything at 48 kHz, including the 12 kHz low-pass, for 1.1 ms
  more per message; it is the fallback for chains with analog stages, but has no profile name.
* The **noise floor moves up by ~3 dB** against `ltc9600` (95 % delivered at ~8 dB SNR uncoded and
  ~6 dB with FEC; `ltc9600` stays at 99 % at 6 dB), which is irrelevant on a digital line.
* Dropouts: the FEC repairs any 0.5 ms dropout in the body (`ltc_fast.rs`) and 91 % of 1 ms dropouts
  anywhere in the burst (the rest hit the preamble or sync); 3 ms (43 bits, ~11 nibbles) is beyond the code. A line with dropouts of
  several ms needs `ltc9600_fec` or slower.

### 10.5 96 kHz paths

Same harness at 96 kHz (`FAST_AB_SET=all96`, 1000 trials per cell). Rates up to 28800 bit/s keep at
least 1.67 samples per half cell. Native 96 kHz converters: linear-phase flat to 40 kHz (≥ 100 dB from
46 kHz) and the elliptic minimum-phase filter at 0.452 fs (43.4 kHz). A 48 kHz device or SRC in the
chain: linear-phase flat to 20 kHz, or the elliptic minimum-phase filter with its edge at 21.7 kHz.

| channel | ltc9600 | **ltc14400** | **ltc14400_fec** | 19200¹ | 19200 FEC¹ | 24000¹ | 28800¹ |
|---|---|---|---|---|---|---|---|
| airtime per message | 8.43 ms | 5.36 ms | 7.03 ms | 4.02 ms | 5.27 ms | 3.22 ms | 2.69 ms |
| SNR 30 … 12 dB | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| SNR 10 dB | 100 | 100 | 100 | 100 | 100 | 99.9 | 99.6 |
| SNR 8 dB | 100 | 99.9 | 99.9 | 99.1 | 99.7 | 98.7 | 97.0 |
| SNR 6 dB | 100 | 99.3 | 99.7 | 95.3 | 99.3 | 89.3 | 74.0 |
| native AD/DA linear-phase ×2 (flat 40 kHz) + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| native AD/DA min-phase ×2 (ellip, 0.452 fs) + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 99.9 |
| 48 kHz hop, linear-phase ×2 (flat 20 kHz) + SNR 20 | 100 | 100 | 100 | 100 | 100 | 0 | 0 |
| 48 kHz hop, min-phase ×1 (ellip, 21.7 kHz) + SNR 20 | 100 | 100 | 100 | 8.8 | 2.8 | 0 | 0 |
| 48 kHz hop, min-phase ×2 (ellip, 21.7 kHz) + SNR 20 | 100 | 57.3 | 66.9 | 0.4 | 0 | 0 | 0 |
| 4-pole LP 32 kHz + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| 4-pole LP 24 kHz + SNR 20 | 100 | 100 | 100 | 100 | 100 | 99.9 | 0 |
| 4-pole LP 18 kHz + SNR 20 | 100 | 100 | 100 | 99.8 | 99.8 | 0 | 0 |
| 4-pole LP 16 kHz + SNR 20 | 100 | 100 | 100 | 43.7 | 45.5 | 0 | 0 |
| clean, SRC ±0.1 % / ±1 %, drift ±1 %, inverted, level −30 / −50 dB | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| dropout 1 ms + SNR 30 | 3.4 | 0.1 | 91.4 | 0 | 91.0 | 0 | 0 |
| clicks 200/s @ +6 dB + SNR 30 | 86.7 | 81.0 | 97.2 | 81.8 | 96.3 | 82.4 | 81.5 |
| mean start → delivery | 8.6 ms | 6.0 ms | 7.6 ms | 4.7 ms | 5.9 ms | 3.8 ms | 3.3 ms |

¹ `BmcParams::fast` at that bit rate (no profile name). No false message in any cell.

* `ltc14400` and `ltc14400_fec` work at 96 kHz as at 48 kHz (3.33 samples per half cell, more noise
  margin). The one weak case is a 96 kHz signal through **two** minimum-phase 48 kHz filters (57–67 %):
  at 96 kHz the burst has content above 21.7 kHz that such a hop removes, which a 48 kHz transmitter
  never produces. A 48 kHz hop through a linear-phase SRC is fine.
* On a **pure 96 kHz path** the limit scales with the sample rate: **28800 bit/s** (1.67 samples per
  half cell, 2.7 ms per message) behaves like `ltc14400` at 48 kHz. Any 48 kHz device or SRC, or a
  4-pole low-pass at 24 kHz, stops it completely; 24000 bit/s survives the 24 kHz low-pass but no
  48 kHz hop, and 19200 bit/s does not survive a single minimum-phase 48 kHz hop either.

### 10.6 Chords and back-to-back messages

`ltc_fast_chord_table`: 4-note chords (4 NoteOns encoded back to back, as the engine sends them),
1000 trials per cell; % of chords with all four notes delivered (mean ms from the first sample to the
last note's delivery, i.e. the last-note latency without engine buffering):

| channel | ltc9600 | ltc9600_fec | ltc_fec | **ltc14400** | **ltc14400_fec** | 14400, 4-bit preamble | 14400, original post/gap | ltc14400 no guard | ltc14400_fec no guard |
|---|---|---|---|---|---|---|---|---|---|
| clean | 100 (33.9) | 100 (43.9) | 100 (87.1) | 100 (22.1) | 100 (28.8) | 100 (21.0) | 100 (22.9) | 100 (22.1) | 100 (28.8) |
| SNR 20 dB | 100 | 100 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| AD/DA linear-phase ×2 + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 | 100 | 100 |
| AD/DA min-phase ×2 (ellip) + SNR 20 | 100 | 100 | 100 | 99.2 | 99.8 | 98.8 | 99.8 | 99.2 | 99.0 |
| AD/DA ×2 + SRC +0.1 % + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 | 100 | 99.9 |
| SRC +1 % + SNR 20 | 100 | 99.9 | 100 | 100 | 100 | 100 | 100 | 100 | 99.9 |
| SRC −1 % + SNR 20 | 100 | 100 | 100 | 100 | 100 | 100 | 100 | 100 | 99.9 |
| drift +1 % (linear interp.) + SNR 20 | 100 | 99.3 | 100 | 100 | 97.3 | 99.8 | 96.5 | 100 | 95.7 |
| drift −1 % (linear interp.) + SNR 20 | 100 | 100 | 100 | 99.9 | 99.7 | 100 | 100 | 99.9 | 98.4 |

A 4-note chord completes in **22 ms** (`ltc14400`) or **29 ms** (`ltc14400_fec`), against 34 / 44 ms at
9600 bit/s and 87 ms for `ltc_fec`. No false message in any cell.

### 10.7 False messages

`fec_false_accept.rs` `false_accept_report`, 600 s per signal (method and target: `fec.md` §9; target
≤ 1 false message per 10 years of continuous worst-case audio):

| profile | signal | false bodies / min | candidates / min | years per false message |
|---|---|---|---|---|
| ltc14400_fec | MFSK traffic | 0.2 | 0 | ∞ (no candidate) |
| ltc14400_fec | white noise, clicks, music, speech, SMPTE LTC | 0 | 0 | ∞ (no false body) |
| ltc14400_fec | uncoded LTC (misconfigured TX, `ltc14400`) | 155 | 15.9 | **940** |
| ltc9600_fec (same run) | uncoded LTC (`ltc9600`) | 6,470 | 243 | 62 |

The misconfigured transmitter is the worst case, as for the other LTC FEC profiles, and it is 94× inside
the target (15× better than `ltc9600_fec`): with a one-cell gap the next burst's sync replaces the lock
before an uncoded 16-nibble burst can be read as a 22-nibble coded body, so false bodies are 40× rarer.
White noise never decodes on either fast profile (`bmc_phy.rs`, `ltc_fec.rs`), MFSK and LTC audio of
other rates never decode on them and vice versa (`ltc_fast.rs`, `mfsk_and_bmc_do_not_cross_decode`), and the fast budget test `worst_case_audio_rarely_reaches_the_crc`
now covers uncoded LTC into `ltc14400_fec`. No false message was observed in any measurement of this
section. (An earlier version of the receiver, before the fractional sync boxes, let 5,000 false bodies
and 570 candidates per minute through on the same signal: 26 years.)

### 10.8 Latency and CPU

`cricket loopback --profiles ltc14400,ltc14400_fec,ltc9600,ltc9600_fec,ltc_fec --notes 60` (release,
MIDI in → MIDI out through the engine, in-process, notes played and released):

| profile | mean | p95 | jitter |
|---|---|---|---|
| **ltc14400** | **11.6 ms** | 15.2 ms | 2.4 ms |
| **ltc14400_fec** | **12.0 ms** | 15.2 ms | 2.6 ms |
| ltc9600 | 12.5 ms | 17.4 ms | 2.7 ms |
| ltc9600_fec | 15.8 ms | 19.5 ms | 2.5 ms |
| ltc_fec | 26.5 ms | 30.7 ms | 3.0 ms |

A second run with 200 notes each: `ltc14400` 11.6 / 15.5 / 2.6 ms, `ltc14400_fec` 11.8 / 15.6 / 2.6,
`ltc9600` 12.5 / 15.6 / 2.2, `ltc9600_fec` 16.2 / 20.1 / 2.9 (mean / p95 / jitter).

The phy saves 2.7 ms (uncoded) and 3.6 ms (FEC) of decode delay against 9600 bit/s. The coded profile
gains it fully (16 → 12 ms); the uncoded one gains less than its airtime, because the engine's own
scheduling now dominates: the encoder and the decoder poll every 5 ms (`TICK`, `IDLE_SLEEP`), so a
NoteOn waits up to 5 ms to be encoded and the decoded burst up to 5 ms to be read, and a NoteOn may
also wait behind a NoteOff repeat. With the fast profiles `ltc14400_fec` costs only ~0.2 ms more than
`ltc14400` end to end. Shorter engine polling is the next step for lower latency (all profiles). The
chord numbers of §10.6 show the phy's share without the engine.

*Update:* the engine no longer polls: the encoder wakes on MIDI input and the decoder on the audio
callback (`digital-transparent.md` §9.1). Same loopback, 60 notes: `ltc14400` 5.5 ms, `ltc14400_fec`
7.2 ms, `ltc9600` 8.2 ms mean, jitter < 0.1 ms; each profile now arrives one airtime after the note.

Receiver CPU (host, release, one thread, ns per input sample, best of 5 passes over 10 s; traffic =
20 NoteOns at 30 dB SNR, noise = search only; `ltc_fast_cpu`):

| profile | traffic | noise |
|---|---|---|
| ltc_fec | 37 | 37 |
| ltc9600 / ltc9600_fec | 42 / 39 | 33 / 40 |
| **ltc14400 / ltc14400_fec** | **86 / 74** | **75 / 73** |
| ltc14400 / ltc14400_fec at 96 kHz | 97 / 93 | 97 / 100 |
| 19200 bit/s at 96 kHz | 84 | 84 |

The fractional correlator doubles the receiver cost: 22 boxes of 2–3 weighted samples (~60
multiply-adds) instead of one running box sum and 22 look-ups. That is still ~0.4 % of one host core
at 48 kHz and, counted as in §5 (≈ 100 multiply-adds per sample), ~5 MHz of a Cortex-M33 (~3 % of
150 MHz). Box integrals taken from a running prefix sum could bring the correlator back to ~25
operations per sample if an MCU needs it.

### 10.9 Recommendation

* **48 kHz (Dante, MADI, AES): `ltc14400_fec`** — 7.0 ms airtime per message, ~12 ms mean / 15 ms p95
  MIDI-in → MIDI-out, 29 ms for a 4-note chord; 100 % through every converter model, SRC ±1 % and
  soft low-passes down to 16 kHz at 20 dB SNR, down to 10 dB SNR on white noise; repairs 0.5 ms
  dropouts (91 % of 1 ms); false messages ≥ 940 years in the worst case. **`ltc14400`** (5.4 ms,
  ~11.6 ms mean, 22 ms chord) where the line has no dropouts or clicks. Use `ltc9600_fec` or slower if
  the chain has an analog stage below ~16 kHz, several-ms dropouts, or less than ~15 dB SNR.
* **96 kHz: also `ltc14400` / `ltc14400_fec`**, which keep their numbers at 96 kHz and survive a
  48 kHz device or SRC in the chain (except two minimum-phase 48 kHz hops). On a path that is known to
  run at 96 kHz end to end, **28800 bit/s** (2.7 ms per message, half the latency of `ltc14400`)
  measures as `ltc14400` does at 48 kHz; it is a candidate for an `ltc28800` profile restricted to
  ≥ 88.2 kHz, not added, because one 48 kHz hop or a 24 kHz low-pass stops it completely and a profile
  that only works at some sample rates would need a new rule in the spec. 24000 bit/s (3.2 ms) is the
  middle ground: it survives a 24 kHz 4-pole low-pass but no 48 kHz hop.
* Not worth it: 16000 bit/s at 48 kHz (fails steep minimum-phase converter filters), and the 4-bit
  preamble (−0.27 ms for ~40× more false locks and 0.1–0.7 points of delivery).
* Before the spec: hardware runs through real Dante / MADI / AES converters and SRC, both directions,
  48 and 96 kHz.

**Candidate spec text (§6.5, not normative):** profiles `ltc14400` (16 nibbles) and `ltc14400_fec`
(22 coded nibbles, §4.6): bit rate 14400 bit/s; preamble 8 `1` bits; sync `V V`; body MSB first; **no
postamble** (the burst is `16 + 6 + 8 · nibbles + 1` half cells: 151 / 199; the transition into the
tail half cell terminates the last body bit); transitions raised-cosine steps of **`w = 1`** half cell
(`t = (n − 0.5 · h) / h`, `n = 0 … ceil((H + 1) · h)`); **1 bit cell of silence** after each burst
(`round(2 · h)` samples). Burst 5.24 / 6.91 ms, burst + gap 5.38 / 7.04 ms at 48 kHz. Receiver (SHOULD,
in addition to §6.5.3): sync boxes that integrate exactly one half cell at fractional delays, the
correlation also evaluated half a sample either side of a peak candidate, rate gain 0.01, and after a
completed body the replacement rule applied to new syncs until the next burst's sync can first arrive.
Requirement on the line: ≥ 16 kHz bandwidth at 48 kHz or more (44.1 kHz, 1.53 samples per half cell,
is only loopback-tested).

Reproduce:

```sh
FAST_AB_TRIALS=1000 FAST_AB_PROFILES=ltc9600,ltc9600_fec,ltc12000/fast=1,ltc14400,ltc14400_fec,ltc14400/pre=4,ltc14400/post=1/gap=4,ltc16000/fast=1,ltc16000_fec/fast=1 \
  cargo test --release -p cricket_core --test ltc_fast_ab ltc_fast_ab_table -- --ignored --nocapture
FAST_AB_TRIALS=1000 FAST_AB_SET=all96 FAST_AB_PROFILES=ltc9600@96,ltc14400@96,ltc14400_fec@96,ltc19200@96/fast=1,ltc19200_fec@96/fast=1,ltc24000@96/fast=1,ltc28800@96/fast=1 \
  cargo test --release -p cricket_core --test ltc_fast_ab ltc_fast_ab_table -- --ignored --nocapture
FAST_AB_TRIALS=1000 cargo test --release -p cricket_core --test ltc_fast_ab ltc_fast_chord_table -- --ignored --nocapture
FAST_AB_TRIALS=1000 FAST_AB_PROFILES=ltc9600,ltc14400,ltc14400/pre=4,ltc14400/frac=0,ltc14400/ki=0.02,ltc14400/edge=0.5 \
  cargo test --release -p cricket_core --test ltc_fast_ab ltc_fast_phy_diag -- --ignored --nocapture
cargo test --release -p cricket_core --test ltc_fast_ab ltc_fast_cpu -- --ignored --nocapture
FALSE_SECONDS=600 FALSE_PROFILES=ltc14400_fec,ltc9600_fec \
  cargo test --release -p cricket_core --test fec_false_accept false_accept_report -- --ignored --nocapture
cargo run --release -p cricket_cli -- loopback --profiles ltc14400,ltc14400_fec,ltc9600,ltc9600_fec,ltc_fec --notes 60
```

Files: `cricket_core/src/phy/bmc/{mod,rx}.rs` (`BmcParams::fast`, `sync_fractional`,
`post_body_guard`), `cricket_core/src/phy/config.rs` (profiles), `cricket_core/tests/ltc_fast.rs`
(regression tests), `cricket_core/tests/ltc_fast_ab.rs` (the tables of this section: converter / SRC
filter models, `FAST_AB_TRIALS`, `FAST_AB_PROFILES` with `name[@khz][/param=value…]` keys,
`FAST_AB_SET`).
