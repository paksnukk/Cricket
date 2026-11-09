//! Allocation-free set of (MIDI channel, note) pairs.

/// A set of (channel 0–15, note 0–127) pairs: one bit per note per channel.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NoteSet([u128; 16]);

impl NoteSet {
    pub fn insert(&mut self, channel: u8, note: u8) {
        self.0[usize::from(channel & 0x0F)] |= 1 << (note & 0x7F);
    }

    pub fn remove(&mut self, channel: u8, note: u8) {
        self.0[usize::from(channel & 0x0F)] &= !(1 << (note & 0x7F));
    }

    pub fn contains(&self, channel: u8, note: u8) -> bool {
        self.0[usize::from(channel & 0x0F)] & (1 << (note & 0x7F)) != 0
    }

    pub fn clear(&mut self) {
        self.0 = [0; 16];
    }

    pub fn clear_channel(&mut self, channel: u8) {
        self.0[usize::from(channel & 0x0F)] = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&bits| bits == 0)
    }

    /// Number of notes in the set.
    pub fn len(&self) -> usize {
        self.0.iter().map(|bits| bits.count_ones() as usize).sum()
    }

    /// The notes of `channel`, ascending.
    pub fn notes(&self, channel: u8) -> impl Iterator<Item = u8> + '_ {
        let bits = self.0[usize::from(channel & 0x0F)];
        (0..128u8).filter(move |n| bits & (1 << n) != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn set_operations() {
        let mut s = NoteSet::default();
        assert!(s.is_empty());
        s.insert(0, 60);
        s.insert(0, 127);
        s.insert(15, 0);
        s.insert(0, 60);
        assert_eq!(s.len(), 3);
        assert!(s.contains(0, 60) && s.contains(15, 0) && !s.contains(1, 60));
        assert_eq!(s.notes(0).collect::<Vec<_>>(), vec![60, 127]);
        s.remove(0, 60);
        assert_eq!(s.notes(0).collect::<Vec<_>>(), vec![127]);
        s.clear_channel(15);
        assert_eq!(s.len(), 1);
        s.clear();
        assert!(s.is_empty());
    }
}
