# Cricket

> **Cricket MIDI: MIDI over a cable that was only ever meant to carry sound.**

You have a Dante network, a MADI snake, or a long analog XLR run between front of house and the stage.
You need MIDI or show-control cues over there. The audio path reaches it; the MIDI cable does not.

Cricket turns MIDI into a short burst of tones, sends it down any spare audio channel, and turns the tones back
into MIDI on the far end. It sounds a bit like a dial-up modem having a very polite conversation.
Like its namesake, it keeps chirping steadily even when there's nothing to say; if the far end suddenly hears
nothing but, well, crickets, it sends All Notes Off so no note is left droning.

Cricket speaks **AML v1** (AudioMIDI Link), an open protocol for MIDI over audio, specified in
[`docs/aml-v1-spec.md`](docs/aml-v1-spec.md). Anything else that speaks AML v1 can talk to it.

> **Status: measured in software only.** Every number in this README comes from `cricket loopback`
> (MIDI in → MIDI out through the engine, in-process) or from the channel simulator, **not from a real
> show chain**. Use Cricket as a backup line before you trust it with the main cue, and send a
> [field report](.github/ISSUE_TEMPLATE/field-report.md) either way. The next release is **v0.2.0**; see
> [`ROADMAP.md`](ROADMAP.md).

## Quick start

1. **Get a binary.** Download `cricket` (CLI) or the Cricket desktop app for your OS from the
   [Releases page](https://github.com/paksnukk/Cricket/releases) once a release exists, or build from
   source: a Rust toolchain (1.87+), on Linux also `libasound2-dev`, then `cargo install --path cricket_cli`
   (or `cargo build --release` → `target/release/cricket`).
2. **Patch a dedicated mono audio channel** from the sending machine to the receiving one. No EQ,
   compressor, limiter or noise gate on that channel; set its gain once and leave it.
3. **See your devices:** `cricket list-devices`.
4. **Sending side** (MIDI in → tones out): `cricket encode --midi-in-name "My Keyboard" --audio-out-name "Dante TX 1-2"`
5. **Receiving side** (tones in → MIDI out): `cricket decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1"`
6. **Same profile on both ends** (`analog-safe` is the default; `cricket profiles` lists them, see [Profiles](#profiles)).
7. **No hardware?** `cricket encode --simulate` plays a C-major test scale (without it, and without MIDI in, the encoder sends heartbeats only); `cricket loopback` measures latency in-process.
8. **Something wrong?** See [Troubleshooting](#troubleshooting).

The desktop app does the same with the encoder and decoder side by side: `cd cricket_gui && npm install && npm run tauri dev`
(needs Node.js and the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/)).

```
 ┌───────────┐  MIDI   ┌─────────┐  ♪ audio ♪  ┌──────────────────┐  ♪ audio ♪  ┌─────────┐  MIDI   ┌──────────┐
 │ keyboard, │ ──────▶ │ Cricket │ ──────────▶ │ Dante / MADI /   │ ──────────▶ │ Cricket │ ──────▶ │ synth,   │
 │ show ctrl │         │ encoder │             │ analog XLR / ... │             │ decoder │         │ lights … │
 └───────────┘         └─────────┘             └──────────────────┘             └─────────┘         └──────────┘
```

## How it works

Each MIDI event becomes a small **packet**: a preamble, a header, a sequence number, the payload and a CRC16.
The packet is sent as **16-tone FSK**: one pure tone at a time, and each tone carries 4 bits. By default the
packet is protected by a Reed–Solomon code, so a few wrong or missing tones are repaired instead of
losing the message (the LTC profiles send the same packets as a timecode-like square wave instead).

- **Level doesn't matter.** Tones are detected by their purity (Goertzel power relative to the window's
  energy), not by their loudness. Anything from −30 dB to 0 dB relative to the transmit level decodes,
  so someone riding the fader doesn't break the link.
- **Packets find their own start.** A preamble marks every packet. After a false start or a missed
  symbol, the decoder rewinds and locks on again without throwing away audio it has already received.
- **Copies are counted once.** With Robust repeat, every packet is sent twice; the second copy is dropped
  by sequence number within a short time window.
- **Nothing is left hanging.** A heartbeat keeps the link alive. If no valid packet arrives for the
  profile's link-dead time (2 s; 1 s on `clean-line-fast` and `digital-line`, 0.5 s on
  `digital-bit-exact`), or the
  audio input stops delivering samples, the decoder sends a **fail-safe burst** once: a Note Off for
  every note it turned on, then Sustain off + All Notes Off on all 16 channels (and re-centers any
  pitch bend it left bent). Each released note's NoteOff is repeated a few times in idle airtime,
  spread over seconds, so a NoteOff lost to noise doesn't leave a note droning forever; the
  transmitter also re-sends held pedals and bends in idle slots, so a receiver that restarted
  recovers them. Cue-only links can switch the burst off (`--no-failsafe-burst`).
- **Important messages first.** The transmitter sends Panic and All Notes Off / All Sound Off, then
  MSC, then NoteOffs, then everything else, then NoteOff repeats, then heartbeats. When fast playing
  would overflow the airtime, it refuses the newest NoteOns beyond the profile's chord limit (10 on
  Analog Fast, 5 on Analog Safe, 11 and more on the LTC profiles) and raises an alarm, never a
  NoteOff, MSC or Panic.
