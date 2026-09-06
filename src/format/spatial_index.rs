//! Coarse spatial index for near-O(1) `x/z -> region` lookups.
//!
//! The world is covered by a grid of `1 << shift` sized cells (64x64 blocks by
//! default). Each cell says whether it is empty, holds exactly one region, or
//! points at a short candidate list. A consumer resolves a position with one
//! array index and, in the rare ambiguous case, a handful of geometry tests -
//! never a scan over all regions.

use crate::config;
use crate::water::model::WaterRegion;

use super::{CELL_EMPTY, CELL_INLINE};

pub struct SpatialIndex {
    pub shift: u32,
    /// Cell coordinate of the grid origin (`world_min_x >> shift`).
    pub origin_cell_x: i32,
    pub origin_cell_z: i32,
    pub cells_x: u32,
    pub cells_z: u32,
    /// `cells_x * cells_z` entries.
    pub cells: Vec<u32>,
    /// Overflow lists: `[count, id, id, ...]` sequences, referenced by cell value.
    pub lists: Vec<u32>,
}

#[allow(dead_code)]
impl SpatialIndex {
    pub fn build(regions: &[WaterRegion], bounds: (i32, i32, i32, i32)) -> SpatialIndex {
        let shift = config::SPATIAL_CELL_SHIFT;
        let (min_x, min_z, max_x, max_z) = bounds;
        let origin_cell_x = min_x >> shift;
        let origin_cell_z = min_z >> shift;
        let cells_x = ((max_x >> shift) - origin_cell_x + 1).max(1) as u32;
        let cells_z = ((max_z >> shift) - origin_cell_z + 1).max(1) as u32;

        // (cell index, region id) pairs, deduplicated by sorting.
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        for region in regions {
            for run in &region.geometry.runs {
                let cz = (run.z >> shift) - origin_cell_z;
                if cz < 0 || cz as u32 >= cells_z {
                    continue;
                }
                let c0 = (run.x0 >> shift) - origin_cell_x;
                let c1 = (run.x1 >> shift) - origin_cell_x;
                for cx in c0.max(0)..=c1.min(cells_x as i32 - 1) {
                    pairs.push((cz as u32 * cells_x + cx as u32, region.id));
                }
            }
        }
        pairs.sort_unstable();
        pairs.dedup();

        let mut cells = vec![CELL_EMPTY; (cells_x as usize) * (cells_z as usize)];
        let mut lists: Vec<u32> = Vec::new();

        let mut i = 0usize;
        while i < pairs.len() {
            let cell = pairs[i].0;
            let mut j = i;
            while j < pairs.len() && pairs[j].0 == cell {
                j += 1;
            }
            let n = j - i;
            if n == 1 {
                cells[cell as usize] = CELL_INLINE | pairs[i].1;
            } else {
                cells[cell as usize] = lists.len() as u32;
                lists.push(n as u32);
                for p in &pairs[i..j] {
                    lists.push(p.1);
                }
            }
            i = j;
        }

        SpatialIndex {
            shift,
            origin_cell_x,
            origin_cell_z,
            cells_x,
            cells_z,
            cells,
            lists,
        }
    }

    /// Candidate region ids for a world position.
    pub fn candidates(&self, x: i32, z: i32) -> &[u32] {
        let cx = (x >> self.shift) - self.origin_cell_x;
        let cz = (z >> self.shift) - self.origin_cell_z;
        if cx < 0 || cz < 0 || cx as u32 >= self.cells_x || cz as u32 >= self.cells_z {
            return &[];
        }
        let v = self.cells[(cz as u32 * self.cells_x + cx as u32) as usize];
        if v == CELL_EMPTY {
            return &[];
        }
        if v & CELL_INLINE != 0 {
            // Inline single ids are returned through the caller-visible slice by
            // pointing at the cell itself is not possible, so callers use
            // `lookup_single` for this case.
            return &[];
        }
        let start = v as usize;
        let n = self.lists[start] as usize;
        &self.lists[start + 1..start + 1 + n]
    }

    /// Single inline region id of a cell, if it has one.
    pub fn inline(&self, x: i32, z: i32) -> Option<u32> {
        let cx = (x >> self.shift) - self.origin_cell_x;
        let cz = (z >> self.shift) - self.origin_cell_z;
        if cx < 0 || cz < 0 || cx as u32 >= self.cells_x || cz as u32 >= self.cells_z {
            return None;
        }
        let v = self.cells[(cz as u32 * self.cells_x + cx as u32) as usize];
        if v != CELL_EMPTY && v & CELL_INLINE != 0 {
            Some(v & !CELL_INLINE)
        } else {
            None
        }
    }

