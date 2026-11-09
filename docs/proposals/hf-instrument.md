# Proposal: HF (instrument-audio) profiles — MIDI on the instrument's own cable

Status: **provisional** (spec §6.6, revision r6), measured in the channel simulator only; nothing
has been on a real instrument chain yet. Code: `PhyConfig::hf_robust` / `hf_fast` (user-facing
`instrument` / `instrument-fast`), `cricket_core/src/phy/{filter,pack}.rs`, the instrument sources
and converter models in `cricket_core/tests/support/mod.rs`, the measurement harness
`cricket_core/tests/hf_profiles.rs`, and the embedder-side tools `cricket hf-mix` / `cricket
hf-scan`. This document records the measurements behind the spec section, the level budget, what
was rejected and why, and what the box has to be.

## 1. Motivation and use case

A guitarist, bassist or keyboard player wants to send MIDI (preset changes, stomps, tap tempo,
cues, notes) **over the instrument cable while playing**, without a second cable, a wireless MIDI
link or a laptop at the amp. AML already carries whole MIDI messages as tones; the instrument's own
signal occupies 40 Hz – ~10 kHz, and a 48 kHz chain passes audio to ~21.8 kHz. The HF profiles put
Robust's tones in the gap: **19.0–21.5 kHz**, above the music and below what a sharp 48 kHz
converter cuts.

```
instrument ──┬── embedder (adds the tones) ── cable ── pedals (buffers, clean boost, comp, gate) ──┐
             │                                                                                     │
      (sender variants)                                                                            ▼
      · 1 MΩ injection from a low-impedance oscillator at a passive pickup (no tone change)   ┌─────────┐
      · after a buffer / on the pedalboard (trivial)                                          │  box    │──► analog thru (0 ms) ──► amp / drive
      · digital, inside a keyboard or a modeller (mix at −30 dBFS)                            │ tap ADC │──► MIDI DIN / USB
                                                                                              │ (48 kHz)│──► optional stripped output (digital, ≤ 1.5 ms)
                                                                                              └─────────┘
```

The **box** passes the audio through analog (hard-wired, 0 ms, nothing added) and taps it with a
high-impedance input (≥ 1 MΩ ‖ ≤ 10 pF) into a 48 kHz codec ADC; `cricket_core` on an ESP32-S3 /
RP2350 class microcontroller runs the receiver and outputs MIDI on DIN and USB. An optional second
output ("to FOH / DI / drive") is the digital path with the band stripped (≤ 1.5 ms), for rigs where
the tones must not continue into a distortion stage (§5).

The receiver is unchanged AML: link layer, RS16 body code, CRC-16 with the link key, duplicate
suppression, the TX scheduler, heartbeats and the fail-safe. Only the physical layer is moved.

## 2. The profiles

| | `instrument` (`hf_robust`) | `instrument-fast` (`hf_fast`) |
|---|---|---|
| Tones | 16 × 166⅔ Hz from 19.0 kHz (19.0–21.5 kHz), 6 ms + 0.5 ms guard | 8 × 333⅓ Hz from 19.0 kHz (19.0–21.33 kHz), 3 ms, **3 bits per symbol** |
| Body | Robust's RS(11,7)×2 coded 22 nibbles, joint acquisition (2 misses) | the same, repacked into 30 symbols; preamble `5 2 6 1` |
| Airtime per message | 175.5 ms (26 × 6.5 ms + silence) | 105 ms (34 × 3 ms + silence) |
| Ramps | 1 ms raised cosine | 0.5 ms raised cosine |
| Level (SHOULD) | −30 dBFS peak | −30 dBFS peak |
| Receiver band | 18.0–22.5 kHz, 8th-order Butterworth both sides | same |
| Measured NoteOn latency (`cricket loopback`, in-process) | mean 213 ms, p95 331 ms; 4-note chord first 505 / last 1032 ms | mean 121 ms, p95 196 ms; 4-note chord first 252 / last 567 ms |

