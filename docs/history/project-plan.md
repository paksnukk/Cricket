# Project Plan: Cricket (Cricket MIDI)

> Historical planning document. "V1" here is the sequential-tone prototype (now the legacy
> prototype phy); "V2" became the shipped protocol, **AML v1** (see `docs/aml-v1-spec.md`).

## 1. Vision
Build a robust bridge that transports MIDI/MSC control data over audio infrastructure (analog XLR, Dante, MADI) by encoding events into short audio tone "symbols".

The system consists of two endpoints:
1. **Encoder (TX):** MIDI/MSC in → AML audio out
2. **Decoder (RX):** AML audio in → MIDI/MSC out

Primary goals (live environment):
- Stable, deterministic latency (low jitter)
- Robust decoding in real-world audio chains (gain changes, EQ, limiters)
- Safe behavior on link loss (panic / all notes off)
- Modular design to allow future protocol upgrades (V2 bitstream/FSK)

---

## 2. Core Design Principles
1. **Deterministic timing:** Fixed symbol duration and known frame structure.
2. **No amplitude-as-data (V1):** Avoid velocity/values relying on signal level.
3. **Known-frequency detection:** Decoder listens for a small set of specific frequencies (Goertzel recommended).
4. **Fast resync:** Each frame starts with a preamble.
5. **Fail-safe:** On stream break or timeout → send "All Notes Off" / "Panic".
6. **Modular architecture:** Separate protocol core, DSP, MIDI I/O, audio I/O, UI/CLI.

---

## 3. Technology Stack (Rust-first)
### 3.1 Language
- **Rust** for all real-time components (encoder/decoder core + audio processing)

### 3.2 Audio I/O
- **cpal** for cross-platform audio I/O (first choice)
- Alternative: PortAudio backend if needed later

### 3.3 MIDI I/O
- **midir** for MIDI input/output ports
- Virtual MIDI ports are OS-specific:
  - macOS: IAC Driver
  - Windows: loopMIDI (or similar)
  - Linux: ALSA seq

### 3.4 UI / Packaging
- **CLI first** (headless friendly, Raspberry Pi)
- **Tauri GUI** later (optional): device/port selection, meters, logs, modes

---

## 4. Protocol: V1 prototype (Sequential Tone Symbols)
### 4.1 What “symbols in sequence” means
A **symbol** is a short tone burst at a predefined frequency for a predefined duration.
A **frame** is a sequence of symbols (preamble + type + payload).

We do **not** play multiple data tones simultaneously (no “chords” for data in V1).
Everything is sent **one symbol at a time** for simpler and more robust decoding.

### 4.2 Frequency Registers (concept)
We separate information into “register bands” (exact numbers to be finalized in Spec.md):
- **PREAMBLE band:** one fixed frequency used only for sync
- **TYPE band:** small set of fixed frequencies defining event type
- **NOTE band:** 12 values (C..B)
- **OCTAVE band:** ~0..10 values
- **VALUE band:** generic value (e.g. velocity quantized, program number, cue index)
- **MSC band:** commands (GO/STOP/RESUME etc.)

### 4.3 Frame Structure (V1)
**Generic frame layout:**
1. PREAMBLE
2. TYPE
3. PAYLOAD A
4. PAYLOAD B (optional)
5. PAYLOAD C (optional)

Examples:
- NOTE_ON: PREAMBLE → TYPE_NOTE_ON → NOTE → OCTAVE → VEL (quantized)
- NOTE_OFF: PREAMBLE → TYPE_NOTE_OFF → NOTE → OCTAVE
- PROGRAM_CHANGE: PREAMBLE → TYPE_PC → PROGRAM
- MSC_LITE: PREAMBLE → TYPE_MSC → MSC_CMD → CUELIST → CUE_INDEX
- HEARTBEAT: PREAMBLE → TYPE_HEARTBEAT

### 4.4 Velocity Strategy (V1)
Velocity is **not transmitted as exact 0–127** in V1.
Instead, velocity is **quantized** (e.g. 16 levels: 0–15) to increase robustness and reduce required frequency resolution.

---

## 5. Latency Targets & Modes
We start conservative to ensure correctness, then optimize.

### 5.1 Measurable targets (initial)
- **Baseline V1:** reliable control transport with stable timing.
- Initial acceptable trigger latency target:
  - **Safe Mode:** ~30–80 ms end-to-end (depending on frame length)
  - Stable is more important than minimal at this stage.

### 5.2 Optimization path (planned)
- Reduce symbol duration and frame size
- Introduce **Fast Mode**:
  - Shorter symbol length
  - Reduced payload
  - Aggressive detection thresholds