- **Links don't hear each other.** Two Cricket links in one multicore or Dante domain leak into each
  other, and the receiver would decode the neighbour's packets. Give each link its own **link key**
  (`--link-key 0x1234` or a passphrase, the same on both ends) and the other link's packets are
  rejected; the status counts them as "other key?" so a mismatch shows up at once.
- **Faders don't clog the line.** Controllers, pitch bend and aftertouch are *latest value wins*: while
  a value is still waiting to be sent, a newer one replaces it. A fader sending 200 values a second
  costs one queued message, its final position arrives within about two message times, and a note
  waits at most for the controller value of its own channel that was set before it. Bank Select,
  RPN/NRPN and Program Change are never merged or reordered, and a NoteOff never overtakes a sustain
  pedal it was played after.

## What it carries

- **All MIDI 1.0 channel messages on all 16 channels:** Note On / Off (with release velocity),
  Polyphonic and Channel Aftertouch, Control Change (including the channel mode messages 120–127),
  Program Change and Pitch Bend; plus Song Position, Song Select and Tune Request.
- **MIDI Show Control (MSC):** GO, STOP, RESUME, GO_OFF and GO/JAM_CLOCK with an optional cue number
  and cue list, LOAD with a cue number and optional list, FIRE (macro 0–127), ALL_OFF, RESTORE, RESET, STANDBY ±, SEQUENCE ±,
  START/STOP/ZERO_CLOCK, MTC_CHASE_ON/OFF, OPEN/CLOSE_CUE_LIST.
  - Cue numbers **0–999.9 with at most one decimal**, cue lists **1–15**. Numbers are carried as
    numbers: `05`, `5.0` and `5` all arrive as `5` (worth knowing if the receiving end is QLab, which
    matches cue numbers as text).
  - The **device ID and command format are set on the decoder** (default: device `0x7F`, all-call;
    format `0x01`, Lighting (General)); the encoder accepts MSC for any device ID and format unless
    told to carry only one device's cues (`encode --msc-accept-device 3`, for rigs with several MSC
    devices on the MIDI line; all-call always passes).
  - An MSC message that does not fit (cue `12.345`, cue `1000`, list `16`, a cue path, TIMED_GO, SET,
    SET_CLOCK, OPEN/CLOSE_CUE_PATH) is **not sent**, and the encoder raises an alarm such as
    "MSC cue 12.345 not representable". It never sends a different cue.
- **Panic** (the GUI button): All Notes Off everywhere.

**Not carried, on purpose:** other System Exclusive, MIDI Clock and the other real-time messages
(Start/Stop/Continue, Active Sensing), and MIDI Time Code. A packet takes 1–273 ms of airtime
depending on the profile; clock and timecode need a steady stream with sub-millisecond timing, so they
would arrive useless and would delay every note and cue behind them. Run timecode as LTC on its own
audio channel instead. Large SysEx dumps would block the line for seconds.

