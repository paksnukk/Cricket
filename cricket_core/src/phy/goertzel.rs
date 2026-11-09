use core::f32::consts::PI;

/// Non-coherent Goertzel power for a single target frequency.
pub fn goertzel_power(samples: &[f32], target_freq: f32, sample_rate: u32) -> f32 {
    let n = samples.len() as f32;
    if n < 2.0 {
        return 0.0;
    }
    let k = (n * target_freq) / sample_rate as f32;
    let w = 2.0 * PI * k / n;
    let coeff = 2.0 * libm::cosf(w);

    let mut q1 = 0.0f32;
    let mut q2 = 0.0f32;
    for &sample in samples {
        let q0 = coeff * q1 - q2 + sample;
        q2 = q1;
        q1 = q0;
    }
    q1 * q1 + q2 * q2 - q1 * q2 * coeff
}

/// Goertzel power for one frequency and the window energy `sum(x^2)`, in one
/// pass over `samples` (`n` of them). Bit-identical to [`goertzel_power`] and
/// [`window_energy`] on the same samples.
pub fn goertzel_power_energy(
    samples: impl IntoIterator<Item = f32>,
    n: usize,
    target_freq: f32,
    sample_rate: u32,
) -> (f32, f32) {
    let nf = n as f32;
    if nf < 2.0 {
        return (0.0, samples.into_iter().map(|s| s * s).sum());
    }
    let k = (nf * target_freq) / sample_rate as f32;
    let w = 2.0 * PI * k / nf;
    let coeff = 2.0 * libm::cosf(w);

    let mut q1 = 0.0f32;
    let mut q2 = 0.0f32;
    let mut energy = 0.0f32;
    for sample in samples {
        let q0 = coeff * q1 - q2 + sample;
        q2 = q1;
        q1 = q0;
        energy += sample * sample;
    }
    (q1 * q1 + q2 * q2 - q1 * q2 * coeff, energy)
}

/// Result of a Goertzel tone-bank decision over one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneDetection {
    /// Index of the strongest tone.
    pub index: u8,
    /// Raw (un-normalized) Goertzel power of the strongest tone.
    pub power: f32,
    /// `10 * log10(best / runner_up)`.
    pub confidence_db: f32,
    /// Level-independent tone purity of the strongest tone, see [`tone_purity`].
    pub purity: f32,
    /// Window energy `sum(x^2)`.
    pub energy: f32,
}

/// Window energy `sum(x^2)`.
pub fn window_energy(samples: &[f32]) -> f32 {
    samples.iter().map(|&s| s * s).sum()
}

/// RMS in dBFS (full-scale sine = -3 dBFS) for a window of `len` samples with energy `energy`
/// (`-inf`-safe: returns a very small value for silence).
pub fn rms_dbfs(energy: f32, len: usize) -> f32 {
    if len == 0 {
        return -200.0;
    }
    let mean_sq = energy / len as f32;
    10.0 * libm::log10f(mean_sq.max(1e-20))
}

/// Level-independent tone purity: `power / (energy * N / 2)`.
///
/// For a steady sinusoid `x[n] = A sin(w n + phi)` filling the window at the
/// target frequency, the Goertzel power is `|X(w)|^2 ~= (A N / 2)^2` and the
/// window energy is `A^2 N / 2`, so `energy * N / 2 = (A N / 2)^2` and the
/// ratio is ~1.0 independent of `A`. Ramps, partial window fill, other tones
/// and noise all lower it; white noise gives ~`2 / N` per bin.
pub fn tone_purity(power: f32, energy: f32, n: usize) -> f32 {
    let denom = energy * n as f32 * 0.5;
    if denom > 0.0 {
        power / denom
    } else {
        0.0
    }
}

