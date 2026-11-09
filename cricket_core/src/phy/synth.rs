use super::config::PhyConfig;
use alloc::vec::Vec;
use core::f32::consts::PI;

/// Encodes MFSK-16 nibble streams into mono audio samples.
pub struct ModemTx {
    config: PhyConfig,
}

impl ModemTx {
    pub fn new(config: PhyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &PhyConfig {
        &self.config
    }

    /// Synthesize audio for a sequence of tone indices (0..15), each followed by guard gap.
    pub fn synthesize_nibbles(&self, nibbles: &[u8]) -> Vec<f32> {
        let mut out = Vec::new();
        let top = (self.config.n_tones.clamp(1, 16) - 1) as u8;
        for &n in nibbles {
            let idx = (n & 0x0F).min(top);
            out.extend(self.synthesize_symbol(self.config.tone_hz(idx)));
            let gap = self.config.gap_samples();
            if gap > 0 {
                out.extend(core::iter::repeat_n(0.0, gap));
            }
        }
        out
    }

    /// Full on-wire burst: preamble + body nibbles. With fewer than 16
    /// tones the body nibbles are repacked into the alphabet's symbols
    /// ([`super::pack`]); the preamble is given in tone indices already.
    pub fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32> {
        let body = super::pack::pack_nibbles(body_nibbles, self.config.bits_per_symbol());
        let mut all = Vec::with_capacity(self.config.preamble.len() + body.len());
        all.extend_from_slice(&self.config.preamble);
        all.extend_from_slice(&body);
        self.synthesize_nibbles(&all)
    }

    fn synthesize_symbol(&self, freq: f32) -> Vec<f32> {
        let total = self.config.symbol_samples();
        let attack =
            libm::roundf((self.config.attack_ms / 1000.0) * self.config.sample_rate_hz as f32)
                as usize;
        let release =
            libm::roundf((self.config.release_ms / 1000.0) * self.config.sample_rate_hz as f32)
                as usize;
        let amp = self.config.amplitude;

        let mut buffer = Vec::with_capacity(total);
        let phase_inc = 2.0 * PI * freq / self.config.sample_rate_hz as f32;
        let mut phase = 0.0f32;

        let shape = |x: f32| {
            if self.config.raised_cosine_ramps {
                0.5 - 0.5 * libm::cosf(PI * x)
            } else {
                x
            }
        };
        for i in 0..total {
            let envelope = if attack > 0 && i < attack {
                shape(i as f32 / attack as f32)
            } else if release > 0 && i >= total.saturating_sub(release) {
                shape((total - i) as f32 / release as f32)
            } else {
                1.0
            };
            buffer.push(libm::sinf(phase) * envelope * amp);
            phase += phase_inc;
            if phase > 2.0 * PI {
                phase -= 2.0 * PI;
            }
        }
        buffer
    }
}