## Profiles

Both ends must use the same profile; it is not signalled on the wire, and a decoder on the wrong
profile hears nothing (it never guesses, and a misspelt name is an error, not another profile). All
profiles carry the same messages and are specified in [`docs/aml-v1-spec.md`](docs/aml-v1-spec.md) §6.
Profiles are named after the line they are for; the older wire-format names in the second column
still work everywhere (`--profile robust` is `analog-safe`), and `cricket profiles` prints this table.

### Which profile?

1. **Analog line, or any bus you don't control** (EQ, compression, unknown gain)? → `analog-safe`.
   Dropouts of 50 ms or more → `analog-safe-repeat`.
2. **A dedicated line with nothing on it** (no EQ, dynamics or noise reduction)? Analog → `clean-line`
   (notes only, without error correction: `analog-fast`). Digital → `clean-line-fast`, or
   `digital-line` on Dante / MADI / AES at 48 or 96 kHz.
3. **`cricket pathcheck` says the path is bit-exact?** → `digital-bit-exact` (under 1 ms; experimental).

| Profile | Older name | Use when | Processing on the bus | Airtime per message | Measured NoteOn latency\* |
|---|---|---|---|---|---|
| `analog-safe` (default) | `robust` (`robust_fec`) | any bus you don't control: cues and notes | EQ, compression or a limiter are tolerated | 175.5 ms | mean ~0.21 s, p95 ~0.33 s |
| `analog-safe-repeat` | `robust_repeat` | dropouts of 50 ms or more (every message is sent twice) | as `analog-safe` | 273 ms | mean ~0.35 s, p95 ~0.5 s |
| `analog-fast` | `fast` | notes over a clean, dedicated analog line that passes 7.5 kHz | none; no error correction | 57 ms | mean ~80 ms, p95 ~115 ms |
| `clean-line` | `ltc_fec` (`ltc4800_fec`) | a dedicated line, analog or digital | none; repairs 1–3 ms dropouts and clicks | 21.8 ms | mean ~21 ms, p95 ~24 ms |
| `clean-line-fast` | `ltc9600_fec` | a dedicated digital line (≥ 6 kHz of bandwidth) | none | 10.9 ms | mean ~11 ms |
| `digital-line` | `ltc14400_fec` | Dante / MADI / AES at 48 or 96 kHz (≥ 16 kHz of bandwidth) | none | 7.0 ms | mean ~7 ms, p95 ~7.5 ms |
| `digital-bit-exact` | `digital` | `cricket pathcheck` passed: samples arrive unchanged | none at all | 0.9 ms | mean ~0.6 ms |

**Advanced, no error correction:** the uncoded `ltc` (= `ltc4800`), `ltc2400`, `ltc9600` and
`ltc14400`, plus the coded `ltc2400_fec` for narrow analog lines, keep their wire-format names; the
GUI lists them under "Advanced: no error correction". The same group holds the **experimental**
`analog-safe-fast` (`robust_fast`): Analog Safe's error correction on 4 ms symbols, 108 ms per
message instead of 175.5 ms, for lines that pass 6.25 kHz; in the simulator it is within 1 dB of
Analog Safe, and `cricket loopback` measures mean ~170 ms, p95 ~220 ms NoteOn latency (Analog Safe
~270 / ~350 ms); not in the spec. A third group, **instrument audio (experimental)**, holds
`instrument` and `instrument-fast` for MIDI on an instrument's own cable (below). Every profile
except `digital-bit-exact` and the instrument profiles transmits at **−12 dBFS peak** by default
(`--tx-level-db`, GUI "TX level"), which leaves headroom on the bus; the instrument profiles default
to −30 dBFS; the receiver is level-independent and decodes anything from 30 dB below that to full
scale.

The three MFSK profiles in detail:

