//! Exact inland-water mask in sparse 32 by 32 tiles. Tile storage includes dry
//! cells, but neither distance nor density ever creates water outside the input
//! run-length geometry.

use std::collections::{HashSet, VecDeque};

use rayon::prelude::*;

use crate::water::model::{Modifiers, WaterKind, WaterRegion};

pub const NONE: u32 = u32::MAX;
const SIDE: usize = 32;
const CELLS: usize = SIDE * SIDE;
const SHIFT: u32 = 5;

struct Tile {
    x: i32,
    z: i32,
    /// Left, right, north, south tile indices, or NONE.
    neighbors: [u32; 4],
}

struct DenseLookup {
    min_x: i32,
    min_z: i32,
    width: usize,
    height: usize,
    tiles: Vec<u32>,
}

pub struct Raster {
    /// Original input-region index, not the region's external ID. Dry cells
    /// inside allocated tiles use NONE.
    pub source: Vec<u32>,
    tiles: Vec<Tile>,
    lookup: Option<DenseLookup>,
}

impl Raster {
    pub fn from_regions(regions: &[WaterRegion]) -> Self {
        let eligible = |r: &WaterRegion| {
            matches!(r.kind, WaterKind::River | WaterKind::Lake)
                && !r.modifiers.contains(Modifiers::CAVE)
        };
        let mut occupied = HashSet::new();
        for region in regions.iter().filter(|r| eligible(r)) {
            for run in &region.geometry.runs {
                for tx in (run.x0 >> SHIFT)..=(run.x1 >> SHIFT) {
                    occupied.insert((run.z >> SHIFT, tx));
                }
            }
        }
        let mut keys: Vec<_> = occupied.into_iter().collect();
        keys.sort_unstable();
        assert!(
            keys.len() <= (u32::MAX as usize) / CELLS,
            "inland raster exceeds its 32-bit cell index capacity"
        );
        let tiles: Vec<_> = keys
            .into_iter()
            .map(|(z, x)| Tile {
                x,
                z,
                neighbors: [NONE; 4],
            })
            .collect();
        let lookup = if tiles.is_empty() {
            None
        } else {
            let min_x = tiles.iter().map(|t| t.x).min().unwrap();
            let max_x = tiles.iter().map(|t| t.x).max().unwrap();
            let min_z = tiles.first().unwrap().z;
            let max_z = tiles.last().unwrap().z;
            let width = (i64::from(max_x) - i64::from(min_x) + 1) as usize;
            let height = (i64::from(max_z) - i64::from(min_z) + 1) as usize;
            let area = width.checked_mul(height);
            // Normal world bounds cost only a few MB. Widely separated islands
            // must not turn this sparse raster into an enormous dense allocation.
            let limit = tiles.len().saturating_mul(64).clamp(65_536, 16_777_216);
            area.filter(|&n| n <= limit).map(|n| {
                let mut slots = vec![NONE; n];
                for (i, tile) in tiles.iter().enumerate() {
                    let x = (tile.x - min_x) as usize;
                    let z = (tile.z - min_z) as usize;
                    slots[z * width + x] = i as u32;
                }
                DenseLookup {
                    min_x,
                    min_z,
                    width,
                    height,
                    tiles: slots,
                }
            })
        };
        let mut raster = Self {
            source: vec![NONE; tiles.len() * CELLS],
            tiles,
            lookup,
        };
        for i in 0..raster.tiles.len() {
            let (x, z) = (raster.tiles[i].x, raster.tiles[i].z);
            raster.tiles[i].neighbors = [
                raster.tile_at(x - 1, z),
                raster.tile_at(x + 1, z),
                raster.tile_at(x, z - 1),
                raster.tile_at(x, z + 1),
            ];
        }
        for (region_index, region) in regions.iter().enumerate().filter(|(_, r)| eligible(r)) {
            assert!(region_index < NONE as usize);
            for run in &region.geometry.runs {
                for tx in (run.x0 >> SHIFT)..=(run.x1 >> SHIFT) {
                    let tile = raster.tile_at(tx, run.z >> SHIFT) as usize;
                    let left = run.x0.max(tx << SHIFT);
                    let right = run.x1.min((tx << SHIFT) + SIDE as i32 - 1);
                    let start = tile * CELLS + (run.z as usize & 31) * SIDE + (left as usize & 31);
                    raster.source[start..start + (right - left + 1) as usize]
                        .fill(region_index as u32);
                }
            }
        }
        raster
    }