- Potential later changes:
  - Smaller frequency set (fewer candidates)
  - Better windowing/ramping
  - Optional redundancy only when needed

---

## 6. Architecture
### 6.1 Crate layout (Cargo workspace)
- `cricket-core`
  - protocol (frame definitions, encoder/decoder state machine)
  - dsp (tone generator, Goertzel detector, gate/threshold, meters)
  - timing (symbol clock, buffering helpers)
- `cricket-io`
  - audio (cpal wrapper)
  - midi (midir wrapper)
  - config (serde + toml)
- `cricket-cli`
  - CLI app (TX/RX modes, logging, config)
- `cricket-gui` (optional, later)
  - Tauri UI + core integration

### 6.2 Decoder as a state machine
Decoder states:
- WAIT_PREAMBLE
- READ_TYPE
- READ_PAYLOAD_1..N
- EMIT_EVENT
- TIMEOUT → PANIC + back to WAIT_PREAMBLE

---

## 7. Milestones
### Phase 0 — Specification & Test Harness (must-have)
- [ ] Write `spec.md` for the V1 prototype (frequencies, symbol duration, frame formats)
- [ ] Implement offline generator → writes WAV test files
- [ ] Implement offline decoder → reads WAV and outputs parsed events
- [ ] Create golden tests: known frames must decode correctly

### Phase 1 — Encoder Proof of Concept (TX)
- [ ] Create Rust workspace + crates (`cricket-core`, `cricket-io`, `cricket-cli`)
- [ ] MIDI input: listen for NOTE_ON/NOTE_OFF
- [ ] Encode events into Cricket frames (symbol sequence)
- [ ] Tone generator with ramp (attack/release)
- [ ] Output audio via cpal
- [ ] CLI flags: select MIDI in/out, audio device, mode (safe/fast placeholder)

### Phase 2 — Decoder Proof of Concept (RX)
- [ ] Audio input via cpal
- [ ] Goertzel detection for candidate frequencies
- [ ] Frame sync using PREAMBLE + TYPE
- [ ] Build decoded events and output through virtual MIDI out
- [ ] Noise gate / threshold logic (avoid false triggers)
- [ ] Timeout + "All Notes Off" fail-safe

### Phase 3 — Reliability & Observability
- [ ] Heartbeat frame + link status (alive/dead)
- [ ] Logging and diagnostics:
  - detected symbols
  - confidence levels / thresholds
  - dropped frames
- [ ] Calibration tools:
  - input meter
  - suggested threshold based on noise floor
- [ ] Stress tests:
  - rapid note bursts
  - dropouts
  - gain changes
  - EQ/limiter simulation

### Phase 4 — MSC-lite + Program Change
- [ ] Add PROGRAM_CHANGE frames
- [ ] Implement MSC-lite:
  - GO / STOP / RESUME (extend later)
  - cue list + cue index as integers
- [ ] Validate end-to-end with target apps (QLab, lighting consoles)

### Phase 5 — Packaging
- [ ] Raspberry Pi setup:
  - systemd service for autostart
  - headless config + logs
- [ ] Optional Tauri GUI:
  - device/port selection
  - meters + link status
  - mode selection (safe/fast)
  - basic debug console

---

## 8. Test Plan
1. **Offline correctness**
   - WAV → decode → compare with expected events
2. **Loopback**
   - Local audio out → audio in
3. **Analog chain**
   - 30–100m XLR, different interfaces, different gain staging
4. **Network audio**
   - Dante Virtual Soundcard / hardware path if available
5. **Failure modes**
   - unplug input, clock drift, silence, noise bursts
6. **Latency measurement**
   - timestamp at MIDI in vs MIDI out, measure jitter and mean latency

---

## 9. Future Development (V2)
- Move from tone-symbol frames to a **bitstream** approach (FSK / MFSK)
- Add checksums / CRC and optional forward error correction
- Support full MIDI bandwidth:
  - CC, Pitch Bend, Aftertouch
  - SysEx (including full MSC payloads)
- Optional encryption/authentication if needed in hostile/noisy environments

---

## 10. AI Agent Rules (Prompt Context)
When working on this project:
1. Prioritize **stable low jitter** over extreme low latency (until Fast Mode phase).
2. Decoder must implement a threshold/noise gate to avoid false triggers.
3. Keep protocol core modular so V2 can replace V1 transport.
4. On stream break or timeout → send "All Notes Off" and reset decoder state.
5. All code must be written in **English** (comments, identifiers, logs).
