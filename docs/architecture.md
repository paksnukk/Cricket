# Cricket architecture

How the crates fit together, where the threads are, what the phy boundary looks like, how a
modulation from another crate plugs in, and which parts of the API are meant to stay stable. For
the protocol itself read [`aml-v1-spec.md`](aml-v1-spec.md); this page is about the code.

## Layers

```text
 cricket_gui (Tauri)      cricket_cli                        ← front ends: thin shells over the engine
 ─────────────────────────────────────────────────────────────────────────────────────────────────
 cricket_engine            audio (cpal) · MIDI (midir) · decoder / encoder / pass-through workers
                           events · loopback measurement · in-memory endpoints (testing)
 ─────────────────────────────────────────────────────────────────────────────────────────────────
 cricket_core              modem      CricketModemTx / CricketModemRx: packets ↔ repeated bodies,
   (no_std + alloc)                   duplicate suppression, fail-safe, the digital-frame detector
                           transport  TransportMessage ↔ MIDI / MSC, TxScheduler, FailSafeMonitor
                           link       Packet, CRC-16 (link key), RS16 FEC + whitening, interleave
                           phy        PhyFactory / PhyTx / PhyRx  ←── the boundary this page is about
                                      ├── mfsk     (MFSK-16: robust, fast, the HF profiles)
                                      ├── bmc      (biphase mark, the LTC profiles)
                                      ├── digital  (bit-transparent frames)
                                      └── Modulation::Custom(Box<dyn PhyFactory>)  ← external crates
                           pathcheck  (feature `pathcheck`, std only) bit-transparency analysis
 ─────────────────────────────────────────────────────────────────────────────────────────────────
 cricket_sim (dev)         the channel simulator: impairments, instrument sources, converter and
                           link models, program material, drift, IMD helpers, chunked decoding
```

Dependencies point downwards only. `cricket_core` has no I/O and no clock: it is driven by the
samples it is given and counts them (`samples_pushed`), which is why every decoder decision is
reproducible from a WAV file and why the same crate runs in the desktop apps, the test harnesses and
on a microcontroller. `cricket_sim` depends on the core and is used by the core's own integration
tests (`cricket_core/tests/support` re-exports it), by the engine's tests and by whatever product
tests need a realistic line; it is `std`-only and never linked into a firmware.

### Features of `cricket_core`

| Feature | Default | Adds |
|---|---|---|
| `std` | on | WAV file I/O (`wav`), the legacy prototype phy (`phy_legacy`), `pathcheck` |
| `pathcheck` | on (via `std`) | `cricket_core::pathcheck`: the bit-transparency probe pattern, its analyzer and the `PathImpairment` path models. String-heavy report formatting that a firmware does not need; `--no-default-features` leaves it out. |

## Threads (engine)

Every worker is a `std::thread` owned by a handle; dropping the handle stops and joins it (at most
`STOP_TIMEOUT`). Workers report through an `mpsc` channel of `EngineEvent`s and never block on the
owner.

| Thread | Owns | Woken by | Does |
|---|---|---|---|
| `cricket-decoder` | one cpal input stream (or a custom source), the MIDI output, a `CricketModemRx` | the cpal input callback (`Thread::unpark`) as soon as samples are queued; otherwise every 5 ms | pulls samples, runs the modem, sends MIDI, emits `Decoded` / `Stats` / alarms; wall-clock watchdog for a stalled input; fail-safe burst on stop |
| `cricket-encoder` | one cpal output stream (or a custom sink), the MIDI input connection, a `CricketModemTx` and the `TxScheduler` | a condition variable signalled by the MIDI callback and `EncoderHandle::send`; otherwise every 5 ms or when the output ring drains to the refill threshold | encodes queued messages while less than 10 ms of audio is buffered ahead, heartbeats when idle |
| `cricket-passthrough` | one cpal input **and** one output stream (or custom endpoints), a user `AudioProcessor` | the input callback, as the decoder | pulls a block, runs the processor (`input → output`), pushes the result to the output ring; emits what the processor emits plus device and stall alarms |
| cpal callbacks | the device rings | the audio driver | copy one channel into / out of a lock-free ring; the input callback unparks its worker. No locks, no allocation. |

The pass-through worker is what a hardware box with an "instrument" HF receiver and a pass-through
audio path needs: the audio goes straight through (bit-exact with `Passthrough`), a copy feeds a
`CricketModemRx` (`TapDecoder`), and an optional strip stage removes the data band from the output.
Measured with the in-memory endpoints (`measure_passthrough_latency`, `passthrough_tests`): the
worker adds **0 samples** of delay (the block that comes in is the block that goes out) and turns a
block around in **~40–90 µs** (blocks of 64–1024 samples; worst case seen 0.3 ms on a loaded
machine), so on real devices the two device buffers set the latency, as for the decoder and
encoder. The strip filter for the instrument profiles is a 129-tap linear-phase FIR: ≤ 0.04 dB of
ripple below 17 kHz, −46.7 dB at 18 kHz, 56 dB of attenuation of a whole burst, and **64 samples
(1.33 ms at 48 kHz) of group delay** on the stripped output only. A processor that needs MIDI out
sends through `ProcessContext::send_midi`; the worker owns the port (opened in its own thread), so
processors stay `Send`.

