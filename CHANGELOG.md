# Changelog

All notable changes to Cricket (the AML v1 reference implementation) are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/) with the whole workspace on one version.

> **Note on numbering.** The entries under *Unreleased* were drafted as "1.1.0" while the project was
> still called AML. The next tagged release is **0.2.0**: 1.0.0 is reserved for "field test passed
> and API frozen" (see [`ROADMAP.md`](ROADMAP.md)). The 1.0.0 the GUI used to display was the app's
> own number, not a workspace release; it is listed below as 0.1.0.

## [Unreleased]

### Breaking

- The project is renamed from AML (AudioMIDI Link) to **Cricket** (Cricket MIDI). Crates are now
  `cricket_core`, `cricket_engine`, `cricket_cli` and `cricket_gui`; the CLI binary is `cricket`; the
  app identifier is `se.dpmstudios.cricket`. The protocol keeps the name AML, with Cricket as its
  reference implementation; its first official version is **AML v1** (spec: `docs/aml-v1-spec.md`),
  with wire version nibble `1` in the packet header, so it does not interoperate with earlier AML
  builds. The legacy prototype phy module is now `phy_legacy`. GUI settings live in a new `cricket`
  config directory; settings from AML builds are not migrated.
- **Wire format:** AML v1 now carries every MIDI 1.0 channel message on all 16 channels (Note On/Off
  with release velocity, poly and channel aftertouch, Control Change including channel mode messages,
  Program Change, Pitch Bend) plus Song Position, Song Select and Tune Request, in one `Midi` message
  type; previously only Note On/Off and Program Change on channel 1. SysEx other than MSC, MIDI Clock
  and other real-time messages, and MIDI Time Code are deliberately not carried (they need timing a
  packet link cannot give and would delay everything else). The packet `SEQ` is now a 2-bit counter
  with six reserved zero bits, which keeps the FEC profiles' protection against false messages within
  0.5 bit of before despite the larger message set (worst measured case: one false message per 62
  years for a misconfigured transmitter, 196 years for natural audio; target 10 years).
- **Wire format:** `robust`, the default profile, now sends a Reed–Solomon coded body (two RS(11,7)
  codewords over GF(16), whitened, spec §4.6) **once** instead of the uncoded body twice, and finds
  the preamble from all 4 symbols together (any 2 may be missed). It cuts airtime per message from
  273 ms to 175.5 ms and mean NoteOn latency in loopback from ~345–360 ms to ~265–270 ms (p95
  ~475–500 → ~340–350 ms), and delivers as often or more in every simulated channel except dropouts
  of ≥ ~50 ms (e.g. −3 dB SNR: 97 % vs 33 %; 5 % wrong tones: 97 % vs 65 %). Both ends must be
  updated together. The previous behaviour is the new profile **`robust_repeat`**, the choice for
  lines with glitches or dropouts of ~50 ms or more (60 ms dropouts: 92 % vs 72 % delivered). The
  name `robust_fec`, which this profile had during development, is still accepted and means
  `robust`. See `docs/proposals/fec.md`.
- **Wire format (FEC profiles):** the coded body is whitened and the decoder only offers the CRC
  candidates within the code's error budget and rejects out-of-range payloads, so the FEC profiles
  (`robust`, `ltc_fec`, `ltc2400_fec`, `ltc9600_fec`) are much harder to fool with foreign audio
  (both ends must run this version). Worst measured case before: one false message per ~530 h of
  MFSK traffic into `ltc2400_fec`, and per 2–22 h of traffic from a transmitter without FEC on the
  same phy; now ≥ 89 years in every measured case (target: one per 10 years), with packet delivery
  unchanged within ~0.5 % in the key conditions. See `docs/proposals/fec.md` §9.
- Workspace version is **0.2.0** everywhere (crates, `tauri.conf.json`, `package.json`); the GUI
  shows the version from the package instead of a hard-coded "v1.0". `cricket --version` prints
  `cricket 0.2.0`. Minimum supported Rust version is 1.87.

#### Stage safety and profile names

- an unknown profile name is an error (`PhyConfig::from_name` returns `Option`; the CLI lists the
  valid names, the GUI falls back to the default with a visible warning) instead of silently
  selecting `robust`.