**Why these plans.** At 48 kHz the usable band is 19.0 kHz (below it, teenagers hear the tones) to
~21.7 kHz (the 0.4535 fs passband edge of a sharp converter). With 16 tones the orthogonal step is
1/T, so the span is 15/T: 6 ms symbols give 2.5 kHz and fit; 4 ms (19.0–22.75 kHz) and 3 ms
(19.0–24.0 kHz) do not. The fastest plan that fits is therefore Robust's timing, or a smaller
alphabet: 8 tones at 3 ms span 2.33 kHz. The survival table (from `hf_report`, per-tone attenuation
through each converter model, tones kept within 6 dB of the plan's strongest):

| plan | airtime | sharp 48 kHz | slow 48 kHz | 44.1 kHz hop (half-band) | 44.1 kHz SRC (sharp) | 19 kHz LP (8th) | 19 kHz codec wall | 96→48 kHz hop | ±1 % clock = bins |
|---|---|---|---|---|---|---|---|---|---|
| `hf_robust`: 16 × 166⅔ Hz, 6 ms (19.0–21.5 kHz) | 162 ms | 16/16 | 9/16 (12 dB tilt) | 16/16 | 11/16 | 4/16 | 3/16 | — | 1.3 |
| `hf_fast`: 8 × 333⅓ Hz, 3 ms, 3 bits (19.0–21.33 kHz) | 105 ms | 8/8 | 5/8 | 8/8 | 6/8 | 2/8 | 2/8 | — | 0.6 |
| 16 × 250 Hz, 4 ms (19.0–22.75 kHz) | 108 ms | 16/16 | 6/16 | 11/16 | 7/16 | 3/16 | 2/16 | — | 0.9 |
| 16 × 333⅓ Hz, 3 ms (19.0–24.0 kHz) | 81 ms | 15/16 | 5/16 | 8/16 | 6/16 | 2/16 | 2/16 | — | 0.7 |
| 8 × 125 Hz, 8 ms, 3 bits (19.0–19.875 kHz) | 280 ms | 8/8 | 8/8 | 8/8 | 8/8 | 5/8 | 4/8 | — | 1.6 |
| `hf_fast_96k`: 16 × 333⅓ Hz, 3 ms (24.0–29.0 kHz) | 81 ms | n/a | n/a | n/a | n/a | n/a | n/a | 2/16 | 0.9 |

The "airtime" column counts a coded message with a 4-symbol preamble. Nothing above 19 kHz survives
a 19 kHz codec; the 8 × 125 Hz plan would survive everything at 48 kHz but costs 280 ms and tolerates
even less clock error. The last column is the frequency shift of the top tone in bins for a 1 %
sample-rate mismatch between the ends: orthogonal detection needs it under ~0.5 bin, so `hf_robust`
tolerates ±0.3 % and `hf_fast` ±0.5 % (crystal-clocked converters are ±0.02 %).

**The 96 kHz variant** (`PhyConfig::hf_fast_96k`, 24.0–29.0 kHz, 78 ms per message) round-trips
in the simulator at 96 kHz (100 % over keys at −30 dBFS) and is **removed entirely** by a 48 kHz
device in the path (0 % through the 96→48 kHz hop model, 2 of 16 tones left within 6 dB). It is a
constructor for native 96 kHz rigs and measurements, not a catalog profile: the engine would have
to open the devices at 96 kHz and the box's codec would have to run there too.

## 3. Simulator: instrument sources, pedalboard, converters

Added to `tests/support` for this study (the Karplus–Strong string and the playing patterns come
from the pedal prototype's harness; the rest is new):

- **Guitar, passive pickup:** strums and lead lines through a resonant second-order low-pass at
  3–5 kHz, Q 2–4 (the pickup inductance against the cable capacitance), so the spectrum at 20 kHz
  is ~30 dB under the resonance rather than absent. **Guitar, bright pickup:** the same strings with
  only a one-pole 6 kHz roll-off (active / piezo). **Bass:** E1 up, 2.5 kHz Q 2 resonance.
  **Bright synth:** band-limited (PolyBLEP) sawtooth chords with an open filter and 5.5 Hz vibrato
  — every harmonic to Nyquist at 1/k, sweeping through the tone slots. **Keys:** electric-piano
  partials through a 4th-order 9 kHz low-pass. **Cymbals:** full-band metallic noise bursts, as a
  sampler or drum machine on the same cable. All at **−18 dBFS RMS** (peaks −8 to +3 dBFS).
- **Pedalboard noise floor** −85 / −95 dBFS white or pink; **PSU spurs** as steady tones at
  −40…−60 dBFS in 19–24 kHz, one exactly in tone slot 6 (20.000 kHz).
- **Converter and link models:** a sharp 48 kHz converter (windowed-sinc, flat to 21.77 kHz, −6 dB
  at Nyquist), the slow-roll-off model of the existing simulator (one pole 10 kHz + second order
  16 kHz: −14…−26 dB across the band), a 44.1 kHz hop with half-band filters in and out (flat to
  20.0 kHz, −6 dB at 22.05 kHz each), a sharp 44.1 kHz SRC (flat to 20.0 kHz, −90 dB from 21.5 kHz),
  a 19 kHz 8th-order codec low-pass, a 19 kHz codec brick wall, and a 96→48 kHz hop.
- **Clock drift** by windowed-sinc resampling (the simulator's linear interpolation attenuates
  20 kHz by up to 12 dB and is wrong for this band).
- An **IMD helper**: tones plus program into `tanh(drive·x)/drive`, residue below 17 kHz relative
  to the driven program (§5).

What the sources leave in the receiver's band (−18 dBFS RMS program; tones at −30 dBFS peak are
−33 dBFS RMS):

| source | peak dBFS | RMS in 18–22.5 kHz (8th-order band, the HF receiver) | RMS after a 2nd-order 13.3–28 kHz band |
|---|---|---|---|
| guitar (passive pickup) | −0.8 | **−83.0 dBFS** | −49.0 dBFS |
| guitar (bright active / piezo pickup) | −1.9 | −42.7 dBFS | −36.9 dBFS |
| bass | −3.6 | −86.9 dBFS | −53.4 dBFS |
| bright synth | −7.3 | −48.0 dBFS | −40.8 dBFS |
| keys | −2.3 | < −150 dBFS | −64.9 dBFS |
| cymbals | +3.4 | **−28.6 dBFS** | −22.9 dBFS |

This is the argument for the 8th-order receiver filter: with Robust's second-order band edges a
guitar leaves −49 dBFS in the measurement, 16 dB under −30 dBFS tones but above −50 dBFS ones; with
the steep band it leaves −83 dBFS. The two sources with real energy in the band are the bright synth
(15 dB under the tones) and cymbals (4 dB *above* them).

## 4. Measurements

`cargo test --release -p cricket_core --test hf_profiles -- --ignored --nocapture`, 400 trials per
cell (a random Note On / Program Change / MSC GO at a random position inside the instrument signal,
decoded in 64-sample chunks), training cells first, then the held-out cells that were never used to
choose anything. Delivered % (mean start → delivery). The third column is `hf_robust` with Robust's
second-order band, kept to show what the steeper filter buys.

### 4.1 Training cells

| chain (instrument at −18 dBFS RMS) | `instrument` (hf_robust) | `instrument-fast` (hf_fast) | hf_robust, 2nd-order band |
|---|---|---|---|
| clean line, no instrument | 100.0 % (170 ms) | 100.0 % (103 ms) | 100.0 % |
| guitar, tones −20 dBFS | 100.0 % | 100.0 % | 100.0 % |
| guitar, tones −30 dBFS | 100.0 % | 100.0 % | 100.0 % |
| guitar, tones −40 dBFS | 100.0 % | 100.0 % | 99.8 % |
| guitar, tones −50 dBFS | 100.0 % | 100.0 % | 22.5 % |
| bass, tones −30 / −40 / −50 dBFS | 100 / 100 / 100 % | 100 / 100 / 100 % | 100 / 100 / 100 % |
| bright synth, tones −20 / −30 / −40 dBFS | 100 / 100 / 100 % | 100 / 100 / 100 % | 100 / 100 / 87.0 % |
| keys, tones −30 / −40 / −50 dBFS | 100 / 100 / 100 % | 100 / 100 / 100 % | 100 / 100 / 100 % |
| cymbals, tones −20 dBFS | 100.0 % | 100.0 % | 99.0 % |
| cymbals, tones −30 dBFS | 72.8 % | 53.8 % | 22.0 % |
| cymbals, tones −40 dBFS | 13.0 % | 9.8 % | 3.2 % |
| bright-pickup guitar, tones −30 / −40 dBFS | 100 / 99.8 % | 100 / 100 % | 100 / 98.8 % |
| guitar + hiss −85 dBFS, tones −30 / −40 / −50 | 100 / 100 / 100 % | 100 / 100 / 100 % | 100 / 100 / 24.0 % |
| guitar + hiss −95 dBFS, tones −50 | 100.0 % | 100.0 % | 24.5 % |
| guitar + spur 20.0 kHz (slot 6) −40 dBFS, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar + spur 20.0 kHz (slot 6) −50 dBFS, tones −30 / −40 | 100 / 100 % | 100 / 100 % | 100 / 100 % |
| guitar + PSU spurs 20.0/−50, 22.3/−40, 23.7/−40, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar, sharp 48 kHz converter, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar, slow roll-off converter, tones −30 | 99.8 % | 100.0 % | 48.2 % |
| guitar, 44.1 kHz hop (half-band filters), tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar, 44.1 kHz sharp SRC, tones −30 | 96.0 % | 83.5 % | 95.8 % |
| guitar, 19 kHz 8th-order codec LP, tones −30 | 39.5 % | 14.5 % | 13.0 % |
| guitar, 19 kHz codec brick wall, tones −30 | 0.0 % | 0.0 % | 0.0 % |
| guitar, gate −40 dBFS 5/50/100 ms, tones −30 / −50 | 100 / 100 % | 100 / 100 % | 100 / 25.8 % |
| guitar, gate −25 dBFS 5/50/100 ms (tuner mute), tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar, comp −30 dBFS 4:1 5/100 ms, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar, limiter −20 dBFS, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar, drift +0.1 % / +0.3 %, tones −30 | 100 / 100 % | 100 / 100 % | 100 / 100 % |
| guitar, drift +1 % / −1 %, tones −30 | 0 / 0 % | 0 / 0 % | 0 / 0 % |
| guitar, 10 ms / 30 ms dropout, tones −30 | 100 / 96.0 % | 98.8 / 83.5 % | 100 / 96.0 % |
| guitar, burst 3 × 1 ms 10/s, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar, box input −20 dB + hiss −95, tones −30 | 100.0 % | 100.0 % | 100.0 % |

No false message in any cell (0 of 3 × 47 × 400 trials).

### 4.2 Held-out cells

| chain (instrument at −18 dBFS RMS) | `instrument` (hf_robust) | `instrument-fast` (hf_fast) | hf_robust, 2nd-order band |
|---|---|---|---|
| guitar, tones −25 / −35 / −45 dBFS | 100 / 100 / 100 % | 100 / 100 / 100 % | 100 / 100 / 84.8 % |
| bass, tones −35 / −45 dBFS | 100 / 100 % | 100 / 100 % | 100 / 100 % |
| bright synth, tones −25 / −35 dBFS | 100 / 100 % | 100 / 100 % | 100 / 100 % |
| keys, tones −35 / −45 dBFS | 100 / 100 % | 100 / 100 % | 100 / 100 % |
| cymbals, tones −25 / −35 dBFS | 99.5 / 26.2 % | 95.5 / 24.0 % | 54.8 / 8.2 % |
| guitar + pink hiss −90 dBFS, tones −40 | 100.0 % | 100.0 % | 100.0 % |
| keys + hiss −85 dBFS, tones −45 | 100.0 % | 100.0 % | 100.0 % |
| guitar + spur 20.0 kHz (slot 6) −45 dBFS, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| guitar + spur 20.08 kHz (between slots) −40 dBFS, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| bass + spur 21.5 kHz (slot 15) −50 dBFS, tones −35 | 100.0 % | 100.0 % | 100.0 % |
| guitar, sharp converter + hiss −90, tones −40 | 100.0 % | 100.0 % | 100.0 % |
| guitar, slow converter + hiss −85, tones −40 | 0.0 % | 0.0 % | 0.0 % |
| keys, 44.1 kHz hop (half-band filters), tones −25 | 100.0 % | 100.0 % | 100.0 % |
| keys, 44.1 kHz sharp SRC, tones −25 | 97.0 % | 86.0 % | 97.0 % |
| bright-pickup guitar, tones −35 dBFS | 100.0 % | 100.0 % | 100.0 % |
| guitar, gate −35 dBFS 10/30/200 ms, tones −35 | 100.0 % | 100.0 % | 100.0 % |
| guitar, comp −25 dBFS 8:1 2/50 ms, tones −35 | 100.0 % | 100.0 % | 100.0 % |
| guitar, drift +0.5 %, tones −30 | 0.0 % | 100.0 % | 0.0 % |
| guitar, drift −0.2 %, tones −35 | 100.0 % | 100.0 % | 100.0 % |
| guitar, 20 ms dropout, tones −35 | 97.5 % | 96.0 % | 97.5 % |
| synth + hiss −85 + sharp converter, tones −30 | 100.0 % | 100.0 % | 100.0 % |
| cymbals + spur 20.0 kHz −50, tones −30 | 69.0 % | 51.5 % | 22.8 % |

No false message in any cell. Held-out agrees with training everywhere: the only surprise is
**slow converter + hiss at −40 dBFS tones: 0 %** — the slow model attenuates the band by 14–26 dB,
which puts −40 dBFS tones at −54…−66 dBFS, under the receiver's −60 dBFS silence floor (the hiss
is attenuated with them, so this is the floor, not noise). Rule: the tones must reach the box's ADC
at ≥ −55 dBFS peak in band; a dark converter costs level, so keep −30 dBFS on unknown chains.