## The phy boundary

`cricket_core::phy::traits` is the whole contract between the modem and a physical layer. Three
object-safe, `Send` traits:

```rust
/// The parameters of a modulation: build its ends and describe the frame to the modem.
pub trait PhyFactory: Debug + Send + Sync {
    fn name(&self) -> &str;                                   // "mfsk", "bmc", "digital", or yours
    fn clone_box(&self) -> Box<dyn PhyFactory>;
    fn tx(&self, config: &PhyConfig) -> Box<dyn PhyTx>;
    fn rx(&self, config: &PhyConfig) -> Box<dyn PhyRx>;
    fn body_coding(&self, config: &PhyConfig) -> BodyCoding;  // Padded16 | Fec(FecMode) | Raw
    fn frame_samples(&self, config: &PhyConfig, body_nibbles: usize) -> usize;
    fn gap_samples(&self, config: &PhyConfig) -> usize;
    fn symbol_duration_ms(&self, config: &PhyConfig) -> f32;
    fn refresh_policy(&self, config: &PhyConfig) -> RefreshPolicy;
    // with defaults: guard_gap_ms, bits_per_symbol, first_copy_ms, fixed_tx_level, needs_bit_exact_path
}

/// Transmit side: one on-wire burst per body.
pub trait PhyTx: Send {
    fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32>;
}

/// Receive side: a stream of samples in, events out.
pub trait PhyRx: Send {
    fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent>;
    fn link_state(&self) -> LinkState;
    fn last_confidence_db(&self) -> f32;
    fn samples_pushed(&self) -> u64;
    fn reset(&mut self);
    // with defaults: set_expected_body_nibbles, confirm, reject
}
```

and the enum that selects a phy, each variant carrying that phy's own parameters:

```rust
#[non_exhaustive]
pub enum Modulation {
    Mfsk(MfskParams),
    Bmc(BmcParams),
    Digital(DigitalParams),
    Custom(Box<dyn PhyFactory>),
}
```

`PhyConfig` holds what every profile has — sample rate, amplitude, `fec`, `repetition_k`, the
fail-safe timers, `interleave_depth`, `link_key`, `digital_detect` — plus the `Modulation`. The
built-in parameter structs implement `PhyFactory` themselves, so `config.modulation.phy()` is the
factory the modem dispatches through, whatever the variant.

What the modem does with it:

1. `CricketModemTx::new(config)` asks the factory for a `PhyTx` and for the `BodyCoding`. Per
   message it builds the packet bytes (header, SEQ, payload, CRC-16 with the link key), lays them
   out as the coding says — zero-padded to 16 nibbles and block-interleaved (`Padded16`), RS16-coded
   and whitened (`Fec`), or as they are (`Raw`) — and calls `synthesize_framed` `repetition_k` times
   with `gap_samples` of silence after each copy.
2. `CricketModemRx::new(config)` asks for a `PhyRx`, tells it the body length
   (`set_expected_body_nibbles`) and collects `RxEvent::BodyNibble`s after each
   `RxEvent::PreambleLocked`, each with its reliability and erasure flag for the FEC decoder. When a
   body is complete it validates the packet (spec §4.7) and calls `confirm` or `reject`; the MFSK
   receiver pauses until then and rewinds its search into the body on `reject`, so a packet that
   starts inside a false lock is not lost. A `Raw` phy reports `RxEvent::Frame`s instead, which go
   through the same validation.
3. Duplicate suppression, link-dead and the fail-safe burst, MIDI mapping and the TX scheduler's
   repeat policy (`RefreshPolicy`: the Fibonacci spread or consecutive slots, the spacing, whether a
   coded body waits for a quiet line) are the modem's and identical for every phy.

The modem never matches on `Modulation`. The only phy-specific code outside `phy/` is the
digital-frame side detector (`PhyConfig::digital_detect`), which is a modem feature: it runs the
digital phy's receiver next to any other phy so one receiver can serve a transmitter that switches
profiles.

## Adding a modulation from another crate

Nothing in `cricket_core` has to change. Implement the three traits on your parameter struct and put
it in `Modulation::Custom`:

