# Cricket documentation

Cricket is the reference implementation of **AML v1** (AudioMIDI Link), MIDI over audio. Start with
the [root README](../README.md); this page lists everything under `docs/` and the status of each proposal.

## Normative

| Document | What it is |
|---|---|
| [`aml-v1-spec.md`](aml-v1-spec.md) | The AML v1 protocol: physical layers, packet format, FEC, validation rules, transport messages, profiles, test vectors. Has a status/revision header, a revision log, errata and the versioning policy at the top. |

The spec stays at `docs/aml-v1-spec.md` for now (code comments, tests and the proposals reference that
path); a move to `docs/spec/` is deferred until the links can be updated in one change.

## Proposals and their status

Proposals hold the design rationale and the measurements. Where a proposal and the spec differ, the
spec applies.

| Proposal | Covers | Status | In the spec |
|---|---|---|---|
| [`proposals/fec.md`](proposals/fec.md) | Reed–Solomon body FEC over GF(16), whitening, false-accept analysis, joint preamble acquisition, MFSK search speed-up | **Adopted** | §3.2.2, §4.6, §4.7, §6.1 (stable) |
| [`proposals/ltc-bmc.md`](proposals/ltc-bmc.md) | LTC-style biphase-mark phy (`ltc`, `ltc2400`, `ltc4800`, `ltc9600`, and `_fec` variants), soft-decision receiver | **Adopted as provisional** | §6.5 (provisional: may change with a revision + minor bump) |
| [`proposals/ltc-bmc.md`](proposals/ltc-bmc.md) §11 | `ltc14400`, `ltc14400_fec` for digital lines | **Experimental** | not yet (candidate for §6.5) |
| [`proposals/digital-transparent.md`](proposals/digital-transparent.md) | Bit-transparent `digital` phy, `+digital` decoder suffix, `cricket pathcheck`, engine latency audit | **Experimental** | not yet (candidate for §6.6) |
| [`proposals/hf-instrument.md`](proposals/hf-instrument.md) | HF (instrument-audio) profiles `hf_robust` / `hf_fast` (`instrument`, `instrument-fast`): MIDI on an instrument's own cable at 19–21.5 kHz, 8th-order receiver band, 3-bit alphabet, level / IMD budget, box sketch, `cricket hf-mix` / `hf-scan` | **Adopted as provisional** | §6.6 (provisional: may change with a revision + minor bump) |
| *pedal commands* (branch, not in `main`) | Tone-signature preset commands embedded in an instrument's own signal | **Experiment, not AML; superseded** | no; replaced by the HF profiles above (see `review-2026-11.md` §F and `proposals/hf-instrument.md` §9) |

Status words: **adopted** = normative text exists and is frozen under the spec's versioning policy;
**provisional** = in the spec but may change with a revision; **experimental** = code and
measurements exist, no spec text, no compatibility promise.

## Code

| Document | What it is |
|---|---|
| [`architecture.md`](architecture.md) | The crates and their layers, the engine's threads, the phy boundary (`PhyFactory` / `PhyTx` / `PhyRx`, `Modulation::Custom`), how another crate adds a modulation, and what of the API is meant to stay stable. |

## Process

| Document | What it is |
|---|---|
| [`release-process.md`](release-process.md) | How a release is made: version bump, CHANGELOG, tag, CI builds, field gate for 1.0. |
| [`../ROADMAP.md`](../ROADMAP.md) | The sprints ahead and the criteria for 1.0. |
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | How to contribute, including spec changes and new profiles. |
| [`../CHANGELOG.md`](../CHANGELOG.md) | What changed, per release. |

## Reviews

| Document | What it is |
|---|---|
| [`review-2026-11.md`](review-2026-11.md) | Internal review of `main` @ 829ab96 (2026-11-08) that produced the roadmap. Synthesis in Swedish; sub-reports in English: [01 UX / field](review-2026-11/01-ux.md), [02 OSS hygiene / structure](review-2026-11/02-oss.md), [03 core / engine correctness](review-2026-11/03-core.md), [04 protocol / DSP performance](review-2026-11/04-perf.md). |

## History

Planning documents kept for the record; they describe intent at the time, not the current state.

| Document | What it is |
|---|---|
| [`history/project-plan.md`](history/project-plan.md) | The original project plan (March 2026). "V1" there is the sequential-tone prototype. |
| [`history/aml-prototype-protocol.md`](history/aml-prototype-protocol.md) | The prototype protocol (semantic tone symbols), still available as `cricket_core::phy_legacy` for debugging. |
| [`history/aml-v1-plan.md`](history/aml-v1-plan.md) | The plan for the bitstream modem that became AML v1 (called "V2" at the time). |
| [`history/v1.1-plan.md`](history/v1.1-plan.md) | The "from v1.0 to a usable product" plan (October 2026) whose items are in the Unreleased changelog. |
