use crate::link::{
    bytes_to_nibbles, decode_packet_bytes, deinterleave, encode_packet_bytes,
    fec_candidate_is_valid, interleave, nibbles_to_bytes, BodySymbol, FecCodec, Packet,
    SEQ_MODULUS,
};
use crate::phy::digital::{self, DigitalFrame, DigitalRx};
use crate::phy::{LinkState, Modulation, PhyConfig, PhyRx, PhyTx, RxEvent};
use crate::transport::{FailSafeMonitor, MessageType, MidiOutput, MscTarget, TransportMessage};
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// Fixed interleaved body size so RX can lock without knowing the type early.
pub const MAX_BODY_NIBBLES: usize = 16;

#[derive(Debug, Clone)]
pub struct ModemStats {
    pub link_state: LinkState,
    /// Bodies that held a valid packet (duplicates included).
    pub crc_pass: u64,
    /// Bodies that completed but held no valid packet: CRC, version,
    /// type (a reserved type nibble), SEQ or payload check failed, or the
    /// FEC decoder found no candidate. The sum with `crc_pass` is the
    /// number of completed bodies.
    pub crc_fail: u64,
    pub last_confidence_db: f32,
    pub last_seq: Option<u8>,
    /// Body symbols repaired by FEC (0 unless `PhyConfig::fec` is enabled).
    pub fec_corrected: u64,
}

#[derive(Debug, Clone)]
pub enum ModemOutput {
    Message(TransportMessage),
    Midi(Vec<u8>),
    LinkDead,
    SyncLost,
    Stats(ModemStats),
}

/// High-level TX: transport message → repeated interleaved audio (MFSK, the
/// BMC (LTC) phy, or the digital phy, per `config.modulation`).
pub struct CricketModemTx {
    phy: PhyTx,
    config: PhyConfig,
    seq: u8,
    fec: Option<FecCodec>,
}

impl CricketModemTx {
    pub fn new(config: PhyConfig) -> Self {
        let phy = PhyTx::new(config.clone());
        let fec = FecCodec::new(config.fec);
        Self {
            phy,
            config,
            seq: 0,
            fec,
        }
    }

    pub fn config(&self) -> &PhyConfig {
        &self.config
    }

    /// SEQ for the next packet: 0, 1, 2, 3, 0, … (spec §4.1).
    pub fn next_seq(&mut self) -> u8 {
        let s = self.seq;
        self.seq = (self.seq + 1) % SEQ_MODULUS;
        s
    }

    pub fn encode_message(&mut self, msg: &TransportMessage) -> Vec<f32> {
        let seq = self.next_seq();
        self.encode_message_with_seq(msg, seq)
    }

    /// Encode `msg` with SEQ `seq` (0–3; panics otherwise).
    pub fn encode_message_with_seq(&self, msg: &TransportMessage, seq: u8) -> Vec<f32> {
        let packet = Packet {
            msg_type: msg.msg_type(),
            seq,
            payload: msg.to_payload(),
        };
        let bytes = encode_packet_bytes(&packet).expect("valid packet");
        let mut nibbles = bytes_to_nibbles(&bytes);
        let interleaved = if matches!(self.config.modulation, Modulation::Digital) {
            // The digital frame carries the packet nibbles as they are: its
            // length follows from the type nibble, there is no pad.
            nibbles
        } else if let Some(fec) = &self.fec {
            // FEC replaces padding + block interleave (codeword interleave).
            fec.encode_body(&nibbles)
        } else {
            while nibbles.len() < MAX_BODY_NIBBLES {
                nibbles.push(0);
            }
            nibbles.truncate(MAX_BODY_NIBBLES);
            interleave(&nibbles, self.config.interleave_depth)
        };

        let mut audio = Vec::new();
        for _ in 0..self.config.repetition_k {
            audio.extend(self.phy.synthesize_framed(&interleaved));
            // Small gap between repetitions
            let gap = repetition_gap_samples(&self.config);
            audio.extend(core::iter::repeat_n(0.0, gap));
        }
        audio
    }

    pub fn encode_heartbeat(&mut self) -> Vec<f32> {
        self.encode_message(&TransportMessage::Heartbeat)
    }
}