- the transmit level of the MFSK and LTC-style profiles defaults to **−12 dBFS peak** (amplitude
  0.25, was 0.8) to leave headroom on the bus; `--tx-level-db` / the GUI "TX level" set it (−60 …
  0). The receiver is level-independent, so mixed versions still interoperate. `digital-bit-exact`
  is unchanged. The spec lists the level as a SHOULD.

### Added

- **Provisional** AML v1 profiles (spec §6.5; may still change in a later revision): `ltc`,
  `ltc2400`, `ltc4800` and `ltc9600` using an LTC-style biphase-mark code (~22 ms mean NoteOn
  latency at 4800 bit/s). They need a dedicated, clean line: no EQ, compression or limiter; see
  `docs/proposals/ltc-bmc.md`.
- **Provisional** (spec §6.5): `ltc_fec`, `ltc2400_fec` and `ltc9600_fec` profiles: the LTC-style
  phy with the Reed–Solomon coded body (+5 ms airtime at 4800 bit/s, ~26 ms mean NoteOn latency in
  loopback). In the simulator a 1 ms / 3 ms dropout is survived by 96 % / 92 % of packets (`ltc`:
  22 % / 0 %) and 100 clicks/s at +12 dB by 93 % (`ltc`: 69 %); see `docs/proposals/ltc-bmc.md` §9.
- **Experimental** `ltc14400` and `ltc14400_fec` profiles for digital lines (Dante, MADI, AES at
  48 kHz or more): the LTC-style phy at 14400 bit/s with sinusoidal edges, no postamble and a
  one-cell gap, so a message takes 5.4 ms (7.0 ms coded) instead of 8.4 ms (10.9 ms) at 9600 bit/s,
  and a 4-note chord 22 ms (29 ms) instead of 34 ms (44 ms); ~12 ms mean NoteOn latency in
  `cricket loopback` (`ltc9600_fec`: ~16 ms). They need ≥ 16 kHz of bandwidth and are not part of
  the spec yet; the other LTC profiles are unchanged.
- **Experimental** `digital` profile for bit-transparent digital paths (Dante / AES67 / MADI / AES3
  routed without processing, USB digital I/O in a bit-exact mode): the packet goes straight into the
  PCM sample values, one nibble per sample at bit 10 of the 24-bit word (survives 16-bit truncation,
  ±1 LSB of dither, 32767 / 2²³−1 scaling and polarity inversion; peak −57.4 dBFS). A message is 21
  samples sent twice: ~0.5–0.7 ms mean NoteOn latency in loopback, a 4-note chord complete after
  ~3.3 ms, ~1,150 messages/s. Any other change to the samples (gain beyond ±0.1 dB, EQ, SRC, drift,
  mixing) drops the frames: no false message in any of 22 simulated paths; worst-case false-accept
  estimate ~10¹⁰ years per receiver. A decoder profile suffix `+digital` (e.g. `ltc9600+digital`)
  also accepts digital frames. Not in the spec yet; see `docs/proposals/digital-transparent.md`.
- `cricket pathcheck send | receive | loop | sim`: is an audio path bit-transparent? Plays a test
  pattern, records the far end (or a WAV file) and reports bit-exactness and what the path does
  (gain, word length and dither, DC offset, SRC / clock drift in ppm, polarity, noise), with an exit
  status for scripts. `sim` runs simulated paths without hardware.
- **MIDI Show Control works end to end:** the decoder outputs real MSC SysEx (previously MSC was
  decoded but never sent to MIDI out), and the encoder accepts MSC SysEx from MIDI in. GO / STOP /
  RESUME / LOAD / GO_OFF / GO/JAM_CLOCK with cue numbers 0–999.9 (one decimal) and cue lists 1–15,
  FIRE, ALL_OFF, RESTORE, RESET, STANDBY ±, SEQUENCE ±, the clock and MTC-chase commands, and OPEN /
  CLOSE_CUE_LIST are carried. The MSC device ID and command format are decoder settings (default
  all-call `0x7F`, Lighting `0x01`; `cricket decode --msc-device-id/--msc-format`, GUI settings). An
  MSC message that cannot be carried exactly (e.g. cue 12.345, a cue path, TIMED_GO) is dropped with
  an alarm instead of being sent as a different cue.
