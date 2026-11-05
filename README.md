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
- **Nothing is left hanging.** A heartbeat keeps the link alive. If no valid packet arrives for 2 s, or the
  audio input stops delivering samples, the decoder sends **Sustain off + All Notes Off on all 16
  channels** once (and re-centers any pitch bend it left bent). Each released note's NoteOff is
  repeated a few times in idle airtime, so a NoteOff lost to noise doesn't leave a note droning forever.
- **Important messages first.** The transmitter sends Panic and All Notes Off / All Sound Off, then
  NoteOffs and MSC, then everything else, then NoteOff repeats, then heartbeats. When fast playing would
  overflow the airtime, it drops the oldest queued NoteOns, never a NoteOff, MSC or Panic, and never a
  chord of up to 4 notes.
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
    format `0x01`, Lighting (General)); the encoder accepts MSC for any device ID and format.
  - An MSC message that does not fit (cue `12.345`, cue `1000`, list `16`, a cue path, TIMED_GO, SET,
    SET_CLOCK, OPEN/CLOSE_CUE_PATH) is **not sent**, and the encoder raises an alarm such as
    "MSC cue 12.345 not representable". It never sends a different cue.
- **Panic** (the GUI button): All Notes Off everywhere.

**Not carried, on purpose:** other System Exclusive, MIDI Clock and the other real-time messages
(Start/Stop/Continue, Active Sensing), and MIDI Time Code. A packet takes 8–273 ms of airtime
depending on the profile; clock and timecode need a steady stream with sub-millisecond timing, so they
would arrive useless and would delay every note and cue behind them. Run timecode as LTC on its own
audio channel instead. Large SysEx dumps would block the line for seconds.

## Profiles

Both ends must use the same profile; it is not signalled on the wire. All profiles carry the same
messages and are specified in [`docs/aml-v1-spec.md`](docs/aml-v1-spec.md) §6.

| | **Robust** (default) | **Robust repeat** | **Fast** |
|---|---|---|---|
| Use it for | Cues and notes over a chain you don't fully trust | Lines with glitches or dropouts of ~50 ms or more | Playing notes over a clean, dedicated line (no EQ, compression or limiter) |
| Symbol | 6 ms + 0.5 ms guard | 6 ms + 0.5 ms guard | 3 ms |
| Tones | 16 × 100 Hz steps from 2.5 kHz | same | 16 × 200 Hz steps from 2.5 kHz |
| Body | Reed–Solomon coded (repairs wrong or missing tones) | uncoded | uncoded |
| Copies per message | 1 | 2 | 1 |
| Airtime per message | 175.5 ms | 273 ms | 57 ms |
| Comfortable rate | ~5 messages/s | ~3–4 messages/s | ~10–17 messages/s |
| Measured NoteOn latency\* | mean ~265–270 ms, p95 ~340–350 ms | mean ~345–360 ms, p95 ~475–500 ms | mean ~80 ms, p95 ~115 ms |

Robust finds a packet even when two of its four preamble tones are lost, and repairs up to 8 missing or
4 wrong tones in the body. In the channel simulator it delivers as often as Robust repeat or more in
every condition except dropouts of 50 ms and longer, where the second copy wins (60 ms dropouts: 92 %
vs 72 % delivered). Robust repeat was the default before v1.1 (as `robust`); a profile name
`robust_fec` from earlier builds now means Robust.

### LTC profiles (dedicated lines)

The LTC profiles send the same packets as a biphase-mark code, like SMPTE timecode, in a fraction of
the airtime. Their wire format is in the spec (§6.5) with status **provisional**: complete and tested
in software, but not yet proven on real show chains, so a later revision may change it.

| Profile | What it is | Measured NoteOn latency\* | Needs |
|---|---|---|---|
| `ltc_fec` | 4800 bit/s with a Reed–Solomon coded body (also `ltc2400_fec`, `ltc9600_fec`); survives 1–3 ms dropouts and click bursts | mean ~26 ms, p95 ~30 ms | A dedicated line: ≥ 4 dB SNR, no EQ, compression or limiter |
| `ltc` | 4800 bit/s, uncoded (also `ltc2400`, `ltc4800`, `ltc9600`) | mean ~22 ms | As `ltc_fec`, and no dropouts or clicks |
| `ltc14400_fec` | **Experimental, fastest:** 14400 bit/s with the coded body (also uncoded `ltc14400`); 7.0 ms (5.4 ms) per message, a 4-note chord in 29 ms (22 ms); repairs dropouts up to ~0.5 ms | mean ~12 ms, p95 ~16 ms | A digital line (Dante, MADI, AES) at 48 or 96 kHz: ≥ 16 kHz bandwidth, ≥ 15 dB SNR |

