# Security policy

## What counts

Cricket turns audio into MIDI and MIDI Show Control commands. The threat that matters on a stage is a
**false or wrong message**: a GO that was never sent, a note that never ends, a crash of the decoder
while a show is running. We treat the following as security issues:

- A way to make a receiver deliver a message that was not transmitted (beyond the false-accept rates
  documented in `docs/proposals/fec.md` §9), or to make it deliver a different message than was sent.
- A way to crash, hang or exhaust the memory or CPU of the decoder or encoder with crafted audio or
  MIDI input (the `#![forbid(unsafe_code)]` crates should panic at worst; a panic in the audio path
  is still a bug we want to hear about).
- Supply-chain problems in the dependencies or the release artifacts.

Operational limitations that are documented (a packet from *any* AML v1 transmitter on the same line
is accepted; the profile is not authenticated) are design limitations tracked in `ROADMAP.md`, not
vulnerabilities, until a link key exists.

## Supported versions

Only the latest release on the `main` branch and the latest tag receive fixes. There is no long-term
support branch before 1.0.

## Reporting

Please **do not** open a public issue. Use GitHub's private vulnerability reporting
("Report a vulnerability" under the repository's *Security* tab), which reaches the maintainers in
`.github/CODEOWNERS`. Include the Cricket version (`cricket --version` or the app's header), the
profile, the operating system and, if possible, a WAV file or a `cricket pathcheck` / loopback log
that reproduces the problem.

You will get an acknowledgement within a week. Fixes are released as a patch version with a CHANGELOG
entry and, where applicable, a RustSec-style note in the release; we credit reporters unless they ask
us not to.

## Dependencies

`cargo deny` (licenses, advisories, duplicate versions) and `cargo audit` run in CI, and Dependabot
opens weekly updates. Advisories that are knowingly accepted are listed with a reason in `deny.toml`.
