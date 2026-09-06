//! Sea level detection.
//!
//! Rather than hard-coding vanilla's 63 we take the water surface Y of every
//! column in an ocean biome, build a histogram and use the dominant height. Ocean
//! surfaces are perfectly flat, so the histogram of a real world has one very
//! sharp peak; a world generated with a custom sea level lands on its own value.
//!
//! The reported sea level follows Minecraft's own convention: the topmost water
//! block sits at `sea_level - 1`.

use crate::config;
use crate::water::scanner::{HIST_LEN, HIST_OFFSET};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeaLevelSource {
    /// Peak of the ocean-biome water surface histogram.
    OceanHistogram,
    /// Peak of the overall water surface histogram (no ocean biomes found).
    AllWaterHistogram,
    /// Neither histogram was conclusive.
    Fallback,
}

impl SeaLevelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SeaLevelSource::OceanHistogram => "ocean-histogram",
            SeaLevelSource::AllWaterHistogram => "all-water-histogram",
            SeaLevelSource::Fallback => "fallback",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SeaLevel {
    pub value: i16,
    pub source: SeaLevelSource,
    pub samples: u64,
    /// Share of samples that landed on the winning surface height.
    pub confidence: f64,
}

/// Picks the dominant surface height out of a histogram.
fn dominant(hist: &[u64; HIST_LEN]) -> Option<(i32, u64, u64)> {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return None;
    }
    let (idx, best) = hist
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| **c)
        .map(|(i, c)| (i, *c))?;
    Some((idx as i32 - HIST_OFFSET, best, total))
}

/// Determines the sea level from the two histograms collected during the scan.
pub fn detect(ocean_hist: &[u64; HIST_LEN], all_hist: &[u64; HIST_LEN]) -> SeaLevel {
    if let Some((y, best, total)) = dominant(ocean_hist) {
        let share = best as f64 / total as f64;
        if total >= config::SEA_LEVEL_MIN_SAMPLES && share >= config::SEA_LEVEL_MIN_SHARE {
            return SeaLevel {
                value: (y + 1) as i16,
                source: SeaLevelSource::OceanHistogram,
                samples: total,
                confidence: share,
            };
        }
    }

    if let Some((y, best, total)) = dominant(all_hist) {
        let share = best as f64 / total as f64;
        if total >= config::SEA_LEVEL_MIN_SAMPLES && share >= config::SEA_LEVEL_MIN_SHARE {
            return SeaLevel {
                value: (y + 1) as i16,
                source: SeaLevelSource::AllWaterHistogram,
                samples: total,
                confidence: share,
            };
        }
    }

    SeaLevel {
        value: config::FALLBACK_SEA_LEVEL,
        source: SeaLevelSource::Fallback,
        samples: all_hist.iter().sum(),
        confidence: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(entries: &[(i32, u64)]) -> Box<[u64; HIST_LEN]> {
        let mut h = Box::new([0u64; HIST_LEN]);
        for (y, c) in entries {
            h[(*y + HIST_OFFSET) as usize] = *c;
        }
        h
    }

    #[test]
    fn dominant_ocean_surface_wins() {
        // A vanilla-ish world: ocean surfaces at y=62 => sea level 63.
        let ocean = hist(&[(62, 900_000), (61, 4_000), (63, 1_000)]);
        let all = hist(&[(62, 900_000), (70, 50_000)]);
        let sl = detect(&ocean, &all);
        assert_eq!(sl.value, 63);
        assert_eq!(sl.source, SeaLevelSource::OceanHistogram);
        assert!(sl.confidence > 0.99);
    }

    #[test]
    fn custom_sea_level_is_detected() {
        let ocean = hist(&[(95, 500_000)]);
        let all = hist(&[(95, 500_000)]);
        assert_eq!(detect(&ocean, &all).value, 96);
    }

    #[test]
    fn falls_back_to_all_water_without_oceans() {
        let ocean = hist(&[]);
        let all = hist(&[(40, 200_000), (41, 1_000)]);
        let sl = detect(&ocean, &all);
        assert_eq!(sl.value, 41);
        assert_eq!(sl.source, SeaLevelSource::AllWaterHistogram);
    }

    #[test]
    fn too_few_samples_fall_back_to_the_configured_default() {
        let ocean = hist(&[(20, 5)]);
        let all = hist(&[(20, 5)]);
        let sl = detect(&ocean, &all);
        assert_eq!(sl.value, config::FALLBACK_SEA_LEVEL);
        assert_eq!(sl.source, SeaLevelSource::Fallback);
    }

    #[test]
    fn a_flat_noisy_histogram_is_rejected() {
        // 100 equally likely heights: no dominant water level.
        let mut entries = Vec::new();
        for y in 0..100 {
            entries.push((y, 1_000));
        }
        let h = hist(&entries);
        let sl = detect(&h, &h);
        assert_eq!(sl.source, SeaLevelSource::Fallback);
    }
}
