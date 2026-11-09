//! MIDI I/O: port listing, the [`MidiSink`] / [`MidiSource`] traits the
//! workers use, and their midir implementations.
//!
//! Device ports are opened inside the worker thread (as before the engine
//! existed); custom endpoints are handed over already constructed.

use crate::devices::DeviceSelector;
use anyhow::{anyhow, Result};
use std::any::Any;

/// Receives raw MIDI messages produced by the decoder.
pub trait MidiSink {
    /// Send one complete MIDI message (e.g. `[0x90, note, vel]`).
    fn send(&mut self, bytes: &[u8]) -> Result<()>;
}

impl MidiSink for midir::MidiOutputConnection {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        midir::MidiOutputConnection::send(self, bytes).map_err(|e| anyhow!("MIDI send: {}", e))
    }
}

/// Callback a [`MidiSource`] delivers raw incoming MIDI messages to.
pub type MidiCallback = Box<dyn FnMut(&[u8]) + Send + 'static>;

/// Keeps a MIDI input connected; dropping it disconnects.
pub struct MidiInputGuard(#[allow(dead_code)] Box<dyn Any>);

impl MidiInputGuard {
    /// Wrap whatever object keeps the connection alive.
    pub fn new<T: 'static>(connection: T) -> Self {
        Self(Box::new(connection))
    }
}

/// A MIDI input for the encoder: once connected it calls `on_message` for
/// every incoming message until the returned guard is dropped.
pub trait MidiSource: Send {
    fn connect(self: Box<Self>, on_message: MidiCallback) -> Result<MidiInputGuard>;
}

/// Where the decoder sends MIDI.
pub enum MidiOut {
    /// No MIDI output: decode to events only.
    None,
    /// A midir output port.
    Device(DeviceSelector),
    /// Any other sink (e.g. a recorder in tests).
    Custom(Box<dyn MidiSink + Send>),
}

/// Where the encoder reads MIDI from.
pub enum MidiIn {
    /// No MIDI input (the encoder then sends heartbeats only, plus the test
    /// scale if asked for).
    None,
    /// A midir input port.
    Device(DeviceSelector),
    /// Any other source (e.g. injected messages in tests).
    Custom(Box<dyn MidiSource>),
}

/// Names of `ports` (as returned by `io.ports()`), in the same order.
fn port_names<T: midir::MidiIO>(io: &T, ports: &[T::Port]) -> Vec<String> {
    ports
        .iter()
        .map(|p| io.port_name(p).unwrap_or_else(|_| "Unknown".into()))
        .collect()
}

/// MIDI input port names in enumeration order (index = [`DeviceSelector::Index`];
/// each name is valid for [`DeviceSelector::Name`]).
pub fn list_inputs() -> Result<Vec<String>> {
    let midi_in = midir::MidiInput::new("cricket list in").map_err(|e| anyhow!("{}", e))?;
    Ok(port_names(&midi_in, &midi_in.ports()))
}

/// MIDI output port names in enumeration order (index = [`DeviceSelector::Index`];
/// each name is valid for [`DeviceSelector::Name`]).
pub fn list_outputs() -> Result<Vec<String>> {
    let midi_out = midir::MidiOutput::new("cricket list out").map_err(|e| anyhow!("{}", e))?;
    Ok(port_names(&midi_out, &midi_out.ports()))
}

/// Position of the selected port in `names` (MIDI has no default port).
fn port_index<S: AsRef<str>>(selector: &DeviceSelector, names: &[S], dir: &str) -> Result<usize> {
    selector
        .resolve(names, &format!("MIDI {}", dir))?
        .ok_or_else(|| anyhow!("No MIDI {} port selected", dir))
}

/// Name of the selected input port (errors if it does not exist).
pub fn input_port_name(selector: &DeviceSelector) -> Result<String> {
    let mut names = list_inputs()?;
    let index = port_index(selector, &names, "in")?;
    Ok(names.swap_remove(index))
}

/// Name of the selected output port (errors if it does not exist).
pub fn output_port_name(selector: &DeviceSelector) -> Result<String> {
    let mut names = list_outputs()?;
    let index = port_index(selector, &names, "out")?;
    Ok(names.swap_remove(index))
}

/// Connect to the selected output port.
pub fn open_output(selector: &DeviceSelector) -> Result<midir::MidiOutputConnection> {
    let midi_out = midir::MidiOutput::new("cricket decode out").map_err(|e| anyhow!("{}", e))?;
    let ports = midi_out.ports();
    let index = port_index(selector, &port_names(&midi_out, &ports), "out")?;
    let port = &ports[index];
    midi_out
        .connect(port, "cricket out")
        .map_err(|e| anyhow!("MIDI connect: {}", e))
}

/// Connect to the selected input port; `on_message` gets every message
/// except MIDI clock / MTC / active sensing (SysEx is delivered, for MSC).
pub fn open_input(
    selector: &DeviceSelector,
    mut on_message: MidiCallback,
) -> Result<midir::MidiInputConnection<()>> {
    let mut midi_in = midir::MidiInput::new("cricket encode in").map_err(|e| anyhow!("{}", e))?;
    // SysEx must come through (MSC); clock, MTC and active sensing are not
    // carried, so drop them at the source.
    midi_in.ignore(midir::Ignore::TimeAndActiveSense);
    let ports = midi_in.ports();
    let index = port_index(selector, &port_names(&midi_in, &ports), "in")?;
    let port = &ports[index];
    midi_in
        .connect(
            port,
            "cricket in",
            move |_stamp, message, _| on_message(message),
            (),
        )
        .map_err(|e| anyhow!("MIDI connect: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_index_by_name_index_and_default() {
        let names = ["Midi Through", "UM-ONE"];
        let by_name = DeviceSelector::Name("UM-ONE".into());
        assert_eq!(port_index(&by_name, &names, "out").unwrap(), 1);
        assert_eq!(
            port_index(&DeviceSelector::Index(0), &names, "in").unwrap(),
            0
        );
        let missing = DeviceSelector::Name("iRig".into());
        assert_eq!(
            port_index(&missing, &names, "out").unwrap_err().to_string(),
            "MIDI out \"iRig\" not found (available: Midi Through, UM-ONE)"
        );
        assert_eq!(
            port_index(&DeviceSelector::Index(2), &names, "in")
                .unwrap_err()
                .to_string(),
            "MIDI in index 2 not found"
        );
        assert_eq!(
            port_index(&DeviceSelector::Default, &names, "out")
                .unwrap_err()
                .to_string(),
            "No MIDI out port selected"
        );
    }
}