/// Samples one on-wire repetition occupies: preamble + body symbol periods
/// plus the one-period gap `CricketModemTx` inserts after each repetition.
/// The body is [`MAX_BODY_NIBBLES`], or the coded length when FEC is enabled.
pub fn repetition_samples(config: &PhyConfig) -> u64 {
    let body_nibbles = config.fec.body_nibbles();
    match &config.modulation {
        Modulation::Bmc(p) => {
            return p.repetition_samples(config.sample_rate_hz, body_nibbles) as u64
        }
        // The largest frame; copies follow each other without a gap.
        Modulation::Digital => return digital::MAX_FRAME_SAMPLES as u64,
        Modulation::Mfsk => {}
    }
    ((config.preamble.len() + body_nibbles + 1) * config.samples_per_symbol_period()) as u64
}

/// Least spacing of NoteOff refreshes for `config` at `sample_rate_hz`
/// (for [`crate::transport::TxScheduler::with_refresh_spacing`]): 0 (every
/// free packet, spec §5.5) except for the digital profile
/// ([`digital::REFRESH_SPACING_MS`]).
pub fn refresh_spacing_samples(config: &PhyConfig, sample_rate_hz: u32) -> u64 {
    match config.modulation {
        Modulation::Digital => sample_rate_hz as u64 * digital::REFRESH_SPACING_MS as u64 / 1000,
        _ => 0,
    }
}

/// Silence `CricketModemTx` appends after each repetition: one symbol
/// period (MFSK), `gap_bits` cells (BMC) or none (digital: frames are
/// delimited by their sync and END symbols).
pub fn repetition_gap_samples(config: &PhyConfig) -> usize {
    match &config.modulation {
        Modulation::Mfsk => config.samples_per_symbol_period(),
        Modulation::Bmc(p) => p.gap_samples(config.sample_rate_hz),
        Modulation::Digital => 0,
    }
}

/// Duplicate-suppression window in samples (docs/aml-v1-spec.md §4.4): one
/// full transmission of K repetitions plus half a repetition of margin.
/// Robust: (2 + 0.5) x 6552 = 16380 samples (~341 ms @ 48 kHz);
/// Fast: (1 + 0.5) x 2736 = 4104 samples (~86 ms); digital: (2 + 0.5) x 21
/// = 52 samples (~1.1 ms).
pub fn dedup_window_samples(config: &PhyConfig) -> u64 {
    let rep = repetition_samples(config);
    rep * config.repetition_k.max(1) as u64 + rep / 2
}

/// High-level RX: audio samples → transport messages + fail-safe.
///
/// Link state: `ModemRx` reports phy lock (`Search` / `Locked`); this type
/// owns link-dead. `stats().link_state` is `Dead` from the moment link-dead
/// fires until the next CRC-valid packet.
///
/// Link-dead timing is in absolute stream samples (see [`FailSafeMonitor`]):
/// it fires at sample `end_of_last_valid_packet + link_dead_ms`, independent
/// of chunking. It is reported by the `push_samples` call whose input contains
/// that sample (resolution: one chunk); the exact sample is available from
/// [`CricketModemRx::last_link_dead_sample`].
pub struct CricketModemRx {
    phy: PhyRx,
    config: PhyConfig,
    body_nibbles: Vec<u8>,
    /// Per-symbol detail for the FEC decoder (filled only when FEC is on).
    body_symbols: Vec<BodySymbol>,
    body_len: usize,
    fec: Option<FecCodec>,
    fec_corrected: u64,
    collecting: bool,
    last_seq: Option<u8>,
    crc_pass: u64,
    crc_fail: u64,
    fail_safe: FailSafeMonitor,
    link_dead: bool,
    last_link_dead_sample: Option<u64>,
    /// Recently accepted packets (raw bytes, end sample) for duplicate suppression.
    recent: VecDeque<(Vec<u8>, u64)>,
    dedup_window: u64,
    /// The same for frames of the `digital_detect` detector, whose window
    /// is the digital profile's (its messages are far closer together).
    recent_digital: VecDeque<(Vec<u8>, u64)>,
    digital_dedup_window: u64,
    /// MIDI mapping and the state the fail-safe burst needs.
    midi: MidiOutput,
    /// Digital-frame detector next to a non-digital phy
    /// (`PhyConfig::digital_detect`).
    digital: Option<DigitalRx>,
}

