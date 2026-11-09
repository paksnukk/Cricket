# Roadmap

Derived from the [November 2026 internal review](docs/review-2026-11.md). Dates are intentions, not
promises; the order is. Versioning: the whole workspace carries one version, the next tag is
**v0.2.0**, and **1.0.0 is reserved for "field test passed and API frozen"**.

## Sprint 1 — hygiene (this sprint)

- One version everywhere (0.2.0), crate metadata, `#![forbid(unsafe_code)]`, unused dependencies out,
  original icon, Keep-a-Changelog, spec status header and versioning policy, docs index and history,
  scaffolding (CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, NOTICE, templates, CODEOWNERS, Dependabot),
  CI matrix, release workflow, supply-chain checks.
- Stage-safe defaults and user-facing profile names (review B1–B4, B8–B10, C7): unknown profile is an
  error, test scale opt-in, fail-safe on Stop, −12 dBFS default TX level, channel selection, alarm
  list, dBFS meter and link traffic light, CLI status line / `--json` / `cricket profiles`.
- Bugs confirmed by the review (A1–A7), landed by the core work: NaN-proof LTC receiver, bounded TX
  queue, hard reset, `crc_fail` accounting, non-panicking constructors, fewer lock re-attempts on
  preamble-like input; see the Unreleased changelog.

## Sprint 2 — protocol improvements → tag v0.2.0

Wire-affecting changes land **before** the first tag, under the spec's policy "before the first tagged
release a revision bump is enough" (spec header). Each step is measured in the simulator.

- Purity measured relative to the bin / band-limited instead of full-band energy (review C1).
- NoteOff refreshes spread exponentially, NoteOn precedence (C2).
- Link key as CRC seed + optional level squelch, so two Cricket links on one stage cannot accept each
  other's packets (C3; wire change).
- Chord / overload policy per profile (C4), fail-safe timing per profile (C6), best-guess decoding of
  uncoded packets (C8), MSC ahead of NoteOff (C9).
- Simulator realism: programme material, dynamics processors, bursty loss, held-out test set (C10);
  a hard per-second cap on lock attempts if the field test still shows CPU pressure on preamble-like
  input (A7 follow-up).
- Possibly orthogonal tone spacing 1/T (C5) — only with a measured gain, and before the tag.
- Then: **tag v0.2.0**, first binaries on the Releases page, spec revision r4 with the wire changes.

## Sprint 3 — field test

As soon as hardware is available. Logging per symbol and per packet to CSV first, then:

- Analog loop through two interfaces + 50 m + a console channel (gate/compressor on and off) at
  −2 / −12 / −20 dBFS; Dante hardware ↔ hardware and DVS ± SRC; 24 h soak with programme material;
  MIDI loop latency vs. buffer size; crosstalk at −20…−40 dB.
- Field reports via the issue template; measured chains replace the routing-guide stub in the README.

## Product track — HF profile family and the hardware box

- [x] `hf_robust` (`instrument`, 19.0–21.5 kHz) and `hf_fast` (`instrument-fast`, 8 tones at
  19.0–21.33 kHz) profiles with an 8th-order band-pass front end and purity measured after the
  filter: AML's link layer, scheduler and fail-safe unchanged; spec §6.6 (provisional), measurements
  in `docs/proposals/hf-instrument.md`, `cricket hf-mix` / `hf-scan` for recordings. Replaces the
  tone-signature "pedal" experiment, which stays outside AML.
- Measure the 19.5–24 kHz noise floor on real pedalboards and power supplies before building anything.
- ESP32-S3 / RP2350 firmware on `cricket_core`, an analog hard-wired pass-through with a high-impedance
  tap, acceptance criteria from the review (§F). Freedom-to-operate search before launch.

## Structure refactor (parallel, 1–2 weeks)

- Phy boundary: `PhyTx` / `PhyRx` traits, `Modulation::Custom`, per-phy parameters, builder,
  `PhyConfig::from_name → Result`, `BodyCoding` declared by the phy; `#[non_exhaustive]` on public enums.
- `cricket_sim` crate for the channel simulator; `pathcheck` behind a feature so firmware does not
  carry its formatting code.
- Engine pass-through node (audio in → process → audio out).
- Fuzz targets for packet parsing, Reed–Solomon, MSC and the receiver streams.
- Spec move to `docs/spec/aml-v1.md` once all links can be updated in one change.
- GUI: a MIDI monitor, counters for dropped / rejected messages, a non-null CSP once verified in
  `tauri dev` (profile names, traffic light, honest latency figures and the alarm list landed in
  Sprint 1).

### Chores

- Dependency majors deferred from Sprint 1 because of API changes: `cpal` 0.15 → 0.18 (removes the
  duplicate `alsa` 0.7/0.9 via `midir` 0.9 → 0.11), `ringbuf` 0.3 → 0.5 (closes RUSTSEC-2026-0293,
  harmless today since the rings hold `f32`), `tauri` 2.10 → current. `cargo tree -d` still shows two
  versions of `bitflags`, `getrandom`, `hashbrown`, `indexmap`, `syn`, `thiserror`, `toml_edit` and
  three of `phf*`, all inside the Tauri/GTK and ALSA stacks.
- `cargo about` for a THIRD-PARTY notice in the bundles.
- Delete merged feature branches.

## 1.0 criteria

1.0.0 is tagged when **all** of these hold:

1. The field gate has passed on at least two real chains (one analog, one digital) with ≥ 30 min
   without a false NoteOn, level offsets of ±12 dB, a > 2 s dropout ending in Link Dead + All Notes
   Off, and resync within 1 s after the signal returns.
2. The spec's stable sections are frozen (version nibble `1`), §6.5 is either adopted or removed, and
   the errata list is empty or folded into a revision.
3. The public API of `cricket_core` and `cricket_engine` has been stable for one minor release.
4. Signed and notarised desktop builds are produced by the release workflow.