    #[inline]
    fn tile_at(&self, x: i32, z: i32) -> u32 {
        if let Some(lookup) = &self.lookup {
            let dx = i64::from(x) - i64::from(lookup.min_x);
            let dz = i64::from(z) - i64::from(lookup.min_z);
            if dx < 0 || dz < 0 || dx >= lookup.width as i64 || dz >= lookup.height as i64 {
                return NONE;
            }
            lookup.tiles[dz as usize * lookup.width + dx as usize]
        } else {
            self.tiles
                .binary_search_by_key(&(z, x), |t| (t.z, t.x))
                .map_or(NONE, |i| i as u32)
        }
    }

    pub fn len(&self) -> usize {
        self.source.len()
    }

    #[inline]
    pub fn coords(&self, i: usize) -> (i32, i32) {
        let tile = &self.tiles[i / CELLS];
        (
            (tile.x << SHIFT) + (i % SIDE) as i32,
            (tile.z << SHIFT) + ((i % CELLS) / SIDE) as i32,
        )
    }

    /// Returns the allocated cell even if that cell is dry. Missing tiles have
    /// no cell index; use is_water to test indices returned by this method.
    #[inline]
    pub fn index_at(&self, x: i32, z: i32) -> Option<usize> {
        let tile = self.tile_at(x >> SHIFT, z >> SHIFT);
        (tile != NONE)
            .then_some(tile as usize * CELLS + (z as usize & 31) * SIDE + (x as usize & 31))
    }

    #[inline]
    pub fn is_water(&self, i: usize) -> bool {
        self.source[i] != NONE
    }

    /// Four cardinal neighbors, including dry cells in allocated tiles.
    #[inline]
    pub fn neighbors(&self, i: usize) -> [Option<usize>; 4] {
        let tile = &self.tiles[i / CELLS];
        let local = i % CELLS;
        let x = local % SIDE;
        let z = local / SIDE;
        let across = |direction: usize, offset: usize| {
            let other = tile.neighbors[direction];
            (other != NONE).then_some(other as usize * CELLS + offset)
        };
        [
            if x > 0 {
                Some(i - 1)
            } else {
                across(0, local + SIDE - 1)
            },
            if x + 1 < SIDE {
                Some(i + 1)
            } else {
                across(1, local - (SIDE - 1))
            },
            if z > 0 {
                Some(i - SIDE)
            } else {
                across(2, local + CELLS - SIDE)
            },
            if z + 1 < SIDE {
                Some(i + SIDE)
            } else {
                across(3, local - (CELLS - SIDE))
            },
        ]
    }

    /// Manhattan distance to dry land: shore water is 1, dry cells are 0.
    /// Missing tiles also count as dry land. Distances saturate at u16::MAX.
    pub fn distances(&self) -> Vec<u16> {
        let mut distance = vec![0; self.len()];
        let mut queue = VecDeque::<u32>::new();
        for (i, &source) in self.source.iter().enumerate() {
            if source == NONE {
                continue;
            }
            if self
                .neighbors(i)
                .iter()
                .any(|n| n.is_none_or(|j| !self.is_water(j)))
            {
                distance[i] = 1;
                queue.push_back(i as u32);
            } else {
                distance[i] = u16::MAX;
            }
        }
        while let Some(i) = queue.pop_front() {
            let next = distance[i as usize].saturating_add(1);
            for neighbor in self.neighbors(i as usize).into_iter().flatten() {
                if distance[neighbor] > next {
                    distance[neighbor] = next;
                    queue.push_back(neighbor as u32);
                }
            }
        }
        distance
    }