### 4.3 What the tables say

1. **Over a guitar, bass or keyboard every message arrives from −20 to −50 dBFS**, with the
   pedalboard hiss floors, with spurs in and between the tone slots, through compressors, limiters,
   gates (including a tuner-mute-style gate: the tones at −30 dBFS do not open a −25 dBFS gate, but
   messages are sent while the player plays), and through the sharp and slow 48 kHz converter
   models. The −30 dBFS default has 20 dB of margin in the simulator.
2. **Full-band sources** on the same cable (cymbal samples: −28.6 dBFS in the band) need the tones
   at −20 dBFS (100 %); at −30 dBFS 73 % / 54 %. A keyboard that plays drum samples should embed
   at −20 dBFS, or digitally after its own mix.
3. **`instrument-fast` costs dropouts, not level:** 10 ms dropouts 98.8 % (vs 100 %), 30 ms 83.5 %
   (vs 96 %), the sharp 44.1 kHz SRC 83.5 % (vs 96 %); one erased 3-bit symbol can erase two
   nibbles of the code. Everything level-related is identical. It tolerates more clock error
   (±0.5 % vs ±0.3 %: its bins are twice as wide).
4. **Not supported, as the spec says:** a 19 kHz codec or wireless link (0–40 %), a sharp 44.1 kHz
   SRC (96 %: the code fills the lost top tones on a quiet line; with a hiss floor it will not), a
   sample-rate mismatch of 1 % (0 %: the tones move 1.3 bins), and anything nonlinear before the box
   (§5). A 44.1 kHz hop through half-band converter filters passed 100 % in the model, but that
   depends on the converter and is not relied on.