/// Pick the strongest tone among `tones`.
pub fn best_tone(samples: &[f32], tones: &[f32], sample_rate: u32) -> Option<ToneDetection> {
    if tones.is_empty() || samples.is_empty() {
        return None;
    }

    let mut best_idx = 0u8;
    let mut best_power = 0.0f32;
    let mut runner_up = 0.0f32;

    for (i, &freq) in tones.iter().enumerate() {
        let power = goertzel_power(samples, freq, sample_rate);
        if power > best_power {
            runner_up = best_power;
            best_power = power;
            best_idx = i as u8;
        } else if power > runner_up {
            runner_up = power;
        }
    }

    let confidence_db = if runner_up > 0.0 {
        10.0 * libm::log10f(best_power / runner_up)
    } else {
        100.0
    };
    let energy = window_energy(samples);

    Some(ToneDetection {
        index: best_idx,
        power: best_power,
        confidence_db,
        purity: tone_purity(best_power, energy, samples.len()),
        energy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{bytes_to_nibbles, encode_packet_bytes, interleave, Packet};
    use crate::modem::MAX_BODY_NIBBLES;
    use crate::phy::{ModemTx, PhyConfig};
    use crate::transport::TransportMessage;

    #[test]
    fn perfect_grid_demod_note60() {
        let config = PhyConfig::robust_repeat();
        let msg = TransportMessage::NoteOn {
            channel: 0,
            note: 60,
            velocity: 100,
        };
        let packet = Packet {
            msg_type: msg.msg_type(),
            seq: 0,
            payload: msg.to_payload(),
        };
        let bytes = encode_packet_bytes(&packet).unwrap();
        let mut nibbles = bytes_to_nibbles(&bytes);
        while nibbles.len() < MAX_BODY_NIBBLES {
            nibbles.push(0);
        }
        nibbles.truncate(MAX_BODY_NIBBLES);
        let interleaved = interleave(&nibbles, config.interleave_depth);

        let mut all_syms = Vec::new();
        all_syms.extend_from_slice(config.preamble());
        all_syms.extend_from_slice(&interleaved);

        let tx = ModemTx::new(config.clone());
        let audio = tx.synthesize_nibbles(&all_syms);
        let period = config.samples_per_symbol_period();
        let sym = config.symbol_samples();
        let tones = config.tone_table();

        for (i, &expect) in all_syms.iter().enumerate() {
            let start = i * period;
            let window = &audio[start..start + sym];
            let det = best_tone(window, &tones, config.sample_rate_hz).unwrap();
            assert_eq!(
                det.index, expect,
                "symbol {i}: expected {expect} got {det:?}"
            );
            // Aligned, ramped synthesized symbol: purity close to 1 at any level.
            assert!(det.purity > 0.9, "symbol {i}: purity {det:?}");
        }
    }

    #[test]
    fn power_energy_matches_separate_passes() {
        let sr = 48_000u32;
        let x: Vec<f32> = (0..288)
            .map(|i| (0.37 * i as f32).sin() * 0.5 + (0.011 * (i * i) as f32).cos() * 0.2)
            .collect();
        for f in [2500.0f32, 3000.0, 3700.0] {
            let (p, e) = goertzel_power_energy(x.iter().copied(), x.len(), f, sr);
            assert_eq!(p.to_bits(), goertzel_power(&x, f, sr).to_bits());
            assert_eq!(e.to_bits(), window_energy(&x).to_bits());
        }
    }

    #[test]
    fn purity_is_level_independent() {
        let sr = 48_000u32;
        for &(n, freq) in &[(288usize, 2500.0f32), (288, 2600.0), (144, 3700.0)] {
            let mut prev: Option<f32> = None;
            for &amp in &[1.0f32, 0.1, 0.001, 1e-5] {
                let x: Vec<f32> = (0..n)
                    .map(|i| amp * (2.0 * PI * freq * i as f32 / sr as f32 + 0.3).sin())
                    .collect();
                let p = tone_purity(goertzel_power(&x, freq, sr), window_energy(&x), n);
                assert!(
                    (p - 1.0).abs() < 0.03,
                    "n={n} f={freq} amp={amp}: purity {p}"
                );
                if let Some(q) = prev {
                    assert!((p - q).abs() < 1e-3);
                }
                prev = Some(p);
            }
        }
        assert_eq!(tone_purity(0.0, 0.0, 288), 0.0);
    }
}
