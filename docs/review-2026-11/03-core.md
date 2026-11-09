# Core/engine correctness review (agent report, 2026-11-08)

Runs: tests 183/0/14 ignored, wall 2m34 (44.7 s test time). pedantic clippy ≈750 (casts, doc_markdown…), no default lint fires. cargo doc 2 warnings (299 with missing_docs). audit/outdated/udeps/deny not installed.

## Bugs (confirmed by probe)
1. BMC receiver permanently poisoned by one non-finite sample (bmc/rx.rs:379-386): IIR state NaN forever. ltc/ltc_fec/ltc14400 5/5 → 0/5 after one NaN. MFSK & digital recover. Fix: sanitize `x.is_finite()` at top of push_one (or reset filter state).
2. TX scheduler has no overall queue bound (scheduler.rs:329-348, coalesce 357-375): coalescing stops at first discrete message of the channel; discrete non-NoteOn (PC, CC 0/6/32/38/96-101/121/122, system common) never dropped. Probe: CC7+NoteOn+PC at 1 kHz for 20 s on Fast → queued 39,653, oldest wait 20 s; push O(n)→O(n²). Spec §5.5 "bounded by coalescing" false. Fix: tier-3 total airtime cap (~1 s) drop oldest + flag; re-coalesce after NoteOn drop; fix spec.
3. crc_fail undercount (modem.rs:429-431): unknown type nibble returns false without crc_fail += 1.

## Likely bugs
4. CricketModemRx::reset() not hard (modem.rs:306-313): leaves body_symbols, recent, fail_safe armed, link_dead, last_seq.
5. Panicking public constructors: BmcParams::new(0)/ltc_with_bit_rate(0) → inf → overflow (rx.rs:270-274, 307); FecMode::Rs16{parity 0|>8} panics in Rs16::new (rs16.rs:563). Validate.
6. MIDI-out failures silent (decoder.rs:643, 687 `let _ = sink.send`); no hot-plug detection. Alarm once.

## Risks
7. Preamble-like input = CPU DoS on MCU (demod.rs:271-446): each matching hop triggers full body lock attempt then rewind by one hop. 5 s of A5C3… → robust 6.3× realtime debug x86, ~160 false locks/s. Mitigate: after rejected body skip full period or cap attempts/s (esp. Joint).
8. MFSK link_state stays Locked after confirm through long silence until link-dead (demod.rs:175-183; BMC rx.rs:794). Document or timeout like DigitalRx.
9. Wall-clock engine tests flaky risk (engine tests.rs:396; loopback_tests.rs:51-56).
10. Unsupported cpal formats fail hard (audio.rs:728-735, 813-820): I24/U24/U32/I8/U8 → error instead of fallback.
11. decode_samples flush 8 symbol periods (modem.rs:541) too short for BMC ≥9600 → offline helper can drop last message.
12. Direct ModemRx users: buffer grows unbounded in Verdict until confirm/reject (demod.rs:437); safe via CricketModemRx.

## Nits
expected_body_nibbles_for dead (clamp(16,16)); note_on_dropped pub field; no rust-version though is_multiple_of (1.87), repeat_n/is_none_or (1.82) used; all-pub PhyConfig/BmcParams; no #[non_exhaustive] on TransportMessage, MessageType, Modulation, RxEvent, ModemOutput, FecMode, MscCommand, MscError, PacketDecodeError, EngineEvent, AlarmKind; cpal input callback calls Thread::unpark (syscall in RT callback; acceptable); encoder ring size uses preferred rate before device rate known (encoder.rs:199-202).

## Verified OK
RS16 exhaustive probe 270,600 cases 0 failures (BM+Forney+Chien correct). Chunk invariance incl. reject/rewind for all profiles. 44.1/96 kHz MFSK decode OK (not in suite). Arithmetic u64 everywhere, synth phase resets per symbol, NaN safe in MFSK/digital. Dedup/SEQ fine. Validation paths fine (MSC cue 0.0/999.9/leading zeros/list 0/15/paths). Scheduler ordering: no path loses NoteOff. no_std: no std leaks, libm, no per-sample alloc in BMC/digital; MFSK 1 window Vec per body symbol/search hit (~154/s robust, ~333/s fast locked) + 3 small Vecs per push_samples. Engine threading/shutdown sound.

## Test-suite health
Deterministic seeds. Slowest: channel_robustness 11.5 s, digital 9.6 s, stuck_notes 5.2 s. 14 ignored harnesses never run in CI. Gaps: MFSK 44.1/96k, NaN input, reset mid-packet, scheduler long-run bound, decode_samples flush for fast BMC. CI missing cargo doc -D warnings, audit/deny, nightly ignored run.

## Top 10 fixes
1 NaN sanitize BMC; 2 queue bound + re-coalesce + spec §5.5; 3 crc_fail count; 4 cap false-lock cost; 5 complete reset(); 6 MIDI-out alarm; 7 tests (44.1/96k, NaN, reset mid-packet, scheduler bound) + nightly harnesses; 8 validate bit_rate/parity; 9 non_exhaustive + builder + dead code + rust-version; 10 loosen wall-clock tests + cargo doc in CI.