5. The **2nd-order band** loses at −50 dBFS over a guitar (22 %), on a bright synth at −40 dBFS
   (87 %), through a slow converter (48 %) and below a gate (26 %): the 8th-order filter is what
   makes the HF band usable at the levels the audibility budget wants.

### 4.4 False messages and cross-decode

60 s of each signal into the `instrument` receiver (method of `fec.md` §9: count phy locks, bodies,
RS candidates and structural passes, apply the CRC-16 factor):

| signal | phy locks / min | bodies / min | RS candidates / min | false messages |
|---|---|---|---|---|
| guitar, bass, bright synth, keys, cymbals (each at −12 dBFS RMS) | 0 | 0 | 0 | 0 |
| white noise −30 dBFS, speech, music | 0 | 0 | 0 | 0 |
| MFSK Robust traffic (2.5–5 kHz), MFSK Fast traffic (2.5–7.5 kHz) | 0 | 0 | 0 | 0 |
| LTC `clean-line` traffic, `ltc14400_fec` traffic | 0 | 0 | 0 | 0 |
| HF traffic with another link key | 761 | 760 | 741 | 0 (deterministic CRC reject, spec §4.1) |

Not one phy lock in 12 minutes of foreign audio: the 18 kHz 8th-order high-pass leaves the receiver
nothing to lock on below the band, and nothing in the simulator's sources puts a steady tone pattern
above it. The ten-year target is therefore not bounded by this measurement but by the link layer,
which is Robust's: the worst case of `fec.md` §9 (≥ 89 years per false message on Robust-class
traffic that *does* lock) carries over. The baseband receivers (`analog-safe`, `analog-safe-repeat`,
`analog-fast`, `analog-safe-fast`) produced 0 locks and 0 messages on 60 s of HF traffic: the two
families can share a line (a guitar with HF tones feeding a desk that also carries an analog-safe
link) without hearing each other.

