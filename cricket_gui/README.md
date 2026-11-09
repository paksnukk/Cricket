# Cricket — desktop app

Cricket MIDI desktop app: carries MIDI / MSC-lite control over a **dedicated audio channel**
(XLR, Dante, MADI, virtual cable), similar in operational mindset to LTC.

## Requirements

- Rust toolchain + Cargo
- Node.js 18+
- macOS (primary release target)

## Run (dev)

```bash
cd cricket_gui
npm install
npm run tauri dev
```

## Build (release)

```bash
cd cricket_gui
npm install
npm run tauri build
```

Artifacts land under `cricket_gui/src-tauri/target/release/bundle/`.

## How to route (like LTC)

1. Patch a **dedicated** mono bus from the TX machine/console to the RX machine.
2. Do **not** put EQ, compression, limiting, or noise reduction on that bus.
3. Set gain once; leave it alone during the show.
4. In Cricket: pick Audio Out + MIDI In on the encoder side; Audio In + MIDI Out on the decoder side.
5. Prefer **Analog Safe** unless the chain is known-clean — then **Clean Line** (~20 ms) or, for
   notes only, **Analog Fast** (~80 ms).
6. **Stop** and **Panic** on the decoder send Sustain off + All Notes Off on all channels to MIDI out.

## Profiles

Named after the line they are for (the older names still work); see the main README for the table.

| Profile | When | Measured NoteOn latency (in-process) |
|---------|------|-----------------|
| Analog Safe (`analog-safe`, was `robust`) | Default / any bus you don't control | ~0.3 s |
| Analog Fast (`analog-fast`, was `fast`) | Clean dedicated analog line, notes only | ~80 ms |
| Clean Line (`clean-line`, was `ltc_fec`) | Dedicated line without processing | ~20 ms |
| Digital Line (`digital-line`, was `ltc14400_fec`) | Dante / MADI / AES at 48 / 96 kHz | ~7 ms |
| Digital Bit-Exact (`digital-bit-exact`, was `digital`) | `cricket pathcheck` passed | < 1 ms |

## Self-test

Use **Self-Test** in the UI (offline encode→decode in process) before going on air.

## CLI (lab)

```bash
cargo run -p cricket_cli -- test
cargo run -p cricket_cli -- loopback
cargo run -p cricket_cli -- list-devices
```

## Docs

- [AML v1 spec](../docs/aml-v1-spec.md)
- [Release / field checklist](../docs/release-checklist.md)