| | **Analog Safe** `analog-safe` (default) | **Analog Safe Repeat** `analog-safe-repeat` | **Analog Fast** `analog-fast` |
|---|---|---|---|
| Use it for | Cues and notes over a chain you don't fully trust | Lines with glitches or dropouts of ~50 ms or more | Playing notes over a clean, dedicated line (no EQ, compression or limiter) |
| Symbol | 6 ms + 0.5 ms guard | 6 ms + 0.5 ms guard | 3 ms |
| Tones | 16 orthogonal tones, 166⅔ Hz apart from 2.5 kHz (2.5–5.0 kHz) | same | 16 orthogonal tones, 333⅓ Hz apart from 2.5 kHz (2.5–7.5 kHz) |
| The line must pass | 2.5–5.0 kHz | 2.5–5.0 kHz | 2.5–7.5 kHz |
| Body | Reed–Solomon coded (repairs wrong or missing tones) | uncoded | uncoded |
| Copies per message | 1 | 2 | 1 |
| Airtime per message | 175.5 ms | 273 ms | 57 ms |
| Comfortable rate | ~5 messages/s | ~3–4 messages/s | ~10–17 messages/s |
| Measured NoteOn latency\* | mean ~210 ms, p95 ~330 ms (legato: ~200 ms, p95 ~295 ms) | mean ~345–360 ms, p95 ~475–500 ms | mean ~75–80 ms, p95 ~115 ms |

Analog Safe finds a packet even when two of its four preamble tones are lost, and repairs up to 8
missing or 4 wrong tones in the body. In the channel simulator it delivers 95 % of messages down to
−10 dB of white-noise SNR (Analog Safe Repeat −9.5 dB, Analog Fast −5 dB), through 100 Hz hum 6 dB
louder than the tones, pink noise 6 dB above the tones, a noise gate, a compressor or limiter on the
bus, and a ±1 % sample-rate mismatch between the ends; it delivers as often as Analog Safe Repeat or
more in every condition except dropouts of 50 ms and longer, where the second copy wins (60 ms
dropouts: 93 % vs 75 % delivered). The receiver listens to the tone band only (a 2nd-order
band-pass), so hum, rumble and speech or music bleeding onto the channel below 2 kHz do not count
against the tones. Analog Safe Repeat was the default before v1.1 (as `robust`); a profile name
`robust_fec` from earlier builds now means Analog Safe.

### Clean-line and digital-line profiles (LTC-style, dedicated lines)

These profiles send the same packets as a biphase-mark code, like SMPTE timecode, in a fraction of
the airtime. Their wire format is in the spec (§6.5) with status **provisional**: complete and tested
in software, but not yet proven on real show chains, so a later revision may change it.

| Profile | What it is | Measured NoteOn latency\* | Needs |
|---|---|---|---|
| `clean-line` (`ltc_fec`) | 4800 bit/s with a Reed–Solomon coded body (also `ltc2400_fec`, and `clean-line-fast` = `ltc9600_fec`); survives 1–3 ms dropouts and click bursts | mean ~21 ms, p95 ~22–24 ms (`clean-line-fast`: ~11 ms) | A dedicated line: ≥ 4 dB SNR, no EQ, compression or limiter |
| `ltc` | 4800 bit/s, uncoded (also `ltc2400`, `ltc4800`, `ltc9600`) | mean ~16 ms (`ltc9600`: ~8 ms) | As `clean-line`, and no dropouts or clicks |
| `digital-line` (`ltc14400_fec`) | **Experimental, fastest LTC:** 14400 bit/s with the coded body (also uncoded `ltc14400`); 7.0 ms (5.4 ms) per message, a 4-note chord in 29 ms (22 ms); repairs dropouts up to ~0.5 ms | mean ~7 ms, p95 ~7.5 ms (`ltc14400`: ~5.5 ms) | A digital line (Dante, MADI, AES) at 48 or 96 kHz: ≥ 16 kHz bandwidth, ≥ 15 dB SNR |

An EQ boost inside the signal band breaks them (+12 dB at 3 kHz stops 4800 bit/s; 2400 bit/s survives
it but not a 1 kHz high-pass), and 9600 bit/s needs more than ~6 kHz of bandwidth (digital lines only).
`ltc14400` and `ltc14400_fec` are not in the spec yet (a candidate for §6.5): in simulation they pass
converter anti-alias filters (also two minimum-phase ones in series), sample-rate converters and ±1 %
clock error, at 48 and 96 kHz, but fail below ~16 kHz of bandwidth, e.g. through analog stages with a
low-pass at 12 kHz.
The measurements behind all profiles are in [docs/proposals/fec.md](docs/proposals/fec.md) and
[docs/proposals/ltc-bmc.md](docs/proposals/ltc-bmc.md).

