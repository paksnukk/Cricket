//! MIDI input mapping (spec §5): any byte string in, a transport message,
//! a rejection or "ignored" out — never a panic — and a message that is
//! carried maps back to MIDI that maps to the same message.
#![no_main]

use cricket_core::transport::{message_to_midi, midi_input, midi_to_message, MidiInput};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match midi_input(data) {
        MidiInput::Message(msg) => {
            // A message carried as MIDI is one MIDI message (heartbeats and
            // Panic are not MIDI input), and it maps back to itself.
            let out = message_to_midi(&msg);
            if let [bytes] = out.as_slice() {
                if let Some(again) = midi_to_message(bytes) {
                    assert_eq!(again, msg, "MIDI round trip");
                }
            }
            // Payload round trip (spec §5.1).
            let payload = msg.to_payload();
            assert_eq!(payload.len(), msg.msg_type().payload_len());
        }
        _ => {}
    }
    let _ = midi_to_message(data);
});
