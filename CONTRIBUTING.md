# Contributing to Cricket

Thanks for helping. Cricket is the reference implementation of the open AML v1 protocol (MIDI over
audio); DPM Studios maintains it and builds products on top of it. Bug reports, field reports,
measurements and code are all welcome.

## Before you start

- Read the [README](README.md) and, for anything protocol-related, [`docs/aml-v1-spec.md`](docs/aml-v1-spec.md)
  including its versioning policy at the top.
- Look at [`ROADMAP.md`](ROADMAP.md) and the open issues; for anything larger than a fix, open an issue
  first so the design can be discussed before the code.
- Field reports (what chain, which profile, what the link stats said) are the most valuable
  contribution right now: use the [field report template](.github/ISSUE_TEMPLATE/field-report.md).

## Developer Certificate of Origin

Contributions are accepted under the [Developer Certificate of Origin 1.1](https://developercertificate.org/).
Sign off every commit to certify that you wrote the change or have the right to submit it under the
project's licenses (MIT OR Apache-2.0):

```sh
git commit -s
```

adds `Signed-off-by: Your Name <you@example.com>`. Unless you say otherwise, your contribution is
dual-licensed as the rest of the project (see the License section of the README).

## Building and checking

```sh
cargo build --workspace
cargo test --workspace                                      # no hardware needed
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --exclude cricket_gui
cargo build -p cricket_core --no-default-features --target thumbv8m.main-none-eabihf
cargo deny check                                            # licenses, advisories, duplicates
(cd cricket_gui && npm ci && npx tsc --noEmit && npm run build)
```

CI runs the same (see [`.github/workflows/ci.yml`](.github/workflows/ci.yml)); a pull request must be
green. Linux needs `libasound2-dev`; the GUI additionally needs the
[Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/). The minimum supported Rust
version is in the workspace `Cargo.toml` (`rust-version`).

Long measurement harnesses are `#[ignore]`d; run one with
`cargo test --release -p cricket_core --test <file> -- --ignored --nocapture` and paste the table into
the relevant proposal when you change something it measures.

## Pull requests

- Small, focused commits in [Conventional Commits](https://www.conventionalcommits.org/) style
  (`fix(core): …`, `feat(engine): …`, `docs: …`, `ci: …`), each signed off.
- Add or extend a test for behaviour changes; the channel simulator in `cricket_core/tests/support`
  covers most protocol-level cases without hardware.
- Add a line to the *Unreleased* section of [`CHANGELOG.md`](CHANGELOG.md) under Breaking / Added /
  Changed / Fixed / Docs, written for users ("what changed and why it matters"), not for the diff.
- Keep `#![forbid(unsafe_code)]`: if you believe a change needs `unsafe`, open an issue first.
- Fill in the [pull request template](.github/PULL_REQUEST_TEMPLATE.md).

## Changing the protocol

Anything that changes what goes on the wire, or what a receiver delivers, is a spec change:

1. Open an issue describing the change, the problem it solves and how it will be measured.
2. Measure it in the channel simulator (A/B against the current behaviour; the harnesses in
   `cricket_core/tests` are the pattern) and put the table in a proposal under `docs/proposals/`
   with a *Status* line (experimental → provisional → adopted).
3. Update `docs/aml-v1-spec.md` in the same pull request as the code, add a row to the revision log,
   and regenerate the test vectors in §9 if they change (`cricket_core/tests/spec_vectors.rs` must
   pass). Follow the versioning policy in the spec header: before the first tagged release a revision
   bump suffices; after it, wire-incompatible changes to stable sections mean a new protocol version,
   and provisional sections may change with a revision plus a minor crate bump.
4. Errata are appended to the spec's *Errata* list, never edited in silently.
5. Write the CHANGELOG entry as **Breaking (wire format)** when both ends must be updated together.

## Adding a profile

A profile fixes the physical layer and the body coding; the packet format and the transport layer are
shared. To add one:

1. Parameters: add a constructor in `cricket_core/src/phy/config.rs` (next to `PhyConfig::robust()`
   and friends) and a name in `PhyConfig::from_name`. Use the existing modulation types where you can
   (MFSK-16 or biphase-mark); a new modulation means a new module under `cricket_core/src/phy/`
   plus its selection in `cricket_core/src/phy/select.rs` and the modem.
2. Body coding: choose uncoded (interleaved) or Reed–Solomon (`FecMode`); the `_fec` naming convention
   marks coded variants.
3. Tests: a round-trip through `cricket_core/tests/support` (offsets, chunk sizes, gain, noise,
   drift, low-pass, corruption), the stuck-note test, and a false-accept measurement if the profile
   changes what a receiver can mistake for a packet.
4. Expose it: the CLI help and the GUI profile selector list profiles by name; add the hint text.
5. Document it: a proposal with the measurements, a README row with **measured** loopback latency
   (`cricket loopback --profiles <name>`), and a spec section only once it is at least provisional.

## Reporting security issues

See [`SECURITY.md`](SECURITY.md). Please do not open public issues for vulnerabilities.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