### Digital bit-exact (bit-transparent) — experimental

For paths that deliver samples **unchanged** (Dante / AES67 / MADI / AES3 routed without processing, a
USB interface's digital I/O in a bit-exact mode), `--profile digital-bit-exact` (older name
`digital`) writes the packet straight into
the sample values, one nibble per sample, like SMPTE ST 337 carries data in AES3: a message is 21
samples, sent twice (0.9 ms on air), at −57.4 dBFS peak.

| | `digital-bit-exact` (`digital`) |
|---|---|
| Measured NoteOn latency\* | mean ~0.5–0.7 ms, p95 ~0.6–0.8 ms; 4-note chord complete after ~3.3 ms |
| Throughput | ~1,150 messages/s |
| Survives | 16-bit truncation, ±1 LSB of dither, 32767 / 2²³−1 scaling, polarity inversion, gain within ±0.1 dB, noise ≤ −100 dBFS |
| Fails (safely: no MIDI, never wrong MIDI) | any other gain, EQ, dynamics, sample-rate conversion, unlocked clocks, mixing, analog |

**Check the path first** with `cricket pathcheck` (below): it plays a test pattern, records the far end
and tells you whether the path is bit-exact and, if not, what it does (gain, word length, dither,
SRC / drift, polarity, offset, noise). On Windows, Cricket currently uses WASAPI shared mode, which goes
through the Windows mixer: run the path check, and prefer macOS / Linux (ALSA `hw:` device) until
exclusive mode / ASIO are supported (the CLI and GUI warn when `digital-bit-exact` or `digital-line`
is chosen on Windows). A decoder profile with `+digital` (e.g. `clean-line-fast+digital`) also
accepts digital frames. Design, false-accept analysis (worst case ~10¹⁰ years per false message), the
fail-safe table and path requirements: [docs/proposals/digital-transparent.md](docs/proposals/digital-transparent.md).
Not part of the spec yet.

### Instrument-audio (HF) profiles — experimental

For MIDI **on an instrument's own cable**: a guitar, bass or keyboard carries the messages above its
music, at 19.0–21.5 kHz on a 48 kHz chain, and a box on the pedalboard passes the audio through
(analog, 0 ms) and taps it to extract the MIDI. `--profile instrument` (older name `hf_robust`) is
Analog Safe's timing, error correction and acquisition moved up; `instrument-fast` (`hf_fast`) is the
same code on 3 ms symbols with 8 tones (3 bits each), 105 ms per message. Both default to
**−30 dBFS** (inaudible to adults above the music; the GUI follows). Spec §6.6, status provisional;
measurements, the level / intermodulation budget, the box sketch and the acceptance criteria are in
[docs/proposals/hf-instrument.md](docs/proposals/hf-instrument.md).

| | `instrument` (`hf_robust`) | `instrument-fast` (`hf_fast`) |
|---|---|---|
| Tones | 16 × 166⅔ Hz, 19.0–21.5 kHz, 6 ms + 0.5 ms guard | 8 × 333⅓ Hz, 19.0–21.33 kHz, 3 ms |
| Airtime per message | 175.5 ms | 105 ms |
| Measured NoteOn latency\* | mean ~213 ms, p95 ~330 ms | mean ~121 ms, p95 ~196 ms |
| In the simulator (−18 dBFS program) | 100 % over guitar, bass and keys from −20 to −50 dBFS and over a bright synth to −40 dBFS, with a −85 dBFS hiss floor, PSU spurs in a tone slot, compressors, gates and 48 kHz converters | the same at every level; dropouts cost more (30 ms: 84 % vs 96 %) |
| Needs | 48 kHz end to end, copper or 48 kHz PCM; nothing nonlinear between embedder and box | same |
| Does not survive | a 44.1 kHz device or SRC, a codec or wireless link, a 1 % sample-rate mismatch, a drive or fuzz before the box | same |

