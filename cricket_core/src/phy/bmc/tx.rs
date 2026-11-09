use super::{frame_intervals, BmcParams};
use crate::phy::{PhyConfig, PhyTx};
use alloc::vec::Vec;
use core::f32::consts::PI;

/// Encodes nibble streams as shaped biphase-mark bursts (see module docs).
pub struct BmcTx {
    config: PhyConfig,
    params: BmcParams,
}

impl BmcTx {
    /// Panics if `config` does not select [`Modulation::Bmc`](crate::phy::Modulation::Bmc)
    /// (the modem builds its phys through [`crate::phy::PhyFactory`], which
    /// cannot get this wrong).
    pub fn new(config: PhyConfig) -> Self {
        let params = config
            .bmc()
            .cloned()
            .unwrap_or_else(|| panic!("BmcTx needs a BMC PhyConfig"));
        Self { config, params }
    }

    pub fn config(&self) -> &PhyConfig {
        &self.config
    }

    /// One burst: preamble, sync, `body_nibbles` (MSB first), postamble, tail.
    pub fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32> {
        // Cell levels (+1 / -1), one per half cell.
        let intervals = frame_intervals(&self.params, body_nibbles);
        let mut cells: Vec<i8> =
            Vec::with_capacity(self.params.frame_half_cells(body_nibbles.len()));
        let mut level = 1i8;
        for &units in &intervals {
            for _ in 0..units {
                cells.push(level);
            }
            level = -level;
        }
        cells.push(level); // tail

        let sr = self.config.sample_rate_hz;
        let half = self.params.half_cell_samples(sr);
        let width = self.params.edge_frac.clamp(0.0, 1.0);
        let amp = self.config.amplitude * core::f32::consts::FRAC_1_SQRT_2;
        let n_cells = cells.len() as i64;
        let cell = |i: i64| -> f32 {
            if i < 0 || i >= n_cells {
                0.0
            } else {
                cells[i as usize] as f32
            }
        };

        // Sample n is at time (n - lead) / half cells; the lead lets the first
        // (half-width) onset ramp start at sample 0.
        let lead = 0.5 * width * half;
        let total = self.params.frame_samples(sr, body_nibbles.len());
        let mut out = Vec::with_capacity(total);
        for n in 0..total {
            let t = (n as f32 - lead) / half;
            let b = libm::roundf(t);
            let d = t - b;
            let bi = b as i64;
            let v = if width > 0.0 && libm::fabsf(d) < 0.5 * width {
                let before = cell(bi - 1);
                let after = cell(bi);
                let s = 0.5 + 0.5 * libm::sinf(PI * d / width);
                before + (after - before) * s
            } else {
                cell(libm::floorf(t) as i64)
            };
            out.push(v * amp);
        }
        out
    }
}

impl PhyTx for BmcTx {
    fn synthesize_framed(&self, body_nibbles: &[u8]) -> Vec<f32> {
        BmcTx::synthesize_framed(self, body_nibbles)
    }
}