    /// Exact square-window water fractions at radii 8, 16, 32 blocks, rounded
    /// to 0..=255. The denominator includes dry/missing cells outside the mask.
    /// Dry output cells stay zero. Each worker uses one small integral image.
    pub fn densities(&self) -> Vec<[u8; 3]> {
        const HALO_SIDE: usize = SIDE * 3;
        const STRIDE: usize = HALO_SIDE + 1;
        let mut density = vec![[0; 3]; self.len()];
        density
            .par_chunks_mut(CELLS)
            .enumerate()
            .for_each(|(tile_index, output)| {
                let tile = &self.tiles[tile_index];
                let mut nearby = [NONE; 9];
                for dz in 0..3 {
                    for dx in 0..3 {
                        nearby[dz * 3 + dx] =
                            self.tile_at(tile.x + dx as i32 - 1, tile.z + dz as i32 - 1);
                    }
                }
                let mut integral = [0u32; STRIDE * STRIDE];
                for z in 0..HALO_SIDE {
                    let mut row_sum = 0u32;
                    for x in 0..HALO_SIDE {
                        let source_tile = nearby[(z / SIDE) * 3 + x / SIDE];
                        if source_tile != NONE {
                            let source_index =
                                source_tile as usize * CELLS + (z % SIDE) * SIDE + x % SIDE;
                            row_sum += u32::from(self.is_water(source_index));
                        }
                        integral[(z + 1) * STRIDE + x + 1] = integral[z * STRIDE + x + 1] + row_sum;
                    }
                }
                for (local, value) in output.iter_mut().enumerate() {
                    if !self.is_water(tile_index * CELLS + local) {
                        continue;
                    }
                    let x = SIDE + local % SIDE;
                    let z = SIDE + local / SIDE;
                    for (scale, radius) in [8, 16, 32].into_iter().enumerate() {
                        let (x0, x1) = (x - radius, x + radius + 1);
                        let (z0, z1) = (z - radius, z + radius + 1);
                        let count = integral[z1 * STRIDE + x1] + integral[z0 * STRIDE + x0]
                            - integral[z0 * STRIDE + x1]
                            - integral[z1 * STRIDE + x0];
                        let area = ((radius * 2 + 1) * (radius * 2 + 1)) as u32;
                        value[scale] = ((count * 255 + area / 2) / area) as u8;
                    }
                }
            });
        density
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water::model::{Bathymetry, RegionGeometry, Run, Temperature, Vegetation};

    fn rectangle(x0: i32, z0: i32, x1: i32, z1: i32) -> WaterRegion {
        WaterRegion {
            id: 999,
            geometry: RegionGeometry {
                min_x: x0,
                min_z: z0,
                max_x: x1,
                max_z: z1,
                runs: (z0..=z1).map(|z| Run { z, x0, x1 }).collect(),
                column_count: ((x1 - x0 + 1) * (z1 - z0 + 1)) as u32,
            },
            kind: WaterKind::River,
            temperature: Temperature::Medium,
            vegetation: Vegetation::Normal,
            depth: None,
            modifiers: Modifiers::empty(),
            surface_y: 63,
            bathymetry: Bathymetry::default(),
            dominant_biome: "minecraft:river".into(),
        }
    }

    #[test]
    fn negative_coordinates_and_tile_seams_preserve_exact_sources() {
        let regions = [rectangle(-33, -1, 33, 1), rectangle(64, 64, 64, 64)];
        let raster = Raster::from_regions(&regions);
        assert_eq!(raster.source.iter().filter(|&&s| s != NONE).count(), 202);
        for z in -1..=1 {
            for x in -33..=33 {
                let i = raster.index_at(x, z).unwrap();
                assert_eq!(raster.coords(i), (x, z));
                assert_eq!(raster.source[i], 0);
                for (n, expected) in raster.neighbors(i).into_iter().zip([
                    (x - 1, z),
                    (x + 1, z),
                    (x, z - 1),
                    (x, z + 1),
                ]) {
                    assert_eq!(
                        n.map(|j| raster.coords(j)),
                        raster.index_at(expected.0, expected.1).map(|_| expected)
                    );
                }
            }
        }
        assert_eq!(raster.source[raster.index_at(64, 64).unwrap()], 1);
        assert!(raster.index_at(1024, 1024).is_none());
        assert!(!raster.is_water(raster.index_at(65, 64).unwrap()));
    }

    #[test]
    fn channel_distance_crosses_tile_seams_and_keeps_dry_cells_zero() {
        let raster = Raster::from_regions(&[rectangle(-64, -2, 64, 2)]);
        let distance = raster.distances();
        for x in [-32, -1, 0, 31, 32] {
            assert_eq!(distance[raster.index_at(x, 0).unwrap()], 3);
            assert_eq!(distance[raster.index_at(x, 1).unwrap()], 2);
            assert_eq!(distance[raster.index_at(x, 2).unwrap()], 1);
        }
        assert_eq!(distance[raster.index_at(-64, 0).unwrap()], 1);
        assert_eq!(distance[raster.index_at(0, 3).unwrap()], 0);
    }

    #[test]
    fn island_is_a_shore_and_missing_tiles_are_dry() {
        let mut ring = rectangle(-65, -65, 65, 65);
        ring.geometry.runs = (-65..=65)
            .flat_map(|z| {
                if z == 0 {
                    vec![Run { z, x0: -65, x1: -1 }, Run { z, x0: 1, x1: 65 }]
                } else {
                    vec![Run { z, x0: -65, x1: 65 }]
                }
            })
            .collect();
        ring.geometry.column_count -= 1;
        let raster = Raster::from_regions(&[ring, rectangle(320, 320, 351, 351)]);
        let distance = raster.distances();
        for (x, z, expected) in [
            (0, 0, 0),
            (0, 1, 1),
            (2, 3, 5),
            (-64, -64, 2),
            (320, 334, 1),
            (335, 335, 16),
        ] {
            assert_eq!(distance[raster.index_at(x, z).unwrap()], expected);
        }
    }

    #[test]
    fn density_matches_square_window_counts_at_all_three_scales() {
        let raster = Raster::from_regions(&[rectangle(-64, -2, 64, 2)]);
        let density = raster.densities();
        for x in [-32, -1, 0, 31, 32] {
            let got = density[raster.index_at(x, 0).unwrap()];
            for (scale, radius) in [8u32, 16, 32].into_iter().enumerate() {
                let width = radius * 2 + 1;
                let expected = (width * 5 * 255 + width * width / 2) / (width * width);
                assert_eq!(got[scale], expected as u8);
            }
        }
        assert_eq!(density[raster.index_at(0, 3).unwrap()], [0, 0, 0]);
        let full = Raster::from_regions(&[rectangle(-64, -64, 64, 64)]);
        assert_eq!(
            full.densities()[full.index_at(0, 0).unwrap()],
            [255, 255, 255]
        );
    }

    #[test]
    fn protected_regions_never_seed_the_inland_mask() {
        let mut regions = vec![rectangle(0, 0, 0, 0); 4];
        regions[0].kind = WaterKind::Sea;
        regions[1].kind = WaterKind::Swamp;
        regions[2].modifiers = Modifiers::CAVE;
        regions[3] = rectangle(1, 0, 1, 0);
        let raster = Raster::from_regions(&regions);
        assert_eq!(raster.source.iter().filter(|&&s| s != NONE).count(), 1);
        assert!(!raster.is_water(raster.index_at(0, 0).unwrap()));
        assert_eq!(raster.source[raster.index_at(1, 0).unwrap()], 3);
        assert_eq!(Raster::from_regions(&[]).len(), 0);
    }

    #[test]
    fn very_distant_tiles_use_bounded_sparse_lookup() {
        let raster = Raster::from_regions(&[
            rectangle(-30_000_000, -30_000_000, -30_000_000, -30_000_000),
            rectangle(30_000_000, 30_000_000, 30_000_000, 30_000_000),
        ]);
        assert!(raster.lookup.is_none());
        assert_eq!(raster.len(), 2 * CELLS);
        for (x, z) in [(-30_000_000, -30_000_000), (30_000_000, 30_000_000)] {
            let i = raster.index_at(x, z).unwrap();
            assert_eq!(raster.coords(i), (x, z));
            assert!(raster.is_water(i));
            assert_eq!(raster.distances()[i], 1);
        }
    }
}