Try it on a real recording before any hardware exists: `cricket hf-mix --wav-in guitar.wav --wav-out
mixed.wav --messages "pc=1:10,note=1:60:100,go=14.5:2"` embeds the messages, and after the mixed file
has been through the rig and recorded at 48 kHz, `cricket hf-scan --wav-in recording.wav` lists what
arrived, with the receiver's counters and the recording's level in the tone band. The tone-signature
"pedal" prototype that preceded this (a branch, never merged) was not adopted: bounded vocabulary, no
CRC, relative-only state (see the proposal §9 and `docs/review-2026-11.md` §F).

\* MIDI in → MIDI out through the engine, in-process (`cricket loopback`, 20–60 notes), while notes are
being played *and released*. Real audio interfaces add their own buffering on top. Run the measurement
yourself rather than trusting this table.

## The CLI in more detail

```sh
# What audio and MIDI devices are there? Which profile?
cricket list-devices
cricket profiles

# Sending side: MIDI in → tones out (by index or by exact name), on channel 2 of the interface
cricket encode --midi-in-name "My Keyboard" --audio-out-name "Dante TX 1-2" --audio-out-channel 2

# Receiving side: tones in → MIDI out. Ctrl-C sends All Notes Off before it stops;
# `panic` + Enter sends it right away.
cricket decode --audio-in-name "Dante RX 1-2" --audio-in-channel 2 --midi-out-name "IAC Bus 1"

# ... sending MSC to a console that listens on device ID 1 (default: 0x7F all-call, format 0x01)
cricket decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1" \
    --msc-device-id 1 --msc-format 0x01

# Two Cricket links in one multicore: give each its own link key (the same on both ends)
cricket --link-key "stage left" encode --midi-in-name "My Keyboard"
cricket --link-key "stage left" decode --midi-out-name "Synth"

# A dedicated clean line? Both ends:
cricket --profile clean-line encode ...
cricket --profile clean-line decode ...

# No hardware at hand? --simulate plays a C-major test scale (without it, and without MIDI in,
# the encoder sends heartbeats only: a decoder on stage never gets notes nobody played)
cricket encode --simulate

# Quieter on the bus than the default −12 dBFS peak (the receiver doesn't mind)
cricket --tx-level-db -20 encode --midi-in-name "My Keyboard"

# Machine-readable: one JSON object per line (level, stats, decoded messages, alarms)
cricket --json decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1"

# How fast is it, really? (no hardware needed; default: analog-fast,analog-safe)
cricket loopback
cricket loopback --profiles analog-safe,analog-safe-repeat,analog-fast,ltc,clean-line --notes 40
cricket loopback --profiles digital-bit-exact,ltc9600 --chords 20 --throughput 100

# Is this path bit-transparent (for --profile digital-bit-exact)? Sender, then receiver:
cricket pathcheck send --audio-out-name "Dante TX 1-2"
cricket pathcheck receive --audio-in-name "Dante RX 1-2" --seconds 5
cricket pathcheck sim                     # simulated paths, no hardware

# Lower device latency (default: the host's buffer; ALSA uses 100 ms)
cricket --profile digital-bit-exact --buffer-frames 128 decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1"
```

From a source checkout, replace `cricket` with `cargo run -p cricket_cli --` (add `--release` for
`loopback`). While a decoder or encoder runs it prints a status line every second (LINK OK / NO SIGNAL /
SIGNAL, NO LOCK / LINK DEAD, input level in dBFS, confidence, CRC pass/fail, FEC corrections, last
message). Exit status: 0 ok, 1 error, 2 usage error, 3 path check "Marginal".

Devices are remembered **by name**, so unplugging and replugging an interface doesn't silently pick another
one. A name that isn't present is an error. Both ends warn when the chosen audio device looks like a
built-in microphone or speaker.

### The GUI

```sh
cd cricket_gui
npm install
npm run tauri dev
```