- The CLI takes a global `--profile` option for every command; `cricket loopback --profiles a,b,…
  --notes N` measures several profiles in one run, and can also measure chords (`--chords N
  --chord-size 4`: first and last note), throughput (`--throughput N`: messages/s) and the engine's
  own overhead (`--zero-airtime`).
- `--buffer-frames N` sets the audio device buffer (the host default can be long: ALSA uses 100 ms),
  and `--sample-format auto|f32|i16|i32` the stream format. Integer device formats are converted
  with power-of-two scaling and rounding (16-bit used 32767 with truncation before; 32-bit streams
  were not supported), without allocating in the audio callback.
- `cricket_core` builds as `no_std` + `alloc` (with `--no-default-features`) so the modem can run on
  microcontrollers such as ESP32-S3, RP2350 and STM32. WAV file I/O and the legacy prototype phy stay
  behind the default `std` feature. CI checks the build for `thumbv8m.main-none-eabihf`.
- `cricket loopback` and the GUI self-test report measured MIDI-in to MIDI-out NoteOn latency (mean,
  95th percentile and jitter) while playing and releasing notes, instead of a theoretical airtime.
  The self-test no longer blocks the UI.
- While encoding, the UI shows how many messages are queued and how late the oldest one already is
  (its wait plus the audio buffered ahead of it). A panic button sends All Notes Off. Profile hints
  say when to use Fast and when to use Robust. Selected audio and MIDI devices are remembered by name
  across restarts and re-plugging. The UI unlocks controls when an engine worker stops and allows
  "none" for optional MIDI in/out.
- If the audio input device stops delivering samples while the link is up, All Notes Off is sent and
  an audio-input-stalled alarm is raised.
- Repository scaffolding for open-source work: `CONTRIBUTING.md` (DCO), `CODE_OF_CONDUCT.md`,
  `SECURITY.md`, `NOTICE`, issue and pull-request templates, `CODEOWNERS`, Dependabot, `deny.toml`,
  `ROADMAP.md`, a docs index (`docs/README.md`) and the November 2026 internal review
  (`docs/review-2026-11.md`). Crate metadata (description, repository, keywords, categories,
  `rust-version`) for crates.io. `#![forbid(unsafe_code)]` in every crate.
- CI: lint (fmt, clippy, no_std clippy, rustdoc with warnings denied, version agreement), tests on
  Linux/macOS/Windows, no_std builds for Cortex-M and RISC-V, an MSRV check, GUI builds on three
  platforms, supply chain (`cargo deny`, `cargo audit`) and a weekly run of the long measurement
  harnesses. A tag `v*` builds the `cricket` CLI for Linux (x64, arm64), macOS (universal) and
  Windows, and the desktop bundles via `tauri-action`, and attaches them to a GitHub Release with
  notes taken from this file.
- The macOS bundle declares `NSMicrophoneUsageDescription` and the audio-input entitlement so a
  signed app may open the audio input.

#### Stage safety and profile names

- **User-facing profile names**, one per kind of line: `analog-safe` (default; was `robust`),
  `analog-safe-repeat` (`robust_repeat`), `analog-fast` (`fast`), `clean-line` (`ltc_fec`),
  `clean-line-fast` (`ltc9600_fec`), `digital-line` (`ltc14400_fec`), `digital-bit-exact`
  (`digital`). The older names remain accepted everywhere (`-` and `_` are equivalent); the
  uncoded `ltc`, `ltc2400`, `ltc9600`, `ltc14400` and `ltc2400_fec` keep their names in an
  "Advanced: no error correction" group. `cricket profiles` and the README's "Which profile?"
  guide explain them.
- **Test scale is opt-in:** with no MIDI in the encoder sends heartbeats only; `--simulate` / the
  GUI checkbox "Play test scale" adds the C-major scale. Before, no MIDI in meant notes on stage
  nobody played.
- **Fail-safe on stop:** stopping a decoder (GUI Stop, Ctrl-C in the CLI, drop) sends Sustain off
  + All Notes Off on all 16 channels to MIDI out before closing it; a decoder-side **Panic** (GUI
  button, `panic` + Enter in the CLI) sends it at once.
