# AML v1 Plan (Bitstream Modem) — New Forward Plan

> Historical planning document. During development this protocol was called "V2" and the
> sequential-tone prototype "V1"; the shipped protocol is **AML v1** and the prototype is
> the legacy prototype phy.

## 1. Why bitstream-first
The prototype protocol (sequential tone symbols) is a good pipeline test, but it is not the right physical layer if we want the system to remain usable through:
- mild/moderate EQ shaping
- mild/moderate compression/limiting
- level changes (gain staging differences, automation, operator mistakes)

Therefore, the bitstream modem (AML v1) becomes the “real” proof of concept: a robust audio modem that transports a framed bitstream with integrity checks and controlled latency.

## 2. Goals (non-negotiable)
- **Robustness**: tolerate moderate EQ/comp/level variation without frequent false events.
- **Deterministic latency**: stable, low-jitter event delivery (consistent decoding cadence).
- **Fast resync**: quick reacquisition after dropouts.
- **Fail-safe**: on link loss, emit safe MIDI behavior (All Notes Off / Panic) and reset decoder state.
- **Observability**: clear metrics (SNR/confidence, CRC pass rate, link status, jitter).

## 3. Constraints & assumptions
We will support two operational tiers:

### 3.1 “Pro Clean” (best latency)
- Dedicated bus is preferred but not strictly required.
- No extreme processing, but mild EQ/comp may exist.

### 3.2 “Venue / Processed” (best robustness)
- Can survive moderate EQ tilt and moderate compression/limiting.
- Trades latency and/or bandwidth for reliability (redundancy, stronger coding).

## 4. High-level architecture (bitstream-first, prototype optional)
We refactor the core so that Cricket is layered:

1. **Transport layer** (frames, message types, reliability policy)
2. **Link layer** (packetization, CRC, optional FEC, interleaving)
3. **Physical layer** (audio modem: modulation/demodulation)

The prototype can remain as a legacy physical layer implementation for debugging, but the plan assumes the bitstream modem is the default.

## 5. Physical layer proposal (recommended)
### 5.1 Modulation: MFSK with Goertzel bank (non-coherent)
**Why this choice**
- **Robust to unknown phase** and many real-world chain effects.
- **Works well with Goertzel** (already in our codebase and conceptually aligned).
- **Tolerates moderate compression** better than amplitude-based schemes.
- Simple to implement and tune in Rust in real time.

**Core idea**
- Each symbol transmits **one of N frequencies** (M-ary FSK).
- Each symbol encodes \( \log_2(N) \) bits.
- Decoder runs a Goertzel bank across those candidate frequencies and chooses the best bin.

**Recommended starting point**
- Sample rate: 48 kHz
- Symbol time: 4–8 ms (profile-dependent)
- Guard gap: optional 0–0.5 ms (only if needed for click/AGC behavior)
- N = 8 or 16 tones (3 or 4 bits per symbol)
- Tone spacing: conservative (e.g. 80–150 Hz) to reduce bin leakage and EQ ambiguity

### 5.2 Preamble + sync strategy
We must solve symbol boundary alignment explicitly.

**Packet preamble**
- A known pattern of tones (or a short chirp-like sweep) long enough to detect reliably.
- Used to:
  - detect “packet start”
  - estimate symbol timing/offset
  - optionally estimate coarse frequency offset (device clock mismatch)

**Sync approach**
- Sliding window energy + matched detection of the preamble sequence.
- Once locked, decode subsequent symbols on an internal symbol clock with limited drift correction.

### 5.3 Optional: BFSK fallback mode (ultra-robust)
For very hostile chains, BFSK (1 bit per symbol) is slower but extremely robust.
We can implement it as a profile/mode using the same pipeline and decoder.

## 6. Link layer: framing, integrity, redundancy
### 6.1 Packet framing
We define packets that carry a small payload with strong integrity:

- **PREAMBLE** (physical-layer) — for acquisition
- **SYNC WORD** (link-layer) — short fixed bit pattern to confirm lock
- **HEADER**:
  - version
  - payload length
  - message type
  - sequence number