The GUI has the encoder and decoder side by side. A traffic light says **LINK OK / NO SIGNAL /
SIGNAL, NO LOCK / LINK DEAD** with the time since the last heartbeat and a hint ("signal present, no
lock: profile mismatch or EQ on the bus?"); the input meter reads in dBFS with the good window
(−30 … 0) marked; link stats show CRC pass/fail and FEC corrections per minute, preambles found vs
CRC fails, the TX queue and its latency. Alarms are a list with timestamps and counts that you
acknowledge, not a banner. Each side has a red **Panic** button (the decoder's sends Sustain off +
All Notes Off on all 16 channels straight to MIDI out, as does **Stop**), the audio channel and TX
level are selectable, and a **Self-Test** measures real loopback latency for Analog Fast and
Analog Safe. The test scale is an explicit checkbox; without MIDI in the encoder sends heartbeats
only.
Build instructions: [`cricket_gui/README.md`](cricket_gui/README.md).


## Troubleshooting

| Symptom | Look at | Fix |
|---|---|---|
| Decoder shows NO SIGNAL (stays in `Search`, CRC pass count 0) | Is audio arriving at all? The dBFS input meter / `input_level` event; `cricket list-devices` and `--audio-in-channel` for the right input and channel; both ends on the **same profile** (a misspelt name is an error, not another profile) | Patch and select the right channel; match `--profile`; raise the send level if the far end is below the −60 dBFS silence floor |
| SIGNAL, NO LOCK; or `Locked` for a moment, then `Search` again; many sync losses | The chain: a gate, compressor, limiter or EQ on the Cricket channel; sample-rate conversion on a digital path; a profile mismatch | Remove processing from that channel (treat it like LTC); try `analog-safe`; on digital lines run `cricket pathcheck` |
| Notes arrive but some hang, or arrive late | TX queue depth and latency in the GUI; the profile's comfortable rate; dense controller data | Use a faster profile on a clean line, play fewer simultaneous messages, let the fail-safe clear notes (link dead → All Notes Off) |
| Everything works until someone touches the fader | Level within −30…0 dB of transmit level? Clipping on the way in? | Keep the default −12 dBFS send level (`--tx-level-db`); level is otherwise irrelevant to detection |
| `clean-line*` / `digital-line` never lock, `analog-*` profiles do | Bandwidth and EQ: the LTC-style profiles need ≥ 6 kHz (`clean-line-fast`) or ≥ 16 kHz (`digital-line`), no EQ boost in band | Use `clean-line` (or `ltc2400_fec`) on analog paths, `clean-line-fast` / `digital-line` on digital lines only |
| `--profile digital-bit-exact` gives nothing | `cricket pathcheck receive` verdict: gain, SRC, dither, mixer? | Route the channel bit-exact (no gain, no SRC, no mixing); on Windows prefer another OS until exclusive mode exists (the CLI and GUI warn) |
| Audio input stalls ("audio-input-stalled" alarm), or MIDI out fails (`MidiOutputError`) | The interface's clock / driver; USB power; an unplugged MIDI interface | Re-plug or re-select the device; the decoder already sent All Notes Off; Stop and Panic also send it |
| "MSC cue … not representable" | Cue number has more than one decimal, a cue path, or a command outside the carried set | Rename the cue (0–999.9, one decimal, list 1–15) or send it another way |
| `cricket --version` or the GUI shows an unexpected version | You may be running an old binary | `cargo install --path cricket_cli --force`, or download the current release |

When you report a problem, the [field report template](.github/ISSUE_TEMPLATE/field-report.md) asks for
the chain, the profile and the decoder's link stats; that is usually enough to tell what happened.

## Routing and cable guide (stub)

Cricket's channel is treated **like an LTC timecode line**:

- **One dedicated mono channel per direction.** Nothing else is mixed onto it (`--audio-out-channel` /
  `--audio-in-channel` pick the channel of a multichannel interface). Return traffic (if any) takes a
  second channel.
- **No processing:** no EQ, dynamics, noise gate, de-esser, limiter or "auto gain" on that channel.
  Insert points and console channel strips are the usual culprits.
- **Set gain once.** Anything from −30 dB to 0 dB relative to the transmit level decodes; the
  transmitter sends at −12 dBFS peak by default (`--tx-level-db`, GUI "TX level"), which leaves
  headroom so nothing upstream limits it.
- **Analog XLR:** balanced line level works over long runs; the `analog-*` profiles live between 2.5
  and 5.5 kHz, so even a narrow-band path carries them. `clean-line-fast` and `digital-line` want a
  digital path.
- **Dante / AES67 / MADI / AES3:** route the channel without sample-rate conversion if you intend to
  use `digital-bit-exact` or `digital-line`; the `analog-*` profiles and `clean-line` don't care about SRC.
- **Virtual cables / DAW loopback:** fine for testing; watch the buffer sizes (`--buffer-frames`).
- **Wireless, codecs, intercom:** compression codecs and wireless links alter the signal; expect only
  `analog-safe` to work, and test first.

A fuller guide with measured chains will replace this stub as field reports come in.

## Hardware box (FAQ)

**Can Cricket run in a small box instead of a computer?** That is the plan. `cricket_core` is
`no_std` + `alloc`, allocates nothing per sample in the LTC and digital phys, and CI builds it for
Cortex-M (`thumbv8m.main-none-eabihf`, `thumbv6m-none-eabi`) and RISC-V (`riscv32imac-unknown-none-elf`).
Candidate parts are the ESP32-S3 and the RP2350 with an I²S codec; the MFSK receivers cost ~15 ns per
sample idle and ~64 ns worst case on a desktop core, the LTC receiver ~60 ns, so a 48 kHz mono stream
leaves headroom even on a microcontroller. What is **not** done: a firmware crate, a board, or any
measurement on real hardware; what is measured is the host-side CPU cost and the simulator results above.
Follow [`ROADMAP.md`](ROADMAP.md) for the product track.

## Project layout

| Crate | What lives there |
|---|---|
| [`cricket_core`](cricket_core) | The modem: tone synthesis and detection, the LTC phy, packets, CRC, Reed–Solomon FEC, interleaving, TX scheduler, fail-safe. Pure Rust, `no_std` + `alloc` for microcontrollers; the default `std` feature adds WAV file I/O, the legacy prototype phy and the `pathcheck` analyzer. The phy boundary (`PhyFactory` / `PhyTx` / `PhyRx`, `Modulation::Custom`) lets another crate add a modulation; see [`docs/architecture.md`](docs/architecture.md). |
| [`cricket_sim`](cricket_sim) | The channel simulator the tests and measurement harnesses run on: impairments, instrument and program sources, converter and link models, bit-transparency path models, chunked decoding. `std` only. |
| [`cricket_engine`](cricket_engine) | The real-time part: audio/MIDI devices, encoder, decoder and pass-through workers, events, loopback latency measurement. |
| [`cricket_cli`](cricket_cli) | Command-line front end. |
| [`cricket_gui`](cricket_gui) | Tauri 2 desktop app (TypeScript + Rust). |

## Testing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The tests don't need hardware. The channel simulator in [`cricket_sim`](cricket_sim) (re-exported by
the tests' `support` module) puts packets through offsets, odd chunk sizes, gain changes, noise, clock
drift, low-pass filtering and random symbol corruption. In [`stuck_notes.rs`](cricket_core/tests/stuck_notes.rs), a thousand NoteOn/NoteOff
pairs go through a corrupted channel, and every note must be released. CI runs all of the above on
every push, on Linux, macOS and Windows, and the long measurement harnesses weekly
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

## Docs

- [`docs/README.md`](docs/README.md): index of all documents and the status of each proposal.
- [`docs/aml-v1-spec.md`](docs/aml-v1-spec.md): the AML v1 protocol, all profiles and test vectors.
- [`docs/architecture.md`](docs/architecture.md): crates, threads, the phy boundary, adding a modulation, what is stable.
- [`docs/proposals/`](docs/proposals): the measurements behind FEC, the LTC profiles and the digital profile.
- [`ROADMAP.md`](ROADMAP.md): what comes next, and what 1.0 means.
- [`CHANGELOG.md`](CHANGELOG.md): what changed.
- [`CONTRIBUTING.md`](CONTRIBUTING.md): how to contribute, including spec changes and new profiles.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by
you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions. See [`NOTICE`](NOTICE) for attribution.