    /// Resolves a position to a region id, exactly as a consumer would.
    pub fn lookup(&self, regions: &[WaterRegion], x: i32, z: i32) -> Option<u32> {
        if let Some(id) = self.inline(x, z) {
            let r = regions.get(id as usize)?;
            return if r.geometry.contains(x, z) {
                Some(id)
            } else {
                None
            };
        }
        for id in self.candidates(x, z) {
            if let Some(r) = regions.get(*id as usize) {
                if r.geometry.contains(x, z) {
                    return Some(*id);
                }
            }
        }
        None
    }

    pub fn byte_size(&self) -> usize {
        4 * 4 + 4 * self.cells.len() + 4 + 4 * self.lists.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water::model::*;

    fn region(id: u32, runs: Vec<Run>) -> WaterRegion {
        let min_x = runs.iter().map(|r| r.x0).min().unwrap();
        let max_x = runs.iter().map(|r| r.x1).max().unwrap();
        let min_z = runs.iter().map(|r| r.z).min().unwrap();
        let max_z = runs.iter().map(|r| r.z).max().unwrap();
        let column_count = runs.iter().map(|r| r.len()).sum();
        WaterRegion {
            id,
            geometry: RegionGeometry {
                min_x,
                min_z,
                max_x,
                max_z,
                runs,
                column_count,
            },
            kind: WaterKind::Lake,
            temperature: Temperature::Medium,
            vegetation: Vegetation::Normal,
            depth: None,
            modifiers: Modifiers::empty(),
            surface_y: 62,
            bathymetry: Bathymetry::default(),
            dominant_biome: "minecraft:plains".into(),
        }
    }

    #[test]
    fn lookup_finds_the_region_that_owns_a_column() {
        let a = region(0, vec![Run { z: 5, x0: 0, x1: 40 }, Run { z: 6, x0: 0, x1: 40 }]);
        let b = region(1, vec![Run { z: 200, x0: 300, x1: 310 }]);
        let regions = vec![a, b];
        let idx = SpatialIndex::build(&regions, (0, 0, 511, 511));

        assert_eq!(idx.lookup(&regions, 0, 5), Some(0));
        assert_eq!(idx.lookup(&regions, 40, 6), Some(0));
        assert_eq!(idx.lookup(&regions, 41, 6), None);
        assert_eq!(idx.lookup(&regions, 305, 200), Some(1));
        assert_eq!(idx.lookup(&regions, 305, 201), None);
        assert_eq!(idx.lookup(&regions, 9999, 9999), None);
    }

    #[test]
    fn cells_with_two_regions_use_an_overflow_list() {
        // Both regions live inside the same 64x64 cell.
        let a = region(0, vec![Run { z: 1, x0: 0, x1: 5 }]);
        let b = region(1, vec![Run { z: 1, x0: 20, x1: 25 }]);
        let regions = vec![a, b];
        let idx = SpatialIndex::build(&regions, (0, 0, 63, 63));
        assert!(idx.inline(0, 1).is_none());
        assert_eq!(idx.candidates(0, 1), &[0, 1]);
        assert_eq!(idx.lookup(&regions, 3, 1), Some(0));
        assert_eq!(idx.lookup(&regions, 22, 1), Some(1));
        assert_eq!(idx.lookup(&regions, 12, 1), None);
    }

    #[test]
    fn negative_world_coordinates_are_indexed() {
        let a = region(0, vec![Run { z: -100, x0: -300, x1: -290 }]);
        let regions = vec![a];
        let idx = SpatialIndex::build(&regions, (-512, -512, -1, -1));
        assert_eq!(idx.lookup(&regions, -295, -100), Some(0));
        assert_eq!(idx.lookup(&regions, -289, -100), None);
    }

    #[test]
    fn empty_cells_report_nothing() {
        let a = region(0, vec![Run { z: 0, x0: 0, x1: 1 }]);
        let regions = vec![a];
        let idx = SpatialIndex::build(&regions, (0, 0, 255, 255));
        assert_eq!(idx.lookup(&regions, 200, 200), None);
        assert!(idx.candidates(200, 200).is_empty());
        assert!(idx.inline(200, 200).is_none());
    }
}