### 4.5 Receiver CPU

Host (release build, 4-core VM), ns per input sample, on 10 s of guitar with and without
back-to-back packets:

| profile | idle on guitar | busy | band filter sections |
|---|---|---|---|
| `analog-safe` (robust) | 53.5 | 74.2 | 2 |
| `instrument` (hf_robust) | 61.8 | 95.5 | 8 |

The six extra biquad sections cost ~8 ns per sample here; at 48 kHz the HF receiver needs ~4.6 ms of
CPU per second busy. Estimate for a microcontroller from the operation count (band filter 40 MACs,
joint-acquisition pre-check ~20 MACs per sample, full bank and tracking while a body is read ~60
MACs per sample): 3–6 MMAC/s, i.e. **~10–25 % of one ESP32-S3 core at 240 MHz with its FPU, 20–40 %
of an RP2350 core at 150 MHz** — comfortable for a mono stream, and the `no_std` build of
`cricket_core` allocates nothing per sample. These are estimates; the real figure comes with the
firmware.

## 5. Audibility and intermodulation budget

**Audibility.** The tones are at 19.0–21.5 kHz and ≤ −30 dBFS peak. Hearing thresholds rise steeply
above 16 kHz; at 20 kHz roughly half of listeners under 30 respond at all, and practically no adult
does at levels below the music. Through a full-range PA at 100 dB SPL, −30 dBFS tones under a
−18 dBFS program are at ~80 dB SPL at 19–21.5 kHz, below the threshold of adult listeners, and the
plan deliberately stays above 19 kHz, where teenagers do hear. What the tones leave *in* the audible
band by themselves: with the 1 ms raised-cosine ramps **−58 dB** of their energy lies below 17 kHz
(Robust's linear 0.3 ms ramps: −40 dB), i.e. below −88 dBFS for −30 dBFS tones. This is why the HF
profiles got their own ramp shape (a wire-format property of these profiles only).

**Intermodulation.** The tones ride on the program into whatever comes after the embedder. If that
is a nonlinear stage (an overdrive, a fuzz, a saturating preamp, a digital modeller without
oversampling), the tones are amplitude-modulated by the program's clipping and their sidebands and
aliases land in the audible band. Measured with the IMD helper (guitar at −18 dBFS RMS, residue
below 17 kHz relative to the driven guitar):

| tone level (dBFS peak) | drive 3 (clean boost) | drive 10 (overdrive) | drive 30 (fuzz) |
|---|---|---|---|
| −20 | −26.2 dB | −13.8 dB | −7.2 dB |
| −30 | −37.8 dB | −25.7 dB | −17.2 dB |
| −40 | −48.0 dB | −36.1 dB | −27.7 dB |
| −50 | −58.0 dB | −46.2 dB | −37.8 dB |

Each 10 dB less tone level buys 10–20 dB of residue, and no sensible level makes a fuzz quiet: at
−30 dBFS the blip is 17 dB under the music, which is a plainly audible artefact (it would be heard
as a short "zzt" of the message's rhythm every time a message is sent while the drive is on). The
pedal study came to the same number (−22…−32 dB) and the same conclusion, which this design adopts:
**the tones must never reach a nonlinear stage.** Hence the box: it taps the signal where the tones
are still clean, and its analog thru is fine for clean, compressed or gated rigs; for a rig with
drive after the box, the player uses the box's **stripped digital output** (band removed in DSP,
≤ 1.5 ms) into the drive, or places the box last before the amp with the drive before the embedder
(a modeller or keyboard embedding digitally after its own processing has no problem at all). An
analog strip filter is not an option: a 4th-order 18 kHz low-pass is only −5 dB at 20 kHz and already
−0.9 dB at 15 kHz.

**Level rule of thumb.** Default −30 dBFS: inaudible, 20 dB of delivery margin over every
instrument. −20 dBFS for a cable that also carries full-band samples (cymbals), knowing that a clean
boost after the embedder then leaves −26 dB of residue. −40…−50 dBFS where the line is quiet and a
clean boost follows (−48 / −58 dB). The receiver does not care within this range.

## 6. Limits

- **48 kHz end to end, copper or 48 kHz PCM.** No 44.1 kHz device, no wireless link, no codec, no
  sample-rate conversion (the sharp SRC leaves 96 % on a quiet line and should be treated as "no").
- **Clock error ≤ ±0.3 %** (`instrument`) / ±0.5 % (`instrument-fast`) between embedder and box;
  crystal-clocked converters are 10× inside this. A receiver with a frequency-offset search would
  lift the limit at ~9× the acquisition cost; not done.
- **Nothing nonlinear between embedder and box**, and nothing nonlinear after the box unless the
  box strips the band (§5).
- **Full-band sources** on the cable (cymbal samples, white-noise effects) want −20 dBFS tones.
- **Dark converters** (−14…−26 dB across the band): keep the tones at −30 dBFS so they reach the
  ADC above −55 dBFS.
- **Dropouts:** `instrument` as Robust (30 ms: 96 %); `instrument-fast` loses 16 % at 30 ms.
- **Latency** is Robust's: 175.5 ms of airtime, ~213 ms mean NoteOn in loopback; `instrument-fast`
  105 ms / ~121 ms. For stomps and tap tempo that is slow; a shorter CRC-protected packet phy in the
  same band (≈ 30–45 ms) is the next step if the field wants it, not more signatures (§8).
- **Not measured:** a real pickup's 19–24 kHz content and a real pedalboard's noise floor (the first
  field measurement, as the review says), real converters at 20–21.5 kHz, long cables with a passive
  pickup, the box's own analog front end.

## 7. The box (hardware sketch)

- **Signal path:** ¼" in → hard-wired ¼" thru (0 ms, no buffer, no relay needed: the tap is passive)
  with a high-impedance tap (≥ 1 MΩ ‖ ≤ 10 pF, JFET or op-amp buffer at the tap only) → anti-alias
  → 48 kHz codec ADC (24-bit; the band of interest is at 0.4–0.45 fs, so a codec with a sharp
  decimation filter, passband ≥ 0.4535 fs) → MCU. Acceptance: ±0.1 dB 20 Hz–20 kHz at 1 MΩ, THD+N
  ≤ 0.003 %, noise ≤ −105 dBu on the thru (i.e. nothing measurable: the thru is a wire).
- **MCU:** ESP32-S3 (240 MHz, FPU, USB OTG for USB-MIDI, I²S) or RP2350 (Cortex-M33 / RISC-V,
  150 MHz, FPU, USB, PIO I²S). `cricket_core` builds `no_std` for `thumbv8m.main-none-eabihf` and
  `riscv32imac-unknown-none-elf` today; §4.5 estimates 10–40 % of one core for the receiver.
- **Outputs:** MIDI DIN (5-pin, 31.25 kbit/s) and USB-MIDI; the fail-safe burst and link-dead
  behaviour as AML §5.3 (All Notes Off when the embedder goes quiet for 2 s).
- **Optional stripped output:** the ADC stream with 18–22.5 kHz removed (the receiver's own filter
  inverted, or a 20 kHz linear-phase FIR) → DAC → ¼" "to drive / DI"; ≤ 1.5 ms added (codec in +
  out + a 64-sample block).
