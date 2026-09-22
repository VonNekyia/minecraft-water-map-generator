//! Add measured roof coverage after all water classification and merging.
//!
//! In height-filtered scans, covered and exposed columns belong to the same
//! hydrological field. Roof evidence must therefore be attached only after the
//! final geometry is settled, without changing its river/lake identity.

use rayon::prelude::*;

use crate::config;
use crate::water::grid::WorldGrid;
use crate::water::model::{Modifiers, Run, WaterRegion};

/// Refresh only the `CAVE` modifier from exact retained columns in each region.
///
/// This is a metadata pass, not a classification pass: no geometry, water kind,
/// biome attributes, or other modifiers change. Returns the number of regions
/// whose covered share meets `CAVE_MIN_SHARE`. Call after all lake detection and
/// merging, only for the height-filtered flow; legacy cave grouping is separate.
pub fn annotate_covered_water(regions: &mut [WaterRegion], grid: &WorldGrid) -> u32 {
    regions
        .par_iter_mut()
        .map(|region| {
            let mut columns = 0u64;
            let mut covered = 0u64;
            for run in &region.geometry.runs {
                columns += run.len() as u64;
                covered += covered_columns(grid, run);
            }
            let cave =
                columns > 0 && covered as f64 / columns as f64 >= config::CAVE_MIN_SHARE as f64;
            region.modifiers.set(Modifiers::CAVE, cave);
            u32::from(cave)
        })
        .sum()
}

/// Count each run in chunk-sized pieces. A chunk row fits in one mask word, so
/// even large oceans require one lookup per 16 columns rather than per block.
fn covered_columns(grid: &WorldGrid, run: &Run) -> u64 {
    let mut covered = 0u64;
    let mut x = run.x0;
    while x <= run.x1 {
        let end = run.x1.min((x & !15) + 15);
        if let Some(chunk) = grid.chunk_at(x, run.z) {
            let bit = ((run.z & 15) * 16 + (x & 15)) as usize;
            let width = (end - x + 1) as u32;
            let mask = ((1u64 << width) - 1) << (bit & 63);
            covered +=
                (chunk.mask[bit >> 6] & chunk.covered_mask[bit >> 6] & mask).count_ones() as u64;
        }
        if end == run.x1 {
            break;
        }
        x = end + 1;
    }
    covered
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::water::grid::{ChunkWater, RegionWater};
    use crate::water::model::{
        Bathymetry, Depth, RegionGeometry, Temperature, Vegetation, WaterKind,
    };

    fn grid(columns: &[(i32, i32, bool)]) -> WorldGrid {
        let mut tiles: BTreeMap<(i32, i32), BTreeMap<usize, ChunkWater>> = BTreeMap::new();
        for &(x, z, covered) in columns {
            let index = (((z >> 4) & 31) * 32 + ((x >> 4) & 31)) as usize;
            let chunk = tiles
                .entry((x >> 9, z >> 9))
                .or_default()
                .entry(index)
                .or_default();
            chunk.set((x & 15) as usize, (z & 15) as usize);
            chunk.water_cols += 1;
            if covered {
                chunk.set_covered((x & 15) as usize, (z & 15) as usize);
            }
        }
        WorldGrid::build(
            tiles
                .into_iter()
                .map(|((rx, rz), chunks)| {
                    let mut tile = RegionWater::new(rx, rz);
                    for (index, chunk) in chunks {
                        tile.insert(index, chunk);
                    }
                    tile
                })
                .collect(),
        )
    }

    fn region(kind: WaterKind, x0: i32, x1: i32, z: i32) -> WaterRegion {
        WaterRegion {
            id: 42,
            geometry: RegionGeometry {
                min_x: x0,
                min_z: z,
                max_x: x1,
                max_z: z,
                runs: vec![Run { z, x0, x1 }],
                column_count: (x1 - x0 + 1) as u32,
            },
            kind,
            temperature: Temperature::Warm,
            vegetation: Vegetation::Sparse,
            depth: Some(Depth::Normal),
            modifiers: Modifiers::DESERT | Modifiers::ICE,
            surface_y: 60,
            bathymetry: Bathymetry {
                mean_depth: 12,
                max_depth: 20,
                contour_shares: [255, 128, 0, 0],
            },
            dominant_biome: "minecraft:badlands".into(),
        }
    }

    #[test]
    fn covered_water_preserves_kind_desert_and_entire_region_geometry() {
        let grid = grid(&(0..16).map(|x| (x, 0, true)).collect::<Vec<_>>());
        for kind in [
            WaterKind::River,
            WaterKind::Lake,
            WaterKind::Swamp,
            WaterKind::Sea,
        ] {
            let original = region(kind, 0, 15, 0);
            let mut regions = vec![original.clone()];
            assert_eq!(annotate_covered_water(&mut regions, &grid), 1);
            assert!(regions[0].modifiers.contains(Modifiers::CAVE));
            regions[0].modifiers.remove(Modifiers::CAVE);
            // Compare all fields: roof evidence may not undo desert classification
            // or change any shape, depth, identity, or environmental attributes.
            assert_eq!(format!("{:?}", regions[0]), format!("{original:?}"));
        }
    }

    #[test]
    fn only_exact_columns_in_final_runs_contribute_to_coverage() {
        // Same cell, but only one of this region's three columns is covered.
        // The fourth covered column belongs to a different final region.
        let grid = grid(&[(0, 0, true), (1, 0, false), (2, 0, false), (3, 0, true)]);
        let mut regions = vec![region(WaterKind::River, 0, 2, 0)];
        regions[0].modifiers.insert(Modifiers::CAVE);
        assert_eq!(annotate_covered_water(&mut regions, &grid), 0);
        assert_eq!(regions[0].modifiers, Modifiers::DESERT | Modifiers::ICE);
    }

    #[test]
    fn half_covered_meets_threshold_across_negative_region_and_chunk_edges() {
        assert_eq!(config::CAVE_MIN_SHARE, 0.5);
        let grid = grid(&(-16..16).map(|x| (x, -1, x < 0)).collect::<Vec<_>>());
        let mut regions = vec![region(WaterKind::Lake, -16, 15, -1)];
        assert_eq!(annotate_covered_water(&mut regions, &grid), 1);
        assert!(regions[0].modifiers.contains(Modifiers::CAVE));
        regions[0].geometry.runs[0].x0 = -15;
        regions[0].geometry.min_x = -15;
        regions[0].geometry.column_count -= 1;
        assert_eq!(annotate_covered_water(&mut regions, &grid), 0);
        assert!(!regions[0].modifiers.contains(Modifiers::CAVE));
    }

    #[test]
    fn empty_geometry_does_not_gain_a_cave_modifier() {
        let mut regions = vec![region(WaterKind::River, 0, 0, 0)];
        regions[0].geometry = RegionGeometry::default();
        regions[0].modifiers.insert(Modifiers::CAVE);
        assert_eq!(
            annotate_covered_water(&mut regions, &WorldGrid::build(vec![])),
            0
        );
        assert_eq!(regions[0].modifiers, Modifiers::DESERT | Modifiers::ICE);
    }
}
