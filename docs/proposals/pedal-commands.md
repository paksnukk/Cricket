# Proposal: pedal commands above the music (experimental)

Status: **prototype, measured in the simulator only.** Not part of AML v1 and not interoperable with it.
Code: `cricket_core/src/pedal.rs` (TX, RX, preset state), `cricket_core/tests/pedal.rs` (regression
checks and the report below), `cricket pedal-wav` / `cricket pedal-scan` (WAV tools for a real rig).
Nothing here has been on real hardware yet.

## 1. Idea

A guitarist (or a footswitch, or a controller on the guitar) wants to step a pedal's presets **over the
instrument cable, while playing**. AML sends whole MIDI messages: 54–273 ms of tones in 2.5–5.5 kHz,
right where the guitar is. Here there are only four messages, so the signal can be little more than a
preamble: **the signature is the message**.

| Command | Meaning on the receiving side |
|---|---|
| `bank-up` / `bank-down` | same slot, next / previous bank |
| `program-up` / `program-down` | next / previous preset (crossing into the next / previous bank) |

The signal sits at **20.0–21.5 kHz**, above everything a guitar, bass or keyboard puts out in practice
(a passive pickup is down ~20–35 dB by 20 kHz, guitar speakers stop at ~5–6 kHz), and inaudible.
The receiving end high-passes the music away, recognizes the signature, and sends MIDI.

## 2. Design

### 2.1 Signal (`PedalConfig::standard`)

| Parameter | Value |
|---|---|
| Sample rate | 48 kHz (the whole chain; 44.1 kHz leaves no room above 20 kHz) |
| Tones | 7, at 20 000 + 250 k Hz (20.0 … 21.5 kHz), whole multiples of 1/symbol, so the Goertzel bins are orthogonal |
| Symbol | 4 ms (192 samples), one tone, flat top with 1 ms raised-cosine attack and release |
| Command | 4 symbols (16 ms), then one symbol of silence |
| Level | peak 0.1 (−20 dBFS) by default; detection is level-independent |

One tone at a time, so a nonlinear stage sees no difference tones between two control tones, and every
burst starts and ends at zero, so there are no phase steps. 99.996 % of the energy is above 19 kHz
(§4.3).

The codewords were chosen by exhaustive search over 4-tuples of the 7 tones:

| Command | Tones (index) | Frequencies (kHz) |
|---|---|---|
| `bank-up` | 0 3 6 1 | 20.00 20.75 21.50 20.25 |
| `bank-down` | 1 6 3 0 | 20.25 21.50 20.75 20.00 |
| `program-up` | 2 5 0 4 | 20.50 21.25 20.00 21.00 |
| `program-down` | 4 1 5 2 | 21.00 20.25 21.25 20.50 |

- any two differ in **every** position, so one corrupted symbol cannot turn one command into another;
- neighbouring symbols are ≥ 3 tones (750 Hz) apart;
- no codeword runs monotonically, so a sweep (a whistle, feedback, a glide) cannot match;
- a codeword shifted by 1–3 symbols agrees with any codeword in at most one position;
- every tone is used 2 or 3 times.

Two shorter variants were measured: `three_symbols` (3 × 4 ms = 12 ms, same tones) and
`short_symbols` (4 × 3 ms = 12 ms, 5 tones 333 Hz apart, 0.75 ms edges).

### 2.2 Receiver (`PedalRx`)

1. **High-pass:** 8th-order Butterworth at 19.5 kHz (biquad cascade, f32). −0.6 dB at 20 kHz, −55 dB at
   15 kHz, −101 dB at 10 kHz (f32 arithmetic floors it near −90 dB).
2. **Every 1 ms** (a quarter symbol), a Goertzel bank over the last 4 ms of filtered signal. The window
   shows a tone when the strongest tone has **purity ≥ 0.5** (its power against the energy of the
   *filtered* window, 1.0 = a pure steady tone), is **≥ 3 dB above the runner-up**, and the window is
   above −100 dBFS RMS. Measuring purity after the filter is the point: the music below 19.5 kHz no
   longer counts against it (the same lesson as AML Robust, whose limit at −3 dB SNR is its purity gate).