impl CricketModemRx {
    pub fn new(config: PhyConfig) -> Self {
        let mut phy = PhyRx::new(config.clone());
        let body_len = config.fec.body_nibbles();
        phy.set_expected_body_nibbles(body_len);
        let fail_safe = FailSafeMonitor::new(config.link_dead_ms, config.sample_rate_hz);
        let dedup_window = dedup_window_samples(&config);
        let fec = FecCodec::new(config.fec);
        let digital = (config.digital_detect && !matches!(config.modulation, Modulation::Digital))
            .then(|| DigitalRx::new(config.clone()));
        Self {
            phy,
            config,
            body_nibbles: Vec::with_capacity(body_len),
            body_symbols: Vec::new(),
            body_len,
            fec,
            fec_corrected: 0,
            collecting: false,
            last_seq: None,
            crc_pass: 0,
            crc_fail: 0,
            fail_safe,
            link_dead: false,
            last_link_dead_sample: None,
            recent: VecDeque::new(),
            dedup_window,
            recent_digital: VecDeque::new(),
            digital_dedup_window: dedup_window_samples(&PhyConfig::digital()),
            midi: MidiOutput::default(),
            digital,
        }
    }

    /// Device ID and command format of the MSC SysEx this receiver outputs
    /// (spec §5.4; default all-call `0x7F`, Lighting (General) `0x01`).
    pub fn set_msc_target(&mut self, target: MscTarget) {
        self.midi.set_msc_target(target);
    }

    pub fn msc_target(&self) -> MscTarget {
        self.midi.msc_target()
    }

    /// The fail-safe burst (spec §5.3) for a caller that declares link-dead
    /// itself ([`declare_link_dead`](Self::declare_link_dead)): CC64 = 0 and
    /// CC123 on every channel, plus pitch bend to center on channels this
    /// receiver left bent.
    pub fn fail_safe_burst(&mut self) -> Vec<Vec<u8>> {
        self.midi.fail_safe()
    }

    pub fn stats(&self) -> ModemStats {
        ModemStats {
            link_state: if self.link_dead {
                LinkState::Dead
            } else {
                self.phy.link_state()
            },
            crc_pass: self.crc_pass,
            crc_fail: self.crc_fail,
            last_confidence_db: self.phy.last_confidence_db(),
            last_seq: self.last_seq,
            fec_corrected: self.fec_corrected,
        }
    }

    /// Total samples pushed so far (absolute stream position).
    pub fn samples_processed(&self) -> u64 {
        self.phy.samples_pushed()
    }

    /// Stream sample at which the most recent link-dead fired.
    pub fn last_link_dead_sample(&self) -> Option<u64> {
        self.last_link_dead_sample
    }

    /// True while link-dead is pending: a CRC-valid packet has been received
    /// and link-dead has not fired (or been declared) since.
    pub fn is_link_armed(&self) -> bool {
        self.fail_safe.is_armed()
    }

    /// Declare the link dead from outside the sample clock, e.g. when a
    /// wall-clock watchdog sees the audio input stop delivering samples.
    ///
    /// Same effect as link-dead firing (state `Dead`, duplicate history
    /// cleared, monitor disarmed until the next valid packet) except that
    /// **no output is produced**: the caller sends the fail-safe burst itself.
    /// Because the monitor is disarmed, the same outage does not fire a second
    /// burst when samples resume. Returns false (and does nothing) if the link
    /// was not armed.
    pub fn declare_link_dead(&mut self) -> bool {
        if !self.fail_safe.disarm() {
            return false;
        }
        self.link_dead = true;
        self.last_link_dead_sample = Some(self.samples_processed());
        self.recent.clear();
        self.recent_digital.clear();
        true
    }

    /// Hard reset of the decoder, as if it had just been created: phy back
    /// to search with its buffer flushed, a body in progress discarded, the
    /// duplicate history cleared, link-dead disarmed (traffic received
    /// before the reset cannot fire it) and `Dead` state and `last_seq`
    /// cleared. The cumulative counters (`crc_pass`, `crc_fail`,
    /// `fec_corrected`), the sample count and `last_link_dead_sample` are
    /// kept. Not used internally (flushing would make timing depend on
    /// chunking).
    pub fn reset(&mut self) {
        self.phy.reset();
        if let Some(d) = self.digital.as_mut() {
            d.reset();
        }
        self.body_nibbles.clear();
        self.body_symbols.clear();
        self.collecting = false;
        self.last_seq = None;
        self.link_dead = false;
        self.fail_safe.disarm();
        self.recent.clear();
        self.recent_digital.clear();
    }

