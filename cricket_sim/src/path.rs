//! Bit-transparency path models for the `digital` profile and `cricket
//! pathcheck sim`: what a digital audio path can do to 24-bit sample codes
//! (gain, word length and dither, float scaling, noise, sample-rate
//! conversion, EQ, polarity, delay, DC), applied in order by [`simulate`].
//! The analyzer that reads the result is `cricket_core::pathcheck::analyze`.

use cricket_core::pathcheck::{Requantize, Rng};
use cricket_core::phy::digital::CODE_SCALE;

/// One processing step of a simulated audio path. Samples are 24-bit
/// codes in between; a step that computes in floating point rounds back to
/// a 24-bit integer at its end, as a real device's output would.
#[derive(Debug, Clone, PartialEq)]
pub enum PathImpairment {
    /// Linear gain in dB.
    Gain(f64),
    /// Negate every sample.
    Polarity,
    /// Insert this many samples of silence in front.
    Delay(usize),
    /// Add a constant, in 24-bit codes (256 = 1 LSB of a 16-bit word).
    DcOffset(i32),
    /// Reduce to `bits` (e.g. 16) and back to 24 bits.
    WordLength { bits: u32, mode: Requantize },
    /// A float stage scaled by 2²³ on the way in and by `scale` on the way
    /// out (e.g. 2²³ − 1), truncating toward zero or rounding.
    FloatScale { scale: f64, truncate: bool },
    /// A 16-bit stage converting `f32 → i16` with ×32767 (truncating or
    /// rounding) and `i16 → f32` with /32768.
    Int16Scale32767 { truncate: bool },
    /// Add Gaussian noise of this RMS level (dBFS); `seed` selects it.
    Noise { dbfs: f64, seed: u64 },
    /// Resample by `ratio` (output rate / input rate) with a windowed-sinc
    /// interpolator: sample-rate conversion or an unlocked clock.
    Resample(f64),
    /// Peaking EQ: centre (Hz at 48 kHz), gain (dB), Q.
    PeakingEq { hz: f64, db: f64, q: f64 },
}

impl PathImpairment {
    /// Named presets: the conditions of the fail-safe table
    /// (`docs/proposals/digital-transparent.md` §5).
    pub fn presets() -> Vec<(&'static str, Vec<PathImpairment>)> {
        use PathImpairment as P;
        let tpdf = P::WordLength {
            bits: 16,
            mode: Requantize::Tpdf,
        };
        vec![
            ("bit-exact", vec![]),
            ("delay-1", vec![P::Delay(1)]),
            ("polarity", vec![P::Polarity]),
            (
                "16bit-truncate",
                vec![P::WordLength {
                    bits: 16,
                    mode: Requantize::Truncate,
                }],
            ),
            ("16bit-tpdf", vec![tpdf.clone()]),
            (
                "float-2^23-1",
                vec![P::FloatScale {
                    scale: 8_388_607.0,
                    truncate: true,
                }],
            ),
            ("int16-32767", vec![P::Int16Scale32767 { truncate: true }]),
            (
                "noise-120dBFS",
                vec![P::Noise {
                    dbfs: -120.0,
                    seed: 1,
                }],
            ),
            (
                "noise-100dBFS",
                vec![P::Noise {
                    dbfs: -100.0,
                    seed: 2,
                }],
            ),
            ("dc+1lsb16", vec![P::DcOffset(256)]),
            ("gain+0.1dB", vec![P::Gain(0.1)]),
            ("gain-0.1dB", vec![P::Gain(-0.1)]),
            ("gain+0.5dB", vec![P::Gain(0.5)]),
            ("gain-1dB", vec![P::Gain(-1.0)]),
            ("gain-0.2dB+16bit-tpdf", vec![P::Gain(-0.2), tpdf]),
            (
                "noise-80dBFS",
                vec![P::Noise {
                    dbfs: -80.0,
                    seed: 3,
                }],
            ),
            ("dc+2lsb16", vec![P::DcOffset(512)]),
            ("drift+10ppm", vec![P::Resample(1.0 + 10e-6)]),
            (
                "src-48k-44k1-48k",
                vec![
                    P::Resample(44_100.0 / 48_000.0),
                    P::Resample(48_000.0 / 44_100.0),
                ],
            ),
            (
                "eq+3dB@10kHz",
                vec![P::PeakingEq {
                    hz: 10_000.0,
                    db: 3.0,
                    q: 1.0,
                }],
            ),
            (
                "eq+1dB@1kHz",
                vec![P::PeakingEq {
                    hz: 1000.0,
                    db: 1.0,
                    q: 1.0,
                }],
            ),
            (
                "8bit-truncate",
                vec![P::WordLength {
                    bits: 8,
                    mode: Requantize::Truncate,
                }],
            ),
        ]
    }
}

