---
name: Field report
about: You ran Cricket on a real audio chain. Tell us what happened, good or bad.
title: "[field] <chain> / <profile>: <one-line outcome>"
labels: field-report
---

<!-- Field reports are the most useful thing you can send right now: every number in the README
     comes from software measurements. A report that says "it just worked for 2 hours" counts. -->

## Chain

- Sending side (computer, OS, audio interface, driver / sample format, buffer size):
- Audio path between the ends (analog XLR length, Dante/AES67/MADI/AES3, mixer or router, any SRC,
  any processing on the channel — EQ, dynamics, gate, limiter — even "none" is useful):
- Receiving side (computer, OS, audio interface, driver / sample format, buffer size):
- MIDI source and destination (keyboard, console, QLab, lighting desk, …):

## Cricket

- Version (`cricket --version` or the app header):
- Profile on both ends:
- CLI or GUI, and the exact options / settings:
- Send level at the interface, and level seen at the receiving interface (dBFS if you have it):

## What you ran and how long

- Duration, traffic (notes, chords, controllers, MSC cues), anything unusual you did on purpose
  (dropped the cable, moved a fader, changed sample rate):

## Link stats from the decoder

<!-- GUI: the stats panel; CLI: the printed events. Paste as text. -->

- Link state over time (Search / Locked / Dead), sync-lost count, CRC pass / fail, confidence (dB),
  FEC repairs if shown:

## Outcome

- [ ] Everything arrived, no stuck notes, no false messages
- [ ] Stuck or missing notes (which, when)
- [ ] A message arrived that was never sent (what, when — this is the one we most want to hear about)
- [ ] Link dropped (how often, how long, did All Notes Off go out)
- [ ] Latency felt (acceptable / noticeable / unusable), and measured if you could
- Logs, WAV recordings of the line, or `cricket pathcheck receive` output are very welcome as
  attachments.
