# OSS hygiene / structure review (agent report, 2026-11-08)

Note: GitHub repo already redirects paksnukk/AML → paksnukk/Cricket (301). Local remote still old name (works via redirect).

## P0
1. Version story incoherent: crates 0.1.0; GUI 1.0.0 (3 places) + "Cricket MIDI v1.0" hard-coded index.html:160; CHANGELOG only "1.1.0 (unreleased)", no 1.0.0 section though commit 7ce0be8 (2026-07-29) "Release AML v1.0"; no git tags; CHANGELOG:16,19 still say aml_cli/aml_core.
2. Wire-version policy undefined: PROTOCOL_VERSION 0x1 unchanged across 3 wire-breaking changes (SEQ 2-bit, whitening, Midi type). Spec has no version/date header, no revision numbering, no errata policy; §7 titled "Release assumptions (v1.0)".
3. Remote points at old name; stale remote-tracking refs (prune).
4. Missing OSS scaffolding: CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, issue/PR templates, CODEOWNERS, dependabot.

## P1
5. Tauri placeholders: all 16 icons default Tauri logo; authors=["you"]; index.html links /vite.svg (404); unused assets; csp null; withGlobalTauri true; no publisher/copyright/licenseFile.
6. Unused deps: hound in cricket_cli; tauri-plugin-opener + capability; serde/serde_json only for phy_legacy.
7. Branches: claude/digital, fec, ltc, ltc-fast merged → delete; claude/tender-johnson-pfysx4 unmerged (pedal). ltc-bmc.md:183 leaks branch name.
8. .gitignore bug: last line `.env.production.local.DS_Store` (missing newline) → neither works; .claude/ not ignored.
9. Orphan constants/symbol-definition.json.
10. Unbounded queues under MIDI floods: encoder.rs:47 PendingQueue unbounded VecDeque; scheduler.rs:204 normal_q only drops NoteOns → PC/Bank/NoteOff/MSC floods grow memory+latency. Add cap + alarm.
11. rustdoc fails with -D warnings: bmc/rx.rs:338, engine lib.rs:30, loopback.rs:194 ×2.

## P2
12. Panics by invariant violation: bmc/rx.rs:242, bmc/tx.rs:17 (constructor panic on wrong Modulation → Result), rx.rs:728 expect, pathcheck.rs:905 unreachable, 15× lock().unwrap() in engine memory.rs. No unsafe → add #![forbid(unsafe_code)].
13. Input validation good; no fuzz targets.
14. cricket_core::pathcheck (34 KB, String/format!) compiled into no_std firmware build → feature-gate.
15. from_name silent fallback (dup of UX #1).
16. Stale docs release-checklist.md, cricket_gui/README.md; CI actions pinned by tag not SHA.

## Proposed layout
Public repo: cricket_core (features std, legacy, pathcheck), NEW cricket_sim (ChannelSim from tests/support 444 lines + pathcheck simulate), cricket_engine, cricket_cli, cricket_gui, docs/{spec/aml-v1.md, proposals/, history/}.
Private repo cricket_product: cricket_embed (new phy), cricket_fw (ESP32/RP2350), box app; depend on core via git tag until API settles, then crates.io. crates.io free: cricket_core, cricket_engine, cricket_cli, cricket_sim, cricket_embed; taken: cricket, aml.

Core API boundary not clean for external phy: Modulation closed enum (defined in bmc/mod.rs:87!), select.rs match dispatch; modem.rs special-cases Digital in ≥6 places; PhyConfig flat pub struct not non_exhaustive. Recommend: PhyTx/PhyRx traits, Modulation::Custom(Box<dyn PhyFactory>), per-phy params in variants, #[non_exhaustive] + builder, from_name → Result, BodyCoding declared by phy. For embed: engine needs duplex pass-through node (audio in → embed/extract → audio out); AudioIn/Out::Custom exist but no processing hook.

## CI / release plan
Current: one Ubuntu job (fmt, clippy, test workspace incl. Tauri → GTK/WebKit install), thumbv8m core build, tsc. Proposed matrix: lint (fmt, clippy all + no_std + testing feature, rustdoc -D warnings); test on ubuntu/macos-14/windows (--exclude cricket_gui, + release core once); no_std (thumbv8m + riscv32imac + thumbv6m); msrv (rust-version ≥1.82); gui build 3 OS; supply-chain (cargo deny, audit weekly, dependabot cargo+npm+actions, pin SHAs). Release on tag v*: CLI linux-x64/aarch64, macos-universal, windows-x64 + sha256; tauri-action dmg/msi/AppImage/deb; signing: Apple Developer ID + notarization secrets; Windows Authenticode or unsigned initially; cargo-about THIRD-PARTY.md.
Versioning: [workspace.package] single version; tag current as v0.2.0 (not 1.1.0); add missing history section; reserve 1.0.0 for field gate passed + API frozen (v1.1-plan D6 still open). Spec: header "AML v1, revision 2026-11 (r3)", change log inside spec, rule: after first tagged release any wire-incompatible change bumps header nibble; provisional sections may change with spec revision + crate minor; errata appended.

## Docs cleanup
project-plan.md, aml-v1-plan.md, aml-prototype-protocol.md → docs/history/; v1.1-plan.md → history after D6, replace with ROADMAP.md; release-checklist.md → release-process.md; aml-v1-spec.md → docs/spec/aml-v1.md with header/changelog/errata, rename §7; proposals keep + status table in docs/README.md, remove branch ref, split ltc-bmc §10; delete constants/; shrink cricket_gui/README.md; CHANGELOG add 0.1.0 section, fix names, Keep-a-Changelog; new CONTRIBUTING (DCO), CODE_OF_CONDUCT, SECURITY, docs/README.md, architecture.md, firmware.md, ISSUE_TEMPLATE/field-report.md. Align README "MIDI over audio" vs spec title "Bitstream Modem".

## Licensing/IP
MIT OR Apache-2.0 fine for proprietary layering; add DCO not CLA; 74/82 commits authored Claude → add NOTICE/AUTHORS naming DPM Studios. Trademark: "Cricket" crowded; "Cricket MIDI" + "AML" safer. Deps: no GPL/AGPL; MPL-2.0 ×5 (fine unmodified), r-efi tri-license, Unicode-3.0, LLVM-exception; npm all permissive; add cargo-about.

## Deps
cpal 0.15.3 → 0.18.2; midir 0.9.1 → 0.11.1 (dup alsa 0.7/0.9); ringbuf 0.3.3 → 0.6.0 (0.4 API rewrite); clap fine; tauri 2.10 → 2.12.5; dirs 5 (+6 via tauri) → 6; vite 6.4 → 7; no rust-version (set ≥1.82). Manifests lack description/repository/readme/keywords/categories.

## Fuzzing
cargo fuzz targets: decode_packet_bytes, fec candidate/decode_body, Rs16::decode, parse_sysex, midi_input, from_name, pathcheck::analyze, stream fuzzers for BmcRx/ModemRx/DigitalRx push_samples (no panic, bounded buffer — demod.rs:114 buffer drained but no hard cap). Nightly 10 min.
