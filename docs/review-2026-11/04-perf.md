# Protocol & DSP performance review (agent report, 2026-11-08)
MC = numpy check in scratchpad/review-perf/mfsk_check.py

## MFSK
- [HIGH] Purity gate sets the noise floor, not detection. tone_purity = P_k/(E·N/2) normalised by FULL-BAND energy, gate 0.25 ⇔ −4.8 dB full-band SNR. MC N=288: at −6 dB correct tone wins 100 %, confidence ≥1.5 dB passes 99 %, purity ≥0.25 passes 5 %; −8 dB: 99.7 % correct, 0 % pass. Per-bin SNR at −6 dB is 15.6 dB → receiver throws away symbols with 15 dB margin. Explains cliffs (97 % at −3, 40 % at −4). Any out-of-band energy (hum, crosstalk, speech bleed, hiss) same effect. Fix (RX-only): bin-relative contrast P_best/median(other 15 bins) or band-pass 2.3–4.2 kHz before energy term; re-tune joint thresholds. Expected +4–6 dB AWGN and out-of-band immunity.
- [MED] Tone spacing 0.6 bins → non-orthogonal (100 Hz × 6 ms; 200 Hz × 3 ms). Adjacent leakage −5.3 dB → clean confidence only 5.3 dB (3.7 at half-hop). 1/T spacing (166.7 / 333 Hz) → −26 dB. Wire change: Robust band 2.5–5.0 kHz. +2–3 dB, allows raising gates.
- [MED] Robust could be faster at equal robustness: Es/N0 at −3 dB cliff ≈18.6 dB/bin vs ideal 16-FSK ~12 dB → 5–6 dB headroom. "Robust-3ms" (3 ms symbols, 333 Hz, no guard, same preamble/RS) = 78 ms/message (−56 %). Measure 3/4 ms variants. 8-FSK net loss; 32-FSK not worth it.
- [MED] Uncoded bodies abort on soft gate (demod.rs:395-397 confidence <1 dB). Take best guess, let CRC decide. Fast cliff moves 1–2 dB. S.
- [LOW] No body timing tracking → Robust dies at ±1 % rate error; re-align each body window ±2 samples. TX peak 0.8 (−1.9 dBFS) hot: default −12 dBFS. Band needs ≥4.2 kHz (fails 3.4 kHz intercoms). −60 dBFS floor gives no crosstalk rejection.

## Link
- [HIGH] No link identity: any valid AML packet at any level ≥ −60 dBFS accepted. Two links in one multicore/Dante with −40 dB crosstalk → valid wrong MSC GOs. Fix: 16-bit link key as CRC init (crc.rs:3; zero wire bits) or XOR into WHITENING; optional level squelch. S.
- [LOW] RS(11,7)×2 right choice. Free gains: known-zero positions as known symbols; Chase re-decode (flip 1–2 weakest to runner-up). ~1 dB, M.
- [LOW] §9 analysis sound. Gaps: crosstalk; echo/replay > dedup window; MSC addressing: TX accepts any device ID/format, RX re-stamps → several MSC devices on one line misfire as all-call. TX-side filter. S.

## Transport
- [HIGH] Chords lose notes on Fast and Robust: budget 250 ms, MIN_KEPT 4 (scheduler.rs:21,24,518) → max 4 queued NoteOns; 5th drops oldest (6th note of chord drops 2nd struck, often bass). Per-profile policy: Fast ≥10 kept or drop newest; alarm per drop. S.
- [HIGH] NoteOff refresh: 6 back-to-back (REFRESH_MAX_REPEATS, spacing 0 except digital, scheduler.rs:235). 4-note release = 24 packets = 4.2 s airtime on Robust; NoteOns wait (~88 ms mean) — explains 269 vs 175.5 ms. Consecutive copies die in one dropout. Use exponential spacing (+1,+3,+8,+20,+50,+100 message times) all profiles; NoteOn pre-empts. −60–90 ms Robust legato latency. S.
- [MED] Fail-safe 500/2000 ms one-size (config.rs:121). LTC could use 100/500 (3 % duty); Robust idle duty 35 %, marginal link false-fires ~20 h. Per-profile; burst optional for MSC-only links. Content: CC123 ignored by several soft-synths/DAWs → send explicit NoteOffs for tracked notes before CC123. TX controller-state refresh (pedal/bend/mod) in idle slots. S–M.
- [LOW] MSC shares tier 2 with NoteOff in arrival order (scheduler.rs:156): chord release delays GO up to 4×175 ms on Robust. Put MSC ahead. Document Robust as cue/notes only (48 keys ≈ 8 s staleness).

## BMC
Good design. [MED] Not covered: noise gates (attack eats 1.7 ms preamble), fast compressors → low-level continuous idle fill (ltc-bmc §2.6) as opt-in; also <10 ms link-dead. [LOW] gates 0.75/0.3/0.6 simulator-tuned; log margins in field. BMC through transform codecs fails where MFSK survives — document.

## Digital
Well reasoned, fails safe. Untested killers: noise-shaped dither (POW-r/UV22 peaks several LSB16 → all frames fail; only flat TPDF simulated); DVS on Windows; aggregate/SRC (safe fail). Both K=2 copies in one Dante packet (42 samples) → space ≥64 samples (+1.3 ms). LSB hiding in program: feasible 4-LSB (−120 dBFS, 192 kbit/s) on 24-bit transparent path, sync sequence; any dither kills it.

## Cross-cutting
Robust 175.5 ms = preamble 26 + body 143 + silence 6.5; guard 13 ms (7 %); info ≈30 of 56 bits. Only symbol time moves needle. Robust/Fast carry +94/+25 ms queueing behind idle traffic. Device buffers unmeasured (+3–10 ms).
[HIGH] Simulator gaps: no program-material interference (speech/music bleed −20…−40 dB — most likely field failure for purity-gated RX), no gate/comp/limiter dynamics, no pink noise, no bursty Dante loss, no codec, no converter loop. Thresholds tuned on same cells/seeds → overfitting risk; keep held-out set, report distance-to-cliff.
Field test priorities: (1) analog loop 2 interfaces + 50 m + console strip (gate/comp on/off) at TX −2/−12/−20 dBFS with per-symbol purity/confidence/erasure + per-packet RS-correction CSV logging (missing observability); (2) Dante hw↔hw and DVS ± SRC, induced loss; pathcheck; (3) 24 h soak with program material; (4) MIDI loop latency vs buffer; (5) bleed −20…−40 dB.

## Spec
Code matches spec where checked. Ambiguities: §4.4 window reference (end sample); §5.3 suppressed duplicate re-arms link-dead; §5.5 state "≤4 queued NoteOns"; §3.1 symbol vs period; §6.5 bandwidth MUST per rate; §5.4 device ID semantics / multi-device unsupported; no MUST on TX level / RX level range.

## Top 10
1 Field test with per-symbol/packet logging (M). 2 Bin-relative/band-limited purity (S–M, +4–6 dB). 3 Link key via CRC init + squelch (S, wire). 4 Exponential NoteOff refresh + pre-emption (S). 5 Chord/overload per profile (S). 6 Orthogonal spacing + Robust-3/4 ms (M, wire, after 1/2). 7 Fail-safe per profile + explicit NoteOffs + controller refresh (S–M). 8 TX default −12 dBFS (S). 9 Uncoded best-guess + body timing + Chase (S/M). 10 Simulator realism + MSC first in tier 2 + continuous LTC option (M).