    pub fn push_samples(&mut self, samples: &[f32]) -> Vec<ModemOutput> {
        let mut outputs = Vec::new();

        let mut events: VecDeque<RxEvent> = self.phy.push_samples(samples).into();
        while let Some(ev) = events.pop_front() {
            match ev {
                RxEvent::PreambleLocked { .. } => {
                    self.collecting = true;
                    self.body_nibbles.clear();
                    self.body_symbols.clear();
                }
                RxEvent::BodyNibble { result, end_sample } => {
                    if self.collecting {
                        self.body_nibbles.push(result.nibble);
                        if self.fec.is_some() {
                            self.body_symbols.push(BodySymbol {
                                nibble: result.nibble,
                                reliability: result.confidence_db,
                                erased: result.erased,
                            });
                        }
                        if self.body_nibbles.len() >= self.body_len {
                            self.collecting = false;
                            // The phy pauses here until told whether the body
                            // decoded; a failure rewinds it so a packet that
                            // starts inside a false lock is not lost.
                            let more = if self.try_decode_body(end_sample, &mut outputs) {
                                self.phy.confirm()
                            } else {
                                self.phy.reject()
                            };
                            events.extend(more);
                        }
                    }
                }
                RxEvent::SyncLost => {
                    self.collecting = false;
                    self.body_nibbles.clear();
                    outputs.push(ModemOutput::SyncLost);
                }
                RxEvent::Frame(frame) => {
                    self.try_decode_frame(&frame, false, &mut outputs);
                }
                RxEvent::Metrics { .. } => {}
            }
        }
        if let Some(mut d) = self.digital.take() {
            for ev in d.push_samples(samples) {
                if let RxEvent::Frame(frame) = ev {
                    self.try_decode_frame(&frame, true, &mut outputs);
                }
            }
            self.digital = Some(d);
        }
        self.poll_fail_safe(self.phy.samples_pushed(), &mut outputs);

        outputs.push(ModemOutput::Stats(self.stats()));
        outputs
    }

    /// Fire link-dead if its deadline sample lies before stream position `now`.
    /// Emits one fail-safe burst (spec §5.3) and `LinkDead`, then clears
    /// the duplicate history. The phy is not flushed: during silence it is
    /// already searching, and a packet still in flight may revive the link.
    fn poll_fail_safe(&mut self, now: u64, outputs: &mut Vec<ModemOutput>) {
        if let Some(at) = self.fail_safe.poll(now) {
            outputs.extend(self.midi.fail_safe().into_iter().map(ModemOutput::Midi));
            outputs.push(ModemOutput::LinkDead);
            self.link_dead = true;
            self.last_link_dead_sample = Some(at);
            self.recent.clear();
            self.recent_digital.clear();
        }
    }

    /// Duplicate if the same packet bytes (same SEQ, type and payload) were
    /// accepted within the dedup window before `end`.
    fn is_duplicate(&mut self, bytes: &[u8], end: u64, aux: bool) -> bool {
        let (recent, window) = if aux {
            (&mut self.recent_digital, self.digital_dedup_window)
        } else {
            (&mut self.recent, self.dedup_window)
        };
        recent.retain(|(_, at)| end.saturating_sub(*at) <= window);
        recent.iter().any(|(b, _)| b.as_slice() == bytes)
    }

    /// Decode the collected body; the last body symbol ended at stream
    /// position `end`. Returns whether it held a valid packet (CRC pass,
    /// including duplicates), i.e. whether the phy lock was genuine.
    fn try_decode_body(&mut self, end: u64, outputs: &mut Vec<ModemOutput>) -> bool {
        let bytes = if self.fec.is_some() {
            match self.fec_packet_bytes() {
                Some(bytes) => bytes,
                None => {
                    self.crc_fail += 1;
                    return false;
                }
            }
        } else {
            let depth = self.config.interleave_depth;
            let deinterleaved = deinterleave(&self.body_nibbles, depth);
            nibbles_to_bytes(&deinterleaved)
        };

        // Try trimming trailing pad zeros from the byte stream by attempting
        // each valid message size from the HDR type.
        if bytes.len() < 4 {
            self.crc_fail += 1;
            return false;
        }

        let hdr = bytes[0];
        let type_nibble = hdr & 0x0F;
        let Some(msg_type) = MessageType::from_u8(type_nibble) else {
            // A reserved type: the body held no packet, like a CRC failure.
            self.crc_fail += 1;
            return false;
        };
        let needed = 2 + msg_type.payload_len() + 2;
        if bytes.len() < needed {
            self.crc_fail += 1;
            return false;
        }
        self.deliver(&bytes[..needed], end, false, outputs)
    }

