# AML Legacy Prototype Protocol (Sequential Tone Symbols)

> This is the original prototype protocol, kept in `cricket_core::phy_legacy` for debugging.
> The current protocol is **AML v1**, described in [`aml-v1-spec.md`](../aml-v1-spec.md).

## 1. Overview

AML (AudioMIDI Link) is a protocol for transporting MIDI and MSC control data over audio signal paths (analog XLR, Dante, MADI) by encoding events as short audio tone symbols.

AML is **event-based**, not sample-accurate audio data transport.
Audio is used purely as a transmission medium for a custom control protocol.

This document describes the **legacy prototype protocol**, optimized for:
- low and deterministic latency
- robustness in real-world audio chains
- simple decoding using known frequencies

---

## 2. Core Principles

1. **Sequential symbols**  
   Only one data symbol is transmitted at a time.  
   No simultaneous “chord-style” encoding is used in the prototype.

2. **Frame-based protocol**  
   All data is transmitted as frames:
  PREAMBLE → TYPE → PAYLOAD…

3. **Known-frequency detection**  
The decoder listens only for a predefined set of frequencies.
Goertzel detection is recommended.

4. **No amplitude-as-data (prototype)**  
Signal level does not carry information.
All values are encoded as frequency symbols.

5. **Fast resynchronization**  
Each frame starts with a PREAMBLE symbol.
Any decoding error causes a reset to WAIT_PREAMBLE.

6. **Fail-safe behavior**  
On signal loss or timeout, the decoder must issue:
- All Notes Off
- Decoder state reset

---

## 3. Symbols and Timing

### 3.1 Symbol Definition

A **symbol** is defined as:
- a sine tone at a specific frequency
- transmitted for a fixed duration
- with short attack/release ramps to avoid clicks

### 3.2 Timing (prototype defaults)

- Sample rate: 48 kHz
- Symbol duration: 6–10 ms (profile-dependent)
- Optional guard gap: 0.5–1 ms
- Attack / Release: 0.3–0.6 ms

Exact timing values are profile-specific (see Profiles).

---

## 4. Frequency Bands

The prototype separates information into frequency bands.
Each band uses fixed spacing to avoid ambiguity.

Typical bands in the prototype:

- PREAMBLE band (single fixed frequency)
- TYPE band (event type)
- NOTE band (pitch class 0–11)
- OCTAVE band (octave number)
- VALUE band (generic 4-bit values)
- MSC CMD band (MSC-lite commands)
- CUELIST band (MSC cue list index)

The exact frequencies are defined by the active profile.

---

## 5. Profiles

AML assumes **professional usage** by default:
- dedicated audio channel
- no EQ, compression, or dynamics processing
- stable gain staging

Profiles define:
- symbol duration
- detection thresholds
- frequency band placement

### Example Profiles

- **pro_clean**  
Optimized for clean digital or controlled analog paths.  
Lower latency, tighter thresholds, NOTE band may start as low as ~200 Hz.

- **noisy_analog**  
More tolerant mode for uncontrolled analog paths.  
Higher symbol duration and frequency spacing.

Only one profile is active at runtime.

---

## 6. Symbol Types

### 6.1 PREAMBLE

- Marks the start of a frame
- Has a unique frequency not used elsewhere
- Required before any frame is decoded

### 6.2 TYPE Symbols

TYPE symbols define how the following payload is interpreted.

Typical prototype types:
- NOTE_ON
- NOTE_OFF
- PROGRAM_CHANGE
- MSC_LITE
- HEARTBEAT
- PANIC

---

## 7. Frame Formats

### 7.1 NOTE ON
PREAMBLE
TYPE_NOTE_ON
NOTE        (pitch class 0–11)
OCTAVE      (0–10)
VALUE       (velocity, quantized 0–15)

### 7.2 NOTE OFF
PREAMBLE
TYPE_NOTE_OFF
NOTE
OCTAVE

### 7.3 PROGRAM CHANGE (prototype)
PREAMBLE
TYPE_PROGRAM_CHANGE
VALUE   (program number, quantized 0–15)

### 7.4 MSC LITE (prototype)
PREAMBLE
TYPE_MSC_LITE
MSC_CMD
CUELIST
VALUE   (cue index)

### 7.5 HEARTBEAT
PREAMBLE
TYPE_HEARTBEAT
Used to indicate link health. Absence triggers link-dead behavior.

---

## 8. Value Encoding

### 8.1 Quantized Values (prototype)

To improve robustness and reduce decoding time:
- Numeric values are quantized to **4 bits (0–15)** in the prototype.

Examples:
- Velocity: `round(velocity / 8)`
- Program number: `program >> 3`

Higher resolution may be added in later protocol versions.

---

## 9. Decoder State Machine

Typical decoder states:

1. WAIT_PREAMBLE
2. READ_TYPE
3. READ_PAYLOAD_N
4. EMIT_EVENT
5. RESET → WAIT_PREAMBLE

Timeouts or invalid symbols immediately reset the decoder state.

---

## 10. Fail-Safe Rules

- If the audio stream stops → send All Notes Off
- If heartbeat is missing beyond timeout → link dead
- Any incomplete or corrupted frame is discarded

---

## 11. Versioning and Future Work

The prototype focuses on:
- event-based control
- robustness
- predictable latency

Planned future extensions:
- Bitstream / FSK transport
- Checksums and error correction
- Full MIDI bandwidth (CC, Pitch Bend, SysEx)
- Higher-resolution value encoding

---

## 12. Summary

AML is a control protocol, not an audio codec.

It treats audio as a **transport layer** for structured symbolic events,
prioritizing determinism, safety, and live reliability over raw bandwidth.