```rust
use cricket_core::link::FecMode;
use cricket_core::phy::{
    BodyCoding, LinkState, Modulation, PhyConfig, PhyFactory, PhyRx, PhyTx, RefreshPolicy, RxEvent,
};

#[derive(Debug, Clone)]
pub struct ResidueParams { pub bits: u8 }

impl PhyFactory for ResidueParams {
    fn name(&self) -> &str { "residue" }
    fn clone_box(&self) -> Box<dyn PhyFactory> { Box::new(self.clone()) }
    fn tx(&self, config: &PhyConfig) -> Box<dyn PhyTx> { Box::new(ResidueTx::new(self.clone(), config)) }
    fn rx(&self, config: &PhyConfig) -> Box<dyn PhyRx> { Box::new(ResidueRx::new(self.clone(), config)) }
    // Use the link layer's body coding as the built-in phys do, or `BodyCoding::Raw`
    // if your frames carry the packet bytes themselves (then report `RxEvent::Frame`).
    fn body_coding(&self, config: &PhyConfig) -> BodyCoding { BodyCoding::from_fec(config.fec) }
    fn frame_samples(&self, config: &PhyConfig, body_nibbles: usize) -> usize { /* sync + body */ 8 + 4 * body_nibbles }
    fn gap_samples(&self, _: &PhyConfig) -> usize { 0 }
    fn symbol_duration_ms(&self, config: &PhyConfig) -> f32 { 4000.0 / config.sample_rate_hz as f32 }
    fn refresh_policy(&self, _: &PhyConfig) -> RefreshPolicy { RefreshPolicy::CONSECUTIVE }
}

struct ResidueTx { /* … */ }
impl PhyTx for ResidueTx {
    fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32> { /* one burst */ todo!() }
}

struct ResidueRx { /* … */ }
impl PhyRx for ResidueRx {
    fn push_samples(&mut self, samples: &[f32]) -> Vec<RxEvent> { /* PreambleLocked, BodyNibble…, or Frame */ todo!() }
    fn link_state(&self) -> LinkState { todo!() }
    fn last_confidence_db(&self) -> f32 { todo!() }
    fn samples_pushed(&self) -> u64 { todo!() }
    fn reset(&mut self) { todo!() }
}

// A profile that uses it: the common parameters from a built-in profile, your phy in place.
let config = PhyConfig::ltc_fec()
    .with_modulation(Modulation::Custom(Box::new(ResidueParams { bits: 2 })))
    .with_fec(FecMode::Rs16 { parity: 4 });
let tx = cricket_core::CricketModemTx::new(config.clone());
let rx = cricket_core::CricketModemRx::new(config);
```

The engine, the CLI and the GUI select profiles by name (`PhyConfig::from_name`, the `PROFILES`
catalog), so a custom phy is used through the library API — build the `PhyConfig` yourself and hand
it to `CricketModemTx` / `CricketModemRx`, or run it through the engine's workers from your own
front end. A phy that the project adopts gets a module under `cricket_core/src/phy/`, a variant and a
catalog entry (see [`CONTRIBUTING.md`](../CONTRIBUTING.md), "Adding a profile").

Rules a phy must keep: a NaN or infinite sample is read as silence (never poisons state); buffers
stay bounded by one packet plus the caller's chunk whatever the input; `frame_samples` agrees with
what `synthesize_framed` returns; the fuzz targets in `cricket_core/fuzz/` are the pattern for
checking both.

## What is stable

The crates are at 0.x: the API can still change with a minor bump, and the CHANGELOG says when it
does. Within that, the intent is:

| Stable (changes announced, deprecations first) | May change with a minor bump |
|---|---|
| The wire format (spec; stable sections frozen under the spec's versioning policy) | Fields of `PhyConfig`, `MfskParams`, `BmcParams`, `DigitalParams` (all `#[non_exhaustive]`: new fields appear, existing ones stay) |
| `CricketModemTx` / `CricketModemRx` and `ModemOutput` | Variants of the `#[non_exhaustive]` enums (`TransportMessage`, `MessageType`, `Modulation`, `RxEvent`, `ModemOutput`, `FecMode`, `MscCommand`, `MscError`, `PacketDecodeError`, `EngineEvent`, `AlarmKind`): match them with a wildcard arm |
| `PhyConfig::from_name` and the names, aliases and tiers in `PROFILES` | Default methods on the phy traits (new ones may appear) |
| The three phy traits' required methods and `BodyCoding` | The receiver tuning parameters inside the param structs (not wire format) |
| `TransportMessage` as a data type (new variants only when the spec grows) | `cricket_engine` internals; the engine's public surface is the handles, configs and events |

Tests pin the wire format (`cricket_core/tests/spec_vectors.rs`), the profile catalog
(`cricket_core/src/phy/config.rs` tests) and every A/B measurement in `cricket_core/tests`; a
structural change that moves any of those numbers is a bug.