An EQ boost inside the signal band breaks them (+12 dB at 3 kHz stops 4800 bit/s; 2400 bit/s survives
it but not a 1 kHz high-pass), and 9600 bit/s needs more than ~6 kHz of bandwidth (digital lines only).
`ltc14400` and `ltc14400_fec` are not in the spec yet (a candidate for §6.5): in simulation they pass
converter anti-alias filters (also two minimum-phase ones in series), sample-rate converters and ±1 %
clock error, at 48 and 96 kHz, but fail below ~16 kHz of bandwidth, e.g. through analog stages with a
low-pass at 12 kHz.
The measurements behind all profiles are in [docs/proposals/fec.md](docs/proposals/fec.md) and
[docs/proposals/ltc-bmc.md](docs/proposals/ltc-bmc.md).

\* MIDI in → MIDI out through the engine, in-process (`cricket loopback`, 20–60 notes), while notes are
being played *and released*. Real audio interfaces add their own buffering on top. Run the measurement
yourself rather than trusting this table.

## Quick start

You need a Rust toolchain. On Linux you also need ALSA headers (`libasound2-dev`). The GUI additionally needs
Node.js and the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```sh
# What audio and MIDI devices are there?
cargo run -p cricket_cli -- list-devices

# Sending side: MIDI in → tones out (by index or by exact name)
cargo run -p cricket_cli -- encode --midi-in-name "My Keyboard" --audio-out-name "Dante TX 1-2"

# Receiving side: tones in → MIDI out
cargo run -p cricket_cli -- decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1"

# ... sending MSC to a console that listens on device ID 1 (default: 0x7F all-call, format 0x01)
cargo run -p cricket_cli -- decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1" \
    --msc-device-id 1 --msc-format 0x01

# No hardware at hand? Encoder plays a C-major scale on its own
cargo run -p cricket_cli -- encode --simulate

# How fast is it, really? (no hardware needed; default: fast,robust)
cargo run --release -p cricket_cli -- loopback
cargo run --release -p cricket_cli -- loopback --profiles robust,robust_repeat,fast,ltc,ltc_fec --notes 40
```

The binary is called `cricket` (`cargo install --path cricket_cli`, or `target/release/cricket`).

Devices are remembered **by name**, so unplugging and replugging an interface doesn't silently pick another
one. A name that isn't present is an error.

### The GUI

```sh
cd cricket_gui
npm install
npm run tauri dev
```

The GUI has the encoder and decoder side by side. It shows live link stats (state, confidence, CRC pass
rate, sync losses), the TX queue and its latency, alarms for missing or stalled devices, a big red
**Panic** button, and a **Self-Test** that measures real loopback latency for Fast and Robust.

## Project layout

| Crate | What lives there |
|---|---|
| [`cricket_core`](cricket_core) | The modem: tone synthesis and detection, the LTC phy, packets, CRC, Reed–Solomon FEC, interleaving, TX scheduler, fail-safe. Pure Rust, `no_std` + `alloc` for microcontrollers; the default `std` feature adds WAV file I/O and the legacy prototype phy. |
| [`cricket_engine`](cricket_engine) | The real-time part: audio/MIDI devices, encoder and decoder workers, events, loopback latency measurement. |
| [`cricket_cli`](cricket_cli) | Command-line front end. |
| [`cricket_gui`](cricket_gui) | Tauri 2 desktop app (TypeScript + Rust). |

## Testing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The tests don't need hardware. A channel simulator in [`cricket_core/tests/support`](cricket_core/tests/support)
puts packets through offsets, odd chunk sizes, gain changes, noise, clock drift, low-pass filtering and
random symbol corruption. In [`stuck_notes.rs`](cricket_core/tests/stuck_notes.rs), a thousand NoteOn/NoteOff
pairs go through a corrupted channel, and every note must be released. CI runs all of the above on
every push.

## Docs

- [`docs/aml-v1-spec.md`](docs/aml-v1-spec.md): the AML v1 protocol, all profiles and test vectors.
- [`docs/v1.1-plan.md`](docs/v1.1-plan.md): what v1.1 set out to fix, and how.
- [`CHANGELOG.md`](CHANGELOG.md): what changed.

## Status

v1.1 is unreleased. Everything above is tested in software and through the channel simulator, but **not yet
on a real show chain**. Use it as a backup line before you trust it with the main cue. Field reports are
very welcome, especially your chain (Dante? analog? any processing?) and what the decoder's link stats
said.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by
you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
