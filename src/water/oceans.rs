//! Which sheets of ocean-biome water are large enough to be seas.
//!
//! Size decides whether water is a sea, but *what* is measured matters more than
//! the threshold. Measuring the whole connected body of water does not work:
//! rivers glue an entire continent together, so on the world this was built
//! against the largest body is 350 million columns of which only 287 million is
//! ocean biome. Every inland lake that Minecraft painted an ocean biome onto
//! hangs off that body through some river and passes any size test you like.
//!
//! So the connectivity that counts here is ocean-biome water alone. A lake with
//! an ocean biome, reachable only through river-biome water, forms its own small
//! sheet and stays a lake.
//!
//! The pass runs at 4x4 cell resolution - the resolution Minecraft stores biomes
//! at - which makes it sixteen times cheaper than a column-level pass and costs
//! nothing in accuracy, because the input is per-cell to begin with.

use rayon::prelude::*;

use crate::water::components::{label_grid, merge_tiles, TileEdges, TileResult, NONE};
use crate::water::grid::{WorldGrid, REGION_CHUNKS};
use crate::world::biome::{BiomeFamily, BiomeRegistry};

/// Cells along one side of a region file (512 blocks / 4 blocks per cell).
const TILE_CELLS: usize = 128;
/// 4x4 cells per chunk, 32x32 chunks per region file.
const CELLS_PER_TILE: usize = REGION_CHUNKS * REGION_CHUNKS * 16;
const WORDS_PER_TILE: usize = CELLS_PER_TILE / 64;

/// Builds the cell key grid of one tile: `0` for ocean-biome water, else `NONE`.
fn ocean_keys(grid: &WorldGrid, registry: &BiomeRegistry, slot: usize, keys: &mut [u32]) -> bool {
    keys.fill(NONE);
    let region = &grid.regions[slot];
    let mut any = false;
    for cz in 0..REGION_CHUNKS {
        for cx in 0..REGION_CHUNKS {
            let Some(chunk) = region.chunk(cz * REGION_CHUNKS + cx) else {
                continue;
            };
            for (i, cell) in chunk.cells.iter().enumerate() {
                if cell.water_cols == 0 {
                    continue;
                }
                // An underground pool is not a sea however big it is.
                if cell.cave_cols * 2 >= cell.water_cols {
                    continue;
                }
                let Some(info) = registry.info(cell.biome) else {
                    continue;
                };
                if info.family != BiomeFamily::Ocean {
                    continue;
                }
                let gx = cx * 4 + (i % 4);
                let gz = cz * 4 + (i / 4);
                keys[gz * TILE_CELLS + gx] = 0;
                any = true;
            }
        }
    }
    any
}

/// Water columns in the cell at tile-cell coordinates `(gx, gz)`.
#[inline]
fn cell_columns(grid: &WorldGrid, slot: usize, gx: usize, gz: usize) -> u32 {
    let region = &grid.regions[slot];
    let chunk_index = (gz / 4) * REGION_CHUNKS + (gx / 4);
    match region.chunk(chunk_index) {
        Some(chunk) => chunk.cells[(gz % 4) * 4 + (gx % 4)].water_cols as u32,
        None => 0,
    }
}

/// Marks, per 4x4 cell, whether it belongs to a sheet of ocean water big enough
/// to be a sea.
pub struct OceanSheets {
    /// One bit per `(slot, chunk, cell)`, `WORDS_PER_TILE` words per region file.
    bits: Vec<u64>,
    /// Number of ocean sheets that reached the threshold.
    pub sea_sheets: u32,
    /// Column counts of the largest sheets, biggest first.
    pub largest: Vec<u32>,
}

