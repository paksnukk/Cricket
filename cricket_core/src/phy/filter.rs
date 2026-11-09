//! Receiver input band-pass for the MFSK receiver (spec §3.2, SHOULD):
//! two second-order Butterworth sections (high-pass below the tone band,
//! low-pass above it) so that the window energy the detection gate
//! normalizes by is the energy *in the tone band*, not of the whole audio
//! band. Hum, rumble, speech and most music energy lie below 2 kHz, hiss
//! is spread over the whole band; none of it should decide whether a tone
//! is present. Receiver-side only: the transmitted signal is unchanged.

use core::f32::consts::PI;

/// Direct-form-I biquad with state (f32, `no_std`).
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Biquad {
    fn from_rbj(b: [f32; 3], a0: f32, a: [f32; 2]) -> Self {
        Self {
            b: [b[0] / a0, b[1] / a0, b[2] / a0],
            a: [a[0] / a0, a[1] / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    /// Butterworth (Q = 1/√2) second-order low-pass.
    pub fn lowpass(cutoff_hz: f32, sample_rate_hz: u32) -> Self {
        let w0 = 2.0 * PI * cutoff_hz / sample_rate_hz as f32;
        let (s, c) = (libm::sinf(w0), libm::cosf(w0));
        let alpha = s / (2.0 * core::f32::consts::FRAC_1_SQRT_2);
        Self::from_rbj(
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
            1.0 + alpha,
            [-2.0 * c, 1.0 - alpha],
        )
    }

    /// Butterworth (Q = 1/√2) second-order high-pass.
    pub fn highpass(cutoff_hz: f32, sample_rate_hz: u32) -> Self {
        let w0 = 2.0 * PI * cutoff_hz / sample_rate_hz as f32;
        let (s, c) = (libm::sinf(w0), libm::cosf(w0));
        let alpha = s / (2.0 * core::f32::consts::FRAC_1_SQRT_2);
        Self::from_rbj(
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
            1.0 + alpha,
            [-2.0 * c, 1.0 - alpha],
        )
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }

    pub fn reset(&mut self) {
        self.x = [0.0; 2];
        self.y = [0.0; 2];
    }
}

/// The receiver's tone-band filter: high-pass at `lo_hz`, low-pass at
/// `hi_hz` (both second-order Butterworth).
#[derive(Debug, Clone, Copy)]
pub struct BandPass {
    hp: Biquad,
    lp: Biquad,
}

impl BandPass {
    pub fn new(lo_hz: f32, hi_hz: f32, sample_rate_hz: u32) -> Self {
        Self {
            hp: Biquad::highpass(lo_hz, sample_rate_hz),
            lp: Biquad::lowpass(hi_hz, sample_rate_hz),
        }
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        self.lp.process(self.hp.process(x))
    }

    pub fn reset(&mut self) {
        self.hp.reset();
        self.lp.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn gain_db(f: &mut BandPass, freq: f32, fs: u32) -> f32 {
        let n = 4800;
        let out: Vec<f32> = (0..2 * n)
            .map(|i| f.process(libm::sinf(2.0 * PI * freq * i as f32 / fs as f32)))
            .collect();
        let e: f32 = out[n..].iter().map(|s| s * s).sum::<f32>() / n as f32;
        10.0 * libm::log10f(e / 0.5)
    }

    #[test]
    fn passes_the_tone_band_and_rejects_hum() {
        let fs = 48_000;
        let mut f = BandPass::new(1750.0, 5200.0, fs);
        for tone in [2500.0f32, 3000.0, 4000.0] {
            let g = gain_db(&mut f, tone, fs);
            assert!(g > -1.5 && g < 0.5, "{tone} Hz: {g} dB");
        }
        assert!(gain_db(&mut f, 100.0, fs) < -45.0);
        assert!(gain_db(&mut f, 50.0, fs) < -55.0);
        assert!(gain_db(&mut f, 15_000.0, fs) < -15.0);
    }
}