- Audio **channel selection** for input and output (`--audio-in-channel` / `--audio-out-channel`,
  GUI dropdowns from the device's channel count); the encoder writes only the selected channel and
  leaves the others silent instead of copying the signal to every channel.
- Alarms: `AlarmCleared { LinkDead }` on the next valid packet, a `MidiOutputError` alarm when
  sends to the MIDI device fail (once per streak, cleared when they succeed again, lost messages
  counted), and the GUI shows an alarm list with timestamps, counts and acknowledge buttons
  instead of a single banner. The engine events and alarm kinds are documented in a table in
  `cricket_engine::events` for integrators.
- GUI meters and labels: input level in dBFS with the −30 … 0 dB good window and a numeric readout
  (the old bar went red at nominal level); confidence bar with a good mark; FEC corrections and
  CRC failures per minute, preambles found vs CRC fails; a LINK OK / NO SIGNAL / SIGNAL, NO LOCK /
  LINK DEAD traffic light with the time since the last heartbeat and a hint line; profile labels
  carry measured latencies (or say "airtime") instead of first-copy airtimes presented as latency.
- CLI: a status line every second while encoding or decoding, `--json` NDJSON event output for
  integration, exit codes 0 / 1 / 2 (usage) / 3 (path check Marginal), `list-devices` reports
  "MIDI unavailable" instead of aborting where there is no MIDI backend, one-line `--profile` help
  plus `cricket profiles`.
- Warnings when the selected audio input looks like a built-in microphone or the output like a
  built-in speaker, and on Windows when `digital-bit-exact` / `digital-line` are used through
  WASAPI shared mode.

### Changed

- Lower engine latency for every profile: the encoder and decoder wake on MIDI input and on the audio
  callback instead of polling every 5 ms, all queued notes of a chord are encoded at once, and idle
  refreshes / heartbeats never queue more than one message ahead of a note. Loopback NoteOn latency
  (mean): `ltc14400` 11.3 → 5.5 ms, `ltc14400_fec` 12.1 → 7.2 ms, `ltc9600` 13.1 → 8.2 ms, `ltc`
  21.8 → 16.1 ms, `ltc_fec` 26.2 → 21.3 ms, with jitter down from ~2–3 ms to < 0.1–0.7 ms; Fast and
  Robust unchanged.
- MIDI/AML decoding tolerates a wide input level range (−30 dB to 0 dB relative to transmit level)
  via level-independent tone detection and a silence floor.
- The MFSK receivers search for a preamble with ~15× less CPU (in silence or noise: ~220 → ~15 ns
  per sample on the host; ~64 ns worst case, on unrelated MFSK audio). The receiver checks the
  expected tone before running the 16-tone bank; with first-symbol detection the results are
  bit-identical.
- The LTC receiver makes soft decisions: a sync correlator that keeps running during a packet (a
  false lock can no longer hide a real packet), integrate-and-compare bit decisions on a tracked bit
  grid, per-nibble reliability, and erasures for dropouts. At 4800 bit/s it delivers 99.8 % of
  packets at 4 dB SNR (before: 44 %) and no simulated condition got worse; CPU cost rises from ~18
  to ~60 ns per sample on the host.
- `robust_repeat` also finds its preamble from all 4 symbols together (one may be missed) instead of
  requiring the first; the wire format is unchanged. It delivers as often or more in every simulated
  channel (−3 dB SNR: 33 % → 46 %; 5 % wrong tones: 65 % → 75 %; 30 / 60 ms dropouts: 96 / 90 % →
  98 / 92 %). Fast keeps first-symbol detection (its 2-symbol preamble is too short).
- TX prioritizes panic and note-offs over new notes. Each released note's note-off is repeated up to
  6 times in idle slots, so a lost note-off does not leave the note hanging; heartbeats are sent only
  when fully idle. Sustained fast traffic keeps queued note-ons under a bounded latency; queued
  note-ons beyond that budget may be dropped, but note-offs and panic are not, and a chord of up to
  4 notes is never dropped. On the `digital` profile NoteOff refreshes are spaced 20 ms apart instead
  of back to back, so a short dropout cannot swallow a release with all its repeats.
- Controller, pitch-bend and aftertouch streams are coalesced while waiting to be sent (latest value
  wins): a fader sending 200 values/s no longer builds up seconds of backlog, its final value arrives
  within about two message times, and notes are delayed by at most one controller update of their own
  channel. Bank Select, RPN/NRPN, Data Entry and Program Change keep their order; a NoteOff never
  overtakes a sustain pedal it was played after; All Notes Off / All Sound Off go out with Panic
  priority; MSC goes out with NoteOff priority.
- Link loss and panic use one fail-safe path: Sustain off and All Notes Off are sent once, on all 16
  channels, without duplicate bursts, and pitch bend is re-centred on channels the decoder left bent.
- Repeated on-air copies of the same packet are suppressed within a short time window; dedup state
  resets when the link is declared dead.
- Dependencies: `clap` 4.4 → 4.5, `dirs` 5 → 6 (deduplicated with Tauri), patch updates for
  `anyhow`, `bytes` and `time` (advisories). `hound` is no longer a dependency of the CLI and the
  unused `tauri-plugin-opener` is gone from the GUI. Major upgrades of `cpal`, `midir`, `ringbuf`
  and `tauri` are deferred (see `ROADMAP.md`).

### Fixed

- False preamble locks and missed body symbols resync without discarding already-received audio. A
  false preamble lock whose body fails to decode no longer swallows a real packet that starts inside
  it: the receiver rewinds and re-scans the buffered audio (MFSK profiles).
- A note released and struck again quickly (a trill or roll) stays in order, so it ends released.
  Panic discards note-ons that are still queued.

### Docs

- The spec (`docs/aml-v1-spec.md`) covers every profile, including the body FEC, the packet
  validation rules a receiver must apply, the LTC profiles (provisional), and test vectors that a
  test keeps in sync with the code. It now has a status/revision header, a revision log, an errata
  section and the versioning policy (wire changes vs. revisions); §7 is "Release assumptions".
- README restructured: Quick start first, then how it works, profiles, troubleshooting, a
  routing/cable guide and a hardware-box FAQ. Planning documents moved to `docs/history/`;
  `docs/release-checklist.md` replaced by `docs/release-process.md`; `cricket_gui/README.md`
  reduced to build/run instructions. The orphan `constants/symbol-definition.json` and the Vite
  template assets are removed; the desktop app has its own icon.

## [0.1.0] — 2026-07-29

The first working release, under the project's old name AML (AudioMIDI Link), published as commit
`7ce0be8` "Release AML v1.0". There was no tag and no changelog at the time; this entry is
reconstructed from the history. The protocol of this release was called "V2" internally (the
bitstream modem) and later became **AML v1** with wire changes, so 0.1.0 builds do not interoperate
with later ones.

### Added

- `aml_core`: an MFSK-16 bitstream modem (16 tones from 2.5 kHz, 100 Hz / 200 Hz spacing) with
  Goertzel detection, a 4-symbol preamble, framed packets (header, sequence number, payload, CRC-16),
  interleaving and packet repetition; two profiles, **Robust** (6.5 ms symbols, two copies) and
  **Fast** (3 ms symbols, one copy); Note On/Off and Program Change on channel 1 and an MSC-lite cue
  message; heartbeat and a link-dead fail-safe that sends All Notes Off; WAV file I/O for tests.
- `aml_gui`: a Tauri 2 desktop app with encoder (MIDI in → audio out) and decoder (audio in → MIDI
  out) side by side, device selection, link statistics, a self-test, and operator guidance to treat
  the audio channel like an LTC line (dedicated, no EQ / compression / limiting). macOS was the
  primary bundle target; the app displayed "v1.0".
- `aml_cli`: `list-devices`, `encode`, `decode`, `test` and `loopback` as a lab tool.
- The earlier sequential-tone prototype (March 2026, "V1", semantic tone symbols without CRC) was
  kept as a legacy debug phy (`docs/history/aml-prototype-protocol.md`).

[Unreleased]: https://github.com/paksnukk/Cricket/compare/7ce0be8...HEAD
[0.1.0]: https://github.com/paksnukk/Cricket/commit/7ce0be8