    /// A complete digital frame: the packet bytes go through the same
    /// validation (spec §4.7: CRC-16, version, type, SEQ, canonical payload)
    /// and duplicate suppression as any other body. `aux`: it came from the
    /// `digital_detect` detector (own duplicate history).
    fn try_decode_frame(
        &mut self,
        frame: &DigitalFrame,
        aux: bool,
        outputs: &mut Vec<ModemOutput>,
    ) -> bool {
        self.deliver(frame.packet(), frame.end_sample, aux, outputs)
    }

    /// Validate the exact packet bytes `slice` (no pad) that ended at stream
    /// position `end`, and deliver them unless they are a duplicate.
    /// Returns whether they held a valid packet (duplicates included).
    fn deliver(
        &mut self,
        slice: &[u8],
        end: u64,
        aux: bool,
        outputs: &mut Vec<ModemOutput>,
    ) -> bool {
        let decoded = decode_packet_bytes(slice).ok().and_then(|packet| {
            let msg = TransportMessage::from_type_payload(packet.msg_type, &packet.payload)?;
            Some((packet, msg))
        });
        match decoded {
            Some((packet, msg)) => {
                self.crc_pass += 1;
                // A deadline that passed before this packet ended fires first.
                self.poll_fail_safe(end, outputs);
                self.fail_safe.on_valid_packet(end);
                self.link_dead = false;

                if self.is_duplicate(slice, end, aux) {
                    return true;
                }
                if aux {
                    self.recent_digital.push_back((slice.to_vec(), end));
                } else {
                    self.recent.push_back((slice.to_vec(), end));
                }
                self.last_seq = Some(packet.seq);

                // Panic: the burst below is the only one; the phy is confirmed
                // past this packet (back in search) right after. The duplicate history is
                // kept so the next repetition of the Panic is suppressed.
                outputs.extend(self.midi.message(&msg).into_iter().map(ModemOutput::Midi));
                outputs.push(ModemOutput::Message(msg));
                true
            }
            // CRC, version, SEQ or payload check failed (a CRC-valid packet
            // with a non-canonical payload is treated as a false decode).
            None => {
                self.crc_fail += 1;
                false
            }
        }
    }
}

impl CricketModemRx {
    /// FEC-decode the collected body into packet bytes (+ zero pad). A
    /// candidate is accepted only if [`fec_candidate_is_valid`].
    fn fec_packet_bytes(&mut self) -> Option<Vec<u8>> {
        let fec = self.fec.as_ref()?;
        let decoded = fec.decode_body(&self.body_symbols, fec_candidate_is_valid)?;
        self.fec_corrected += decoded.corrected as u64;
        Some(nibbles_to_bytes(&decoded.nibbles))
    }
}

/// Offline helpers used by tests and CLI.
pub fn encode_to_samples(config: PhyConfig, messages: &[TransportMessage]) -> Vec<f32> {
    let mut tx = CricketModemTx::new(config);
    let mut all = Vec::new();
    for msg in messages {
        all.extend(tx.encode_message(msg));
        // Inter-message silence
        all.extend(core::iter::repeat_n(
            0.0,
            tx.config().samples_per_symbol_period() * 2,
        ));
    }
    all
}

/// Silence to feed a receiver after the last input sample so that a
/// transmission ending there is fully decoded: one repetition of `config`
/// plus eight symbol periods. Covers the MFSK receiver's one-period
/// lookahead per symbol, the BMC receiver's bit decision delay (a few half
/// cells past the last bit) and the input chunking, for every profile and
/// sample rate.
pub fn flush_samples(config: &PhyConfig) -> usize {
    repetition_samples(config) as usize + config.samples_per_symbol_period() * 8
}

pub fn decode_samples(config: PhyConfig, samples: &[f32]) -> Vec<TransportMessage> {
    let flush = flush_samples(&config);
    let mut rx = CricketModemRx::new(config);
    let mut messages = Vec::new();
    // Process in chunks to simulate streaming
    const CHUNK: usize = 256;
    for chunk in samples.chunks(CHUNK) {
        for out in rx.push_samples(chunk) {
            if let ModemOutput::Message(m) = out {
                messages.push(m);
            }
        }
    }
    // Flush remaining by pushing silence
    let silence = vec![0.0; flush];
    for out in rx.push_samples(&silence) {
        if let ModemOutput::Message(m) = out {
            messages.push(m);
        }
    }
    messages
}
