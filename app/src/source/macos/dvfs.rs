//! DVFS (frequency/voltage) tables from the SoC power manager.
//!
//! Apple Silicon does not expose a "current MHz" register. Frequency has to be
//! reconstructed: the `pmgr` device-tree node lists the discrete performance
//! states each block can run at, and IOReport reports how long the block spent
//! in each one (see [`super::ioreport`]). Multiplying the two gives an
//! effective clock — the same thing `powermetrics` prints.
//!
//! The tables are packed arrays of `(frequency, voltage)` `u32` pairs. Units
//! are **not** consistent between blocks on the same machine: on this M5 the
//! CPU tables are in kHz (max 4,464,000 = 4464 MHz) while the GPU table is in
//! Hz (max 1,578,000,000 = 1578 MHz), so the scale is detected from the
//! magnitude rather than assumed.

use super::iokit;

/// Device-tree path to the power manager.
const PMGR_PATH: &str = "IODeviceTree:/arm-io/pmgr";

/// Which block's performance-state table to read.
///
/// The `-sram` variants are used for the CPU clusters because the plain
/// `voltage-states1`/`5` entries describe a different rail; the SRAM tables are
/// the ones whose frequencies match the cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// Efficiency cluster.
    Ecpu,
    /// Performance cluster.
    Pcpu,
    Gpu,
}

impl Block {
    fn property(self) -> &'static str {
        match self {
            Block::Ecpu => "voltage-states1-sram",
            Block::Pcpu => "voltage-states5-sram",
            Block::Gpu => "voltage-states9",
        }
    }
}

/// One DVFS performance state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct State {
    pub mhz: f32,
    /// Rail voltage for this state, in volts. The tables pair every frequency
    /// with the voltage needed to sustain it, which is where the CPU "VID"
    /// reading comes from — there is no separate voltage sensor on Apple
    /// Silicon.
    pub volts: f32,
}

/// Available frequencies for a block, in MHz, in performance-state order.
///
/// Empty when the node or property is missing — every caller treats that as
/// "no frequency sensors for this block" rather than an error.
pub fn frequencies_mhz(block: Block) -> Vec<f32> {
    states(block).into_iter().map(|s| s.mhz).collect()
}

/// Full performance-state table (frequency + voltage).
pub fn states(block: Block) -> Vec<State> {
    let Some(entry) = iokit::entry_from_path(PMGR_PATH) else {
        return Vec::new();
    };
    let Some(props) = iokit::properties(entry.0) else {
        return Vec::new();
    };
    let Some(bytes) = iokit::dict_data(&props, block.property()) else {
        return Vec::new();
    };
    parse_states(&bytes)
}

/// Decode packed `(freq, voltage)` `u32` pairs.
fn parse_states(bytes: &[u8]) -> Vec<State> {
    let pairs: Vec<(u32, u32)> = bytes
        .chunks_exact(8)
        .map(|c| {
            (
                u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        })
        .collect();
    if pairs.is_empty() {
        return Vec::new();
    }

    // Detect the unit from the largest entry. No Apple SoC runs at 100 GHz, and
    // none has a 100 MHz *maximum*, so this threshold separates Hz from kHz
    // without needing a per-block table that would rot on the next chip.
    let max = pairs.iter().map(|(f, _)| *f).max().unwrap_or(0) as f64;
    let to_mhz: f64 = if max >= 100_000_000.0 { 1.0e6 } else { 1.0e3 };

    pairs
        .iter()
        .map(|(freq, mv)| State {
            mhz: (*freq as f64 / to_mhz) as f32,
            // Voltages are millivolts (790 => 0.790 V).
            volts: *mv as f32 / 1000.0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_tables_are_plausible_and_ascending() {
        for block in [Block::Ecpu, Block::Pcpu] {
            let states = frequencies_mhz(block);
            if states.is_empty() {
                crate::source::macos::absent(&format!("{block:?} DVFS table"));
                continue;
            }
            // Every Apple core sits between a few hundred MHz and ~6 GHz. A
            // unit-scale mistake lands far outside this on either side.
            for mhz in &states {
                assert!(
                    (100.0..=6000.0).contains(mhz),
                    "{block:?} state {mhz} MHz implies the kHz/Hz scale was misread"
                );
            }
            let top = states.last().copied().unwrap();
            assert!(top >= 2000.0, "{block:?} top state {top} MHz is too low");
        }
    }

    /// The GPU table is stored in Hz where the CPU tables are in kHz — this is
    /// the case the magnitude heuristic exists for.
    #[test]
    fn gpu_table_uses_a_different_unit_but_still_decodes_to_mhz() {
        let states = frequencies_mhz(Block::Gpu);
        if states.is_empty() {
            return crate::source::macos::absent("GPU DVFS table");
        }
        let top = states.last().copied().unwrap();
        assert!(
            (300.0..=4000.0).contains(&top),
            "GPU top state {top} MHz implies the Hz/kHz scale was misread"
        );
    }

    #[test]
    fn scale_detection_handles_both_units() {
        // kHz-encoded: 972 MHz and 4464 MHz.
        let khz = [972_000u32, 790, 4_464_000, 980]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<u8>>();
        assert_eq!(parse_states(&khz).iter().map(|s| s.mhz).collect::<Vec<_>>(), vec![972.0, 4464.0]);
        assert_eq!(parse_states(&khz)[0].volts, 0.790);

        // Hz-encoded: 338 MHz and 1578 MHz.
        let hz = [338_000_000u32, 500, 1_578_000_000, 900]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<u8>>();
        assert_eq!(parse_states(&hz).iter().map(|s| s.mhz).collect::<Vec<_>>(), vec![338.0, 1578.0]);
    }

    #[test]
    fn truncated_table_does_not_panic() {
        assert!(parse_states(&[]).is_empty());
        // Fewer than one full pair — chunks_exact drops the remainder.
        assert!(parse_states(&[1, 2, 3]).is_empty());
    }
}
