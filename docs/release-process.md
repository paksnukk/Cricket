# Release process

How a Cricket release is cut. The whole workspace carries **one version**; the next tag is `v0.2.0`,
and `1.0.0` waits for the field gate below. Nothing here requires a maintainer's laptop: the tag drives
the [release workflow](../.github/workflows/release.yml).

## 1. Decide the version

- Patch: fixes only, no wire change, no API change.
- Minor: new features; a change to a *provisional* or *experimental* spec section (spec header, policy
  item 3); any API addition.
- A wire-incompatible change to a *stable* spec section after `v0.2.0` is a new protocol version
  (AML v2) and a new major of the crates.

## 2. Bump the version (three places)

```sh
# workspace Cargo.toml → [workspace.package] version
# cricket_gui/src-tauri/tauri.conf.json → "version"
# cricket_gui/package.json → "version"  (then `npm install` to refresh package-lock.json)
.github/scripts/check-versions.sh      # fails if the three disagree
cargo run -p cricket_cli -- --version  # prints "cricket X.Y.Z"
```

## 3. CHANGELOG

Move the *Unreleased* entries into a new `## [X.Y.Z] — YYYY-MM-DD` section, grouped as
**Breaking / Added / Changed / Fixed / Docs**, and add the compare links at the bottom. The release
notes on GitHub are extracted from exactly that section by
[`.github/scripts/release-notes.sh`](../.github/scripts/release-notes.sh); run it locally to see them:

```sh
.github/scripts/release-notes.sh X.Y.Z
```

If the release changes the spec, add a revision to the spec's revision log in the same commit.

## 4. Verify

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --exclude cricket_gui
cargo build -p cricket_core --no-default-features --target thumbv8m.main-none-eabihf --release
cargo deny check
(cd cricket_gui && npm ci && npm run build)
```

CI runs the same on push; the weekly schedule also runs the long measurement harnesses
(`cargo test --release -p cricket_core -- --ignored`), whose tables feed the proposals.

## 5. Tag

```sh
git tag -a vX.Y.Z -m "Cricket vX.Y.Z"
git push origin vX.Y.Z
```

The tag triggers `release.yml`, which

1. creates a **draft** GitHub Release with the CHANGELOG section as notes;
2. builds the `cricket` CLI for Linux x64 and arm64 (native runners), macOS (universal binary via
   `lipo`) and Windows x64, strips them, and attaches `.tar.gz` / `.zip` archives with `.sha256` files;
3. builds the desktop app with `tauri-action`: `.dmg` (universal), `.msi` and NSIS `.exe`, `.AppImage`
   and `.deb`, and attaches them to the same release.

Review the draft, then publish it. Publishing a release that is still a draft is the last manual step.

### Signing and notarisation

The workflow signs and notarises when these repository secrets exist, and produces **unsigned** builds
when they do not (the step prints which ones were found):

| Secret | Used for |
|---|---|
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD` | Developer ID Application certificate (base64 `.p12`) and its password |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: DPM Studios (TEAMID)` |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | Notarisation (app-specific password) |
| `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Tauri updater signatures (only if the updater is enabled later) |

Unsigned macOS builds open with right-click → Open; unsigned Windows builds show the SmartScreen
prompt. Say so in the release notes when the secrets were absent. The bundle declares
`NSMicrophoneUsageDescription` (`cricket_gui/src-tauri/Info.plist`) and the audio-input entitlement
(`Entitlements.plist`), which a signed app needs to open an audio input.

### Icons

The desktop icon set is generated from `cricket_gui/src-tauri/icons/icon.svg`: render it to a
1024 × 1024 PNG (any SVG renderer; the shapes are plain enough to redraw with a script) and run
`npx tauri icon <png> -o src-tauri/icons` from `cricket_gui/`, then remove the `android/` and `ios/`
folders it adds. `cricket_gui/public/icon.png` (the favicon) is the 128 px output.

## 6. Field gate (required for 1.0.0, recommended for every minor)

The operational model is a **dedicated channel like LTC**: no EQ, compression or limiting on the
Cricket channel, gain set once.

| Test | Pass criteria |
|---|---|
| Dante / XLR / virtual 1:1, Robust | ≥ 30 min continuous without a false NoteOn |
| Level offset ±12 dB | Notes still decode; no stuck notes |
| Sample-rate change on the interface | App adapts or warns clearly in the log |
| Audio dropout > 2 s | Link Dead + All Notes Off on MIDI out |
| Resync after dropout | Valid packets within < 1 s after the signal returns |
| Fast (or an LTC profile) on a clean chain | Usable latency, no CRC storm; Robust works as fallback on the same chain |
| Operator view | Link state, alarms and the Panic button behave as the README describes |

Record the chain, profile, link stats and OS/interface with the
[field report template](../.github/ISSUE_TEMPLATE/field-report.md); the results go into the proposals
and replace the simulator-only figures in the README.