- **Sender variants:** (a) a low-impedance oscillator on the instrument injected through 1 MΩ at a
  passive pickup (the source impedance is so high that the pickup's tone does not change; the tone
  level is set by the divider against the pickup's ~10 kΩ–1 MΩ load, so it needs calibration once);
  (b) summed after a buffer or on the pedalboard (trivial, an op-amp summer); (c) digital inside a
  keyboard or modeller (mix the encoder's samples at −30 dBFS into the output before the DAC).
- **Placement:** the box first on the pedalboard is the near-ideal channel measured here. At FOH,
  after a DI, a long cable and possibly a wireless system, only 48 kHz PCM or copper survives.

## 8. Acceptance criteria for "playable"

From the review (§F) and this study:

1. 0 ms added latency on the analog thru; ≤ 1.5 ms on the stripped digital output.
2. Thru: ±0.1 dB 20 Hz–20 kHz at 1 MΩ, THD+N ≤ 0.003 %, noise ≤ −105 dBu.
3. 100 % of 1 000 commands delivered through 6 m of cable and three buffered pedals, at −30 dBFS.
4. 0 false messages in 24 h of playing, with the PSU and the pedalboard powered.
5. Inaudible in an A/B test through a full-range PA at 100 dB SPL with the tones at ≤ −30 dBFS.
6. IMD blip through a high-gain stage ≤ −45 dB under the program, or the band stripped before it.
7. Message latency as measured here (`cricket loopback`): ~213 ms (`instrument`) / ~121 ms
   (`instrument-fast`) mean NoteOn in-process; the box adds its codec and MIDI byte time (+3–5 ms).

## 9. Why not the 4-signature pedal prototype

The prototype on branch `claude/tender-johnson-pfysx4` (`cricket_core/src/pedal.rs`,
`docs/proposals/pedal-commands.md`) sent four relative commands (bank ±, program ±) as 16 ms tone
signatures at 20.0–21.5 kHz. The review (`docs/review-2026-11.md` §F) found, and this study agrees:

- **Bounded vocabulary:** at most 7 commands with 7 tones; real needs (direct presets, stomp on/off,
  tap tempo, tuner, snapshots, expression, cues) need a payload and a check, which is AML's link
  layer.
- **No CRC:** a signature *is* the message; nothing catches a near miss, and tolerating one missed
  symbol produced 908 false commands in 10 minutes of AML traffic moved into the band.
- **Relative-only state:** a missed or doubled command leaves the pedal one preset off, and nothing
  corrects it; AML sends absolute messages with sequence numbers and duplicate suppression.
- Its simulator was unrealistically dark above 19.5 kHz (−116 dBFS), so the "−50/−60 dB under the
  music" rows were artefacts; this study's guitar leaves −83 dBFS in the band and the bright
  sources −48 / −29 dBFS.

Its ideas that this design keeps, with credit: one tone at a time in a band above the music, purity
measured **after** a steep (8th-order Butterworth) high-pass, level-independent detection on
orthogonal bins, raised-cosine symbol edges, the synthetic guitar / bass / keys generators and the
false-trigger harness, the IMD-through-a-fuzz measurement, and the WAV tools (`pedal-wav` /
`pedal-scan` became `hf-mix` / `hf-scan`). It stays an experiment outside AML; it was not merged.

## 10. Trying it on a real recording (before hardware)

```sh
# Embed a Program Change, a note (on + off), a CC and an MSC GO every second into a 48 kHz
# recording of the instrument (the first channel of a stereo file is used):
cricket hf-mix --wav-in guitar.wav --wav-out mixed.wav \
    --messages "pc=1:10,note=1:60:100,cc=1:7:90,go=14.5:2" --repeat

# Play mixed.wav through the rig (cable, pedals, interface) and record the far end at 48 kHz, then:
cricket hf-scan --wav-in recording.wav
#   -> every decoded message with its time, packets passed / failed, tones repaired by the code,
#      and the recording's level in the tone band (18–22.5 kHz) next to its overall level.

# Variants: --profile instrument-fast, --tx-level-db -20 (a cable with full-band samples), a
# 44.1 kHz recording is refused with the reason, and `--profile analog-safe` scans a recording
# of the baseband profiles the same way.
```

## 11. IP note

Ultrasonic data-over-audio is a patented field: LISNR (18.7–19.2 kHz, ~130 patents), Chirp (now
Sonos), CUE Audio, Intrasonics; Nielsen and Verance for masking-based watermarks. No product was
found that hides MIDI or timecode in an instrument's or a playback signal ultrasonically, and AML's
mechanism (MFSK packets with FEC and CRC, a published open protocol) predates nothing here on its
own merits; still, a freedom-to-operate search in CPC G10H 1/0058, G10H 3/18, H04B 11/00 and H04H
20/31 is due **before a product launch**, not before field tests.

## 12. Negative results, for the record

- 16 tones at 3 ms (19–24 kHz) and at 4 ms (19–22.75 kHz): do not fit under a sharp 48 kHz
  converter (15/16 and 16/16 tones pass the sharp model, but 44.1 kHz hops and the slow model take
  the top; the 4 ms plan's top tone at 22.75 kHz sits in every converter's transition band). Not
  built.
- A 2nd-order receiver band (Robust's) at 19–21.5 kHz: works at −30 dBFS, fails at −50 dBFS over a
  guitar and through a slow converter; superseded by the 8th-order band.
- Robust's linear 0.3 ms ramps at 19 kHz: −40 dB of leakage into the audible band; replaced by
  raised cosines (−58 dB).
- The 96 kHz variant as a catalog profile: works only on 96 kHz rigs, removed by any 48 kHz device,
  and the engine opens devices at the profile rate but every other profile is 48 kHz; kept as a
  constructor.
- Survival of a 44.1 kHz hop: depends on the converter filter (100 % with half-band filters, 96 %
  / 83.5 % through a sharp SRC on a quiet line); listed as unsupported rather than as a feature.
- A −1 %/+1 % sample-rate mismatch: 0 % on both profiles; a frequency-offset search was not added.