fn round24(x: f64) -> f64 {
    libm::round(x).clamp(-8_388_608.0, 8_388_607.0)
}

/// Run `samples` through `path` (steps in order).
pub fn simulate(path: &[PathImpairment], samples: &[f32]) -> Vec<f32> {
    let mut c: Vec<f64> = samples
        .iter()
        .map(|&x| x as f64 * CODE_SCALE as f64)
        .collect();
    for step in path {
        c = apply(step, c);
    }
    c.iter().map(|&v| (v / CODE_SCALE as f64) as f32).collect()
}

fn apply(step: &PathImpairment, mut c: Vec<f64>) -> Vec<f64> {
    use PathImpairment as P;
    match *step {
        P::Gain(db) => {
            let g = libm::pow(10.0, db / 20.0);
            c.iter_mut().for_each(|v| *v = round24(*v * g));
        }
        P::Polarity => c.iter_mut().for_each(|v| *v = -*v),
        P::Delay(n) => {
            let mut out = vec![0.0; n];
            out.extend(c);
            c = out;
        }
        P::DcOffset(d) => c.iter_mut().for_each(|v| *v = round24(*v + d as f64)),
        P::WordLength { bits, mode } => {
            let q = libm::pow(2.0, 24.0 - bits as f64);
            let mut rng = Rng::new(bits as u64 ^ 0xD17E);
            for v in c.iter_mut() {
                let x = *v / q;
                let y = match mode {
                    Requantize::Truncate => libm::floor(x),
                    Requantize::Round => libm::round(x),
                    Requantize::Tpdf => libm::round(x + rng.uniform() + rng.uniform() - 1.0),
                };
                *v = round24(y * q);
            }
        }
        P::FloatScale { scale, truncate } => {
            for v in c.iter_mut() {
                let y = *v / CODE_SCALE as f64 * scale;
                *v = if truncate {
                    libm::trunc(y)
                } else {
                    libm::round(y)
                };
            }
        }
        P::Int16Scale32767 { truncate } => {
            for v in c.iter_mut() {
                let y = *v / CODE_SCALE as f64 * 32_767.0;
                let i = if truncate {
                    libm::trunc(y)
                } else {
                    libm::round(y)
                };
                *v = i * 256.0;
            }
        }
        P::Noise { dbfs, seed } => {
            let sigma = CODE_SCALE as f64 * libm::pow(10.0, dbfs / 20.0);
            let mut rng = Rng::new(seed);
            c.iter_mut()
                .for_each(|v| *v = round24(*v + sigma * rng.gaussian()));
        }
        P::Resample(ratio) => c = resample(&c, ratio),
        P::PeakingEq { hz, db, q } => {
            // RBJ peaking biquad at 48 kHz.
            let a = libm::pow(10.0, db / 40.0);
            let w0 = 2.0 * core::f64::consts::PI * hz / 48_000.0;
            let alpha = libm::sin(w0) / (2.0 * q);
            let cw = libm::cos(w0);
            let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a);
            let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a);
            let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
            for v in c.iter_mut() {
                let x0 = *v;
                let y0 = (b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2) / a0;
                x2 = x1;
                x1 = x0;
                y2 = y1;
                y1 = y0;
                *v = round24(y0);
            }
        }
    }
    c
}

/// Windowed-sinc (Hann, 32 taps) resampler; output sample `k` is the input
/// at time `k / ratio`, low-passed below the lower of the two Nyquists.
fn resample(c: &[f64], ratio: f64) -> Vec<f64> {
    const HALF: i64 = 16;
    let cutoff = ratio.min(1.0) * 0.95;
    let n_out = (c.len() as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(n_out);
    for k in 0..n_out {
        let t = k as f64 / ratio;
        let center = libm::floor(t) as i64;
        let mut acc = 0.0;
        for j in (center - HALF + 1)..=(center + HALF) {
            if j < 0 || j as usize >= c.len() {
                continue;
            }
            let d = t - j as f64;
            let x = core::f64::consts::PI * d * cutoff;
            let sinc = if libm::fabs(x) < 1e-12 {
                1.0
            } else {
                libm::sin(x) / x
            };
            let w = 0.5 + 0.5 * libm::cos(core::f64::consts::PI * d / HALF as f64);
            acc += c[j as usize] * cutoff * sinc * w;
        }
        out.push(round24(acc));
    }
    out
}
