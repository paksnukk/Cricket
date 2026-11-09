# Cricket — desktop app

The Tauri 2 desktop front end of [Cricket](../README.md) (MIDI over audio, AML v1): encoder and
decoder side by side, live link stats, alarms, Panic and a self-test. What it does, which profile to
pick and how to route the audio channel are in the [root README](../README.md); the protocol is in
[`docs/aml-v1-spec.md`](../docs/aml-v1-spec.md).

## Requirements

- Rust toolchain (1.87+) and Cargo
- Node.js 20+ (18 works) and npm
- The [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS; on Linux also
  `libasound2-dev` for the audio/MIDI backends

## Run (development)

```sh
cd cricket_gui
npm install
npm run tauri dev
```

## Build (release bundles)

```sh
cd cricket_gui
npm install
npm run tauri build
```

Bundles land under `target/release/bundle/` of the workspace (`.dmg`/`.app`, `.msi`/`.exe`,
`.AppImage`/`.deb` depending on the OS). The [release workflow](../.github/workflows/release.yml)
builds and signs them on tags; see [`docs/release-process.md`](../docs/release-process.md).

## Notes

- The version shown in the window header comes from the package version (`tauri.conf.json`, kept
  equal to the workspace `Cargo.toml` by `.github/scripts/check-versions.sh`).
- `app.security.csp` is `null` for now: a Content Security Policy has to be verified with `tauri dev`
  (HMR) and the IPC bridge before it is enabled (tracked in `ROADMAP.md`).
- Icons: `src-tauri/icons/icon.svg` is the source; the PNG/ICNS/ICO set is generated with
  `npx tauri icon` (see the release process).
- `Info.plist` adds `NSMicrophoneUsageDescription`; `Entitlements.plist` grants audio input to signed
  builds.