- **PAYLOAD** (MIDI/MSC events, compact binary)
- **CRC** (CRC-16 or CRC-32)

### 6.2 Redundancy knobs (profiles)
We add profile-controlled redundancy:
- **Repetition**: repeat each packet K times (cheap, effective, increases latency)
- **Interleaving**: spread bits across time to survive bursts (helps with limiter pumping)
- **Simple FEC** (optional in first iteration):
  - start with parity / block code (very simple)
  - upgrade later if needed (e.g. convolutional/Viterbi or Reed-Solomon) once measurements justify it

Pragmatic order: CRC + repetition + interleaving first. Add true FEC only if required.

## 7. Transport layer: message model (what we send)
### 7.1 Event-based messages
We continue to be **event-based**, but now the payload is a binary message.

**Minimum set (POC)**
- Note On / Note Off
- Program Change
- MSC-lite (GO/STOP/RESUME/LOAD)
- Heartbeat / Link status
- Panic / All Notes Off

### 7.2 Safety policy
- If link dead (heartbeat missing or repeated CRC fails beyond timeout): emit **All Notes Off** and reset decoder state.
- If decoder loses lock: immediately drop to “search” mode and require a new preamble.

## 8. Latency targets & modes (AML v1)
We target stable latency first, then squeeze.

- **Robust Mode (default POC)**: ~20–60 ms end-to-end, high confidence, low false triggers.
- **Fast Mode (later)**: ~10–30 ms end-to-end, tighter tuning, less redundancy.

“Fast” is only enabled once Robust Mode is proven stable in real chains.

## 9. Concrete work plan (milestones mapped to current repo)
This plan assumes existing crates: `cricket_core`, `cricket_cli`, `cricket_gui` (Tauri).

### Milestone A — Spec + offline harness (must-have, AML v1)
- Add `docs/aml-v1-spec.md` (symbol timing, tone set, packet format, CRC choice).
- Implement offline TX: message -> samples -> WAV.
- Implement offline RX: WAV -> packets -> decoded messages.
- Golden tests: fixed WAV inputs must decode to known outputs.

### Milestone B — Physical layer (real-time)
- Create `cricket_core::phy` module:
  - tone table generation per profile
  - symbol synthesizer (windowed tone burst)
  - Goertzel-bank demodulator with sliding sync/preamble detection
  - confidence metrics (best vs runner-up, SNR estimate)

### Milestone C — Link layer
- Create `cricket_core::link` module:
  - packet encode/decode
  - CRC
  - repetition + interleaving (profile-controlled)
  - sequence numbers + heartbeat handling

### Milestone D — Transport (MIDI/MSC mapping)
- Create compact binary message format:
  - note events (channel optional later)
  - MSC-lite fields
- Ensure fail-safe policy is enforced in decoder output logic.

### Milestone E — Real-time CLI loopback and measurements
- `cricket_cli`:
  - loopback test mode: TX samples -> RX -> compare messages
  - latency/jitter measurement with timestamps
  - stress tests (bursts, dropouts)

### Milestone F — GUI observability (Tauri)
- Show:
  - link state (search/locked/dead)
  - CRC pass rate / drop rate
  - confidence/SNR meter
  - detected symbol timing drift (if tracked)

## 10. Success criteria (what “useful on riktigt” means)
POC is successful when:
- It can run for long periods without false Note Ons under realistic noise/processing.
- It recovers quickly after dropouts.
- It tolerates:
  - moderate level offsets (e.g. ±12 dB)
  - mild/moderate EQ shaping
  - moderate compression/limiting (not extreme distortion)
- It maintains stable latency (low jitter) within the selected mode.

## 11. Open design decisions (to be settled in the AML v1 spec)
- Tone band placement (avoid common audio content, avoid harsh EQ bands).
- MFSK order (8 vs 16 tones) vs symbol time trade-off.
- CRC choice (16 vs 32).
- Whether to add true FEC in POC or rely on repetition+interleaving.
- Channelization: mono only vs dual-mono redundancy.

