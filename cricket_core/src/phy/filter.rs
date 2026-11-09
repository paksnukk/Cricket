//! Receiver input band-pass for the MFSK receiver (spec §3.2, SHOULD):
//! Butterworth sections (high-pass below the tone band, low-pass above it)
//! so that the window energy the detection gate normalizes by is the energy
//! *in the tone band*, not of the whole audio band. Hum, rumble, speech and
//! most music energy lie below 2 kHz, hiss is spread over the whole band;
//! none of it should decide whether a tone is present. Receiver-side only:
//! the transmitted signal is unchanged.
//!
//! The baseband profiles use one second-order section per side
//! ([`BandPass::new`]). The HF (instrument-audio) profiles, whose tones sit
//! at 19–21.5 kHz above an instrument whose own spectrum is 30–60 dB
//! stronger a few kHz below, use an eighth-order high-pass
//! ([`BandPass::with_order`], spec §6.6): the second-order edge lets the
//! instrument's 3–5 kHz resonance through at only −26 dB, which counts
//! against the tones' purity.

use alloc::vec::Vec;
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
        Self::lowpass_q(cutoff_hz, core::f32::consts::FRAC_1_SQRT_2, sample_rate_hz)
    }

    /// Butterworth (Q = 1/√2) second-order high-pass.
    pub fn highpass(cutoff_hz: f32, sample_rate_hz: u32) -> Self {
        Self::highpass_q(cutoff_hz, core::f32::consts::FRAC_1_SQRT_2, sample_rate_hz)
    }

    /// Second-order low-pass with quality `q` (one section of a
    /// higher-order Butterworth, see [`butterworth_q`]).
    pub fn lowpass_q(cutoff_hz: f32, q: f32, sample_rate_hz: u32) -> Self {
        let w0 = 2.0 * PI * cutoff_hz / sample_rate_hz as f32;
        let (s, c) = (libm::sinf(w0), libm::cosf(w0));
        let alpha = s / (2.0 * q);
        Self::from_rbj(
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
            1.0 + alpha,
            [-2.0 * c, 1.0 - alpha],
        )
    }

    /// Second-order high-pass with quality `q`.
    pub fn highpass_q(cutoff_hz: f32, q: f32, sample_rate_hz: u32) -> Self {
        let w0 = 2.0 * PI * cutoff_hz / sample_rate_hz as f32;
        let (s, c) = (libm::sinf(w0), libm::cosf(w0));
        let alpha = s / (2.0 * q);
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

/// Section quality factors of a Butterworth filter of even `order`
/// (`order / 2` of them): `1 / (2 cos(π (2k + 1) / (2 order)))`, so that
/// the cascade is maximally flat with −3 dB at the cutoff. Order 2 gives
/// `1/√2`; order 8 gives 0.510, 0.601, 0.900 and 2.563.
pub fn butterworth_q(order: usize) -> impl Iterator<Item = f32> {
    let order = order.max(2) & !1;
    (0..order / 2)
        .map(move |k| 1.0 / (2.0 * libm::cosf(PI * (2 * k + 1) as f32 / (2 * order) as f32)))
}

/// The receiver's tone-band filter: a Butterworth high-pass at `lo_hz` and
/// a Butterworth low-pass at `hi_hz`, each of the same even order.
#[derive(Debug, Clone)]
pub struct BandPass {
    sections: Vec<Biquad>,
}

impl BandPass {
    /// Second-order on both sides (the baseband profiles).
    pub fn new(lo_hz: f32, hi_hz: f32, sample_rate_hz: u32) -> Self {
        Self::with_order(lo_hz, hi_hz, 2, sample_rate_hz)
    }

    /// `order` is rounded down to an even number of at least 2. A side
    /// whose cutoff is not inside (0, fs / 2) is left out, so a band that
    /// reaches Nyquist (the HF profiles at 48 kHz) has no low-pass instead
    /// of an unstable one.
    pub fn with_order(lo_hz: f32, hi_hz: f32, order: usize, sample_rate_hz: u32) -> Self {
        let nyquist = sample_rate_hz as f32 / 2.0;
        let mut sections = Vec::with_capacity(order.max(2));
        if lo_hz > 0.0 && lo_hz < nyquist {
            if order <= 2 {
                sections.push(Biquad::highpass(lo_hz, sample_rate_hz));
            } else {
                sections.extend(
                    butterworth_q(order).map(|q| Biquad::highpass_q(lo_hz, q, sample_rate_hz)),
                );
            }
        }
        if hi_hz > 0.0 && hi_hz < nyquist {
            if order <= 2 {
                sections.push(Biquad::lowpass(hi_hz, sample_rate_hz));
            } else {
                sections.extend(
                    butterworth_q(order).map(|q| Biquad::lowpass_q(hi_hz, q, sample_rate_hz)),
                );
            }
        }
        Self { sections }
    }

    /// Biquad sections in the cascade (5 multiply-adds per sample each).
    pub fn sections(&self) -> usize {
        self.sections.len()
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let mut v = x;
        for s in self.sections.iter_mut() {
            v = s.process(v);
        }
        v
    }

    pub fn reset(&mut self) {
        for s in self.sections.iter_mut() {
            s.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(f.sections(), 2);
        for tone in [2500.0f32, 3000.0, 4000.0] {
            let g = gain_db(&mut f, tone, fs);
            assert!(g > -1.5 && g < 0.5, "{tone} Hz: {g} dB");
        }
        assert!(gain_db(&mut f, 100.0, fs) < -45.0);
        assert!(gain_db(&mut f, 50.0, fs) < -55.0);
        assert!(gain_db(&mut f, 15_000.0, fs) < -15.0);
    }

    /// The second-order sections of `with_order(…, 2, …)` are the ones
    /// `new` builds (the baseband profiles' filter is unchanged).
    #[test]
    fn order_two_is_the_plain_band_pass() {
        let a = BandPass::new(1750.0, 6500.0, 48_000);
        let b = BandPass::with_order(1750.0, 6500.0, 2, 48_000);
        assert_eq!(a.sections.len(), b.sections.len());
        for (x, y) in a.sections.iter().zip(&b.sections) {
            assert_eq!(x.b, y.b);
            assert_eq!(x.a, y.a);
        }
        let qs: Vec<f32> = butterworth_q(8).collect();
        for (q, want) in qs.iter().zip([0.5098, 0.6013, 0.9000, 2.5629]) {
            assert!((q - want).abs() < 1e-3, "{q} vs {want}");
        }
    }

    /// The HF band: an 8th-order high-pass at 18 kHz keeps 19–21.5 kHz
    /// within 2 dB and takes a guitar's 3–5 kHz resonance down by more
    /// than 80 dB; the band reaching Nyquist gets no low-pass section.
    #[test]
    fn hf_band_is_steep_and_has_no_lowpass_at_nyquist() {
        let fs = 48_000;
        let mut f = BandPass::with_order(18_000.0, 27_000.0, 8, fs);
        assert_eq!(f.sections(), 4);
        for tone in [19_000.0f32, 20_000.0, 21_500.0] {
            let g = gain_db(&mut f, tone, fs);
            assert!(g > -2.0 && g < 0.5, "{tone} Hz: {g} dB");
        }
        assert!(gain_db(&mut f, 15_000.0, fs) < -12.0);
        assert!(gain_db(&mut f, 10_000.0, fs) < -40.0);
        assert!(gain_db(&mut f, 4_000.0, fs) < -80.0);
        let mut with_lp = BandPass::with_order(18_000.0, 22_500.0, 8, fs);
        assert_eq!(with_lp.sections(), 8);
        assert!(gain_db(&mut with_lp, 21_500.0, fs) > -2.0);
        assert!(gain_db(&mut with_lp, 23_800.0, fs) < -6.0);
    }
}
