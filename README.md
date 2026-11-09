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
6. **Same profile on both ends** (`--profile robust` is the default; see [Profiles](#profiles)).
7. **No hardware?** `cricket encode --simulate` plays a C-major scale; `cricket loopback` measures latency in-process.
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
(Start/Stop/Continue, Active Sensing), and MIDI Time Code. A packet takes 1–273 ms of airtime
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
vs 72 % delivered). Robust repeat was the default in earlier builds (as `robust`); a profile name
`robust_fec` from earlier builds now means Robust.

### LTC profiles (dedicated lines)

The LTC profiles send the same packets as a biphase-mark code, like SMPTE timecode, in a fraction of
the airtime. Their wire format is in the spec (§6.5) with status **provisional**: complete and tested
in software, but not yet proven on real show chains, so a later revision may change it.

| Profile | What it is | Measured NoteOn latency\* | Needs |
|---|---|---|---|
| `ltc_fec` | 4800 bit/s with a Reed–Solomon coded body (also `ltc2400_fec`, `ltc9600_fec`); survives 1–3 ms dropouts and click bursts | mean ~21 ms, p95 ~22–24 ms | A dedicated line: ≥ 4 dB SNR, no EQ, compression or limiter |
| `ltc` | 4800 bit/s, uncoded (also `ltc2400`, `ltc4800`, `ltc9600`) | mean ~16 ms (`ltc9600`: ~8 ms) | As `ltc_fec`, and no dropouts or clicks |
| `ltc14400_fec` | **Experimental, fastest LTC:** 14400 bit/s with the coded body (also uncoded `ltc14400`); 7.0 ms (5.4 ms) per message, a 4-note chord in 29 ms (22 ms); repairs dropouts up to ~0.5 ms | mean ~7 ms, p95 ~7.5 ms (`ltc14400`: ~5.5 ms) | A digital line (Dante, MADI, AES) at 48 or 96 kHz: ≥ 16 kHz bandwidth, ≥ 15 dB SNR |

An EQ boost inside the signal band breaks them (+12 dB at 3 kHz stops 4800 bit/s; 2400 bit/s survives
it but not a 1 kHz high-pass), and 9600 bit/s needs more than ~6 kHz of bandwidth (digital lines only).
`ltc14400` and `ltc14400_fec` are not in the spec yet (a candidate for §6.5): in simulation they pass
converter anti-alias filters (also two minimum-phase ones in series), sample-rate converters and ±1 %
clock error, at 48 and 96 kHz, but fail below ~16 kHz of bandwidth, e.g. through analog stages with a
low-pass at 12 kHz.
The measurements behind all profiles are in [docs/proposals/fec.md](docs/proposals/fec.md) and
[docs/proposals/ltc-bmc.md](docs/proposals/ltc-bmc.md).

### Digital (bit-transparent) — experimental

For paths that deliver samples **unchanged** (Dante / AES67 / MADI / AES3 routed without processing, a
USB interface's digital I/O in a bit-exact mode), `--profile digital` writes the packet straight into
the sample values, one nibble per sample, like SMPTE ST 337 carries data in AES3: a message is 21
samples, sent twice (0.9 ms on air), at −57.4 dBFS peak.

| | `digital` |
|---|---|
| Measured NoteOn latency\* | mean ~0.5–0.7 ms, p95 ~0.6–0.8 ms; 4-note chord complete after ~3.3 ms |
| Throughput | ~1,150 messages/s |
| Survives | 16-bit truncation, ±1 LSB of dither, 32767 / 2²³−1 scaling, polarity inversion, gain within ±0.1 dB, noise ≤ −100 dBFS |
| Fails (safely: no MIDI, never wrong MIDI) | any other gain, EQ, dynamics, sample-rate conversion, unlocked clocks, mixing, analog |

**Check the path first** with `cricket pathcheck` (below): it plays a test pattern, records the far end
and tells you whether the path is bit-exact and, if not, what it does (gain, word length, dither,
SRC / drift, polarity, offset, noise). On Windows, Cricket currently uses WASAPI shared mode, which goes
through the Windows mixer: run the path check, and prefer macOS / Linux (ALSA `hw:` device) until
exclusive mode / ASIO are supported. A decoder profile with `+digital` (e.g. `ltc9600+digital`) also
accepts digital frames. Design, false-accept analysis (worst case ~10¹⁰ years per false message), the
fail-safe table and path requirements: [docs/proposals/digital-transparent.md](docs/proposals/digital-transparent.md).
Not part of the spec yet, and not in the GUI.

\* MIDI in → MIDI out through the engine, in-process (`cricket loopback`, 20–60 notes), while notes are
being played *and released*. Real audio interfaces add their own buffering on top. Run the measurement
yourself rather than trusting this table.

## The CLI in more detail

```sh
# What audio and MIDI devices are there?
cricket list-devices

# Sending side: MIDI in → tones out (by index or by exact name)
cricket encode --midi-in-name "My Keyboard" --audio-out-name "Dante TX 1-2"

# Receiving side: tones in → MIDI out
cricket decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1"

# ... sending MSC to a console that listens on device ID 1 (default: 0x7F all-call, format 0x01)
cricket decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1" \
    --msc-device-id 1 --msc-format 0x01

# No hardware at hand? Encoder plays a C-major scale on its own
cricket encode --simulate

# How fast is it, really? (no hardware needed; default: fast,robust)
cricket loopback
cricket loopback --profiles robust,robust_repeat,fast,ltc,ltc_fec --notes 40
cricket loopback --profiles digital,ltc9600 --chords 20 --throughput 100

# Is this path bit-transparent (for --profile digital)? Sender, then receiver:
cricket pathcheck send --audio-out-name "Dante TX 1-2"
cricket pathcheck receive --audio-in-name "Dante RX 1-2" --seconds 5
cricket pathcheck sim                     # simulated paths, no hardware

# Lower device latency (default: the host's buffer; ALSA uses 100 ms)
cricket --profile digital --buffer-frames 128 decode --audio-in-name "Dante RX 1-2" --midi-out-name "IAC Bus 1"
```

From a source checkout, replace `cricket` with `cargo run -p cricket_cli --` (add `--release` for
`loopback`). Devices are remembered **by name**, so unplugging and replugging an interface doesn't
silently pick another one. A name that isn't present is an error.

### The GUI

```sh
cd cricket_gui
npm install
npm run tauri dev
```

The GUI has the encoder and decoder side by side. It shows live link stats (state, confidence, CRC pass
rate, sync losses), the TX queue and its latency, alarms for missing or stalled devices, a big red
**Panic** button, and a **Self-Test** that measures real loopback latency for Fast and Robust.
Build instructions: [`cricket_gui/README.md`](cricket_gui/README.md).

## Troubleshooting

| Symptom | Look at | Fix |
|---|---|---|
| Decoder stays in `Search`, CRC pass count 0 | Is audio arriving at all? The input level / `input_level` event; `cricket list-devices` for the right input; both ends on the **same profile** | Patch and select the right channel; match `--profile`; raise the send level if the far end is below the −60 dBFS silence floor |
| `Locked` for a moment, then `Search` again; many sync losses | The chain: a gate, compressor, limiter or EQ on the Cricket channel; sample-rate conversion on a digital path | Remove processing from that channel (treat it like LTC); try `robust`; on digital lines run `cricket pathcheck` |
| Notes arrive but some hang, or arrive late | TX queue depth and latency in the GUI; the profile's comfortable rate; dense controller data | Use a faster profile on a clean line, play fewer simultaneous messages, let the fail-safe clear notes (link dead → All Notes Off) |
| Everything works until someone touches the fader | Level within −30…0 dB of transmit level? Clipping on the way in? | Keep ~−12 dBFS headroom on the send; level is otherwise irrelevant to detection |
| `ltc*` profiles never lock, MFSK profiles do | Bandwidth and EQ: the LTC profiles need ≥ 6 kHz (9600 bit/s) or ≥ 16 kHz (14400 bit/s), no EQ boost in band | Use `ltc2400`/`ltc_fec` on analog paths, `ltc9600*`/`ltc14400*` on digital lines only |
| `--profile digital` gives nothing | `cricket pathcheck receive` verdict: gain, SRC, dither, mixer? | Route the channel bit-exact (no gain, no SRC, no mixing); on Windows prefer another OS until exclusive mode exists |
| Audio input stalls, "audio-input-stalled" alarm | The interface's clock / driver; USB power | Re-plug or re-select the device; the decoder already sent All Notes Off |
| "MSC cue … not representable" | Cue number has more than one decimal, a cue path, or a command outside the carried set | Rename the cue (0–999.9, one decimal, list 1–15) or send it another way |
| `cricket --version` or the GUI shows an unexpected version | You may be running an old binary | `cargo install --path cricket_cli --force`, or download the current release |

When you report a problem, the [field report template](.github/ISSUE_TEMPLATE/field-report.md) asks for
the chain, the profile and the decoder's link stats; that is usually enough to tell what happened.

## Routing and cable guide (stub)

Cricket's channel is treated **like an LTC timecode line**:

- **One dedicated mono channel per direction.** Nothing else is mixed onto it. Return traffic (if any)
  takes a second channel.
- **No processing:** no EQ, dynamics, noise gate, de-esser, limiter or "auto gain" on that channel.
  Insert points and console channel strips are the usual culprits.
- **Set gain once.** Anything from −30 dB to 0 dB relative to the transmit level decodes; the send
  level defaults to −1.9 dBFS peak. Leave ~10 dB headroom so nothing upstream limits it.
- **Analog XLR:** balanced line level works over long runs; the MFSK profiles live between 2.5 and
  5.5 kHz, so even a narrow-band path carries them. LTC profiles above 4800 bit/s want a digital path.
- **Dante / AES67 / MADI / AES3:** route the channel without sample-rate conversion if you intend to
  use `digital` or `ltc14400*`; MFSK and the 2400/4800 LTC rates don't care about SRC.
- **Virtual cables / DAW loopback:** fine for testing; watch the buffer sizes (`--buffer-frames`).
- **Wireless, codecs, intercom:** compression codecs and wireless links alter the signal; expect only
  `robust` to work, and test first.

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
every push, on Linux, macOS and Windows, and the long measurement harnesses weekly
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

## Docs

- [`docs/README.md`](docs/README.md): index of all documents and the status of each proposal.
- [`docs/aml-v1-spec.md`](docs/aml-v1-spec.md): the AML v1 protocol, all profiles and test vectors.
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