impl OceanSheets {
    pub fn build(grid: &WorldGrid, registry: &BiomeRegistry, min_columns: u32) -> Self {
        let n = grid.regions.len();
        if n == 0 {
            return OceanSheets {
                bits: Vec::new(),
                sea_sheets: 0,
                largest: Vec::new(),
            };
        }

        // Pass A: label each tile's ocean cells and total their water columns.
        let mut tiles: Vec<TileResult> = (0..n)
            .into_par_iter()
            .map(|slot| {
                let mut keys = vec![NONE; TILE_CELLS * TILE_CELLS];
                let region = &grid.regions[slot];
                if !ocean_keys(grid, registry, slot, &mut keys) {
                    return empty_tile(region.region_x, region.region_z);
                }
                let labels = label_grid(TILE_CELLS, TILE_CELLS, &keys);
                // `sizes` carries water columns, not cells, so the threshold can
                // stay in the same unit as every other size in the tool.
                let mut sizes = vec![0u32; labels.count as usize];
                for gz in 0..TILE_CELLS {
                    for gx in 0..TILE_CELLS {
                        let label = labels.labels[gz * TILE_CELLS + gx];
                        if label != NONE {
                            sizes[label as usize] += cell_columns(grid, slot, gx, gz);
                        }
                    }
                }
                TileResult {
                    rx: region.region_x,
                    rz: region.region_z,
                    base: 0,
                    count: labels.count,
                    sizes,
                    edges: TileEdges::from_labels(&labels, &keys),
                }
            })
            .collect();

        // Nothing is filtered here - we need every sheet's size to judge it.
        let merged = merge_tiles(&mut tiles, 1);
        let sea_sheets = merged
            .sizes
            .iter()
            .filter(|s| **s >= min_columns)
            .count() as u32;
        let mut largest = merged.sizes.clone();
        largest.sort_unstable_by(|a, b| b.cmp(a));
        largest.truncate(48);

        // Pass B: mark the cells of the sheets that made it. The labelling is a
        // pure function of the key grid, so rebuilding it reproduces pass A
        // exactly and saves keeping 2509 label grids alive.
        let per_tile: Vec<Vec<u64>> = (0..n)
            .into_par_iter()
            .map(|slot| {
                let mut words = vec![0u64; WORDS_PER_TILE];
                let mut keys = vec![NONE; TILE_CELLS * TILE_CELLS];
                if !ocean_keys(grid, registry, slot, &mut keys) {
                    return words;
                }
                let labels = label_grid(TILE_CELLS, TILE_CELLS, &keys);
                let base = tiles[slot].base;
                for gz in 0..TILE_CELLS {
                    for gx in 0..TILE_CELLS {
                        let label = labels.labels[gz * TILE_CELLS + gx];
                        let dense = merged.resolve(base, label);
                        if dense == NONE || merged.sizes[dense as usize] < min_columns {
                            continue;
                        }
                        let bit = cell_bit(gx, gz);
                        words[bit / 64] |= 1u64 << (bit % 64);
                    }
                }
                words
            })
            .collect();

        let mut bits = Vec::with_capacity(n * WORDS_PER_TILE);
        for w in per_tile {
            bits.extend_from_slice(&w);
        }

        OceanSheets {
            bits,
            sea_sheets,
            largest,
        }
    }

    /// Whether the cell holding this column is part of a sea-sized ocean sheet.
    #[inline]
    pub fn is_sea_cell(&self, slot: usize, chunk_index: usize, cell: usize) -> bool {
        let gx = (chunk_index % REGION_CHUNKS) * 4 + (cell % 4);
        let gz = (chunk_index / REGION_CHUNKS) * 4 + (cell / 4);
        let bit = slot * CELLS_PER_TILE + cell_bit(gx, gz);
        match self.bits.get(bit / 64) {
            Some(w) => w & (1u64 << (bit % 64)) != 0,
            None => false,
        }
    }

    /// Same, addressed by world block coordinates.
    pub fn is_sea_at(&self, grid: &WorldGrid, x: i32, z: i32) -> bool {
        let Some(slot) = grid.region_slot(x >> 9, z >> 9) else {
            return false;
        };
        let chunk_index = (((z >> 4) & 31) as usize) * REGION_CHUNKS + (((x >> 4) & 31) as usize);
        let cell = (((z & 15) >> 2) as usize) * 4 + (((x & 15) >> 2) as usize);
        self.is_sea_cell(slot, chunk_index, cell)
    }
}

/// Bit index of a tile-cell within one tile's bit block.
#[inline]
fn cell_bit(gx: usize, gz: usize) -> usize {
    gz * TILE_CELLS + gx
}