3. **Match:** a command is recognized when the windows that ended 3, 2, 1 and 0 symbols ago show its
   four tones. All four must match: there is no CRC to catch a near miss. Optionally
   (`max_misses = 1`) one window may show *no* tone (a dropout or a click), never a *different* tone.
4. After a detection the receiver ignores the next 16 ms (the same transmission still matches at
   neighbouring steps).

Cost: ~52 multiplies and ~80 additions per sample (filter 20, Goertzel 7 tones × 4 overlapping windows,
energy), 33 ns per sample on the host. From the operation count, roughly 5–15 % of a Cortex-M4F at
168 MHz (an estimate, not measured). `cricket_core` builds `no_std`; the receiver does not allocate per sample.

### 2.3 MIDI (`PresetState`)

MIDI has no "program up". The receiver keeps the current preset and sends it as absolute MIDI:
`Flat` (Program Change = bank × presets per bank + slot, ≤ 128 presets) or `BankSelect` (CC 0 = bank,
then Program Change = slot). Both wrap around. The cost of relative commands: a missed or doubled
command leaves the pedal one preset off, and nothing corrects it. `PresetState::set_index` is the hook
for an absolute resync (§5).

## 3. How it was measured

`cargo test --release -p cricket_core --test pedal -- --ignored --nocapture` (variables in the file
header). Deterministic; every number below comes from that report.

**Music** (48 kHz, normalized to −18 dBFS RMS):

- **band:** Karplus–Strong guitar (strummed open chords from low E and lead lines, through a 6 kHz
  4th-order low-pass like a pickup and cable) at −20 dBFS, bass from E1 (2 kHz low-pass) at −22 dBFS,
  keys (polysynth chords through a 9 kHz low-pass, like an electric piano, organ or pad) at −24 dBFS.
  In 19.5–24 kHz the guitar leaves −116 dBFS and the bass −97 dBFS.
- **bright guitar:** the guitar without the pickup low-pass. Every pluck starts with a white-noise burst
  up to 24 kHz (a piezo or very bright active pickup, the worst case for a guitar).
- **bright synth:** the polysynth unfiltered: naive (aliasing) sawtooths with vibrato. Its energy in
  19.5–24 kHz is only ~22 dB below its own level, more than the commands at −30 dB. An analog
  sawtooth with an open filter is similar.
- **fuzz guitar:** the bright guitar through a digital fuzz without oversampling (tanh, drive 30), whose
  aliased harmonics fill the whole band.

**Commands** go in at random times 50–250 ms apart (about 4 per second) at a level relative to the
music (control tone RMS / music RMS). "Control −30 dB" means commands 30 dB below the music.
**Latency** is from the start of a command to its detection; it does not include converter or pedal
buffers (typically 1–5 ms more) or the MIDI byte time (~1 ms).

