## What and why

<!-- One paragraph: what changes for a user or a developer, and why. Link the issue. -->

## Kind of change

- [ ] Fix
- [ ] Feature
- [ ] Wire format / spec change (both ends must be updated; spec revision log updated; test vectors regenerated)
- [ ] Docs / CI / chore

## Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` pass locally
- [ ] Tests added or extended for behaviour changes (channel simulator where it applies)
- [ ] `CHANGELOG.md` *Unreleased* entry under Breaking / Added / Changed / Fixed / Docs
- [ ] Measurements, if any, are in a proposal under `docs/proposals/` and marked measured vs. estimated
- [ ] Commits are signed off (`git commit -s`, DCO)

## How it was tested

<!-- Commands, harness output, or the chain you ran it on (profile, interface, OS). -->
