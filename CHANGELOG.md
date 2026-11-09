# Changelog

All notable changes to Cricket (the AML v1 reference implementation) are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/) with the whole workspace on one version.

> **Note on numbering.** The entries under *Unreleased* were drafted as "1.1.0" while the project was
> still called AML. The first tagged release is **0.2.0**: 1.0.0 is reserved for "field test passed
> and API frozen" (see [`ROADMAP.md`](ROADMAP.md)). The 1.0.0 the GUI used to display was the app's
> own number, not a workspace release; it is listed below as 0.1.0.

## [Unreleased]

### Changed

The structural changes below are **API changes without a wire-format or behaviour change** (spec
vectors, all 256 tests, the A/B tables at fixed seeds, `cricket test` and `cricket loopback` give
identical results); together they are a **minor bump for the crates, 0.3.0 when released**. Overview
and the rules in [`docs/architecture.md`](docs/architecture.md).

- **Phy trait boundary** (`cricket_core::phy::traits`): the modem talks to a physical layer only
  through `PhyFactory` (the parameters of a plan: build the ends, declare the frame length, the
  `BodyCoding` — `Padded16` / `Fec(mode)` / `Raw` — and the TX scheduler's `RefreshPolicy`), `PhyTx`
  (one burst per body) and `PhyRx` (samples in, `RxEvent`s out). `Modulation` moves to
  `phy::modulation` with per-phy parameters in its variants — `Mfsk(MfskParams)`, `Bmc(BmcParams)`,
  `Digital(DigitalParams)` — and a `Custom(Box<dyn PhyFactory>)` variant through which another crate
  plugs in a modulation (no `Arc`, so it builds on Cortex-M0+). `modem.rs` no longer special-cases
  the digital phy. **Migration:** `Modulation::Mfsk` / `Digital` patterns become `Mfsk(_)` /
  `Digital(_)`; the MFSK-only fields of `PhyConfig` (`symbol_duration_ms`, `preamble`, `n_tones`,
  `rx_band_hz`, `acquisition`, …) live in `MfskParams`, reached through `config.mfsk()` /
  `mfsk_mut()` / `map_mfsk(|p| …)` or the accessors `config.symbol_duration_ms()`,
  `config.preamble()`, `config.rx_band_hz()` …, which answer for any phy; the common fields stay
  public. Direct constructors of a phy (`ModemRx::new`, `BmcRx::new`, …) still take a `PhyConfig`
  and now panic if it selects another modulation (the modem goes through the factory).
- **`PhyConfig` is `#[non_exhaustive]`** (so are `MfskParams`, `BmcParams`, `DigitalParams`): build
  one from a constructor, `from_name`, `PhyConfig::builder()` or the `with_*` adjusters
  (`with_fec`, `with_repetition_k`, `with_modulation`, `with_link_dead_ms`, …) instead of a struct
  literal. Constructors, `from_name` and the `PROFILES` catalog (names, aliases, tiers) are unchanged.
- **`#[non_exhaustive]`** on the public enums that will grow: `TransportMessage`, `MessageType`,
  `Modulation`, `RxEvent`, `ModemOutput`, `FecMode`, `MscCommand`, `MscError`, `PacketDecodeError`,
  `EngineEvent`, `AlarmKind`. A `match` outside the crate needs a wildcard arm. `TxScheduler`'s drop
  counters are read through `note_ons_dropped()`, `queue_dropped()` and `take_dropped()`.
- **`cricket_sim` crate**: the channel simulator (impairments, instrument and program sources,
  converter and link models, gate / compressor / limiter, pink noise, bursty loss, hiss and spurs,
  drift, IMD helpers, chunked decoding) moves out of `cricket_core/tests/support` into a workspace
  crate of its own (`std`, publishable), together with the bit-transparency path models
  `PathImpairment` / `simulate` (formerly `cricket_core::pathcheck`). The core's tests use it as a
  dev-dependency (`support` re-exports it); the CLI's `pathcheck sim` uses it directly.
- **`pathcheck` feature** of `cricket_core` (on with `std`): a `--no-default-features` firmware build
  leaves out the bit-transparency analyzer and its report formatting. thumbv8m release object code
  183.5 KB (0.2.0) → 174.6 KB without it (192.9 KB with it).

### Added

- **Engine pass-through node** (`cricket_engine::passthrough`, CLI `cricket passthrough`): audio in →
  a pluggable `AudioProcessor` → audio out on one input and one output stream, the duplex path a
  hardware box needs. `Passthrough` is the identity (bit-exact; 0 samples of added delay and a
  turnaround of ~40–90 µs per block in-process, measured with the in-memory endpoints by
  `measure_passthrough_latency`); `TapDecoder` passes the audio unchanged while a copy is decoded with
  the chosen profile, emitting the decoder worker's events and sending its MIDI, with an optional
  **strip** stage (`--strip`) that removes the profile's receiver band from the output: a 129-tap
  linear-phase FIR for the instrument profiles (≤ 0.04 dB ripple below 17 kHz, −46.7 dB at 18 kHz,
  56 dB of attenuation of a burst, 64 samples = 1.33 ms of group delay on that output). The output
  ring is bounded (oldest audio dropped, counted) so a slow output cannot grow the delay.
- **Fuzz targets** (`cricket_core/fuzz`, cargo-fuzz, nightly): packet bytes, FEC body decode, RS16,
  MSC SysEx, MIDI input, profile names, and sample-stream fuzzers for the MFSK, BMC, digital and
  whole-modem receivers (no panic on any input, NaN included; bounded buffers; consistent counters).
  A weekly CI job runs each for ten minutes; see `CONTRIBUTING.md` for the local recipe.

### Fixed

- `MscMessage::parse_sysex` panicked on a 6-byte SysEx with the MSC header (`F0 7F id 02 fmt F7`,
  no command byte), reachable from the encoder's MIDI input; it is `MscError::Malformed` now. Found
  by the `msc_parse_sysex` and `midi_input` fuzz targets.
- `PhyConfig::from_name` accepted whitespace between a profile name and the `+digital` suffix
  (`"digital \n+digital"`) that `canonical_name` and `profile_info` rejected; the three agree now.
  Found by the `from_name` fuzz target. Well-formed names are unaffected.
- **HF (instrument-audio) profiles** `instrument` (= `hf_robust`) and `instrument-fast`
  (= `hf_fast`), spec **§6.6 (provisional, revision r6)**: MIDI embedded on an instrument's own
  signal above the music and extracted by a box that passes the audio through. `instrument` is
  Analog Safe's timing, coded body and joint acquisition with the 16 orthogonal tones moved to
  19.0–21.5 kHz (175.5 ms per message); `instrument-fast` is the same code on 3 ms symbols with an
  **8-tone alphabet (3 bits per symbol)** at 19.0–21.33 kHz, 105 ms per message. Both use
  raised-cosine symbol ramps (−58 dB of leakage below 17 kHz against −40 dB with linear ramps) and
  send at **−30 dBFS** by default; the receiver band-passes 18–22.5 kHz with an **8th-order**
  Butterworth (`PhyConfig::rx_band_order`; the baseband profiles keep their 2nd-order filter,
  bit-identically). In the channel simulator (400 trials per cell, training and held-out tables):
  100 % delivered over a passive-pickup guitar, a bass and keys at every level from −20 to −50 dBFS
  and over a bright synth to −40 dBFS, with a −85 dBFS pedalboard hiss floor, PSU spurs in a tone slot, compressors,
  gates and sharp or slow 48 kHz converters; 0 false messages in 60 s each of guitar, bass, keys,
  synth, cymbals, noise, speech, music, MFSK and LTC traffic (no phy lock at all), and the baseband
  receivers never lock on HF traffic. Not supported, as measured: 44.1 kHz devices or SRC, codecs
  and wireless links, a 1 % sample-rate mismatch, and any drive stage between embedder and box (the
  tones' intermodulation residue through a fuzz is only 17 dB under the music at −30 dBFS). A 96 kHz
  variant (`PhyConfig::hf_fast_96k`, 24–29 kHz) exists as a constructor only. Catalog tier
  "instrument" (CLI `cricket profiles`, GUI selector group); `cricket loopback` measures mean
  ~213 / ~121 ms NoteOn latency. `docs/proposals/hf-instrument.md`.
- `cricket hf-mix --wav-in rec.wav --wav-out mixed.wav --messages "pc=1:10,note=1:60:100,go=14.5:2"`
  embeds messages into a 48 kHz recording, and `cricket hf-scan --wav-in rec.wav` decodes one and
  prints the receiver's counters and the recording's level in the tone band, so a real rig can be
  tried before hardware exists (a 44.1 kHz recording is refused with the reason).
- Simulator (`cricket_core/tests/support`): instrument sources (passive-pickup and bright-pickup
  guitar, bass, PolyBLEP synth, electric-piano keys, cymbals), pedalboard hiss floors, PSU spurs,
  converter and link models (sharp / slow 48 kHz, 44.1 kHz hop and SRC, 19 kHz codec LP and wall,
  96→48 kHz hop), windowed-sinc clock drift, and an intermodulation-residue helper; the HF
  measurement harness `tests/hf_profiles.rs` (`hf_report`).
- Engine: a warning when an instrument profile ends up on a device that opened below 48 kHz. CLI:
  `--tx-level-db` and `--profile` now default to the profile's own level and to `analog-safe`
  (`instrument` for the HF tools) instead of always passing −12 dBFS; the GUI moves the TX level
  field to a profile's default when the field was never customized.

## [0.2.0] — 2026-11-09

First tagged release. Spec revision **r5** of AML v1 is the wire format this release freezes for the
stable sections (see the spec header policy). Everything below was measured in the channel simulator
and in-process loopback; nothing has been proven on a real show chain yet.

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

### Link key, scheduler and fail-safe

- **Wire format (link key):** the packet CRC-16 is now computed with a configurable 16-bit **link
  key** as its init value (spec §4.1, revision r4). Two Cricket links in one multicore or Dante
  domain leak into each other at about −40 dB, and the receivers are level-independent, so a
  receiver delivered the neighbouring link's valid packets — including MSC GOs. With different keys
  every such packet fails the CRC deterministically (the CRC is affine in its init), at zero wire
  bits and with the false-accept arithmetic unchanged. The default key `0xFFFF` is the
  CRC-16/CCITT-FALSE init, so unkeyed links are bit-identical to before and interoperate with older
  builds; a keyed link does not. In the simulator, two links at −40 dB crosstalk deliver all 12 of
  the neighbour's packets unkeyed on every profile and none with different keys, with the own
  delivery unchanged (`cricket_core/tests/link_key.rs`). CLI `--link-key 0x1234` or
  `--link-key "a passphrase"` (hashed to 16 bits), a GUI field, `PhyConfig::with_link_key`,
  `DecoderConfig` / `EncoderConfig::link_key`. The status shows the key masked and a **wrong-key
  suspects** count (`LinkStats::wrong_key_suspects`, `ModemStats::wrong_key_suspects`, CLI "(N other
  key?)", GUI, `--json`): bodies with a packet's structure whose CRC failed, which random noise
  produces with probability about 2⁻¹⁷, so a key mismatch between the two ends shows up in
  seconds. Keying the whitening sequence as well was evaluated and not done: a neighbour's clean
  body decodes to one candidate that fails the CRC deterministically, and measured same-profile
  other-key traffic offers the CRC fewer candidates per body than the misconfigured uncoded
  transmitter the whitening already guards against (`fec_false_accept.rs`,
  `other_key_traffic_never_decodes` and the report's "other-key traffic" row).
- **MSC goes ahead of Note Off** in the TX scheduler (spec §5.5 tiers 2 and 3): a chord release no
  longer delays a GO by up to four messages (0.7 s on Robust).
- **Chords on Fast and Robust:** the queued Note On limit is per profile, max(⌊250 ms / airtime⌋,
  min(10, max(4, ⌊1 s / airtime⌋))): 5 on Robust (was 4), 4 on Robust repeat, **10 on Fast** (was
  4), unchanged 11–46 on the LTC profiles. Beyond it the **newest** Note On is refused instead of the
  oldest being dropped: the notes struck first (the bass, the hand that leads) are the ones that go
  out, and a note that would only arrive late is reported; before, the 6th note of a chord on Fast
  dropped the 2nd struck. A Note Off for a refused note is not sent either (unless the note is
  sounding from an earlier strike). Every drop raises an alarm (`AlarmKind::MessageDropped`,
  `"message_dropped"`, with the count and the profile's limit) and is counted in
  `TxStatus::dropped` (CLI status "dropped N", `--json`, GUI). `TxScheduler::note_on_dropped` (a
  flag) is now `note_ons_dropped` (a count), with `max_note_ons()`. In the scheduler harness a
  6-note chord now arrives whole on Fast (342 ms) and loses only its last note on Robust (before: the
  first two on both). `cricket loopback --chords 10 --chord-size 6` therefore completes on
  `analog-fast` (it used to wait for the two lost notes) and still reports the sixth note as not
  delivered on `analog-safe`, whose limit is 5.
- **Note Off refresh policy** (spec §5.5): on the MFSK profiles the six repeats of a released note
  follow a Fibonacci schedule (1, 2, 3, 5, 8, 13 message times after the Note Off) instead of
  filling the first six free slots back to back, so consecutive copies no longer fall into one
  dropout and a release costs a few idle slots spread over seconds instead of a 4.2 s burst for a
  4-note release on Robust. On Robust (coded: 0.3 % of Note Offs lost at 2 % symbol errors, and a
  175 ms message) a due repeat also waits for one message time of idle line, so a note struck
  shortly after another never waits behind a repeat; Robust repeat and Fast, which lose 6–23 % of
  Note Offs at 2 % errors, keep the first free slot. The LTC profiles and `digital-bit-exact`
  (messages of a few ms) keep the first six free slots, as before: tried with the spread schedule,
  `clean-line` put a repeat where the next note lands and its loopback p95 rose from 21 to 38 ms
  (a recorded negative result). While repeats are pending but not due, the Heartbeat slot carries
  one instead of a Heartbeat. Measured (`cricket loopback --notes 60`, in-process, before → after):
  `analog-safe` mean 269 → **212 ms** (p95 347 → 330) detached and 486 → **199 ms** (p95 526 → 294)
  legato (`--legato`: a note every 300–600 ms, released 60 ms after the next strike);
  `analog-fast` 82 → 78 ms (p95 119 → 117) detached, 89 → 84 ms legato; `clean-line` (21 / 22 ms)
  and `digital-line` (7.3 / 7.3 ms) unchanged. In the scheduler harness (`tests/tx_policy.rs`, legato) Robust goes from
  324 to 250 ms mean with 47 instead of 145 repeats on the line (line duty 100 → 83 %). The
  stuck-note harness (`tests/stuck_notes.rs`, 0.5–2 % symbol errors, Robust / Robust repeat / Fast)
  still ends every run with no note stuck past link-dead time; Robust repeat at 2 % now has longer
  stuck episodes under its 11 % Note Off loss (longest 1.8 s, was 0.7 s), the price of spacing the
  repeats. `TxScheduler::with_refresh_schedule` / `with_refresh_quiet`, `modem::refresh_schedule` /
  `refresh_quiet_samples`, `next_idle_sample`; `with_refresh_spacing` now spaces the repeats of one
  note.
- **Controller refresh:** the transmitter remembers the pedal (CC 64/66/67/69), Modulation (CC 1)
  and Pitch Bend state it sent and, in idle Heartbeat slots, re-sends one state that differs from
  the default (pedal down, modulation on, bend off centre) instead of a Heartbeat, round robin. A
  receiver that restarted or ran its fail-safe recovers the pedal and bend within a few Heartbeat
  intervals; the airtime cost is zero (it replaces the Heartbeat), the receiver sees an idempotent
  repeat.
- **Fail-safe per profile** (spec §5.3, §6): heartbeat / link-dead stay **500 / 2000 ms** on the
  MFSK profiles and the LTC profiles up to 4800 bit/s, and become **250 / 1000 ms** on
  `clean-line-fast` (`ltc9600*`) and `digital-line` (`ltc14400*`) and **100 / 500 ms** on
  `digital-bit-exact`, so a dead line clears stuck notes in 1 s (0.5 s) instead of 2 s where a
  heartbeat is cheap. The rule: the heartbeat interval is a quarter to a fifth of link-dead (3–4
  heartbeats in a row must be lost for a false link-dead: at 97 % delivery one per 5–170 h on
  Robust, 2.6–86 h on `clean-line-fast`, at 99 % one per 70 h–290 d) and at least 20 message
  airtimes, so a heartbeat in flight stays out of the p95 note latency (`clean-line`'s 21.8 ms
  message would be 17 % of a 125 ms interval, so it keeps 500 / 2000 ms). CLI `decode
  --link-dead-ms` overrides it (`DecoderConfig::link_dead_ms` as before).
- **Fail-safe burst content:** the burst now starts each channel with an explicit **Note Off for
  every note this receiver turned on** and has not seen released (several soft synths and DAWs
  ignore CC123), then Sustain off, All Notes Off and the pitch-bend centre as before (spec §5.3).
  `MidiOutput::sounding()` exposes the tracked notes.
- **Fail-safe burst optional** for cue-only (MSC) links where a stray All Notes Off is unwanted:
  CLI `decode --no-failsafe-burst`, GUI checkbox, `DecoderConfig::fail_safe_burst`,
  `CricketModemRx::set_fail_safe_burst`. Link-dead is still detected and reported; Panic, link-dead,
  input stall and stop then send nothing to MIDI out.
- **MSC addressing at the transmitter** (spec §5.4, "multi-device rigs: filter at TX"): with several
  MSC devices on the MIDI line, a cue for the sound console was carried and re-addressed to the
  receiver's single target (a lighting all-call GO). The encoder can now carry only MSC addressed to
  one device ID and/or command format (all-call `0x7F` / all-types `0x7F` always pass): CLI
  `encode --msc-accept-device 3 --msc-accept-format 0x10`, a GUI device selector,
  `EncoderConfig::msc_filter` (`MscFilter`), `transport::midi_input_filtered`. The default accepts
  everything, as before; the first filtered cue is logged.
- Spec (`docs/aml-v1-spec.md`, revision r4): §4.1 link key, §4.7 wrong-key count, §5.3 fail-safe
  per profile, explicit Note Offs, burst switch and "a suppressed duplicate re-arms link-dead", §5.4
  TX filter, §5.5 tiers / Note On limit / refresh schedule / controller refresh, §6 and §6.5 timer
  rows, §8 metrics. `cricket loopback --legato` plays overlapping notes (the previous note is
  released 60 ms after the next strike) for the refresh measurements.

#### Stage safety and profile names

- An unknown profile name is an error (`PhyConfig::from_name` returns `Option`; the CLI lists the
  valid names, the GUI falls back to the default with a visible warning) instead of silently
  selecting `robust`.
- The transmit level of the MFSK and LTC-style profiles defaults to **−12 dBFS peak** (amplitude
  0.25, was 0.8) to leave headroom on the bus; `--tx-level-db` / the GUI "TX level" set it (−60 …
  0). The receiver is level-independent, so mixed versions still interoperate. `digital-bit-exact`
  is unchanged. The spec lists the level as a SHOULD.

### MFSK receiver and tone plan

- **Wire format (MFSK profiles, spec r4):** the tone step is now 1 / symbol duration, which makes
  the 16 tones orthogonal over a symbol: `analog-safe` and `analog-safe-repeat` use 166⅔ Hz steps
  (2.5–5.0 kHz, was 100 Hz / 2.5–4.0 kHz), `analog-fast` 333⅓ Hz (2.5–7.5 kHz, was 200 Hz /
  2.5–5.5 kHz). Both ends must be updated together. A tone now leaks −26 dB into its neighbours'
  detector bins instead of −5.3 dB, so the confidence margin on a clean line goes from ~5 dB to
  ≳ 20 dB; a ±1 % sample-rate mismatch between the ends (a misconfigured sample-rate converter) no
  longer kills the MFSK profiles (delivery at ±1 % and 6 dB SNR: 0–16 % → 100 % for all three);
  false preamble locks on random tones fall from 13 / 10 to 4 per minute for the Robust profiles;
  and `analog-safe` offers a misconfigured uncoded transmitter 9 instead of 53 CRC candidates per
  minute (one false message per ~1,660 instead of ~280 years). The price is bandwidth, now a MUST
  in the spec: the line has to pass 2.5–5.0 kHz (`analog-safe`, `analog-safe-repeat`) or 2.5–7.5 kHz
  (`analog-fast`); through a 3.4 kHz telephone-grade low-pass `analog-fast` still delivers 92 %
  (was 55 %), the other two 100 %.
- **Receiver (no wire change):** the MFSK receiver band-passes its input to the tone band
  (second-order Butterworth at 0.7 × the lowest and 1.3 × the highest tone) before any detection,
  so the tone-purity gate normalizes by in-band energy. The old full-band purity failed at −4.8 dB
  full-band SNR whatever the per-bin SNR, and any out-of-band energy (hum, speech or music bleed,
  hiss, pink noise) killed detection the same way. Uncoded bodies (`analog-safe-repeat`,
  `analog-fast`) keep the best guess for a symbol that fails the gate instead of abandoning the
  packet (the CRC-16 and the §4.7 checks decide; a rejected body rewinds the search like a failed
  preamble), and every gated body symbol re-centres the symbol clock with an early-late gate. The
  purity gate is 0.20 for all MFSK profiles (was 0.25 / 0.30), the lowest value at which white noise
  and music produce no phy lock (at 0.15, `analog-fast` locks 13 times a minute on white noise); the
  joint-acquisition "contradicting symbol" bar is the gate purity (was half of it). A NaN or
  infinite input sample is read as silence so it cannot poison the filter.
- Measured on the channel simulator (400 trials per cell, `tests/fec.rs` `mfsk_ab_table`; the
  held-out table with other parameters and seeds agrees in every cell, none got worse): 95 %
  delivery cliff in white noise `analog-safe` −3 → −10 dB, `analog-safe-repeat` −2 → −9.5 dB,
  `analog-fast` +0.5 → −5 dB; hum 100 Hz at +6 dB over the tones: 0 → 100 % for all three; pink
  noise at −6 dB SNR: 0 → 100 / 100 / 98 %; `analog-fast` through a noise gate with a 5 ms attack:
  0.2 → 99.5 %; 5 ms network packet-loss bursts twice a second: `analog-safe-repeat` 78 → 88 %. Of
  the gain, the input filter alone is worth 6 dB for `analog-safe` and 4 dB for `analog-safe-repeat`,
  the best guess another 2.5 dB for `analog-safe-repeat` and 1 dB for `analog-fast`, the orthogonal
  tones 2.5 dB for `analog-fast` and the whole ±1 % drift tolerance. False-trigger rates on white
  noise and music stay at zero, `analog-fast` on random MFSK tones at 82 locks per minute as before.
  Receiver CPU on a host (release, ns per input sample): silence 39 → 44 (`analog-safe`), 27 → 32
  (`-repeat`), 15 → 19 (`analog-fast`); traffic at 0 dB 60 → 70 / 60 → 72 / 55 → 69; worst case
  (back-to-back preamble tones) 901 → 978 / 709 → 795 / 646 → 766; random tones 166 → 82 for
  `analog-safe`. The filter is two biquads per sample, the tracking two single-tone Goertzels per
  body symbol.
- **Experimental profile `analog-safe-fast`** (`robust_fast`, listed with the advanced profiles):
  `analog-safe`'s preamble, Reed–Solomon body and joint acquisition on 4 ms symbols with 250 Hz
  orthogonal tones (2.5–6.25 kHz), 108 ms per message instead of 175.5 ms. Simulator: cliff −9 dB
  (`analog-safe` −10 dB), 30 ms dropouts 99 % (96 %), 60 ms dropouts 75.5 % (75 %), 2 % drift 99 %
  (6.5 %), 16 random-tone locks per minute (4). A 3 ms variant (78 ms per message) measured a −8 dB
  cliff and 83 % on 30 ms dropouts and was not kept. `cricket loopback`: mean ~170 ms, p95 ~220 ms
  NoteOn latency (`analog-safe` ~270 / ~350 ms, `analog-fast` ~82 / ~120 ms, `clean-line` ~21 ms,
  all unchanged by this work). Not in the spec and not the default; both ends must select it. If it holds up in the field it is the candidate to replace `analog-safe` as
  the default (same robustness, 38 % less airtime); it does not replace `analog-fast`, which stays
  twice as fast.
- Simulator (`cricket_core/tests/support`): program bleed (speech-like and music-like material at a
  level relative to the tones), a noise gate, a compressor, a limiter, pink noise, Dante-like bursty
  block loss, cascaded converters (steep anti-alias or slow roll-off) and a 16 kHz codec brick wall.
  `tests/fec.rs` gains the MFSK training table, a held-out table used only for final reporting, and
  a "distance to cliff" row (95 % AWGN cliff per profile) under every A/B table.
- Negative results, recorded so they are not retried blind: a purity gate of 0.15 buys another dB
  of cliff but makes `analog-fast` lock 13 times a minute on white noise; a Fast tone plan from
  2.0 kHz (2.0–7.0 kHz) gains 5 points on a 3.4 kHz line at 104 instead of 82 random-tone locks per
  minute; a higher confidence gate for `analog-safe` (4–6 dB) changes nothing in noise and loses
  delivery at 2 % drift; ±3 / ±6 sample argmax clock tracking equals the early-late gate on every
  cell (the early-late gate costs half the Goertzels).

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

#### Core fixes

- The LTC (BMC) receiver no longer goes silent for good after a NaN or infinite input sample (a
  glitching driver): such samples are treated as silence and the filters restart if they overflow.
- The TX scheduler bounds the whole tier-3 queue (Note On, Program Change, controllers, system
  common and held Note Offs) to 1 s of airtime, dropping the oldest messages beyond it (the 4
  protected Note Ons and held pedals excepted) and merging controller values a dropped message had
  kept apart; `TxScheduler::queue_dropped` counts the drops and `with_queue_budget` sets the
  bound. Pushes no longer scan the queue (spec §5.5).
- The MFSK receivers do not retry a lock attempt that fine-aligns to the start they just failed
  on: on preamble-like tones (a CPU worst case) the work per sample drops by about half (robust
  1550 → 840 ns, fast 1340 → 600 ns per sample on the host; a false body is read once on Fast, at
  most twice on Robust instead of four to six times), with delivery in the simulated channels
  unchanged.
- `LinkState::Locked` means the same on every phy: a body is being read, or one completed within
  the last second (`LOCK_HOLD_MS`); the MFSK and LTC receivers no longer report `Locked`
  indefinitely after the last packet.
- `CricketModemRx::reset` is a hard reset: it also clears the duplicate history, disarms link-dead
  (traffic before the reset cannot fire it), and clears `Dead` state and `last_seq`. A body whose
  type nibble is reserved now counts in `crc_fail`. `decode_samples` flushes a full repetition
  plus eight symbol periods, for every profile and sample rate.
- `BmcParams::new` clamps the bit rate to 300–100 000 bit/s (`bmc::BIT_RATE_RANGE`),
  `FecMode::Rs16` clamps `parity` to 1–8 (`FecMode::parity`), and `Rs16::new` returns
  `Err(BadParams)` instead of panicking, so no profile constructor panics on out-of-range
  parameters.

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

[Unreleased]: https://github.com/paksnukk/Cricket/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/paksnukk/Cricket/compare/7ce0be8...v0.2.0
[0.1.0]: https://github.com/paksnukk/Cricket/commit/7ce0be8