**Channel:** white noise (SNR relative to the control tone, over the full 24 kHz band), clicks (1–4
sample spikes), a dropout inside every command, clock drift (windowed-sinc resampling; the channel
simulator's linear interpolation would itself attenuate 20 kHz by up to 12 dB), a converter low-pass,
an overall level change, and a digital fuzz between encoder and decoder.

## 4. Results

### 4.1 Delivery and latency

30 s × 2 seeds per cell (~350 commands). Delivered % (mean / p95 latency). No cell produced a wrong
or a false command.

| channel | standard (4 x 4 ms) | three_symbols (3 x 4 ms) | short_symbols (4 x 3 ms) | standard, 1 miss allowed |
|---|---|---|---|---|
| clean, no music | 100.0 % (15.9 / 16.3 ms) | 100.0 % (11.8 / 12.3 ms) | 100.0 % (12.0 / 12.3 ms) | 100.0 % (15.7 / 16.2 ms) |
| band, control -20 dB | 100.0 % (15.9 / 16.3 ms) | 100.0 % (11.8 / 12.3 ms) | 100.0 % (12.0 / 12.3 ms) | 100.0 % (15.7 / 16.2 ms) |
| band, control -30 dB | 100.0 % (15.9 / 16.3 ms) | 100.0 % (11.8 / 12.3 ms) | 100.0 % (12.0 / 12.3 ms) | 100.0 % (15.7 / 16.2 ms) |
| band, control -40 dB | 100.0 % (15.9 / 16.3 ms) | 100.0 % (11.8 / 12.3 ms) | 100.0 % (12.0 / 12.3 ms) | 100.0 % (15.7 / 16.2 ms) |
| band, control -50 dB | 100.0 % (15.9 / 16.3 ms) | 100.0 % (11.8 / 12.3 ms) | 100.0 % (12.0 / 12.3 ms) | 100.0 % (15.7 / 16.2 ms) |
| band, control -60 dB | 99.7 % (15.9 / 16.3 ms) | 99.4 % (11.8 / 12.3 ms) | 99.4 % (12.0 / 12.3 ms) | 100.0 % (15.7 / 16.2 ms) |
| band -30 + noise, SNR 10 dB | 100.0 % (15.9 / 16.4 ms) | 100.0 % (11.8 / 12.3 ms) | 100.0 % (12.0 / 12.3 ms) | 100.0 % (15.8 / 16.3 ms) |
| band -30 + noise, SNR 3 dB | 100.0 % (16.0 / 16.5 ms) | 100.0 % (12.0 / 12.4 ms) | 100.0 % (12.1 / 12.4 ms) | 100.0 % (15.9 / 16.4 ms) |
| band -30 + noise, SNR 0 dB | 94.3 % (16.1 / 16.6 ms) | 95.2 % (12.1 / 12.6 ms) | 90.5 % (12.2 / 12.6 ms) | 100.0 % (16.0 / 16.5 ms) |
| band -30 + noise, SNR -3 dB | 18.0 % (16.3 / 16.7 ms) | 31.4 % (12.2 / 12.7 ms) | 20.1 % (12.3 / 12.6 ms) | 56.0 % (16.2 / 16.7 ms) |
| band -30 + clicks 100/s, +20 dB | 96.6 % (16.0 / 16.5 ms) | 97.2 % (11.9 / 12.4 ms) | 93.0 % (12.0 / 12.4 ms) | 100.0 % (15.8 / 16.3 ms) |
| band -30, 1 ms dropout per command | 28.0 % (16.2 / 16.7 ms) | 31.5 % (12.1 / 12.9 ms) | 18.8 % (12.2 / 12.6 ms) | 100.0 % (15.9 / 16.3 ms) |
| band -30, 3 ms dropout per command | 0.0 % | 1.1 % (12.2 / 12.6 ms) | 0.0 % | 79.3 % (15.9 / 16.5 ms) |
| band -30, clock drift +500 ppm | 100.0 % (15.9 / 16.3 ms) | 100.0 % (11.8 / 12.3 ms) | 100.0 % (12.0 / 12.3 ms) | 100.0 % (15.8 / 16.3 ms) |
| band -30, converter low-pass 21 kHz | 100.0 % (16.5 / 17.0 ms) | 100.0 % (12.4 / 13.0 ms) | 100.0 % (12.5 / 12.9 ms) | 100.0 % (16.1 / 16.6 ms) |
| band -30, level -40 dB, SNR 20 dB | 100.0 % (15.9 / 16.3 ms) | 100.0 % (11.8 / 12.3 ms) | 100.0 % (12.0 / 12.3 ms) | 100.0 % (15.8 / 16.2 ms) |
| bright guitar, control -30 dB | 84.6 % (15.9 / 16.3 ms) | 88.0 % (11.8 / 12.3 ms) | 88.3 % (12.0 / 12.3 ms) | 88.6 % (15.8 / 16.3 ms) |
| bright synth, control -10 dB | 100.0 % (15.9 / 16.4 ms) | 100.0 % (11.9 / 12.4 ms) | 100.0 % (12.0 / 12.4 ms) | 100.0 % (15.8 / 16.3 ms) |
| bright synth, control -20 dB | 24.9 % (16.2 / 16.6 ms) | 28.9 % (12.2 / 12.7 ms) | 25.7 % (12.2 / 12.6 ms) | 47.7 % (16.1 / 16.6 ms) |
| bright synth, control -30 dB | 2.0 % (16.1 / 16.5 ms) | 1.4 % (12.0 / 12.3 ms) | 2.8 % (12.1 / 12.4 ms) | 2.6 % (16.0 / 16.5 ms) |
| fuzz guitar (aliasing), control -30 dB | 52.3 % (15.9 / 16.4 ms) | 51.0 % (11.9 / 12.4 ms) | 48.3 % (12.0 / 12.5 ms) | 58.3 % (15.8 / 16.4 ms) |
| guitar -30, fuzz before the decoder | 71.4 % (16.1 / 16.6 ms) | 71.4 % (12.0 / 12.5 ms) | 71.5 % (12.1 / 12.5 ms) | 76.9 % (15.9 / 16.4 ms) |

- **Music does not get in the way.** With the band mix, every command arrives with the commands down to
  50 dB below the music, and 99–100 % at 60 dB below. Latency is the signal length: 16 ms (standard)
  or 12 ms (shorter variants), p95 within 0.5 ms of it.
- **What hurts is energy above 19.5 kHz:** a bright synth on the same signal needs the commands at
  −10 dB; a digital fuzz before the point where commands are added loses ~40–50 % of them, one
  between encoder and receiver ~25–30 %; a bright (piezo-like) pick loses ~11–15 %. The receiver
  should come first in the chain.
- **Dropouts:** the default needs every symbol, so a 1 ms dropout inside a command loses ~70 % of them.
  With one miss allowed, 1 ms dropouts cost nothing, 3 ms dropouts 21 %, and noise and clicks get much
  better too. But see §4.2.
- **Noise floor:** 100 % down to 3 dB SNR over the full band, i.e. noise as loud as the commands
  themselves; at 0 dB 94 % (100 % with one miss).

### 4.2 False triggers

10 minutes of each signal, nothing sent. "Windows through the gate": share of 1 ms steps whose window
shows some tone. "Model": an estimate from the per-tone gate rates assuming independent windows (a tone
never seen counts as seen once), to extrapolate beyond 10 minutes. It is meaningless for structured
signals (the sweep, MFSK traffic), whose windows are not independent; there only the measured count
means anything.

| signal | variant | windows through the gate | commands | model |
|---|---|---|---|---|
| band, loud (-12 dBFS RMS) | standard (4 x 4 ms) | 0.030 % | 0 (0) | 1 per 3.8e8 years |
| band, loud (-12 dBFS RMS) | three_symbols (3 x 4 ms) | 0.030 % | 0 (0) | 1 per 9.2e2 years |
| band, loud (-12 dBFS RMS) | short_symbols (4 x 3 ms) | 0.154 % | 0 (0) | 1 per 4.6e5 years |
| band, loud (-12 dBFS RMS) | standard, 1 miss allowed | 0.030 % | 0 (0) | 1 per 3.1e2 years |
| bright guitar | standard (4 x 4 ms) | 0.014 % | 0 (0) | 1 per 9.5e8 years |
| bright guitar | three_symbols (3 x 4 ms) | 0.014 % | 0 (0) | 1 per 3.4e3 years |
| bright guitar | short_symbols (4 x 3 ms) | 0.085 % | 0 (0) | 1 per 2.1e5 years |
| bright guitar | standard, 1 miss allowed | 0.014 % | 0 (0) | 1 per 9.8e2 years |
| fuzz guitar (aliasing) | standard (4 x 4 ms) | 0.000 % | 0 (0) | 1 per 1.0e12 years |
| fuzz guitar (aliasing) | three_symbols (3 x 4 ms) | 0.000 % | 0 (0) | 1 per 1.7e6 years |
| fuzz guitar (aliasing) | short_symbols (4 x 3 ms) | 0.009 % | 0 (0) | 1 per 7.6e7 years |
| fuzz guitar (aliasing) | standard, 1 miss allowed | 0.000 % | 0 (0) | 1 per 4.3e5 years |
| bright synth (aliasing saw, vibrato) | standard (4 x 4 ms) | 0.000 % | 0 (0) | 1 per 1.0e12 years |
| bright synth (aliasing saw, vibrato) | three_symbols (3 x 4 ms) | 0.000 % | 0 (0) | 1 per 1.7e6 years |
| bright synth (aliasing saw, vibrato) | short_symbols (4 x 3 ms) | 0.000 % | 0 (0) | 1 per 1.6e12 years |
| bright synth (aliasing saw, vibrato) | standard, 1 miss allowed | 0.000 % | 0 (0) | 1 per 4.3e5 years |
| band + clicks 200/s at 0 dBFS | standard (4 x 4 ms) | 0.012 % | 0 (0) | 1 per 2.1e9 years |
| band + clicks 200/s at 0 dBFS | three_symbols (3 x 4 ms) | 0.012 % | 0 (0) | 1 per 4.0e3 years |
| band + clicks 200/s at 0 dBFS | short_symbols (4 x 3 ms) | 0.069 % | 0 (0) | 1 per 5.6e6 years |
| band + clicks 200/s at 0 dBFS | standard, 1 miss allowed | 0.012 % | 0 (0) | 1 per 1.6e3 years |
| white noise -30 dBFS | standard (4 x 4 ms) | 0.002 % | 0 (0) | 1 per 6.6e10 years |
| white noise -30 dBFS | three_symbols (3 x 4 ms) | 0.002 % | 0 (0) | 1 per 4.3e5 years |
| white noise -30 dBFS | short_symbols (4 x 3 ms) | 0.042 % | 0 (0) | 1 per 2.0e5 years |
| white noise -30 dBFS | standard, 1 miss allowed | 0.002 % | 0 (0) | 1 per 5.3e4 years |
| whistle sweeping 19-22.5 kHz | standard (4 x 4 ms) | 41.750 % | 0 (0) | 183.5 / h |
| whistle sweeping 19-22.5 kHz | three_symbols (3 x 4 ms) | 41.750 % | 0 (0) | 3074.9 / h |
| whistle sweeping 19-22.5 kHz | short_symbols (4 x 3 ms) | 39.950 % | 0 (0) | 792.2 / h |
| whistle sweeping 19-22.5 kHz | standard, 1 miss allowed | 41.750 % | 0 (0) | 7339.4 / h |
| AML Robust traffic (2.5-4 kHz) | standard (4 x 4 ms) | 0.003 % | 0 (0) | 1 per 4.0e10 years |
| AML Robust traffic (2.5-4 kHz) | three_symbols (3 x 4 ms) | 0.003 % | 0 (0) | 1 per 1.5e5 years |
| AML Robust traffic (2.5-4 kHz) | short_symbols (4 x 3 ms) | 0.042 % | 0 (0) | 1 per 2.0e5 years |
| AML Robust traffic (2.5-4 kHz) | standard, 1 miss allowed | 0.003 % | 0 (0) | 1 per 3.2e4 years |
| AML Robust moved to 20-21.5 kHz | standard (4 x 4 ms) | 61.082 % | 0 (0) | 881.7 / h |
| AML Robust moved to 20-21.5 kHz | three_symbols (3 x 4 ms) | 61.082 % | 0 (0) | 7581.6 / h |
| AML Robust moved to 20-21.5 kHz | short_symbols (4 x 3 ms) | 64.152 % | 0 (0) | 3784.6 / h |
| AML Robust moved to 20-21.5 kHz | standard, 1 miss allowed | 61.082 % | 908 (5448.0 / h) | 15990.5 / h |

- **No false command was measured from music, clicks, noise or AML traffic** in any variant with the
  default rule.
- The default (4 symbols, all must match) has the largest margin by far: ≥ ~10⁸ years by the model on
  every music and noise signal. **Three symbols** cut it to ~10³ years on loud music; **3 ms symbols**
  (wider bins, more noise per bin) to ~10⁵.
- **One miss allowed** still clears a 10-year target against music and noise by the model (≥ ~300
  years on loud music, against ≥ 10⁸ for the default), but it produced **908 false commands in 10
  minutes** of AML Robust traffic moved into the same band, which the default rule rejects completely.
  Use it only on a line that carries nothing else above 19.5 kHz.
- The whistle sweeping 19–22.5 kHz shows a tone in 42 % of windows but never matches: no codeword is
  monotonic.

**Purity gate.** With the gate at 0.35 instead of 0.5 (`PEDAL_MIN_PURITY=0.35`, a 15 s × 1 seed /
2 min run), delivery improves where the band is noisy: SNR −3 dB 18 → 80 %, bright synth at −20 dB
25 → 77 %, fuzz before the receiver 71 → 77 %. But noise passes the gate ~100× more often, and the model
drops to ~400 years on white noise and ~240 years on AML traffic for the default rule (and to ~1 per
200–400 hours with one miss allowed). 0.5 stays the default.

### 4.3 Leakage into the audible band

| variant | energy below 19 kHz | below 18 kHz | below 16 kHz | below 12 kHz |
|---|---|---|---|---|
| standard (4 x 4 ms) | -43.9 dB | -59.6 dB | -74.0 dB | -88.1 dB |
| three_symbols (3 x 4 ms) | -43.9 dB | -59.5 dB | -73.9 dB | -88.1 dB |
| short_symbols (4 x 3 ms) | -33.6 dB | -52.6 dB | -67.7 dB | -82.1 dB |
| standard, 1 miss allowed | -43.9 dB | -59.6 dB | -74.0 dB | -88.1 dB |

- Of the standard signal, −44 dB of the energy lies below 19 kHz and −74 dB below 16 kHz: with commands
  30 dB under the music, what reaches below 16 kHz is ~100 dB under the music.
- **Strip filter.** A receiving pedal should remove the band from the audio it passes on: a
  Butterworth low-pass at 18 kHz takes the commands down by 35 dB (8th order) or 56 dB (16th order).
- **Nonlinear stages after the receiver.** A digital fuzz without oversampling folds the commands'
  harmonics back into the audible band (2 × 20.5 kHz = 41 kHz aliases to 7 kHz). Measured artifact
  below 16 kHz, relative to the fuzzed music:

| control level | no strip filter | strip filter (16th order, 18 kHz) before the fuzz |
|---|---|---|
| -20 dB | -38.2 dB | -89.1 dB |
| -30 dB | -48.8 dB | -99.1 dB |
| -40 dB | -58.9 dB | -109.1 dB |

So: strip the band in the receiver, before anything nonlinear.

## 5. Findings

1. **It works, and fast.** 16 ms from the start of a command to its recognition, constant (p95 within
   0.5 ms), with the commands 30–50 dB below a guitar/bass/keys mix and no false commands measured.
   Compared with AML Robust (~265 ms mean), this is footswitch territory.
2. **Default: 4 × 4 ms, every symbol must match.** The shorter variants save 4 ms but give up most of
   the false-trigger margin (three symbols) or some of it and 10 dB of leakage (3 ms symbols).
3. **Where it goes in the chain matters more than the level.** Receiver first, strip filter on its
   output. Anything that creates energy above 19.5 kHz before the receiver (digital fuzz/amp modelers
   without oversampling, bright aliasing synths, piezo pick transients) costs commands; raising the
   commands to −10/−20 dB helps against those.
4. **The chain must be 48 kHz and pass 21.5 kHz.** Many converters at 48 kHz pass up to ~21.7 kHz;
   44.1 kHz, MP3/AAC, many wireless systems and analog pedals with treble roll-off do not.
5. **Relative commands drift.** A missed or doubled command leaves the pedal off by one. A resync of the
   absolute preset is needed, but **not as AML Robust in the same band**: its tones overlap the command
   tones (61 % of windows show a tone) and, with one miss allowed, it produced false commands. A resync
   message should be built from the same signature phy (e.g. a fifth "sync" codeword followed by the
   preset number in tone digits, with a check) so the receiver can tell the two apart.

## 6. Open questions / next steps

- **Real hardware.** `cricket pedal-wav --mix-into <recording>` writes commands into a 48 kHz recording;
  play it through the rig (pedals, interface, wireless), record the far end and run
  `cricket pedal-scan --in <recording>`. Of interest: converters' real response at 20–21.5 kHz, analog
  pedals (buffers, treble roll-off), wireless systems, long cables with a passive guitar.
- A microcontroller build of the receiver with an audio codec at 48 kHz, and its real CPU load.
- The absolute resync message (§5, item 5) and how often to send it.
- A real-time path in the engine (audio in → `PedalRx` → MIDI out) and the encoder side (MIDI or
  footswitch in → commands mixed into the instrument signal).
- Whether to tolerate one miss on lines that carry nothing else in the band (wireless, Dante), where
  dropouts are the main loss.