fn empty_tile(rx: i32, rz: i32) -> TileResult {
    TileResult {
        rx,
        rz,
        base: 0,
        count: 0,
        sizes: Vec::new(),
        edges: TileEdges {
            edges: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water::grid::{CellInfo, ChunkWater, RegionWater};

    /// One region file, every chunk full of water, biome chosen per chunk column.
    fn region_with<F: Fn(usize, usize) -> u16>(rx: i32, rz: i32, biome_at: F) -> RegionWater {
        let mut region = RegionWater::new(rx, rz);
        for cz in 0..REGION_CHUNKS {
            for cx in 0..REGION_CHUNKS {
                let mut cw = ChunkWater::default();
                for z in 0..16 {
                    for x in 0..16 {
                        cw.set(x, z);
                    }
                }
                cw.water_cols = 256;
                let biome = biome_at(cx, cz);
                for cell in cw.cells.iter_mut() {
                    *cell = CellInfo {
                        biome,
                        surface_y: 62,
                        depth: 20,
                        water_cols: 16,
                        ice_cols: 0,
                        cave_cols: 0,
                    };
                }
                region.insert(cz * REGION_CHUNKS + cx, cw);
            }
        }
        region
    }

    #[test]
    fn a_big_sheet_of_ocean_counts_and_a_small_one_does_not() {
        let registry = BiomeRegistry::vanilla_only();
        let ocean = registry.id_of("minecraft:ocean");
        let river = registry.id_of("minecraft:river");
        // Chunk columns 0..8 are ocean (8 * 16 * 512 = 65 536 columns), the rest
        // is river. One region file is 512x512 = 262 144 columns.
        let grid = WorldGrid::build(vec![region_with(0, 0, |cx, _| {
            if cx < 8 {
                ocean
            } else {
                river
            }
        })]);

        let big = OceanSheets::build(&grid, &registry, 60_000);
        assert_eq!(big.sea_sheets, 1);
        assert!(big.is_sea_at(&grid, 10, 10));
        assert!(!big.is_sea_at(&grid, 400, 10), "river water is not a sea");

        // Same world, higher bar: the sheet no longer qualifies.
        let small = OceanSheets::build(&grid, &registry, 70_000);
        assert_eq!(small.sea_sheets, 0);
        assert!(!small.is_sea_at(&grid, 10, 10));
    }

    #[test]
    fn river_water_does_not_join_two_ocean_sheets() {
        let registry = BiomeRegistry::vanilla_only();
        let ocean = registry.id_of("minecraft:ocean");
        let river = registry.id_of("minecraft:river");
        // Two ocean strips of 4 chunk columns each (32 768 columns), separated by
        // a river strip. As one body of water they would clear 60 000 easily.
        let grid = WorldGrid::build(vec![region_with(0, 0, |cx, _| {
            if !(4..28).contains(&cx) {
                ocean
            } else {
                river
            }
        })]);
        let sheets = OceanSheets::build(&grid, &registry, 60_000);
        assert_eq!(
            sheets.sea_sheets, 0,
            "the river must not glue the two sheets into one sea"
        );
        assert_eq!(sheets.largest[0], 32_768);
        assert_eq!(sheets.largest[1], 32_768);
    }

    #[test]
    fn sheets_are_stitched_across_region_files() {
        let registry = BiomeRegistry::vanilla_only();
        let ocean = registry.id_of("minecraft:ocean");
        // Two neighbouring region files, ocean everywhere: one sheet of 524 288.
        let grid = WorldGrid::build(vec![
            region_with(0, 0, |_, _| ocean),
            region_with(1, 0, |_, _| ocean),
        ]);
        let sheets = OceanSheets::build(&grid, &registry, 400_000);
        assert_eq!(sheets.sea_sheets, 1);
        assert_eq!(sheets.largest[0], 524_288);
        assert!(sheets.is_sea_at(&grid, 10, 10));
        assert!(sheets.is_sea_at(&grid, 900, 10));
    }

    #[test]
    fn cave_water_never_forms_a_sea() {
        let registry = BiomeRegistry::vanilla_only();
        let ocean = registry.id_of("minecraft:ocean");
        let mut region = region_with(0, 0, |_, _| ocean);
        // Turn the whole region into roofed-over water.
        for cz in 0..REGION_CHUNKS {
            for cx in 0..REGION_CHUNKS {
                if let Some(_c) = region.chunk(cz * REGION_CHUNKS + cx) {
                    region.set_cave_for_test(cz * REGION_CHUNKS + cx);
                }
            }
        }
        let grid = WorldGrid::build(vec![region]);
        let sheets = OceanSheets::build(&grid, &registry, 1000);
        assert_eq!(sheets.sea_sheets, 0);
    }

    #[test]
    fn a_world_without_oceans_has_no_sheets() {
        let registry = BiomeRegistry::vanilla_only();
        let plains = registry.id_of("minecraft:plains");
        let grid = WorldGrid::build(vec![region_with(0, 0, |_, _| plains)]);
        let sheets = OceanSheets::build(&grid, &registry, 100);
        assert_eq!(sheets.sea_sheets, 0);
        assert!(!sheets.is_sea_at(&grid, 100, 100));
    }
}
