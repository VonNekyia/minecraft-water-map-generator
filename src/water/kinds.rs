//! The per-cell classification field, and the smoothing that makes it coherent.
//!
//! What a column of water *is* gets decided from its biome, and Minecraft's biome
//! grid is noisy at the 4x4 resolution it is stored at. A river bank cell reports
//! the land biome, a thin strip of river biome cuts across a lake, an ice edge
//! frays. Feeding that straight into connected-component labelling turns every
//! speckle into its own region: lakes hemmed with lake-coloured banks, a lake with
//! four blocks of river through the middle and lake again on the far side.
//!
//! Absorption cannot repair this afterwards. Merging a bank fragment into the
//! river renames it, and once absorption is allowed to rename things it also lets
//! rivers swallow the ponds they run through. The speckle has to go *before* the
//! labelling, not after.
//!
//! So the classification is built as a field first - one code per 4x4 cell for the
//! whole world - and then run through a majority filter. Anything narrower than
//! about two cells that disagrees with its surroundings is absorbed by them, which
//! is exactly the width of the artefacts the biome grid produces. Real features
//! survive because only *water* cells vote: a river four blocks wide running
//! through dry land has no dissenting neighbours at all.
//!
//! Cave water is frozen out of the filter entirely. Whether water can see the sky
//! is measured, not inferred, and no majority of neighbours should overturn it.

use rayon::prelude::*;

use crate::config;
use crate::water::classifier::{self, HydroInfo, Surroundings};
use crate::water::components::{label_grid, MergedComponents, NONE, TILE};
use crate::water::grid::{WorldGrid, REGION_CHUNKS};
use crate::water::model::{Temperature, WaterKind};
use crate::water::oceans::OceanSheets;
use crate::water::proximity::WaterProximity;
use crate::world::biome::{BiomeFamily, BiomeRegistry};

/// 4x4 cells per chunk, 32x32 chunks per region file.
const CELLS_PER_TILE: usize = REGION_CHUNKS * REGION_CHUNKS * 16;
/// Cells along one side of a region file.
const TILE_CELLS: usize = REGION_CHUNKS * 4;

/// Code for a cell that holds no water.
pub const NO_WATER: u8 = 0xFF;

/// Packs a classification into one byte: temperature, kind, ice, cave.
#[inline]
pub fn pack(kind: WaterKind, temperature: Temperature, icy: bool, cave: bool) -> u8 {
    temperature as u8 | ((kind as u8) << 2) | ((icy as u8) << 4) | ((cave as u8) << 5)
}

#[inline]
pub fn unpack(code: u8) -> Option<(WaterKind, Temperature, bool, bool)> {
    if code == NO_WATER {
        return None;
    }
    Some((
        WaterKind::from_u8((code >> 2) & 0b11)?,
        Temperature::from_u8(code & 0b11)?,
        code & 0b1_0000 != 0,
        code & 0b10_0000 != 0,
    ))
}

#[inline]
fn is_cave(code: u8) -> bool {
    code != NO_WATER && code & 0b10_0000 != 0
}

/// One round of majority filtering over a per-cell byte field.
///
/// Reads the whole field and writes a copy, so the result does not depend on the
/// order cells are visited in - and neighbours across a region file border come
/// from the same global array, so tile seams behave like the interior. `frozen`
/// cells neither change nor vote.
fn majority_filter<F>(grid: &WorldGrid, codes: &mut [u8], empty: u8, frozen: F) -> u64
where
    F: Fn(u8) -> bool + Sync,
{
    let n = grid.regions.len();
    let read = |x: i32, z: i32| -> u8 {
        let Some(slot) = grid.region_slot(x >> 9, z >> 9) else {
            return empty;
        };
        let gx = ((x >> 2) & (TILE_CELLS as i32 - 1)) as usize;
        let gz = ((z >> 2) & (TILE_CELLS as i32 - 1)) as usize;
        codes[slot * CELLS_PER_TILE + gz * TILE_CELLS + gx]
    };

    let updates: Vec<Vec<u8>> = (0..n)
        .into_par_iter()
        .map(|slot| {
            let region = &grid.regions[slot];
            let base_x = region.region_x * 512;
            let base_z = region.region_z * 512;
            let mut out = vec![empty; CELLS_PER_TILE];
            for gz in 0..TILE_CELLS {
                for gx in 0..TILE_CELLS {
                    let idx = gz * TILE_CELLS + gx;
                    let own = codes[slot * CELLS_PER_TILE + idx];
                    out[idx] = own;
                    if own == empty || frozen(own) {
                        continue;
                    }
                    let wx = base_x + gx as i32 * 4;
                    let wz = base_z + gz as i32 * 4;
                    let mut votes: [(u8, u8); 9] = [(empty, 0); 9];
                    let mut seen = 0usize;
                    for dz in -1..=1i32 {
                        for dx in -1..=1i32 {
                            let c = read(wx + dx * 4, wz + dz * 4);
                            if c == empty || frozen(c) {
                                continue;
                            }
                            match votes[..seen].iter_mut().find(|(v, _)| *v == c) {
                                Some(entry) => entry.1 += 1,
                                None => {
                                    votes[seen] = (c, 1);
                                    seen += 1;
                                }
                            }
                        }
                    }
                    // The mode wins; a tie leaves the cell alone.
                    let mut best = own;
                    let mut best_n = votes[..seen]
                        .iter()
                        .find(|(v, _)| *v == own)
                        .map(|(_, n)| *n)
                        .unwrap_or(0);
                    for (v, count) in &votes[..seen] {
                        if *count > best_n {
                            best = *v;
                            best_n = *count;
                        }
                    }
                    out[idx] = best;
                }
            }
            out
        })
        .collect();

    let mut changed = 0u64;
    for (slot, out) in updates.into_iter().enumerate() {
        let start = slot * CELLS_PER_TILE;
        for (i, c) in out.into_iter().enumerate() {
            if codes[start + i] != c {
                changed += 1;
            }
            codes[start + i] = c;
        }
    }
    changed
}

const NO_FAMILY: u8 = 0xFF;

/// Which family of water each 4x4 cell holds, denoised.
///
/// This is the *input* to the classification, and it has to be cleaned before the
/// fringe rule runs rather than after. The fringe rule dilates the river label by
/// [`config::FRINGE_CELL_RADIUS`] to repair the 4x4 quantisation along a bank -
/// but dilation applied to noise makes the noise bigger. One stray river cell
/// inside a lake grew into a band five cells wide cutting the lake in two.
/// Filtering the seeds first removes the stray cell; only then is the label grown.
pub struct FamilyField {
    codes: Vec<u8>,
    pub smoothed_cells: u64,
}

impl FamilyField {
    pub fn build(grid: &WorldGrid, registry: &BiomeRegistry) -> Self {
        Self::build_with_passes(grid, registry, config::FAMILY_SMOOTHING_PASSES)
    }

    /// Same, with an explicit pass count. Exposed for tests that want to isolate
    /// the distance transform from the denoising, or vice versa.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn build_with_passes(grid: &WorldGrid, registry: &BiomeRegistry, passes: usize) -> Self {
        let n = grid.regions.len();
        if n == 0 {
            return FamilyField {
                codes: Vec::new(),
                smoothed_cells: 0,
            };
        }
        let per_tile: Vec<Vec<u8>> = (0..n)
            .into_par_iter()
            .map(|slot| {
                let mut codes = vec![NO_FAMILY; CELLS_PER_TILE];
                let region = &grid.regions[slot];
                for cz in 0..REGION_CHUNKS {
                    for cx in 0..REGION_CHUNKS {
                        let Some(chunk) = region.chunk(cz * REGION_CHUNKS + cx) else {
                            continue;
                        };
                        for (i, cell) in chunk.cells.iter().enumerate() {
                            if cell.water_cols == 0 {
                                continue;
                            }
                            let Some(info) = registry.info(cell.biome) else {
                                continue;
                            };
                            let gx = cx * 4 + (i % 4);
                            let gz = cz * 4 + (i / 4);
                            codes[gz * TILE_CELLS + gx] = info.family as u8;
                        }
                    }
                }
                codes
            })
            .collect();

        let mut codes = Vec::with_capacity(n * CELLS_PER_TILE);
        for t in per_tile {
            codes.extend_from_slice(&t);
        }
        let mut smoothed = 0;
        for _ in 0..passes {
            smoothed += majority_filter(grid, &mut codes, NO_FAMILY, |_| false);
        }
        FamilyField {
            codes,
            smoothed_cells: smoothed,
        }
    }

    #[inline]
    pub fn at(&self, slot: usize, chunk_index: usize, cell: usize) -> Option<BiomeFamily> {
        family_from_u8(*self.codes.get(cell_index(slot, chunk_index, cell))?)
    }

    /// Same, addressed by world block coordinates.
    #[inline]
    pub fn at_world(&self, grid: &WorldGrid, x: i32, z: i32) -> Option<BiomeFamily> {
        let slot = grid.region_slot(x >> 9, z >> 9)?;
        let chunk_index = (((z >> 4) & 31) as usize) * REGION_CHUNKS + (((x >> 4) & 31) as usize);
        let cell = (((z & 15) >> 2) as usize) * 4 + (((x & 15) >> 2) as usize);
        self.at(slot, chunk_index, cell)
    }
}

fn family_from_u8(code: u8) -> Option<BiomeFamily> {
    Some(match code {
        0 => BiomeFamily::Land,
        1 => BiomeFamily::Ocean,
        2 => BiomeFamily::River,
        3 => BiomeFamily::Swamp,
        _ => return None,
    })
}

/// The classification of every 4x4 water cell in the world.
pub struct KindField {
    codes: Vec<u8>,
    /// Cells the majority filter changed.
    pub smoothed_cells: u64,
}

/// Everything the field needs from the earlier passes.
pub struct KindInputs<'a> {
    pub sheets: &'a OceanSheets,
    pub families: &'a FamilyField,
    pub hydro: &'a MergedComponents,
    pub hydro_infos: &'a [HydroInfo],
    /// Global label base of each region file, by slot.
    pub hydro_bases: &'a [u32],
}

impl KindField {
    pub fn build(
        grid: &WorldGrid,
        registry: &BiomeRegistry,
        inputs: &KindInputs<'_>,
        min_sea_columns: u32,
    ) -> Self {
        let n = grid.regions.len();
        if n == 0 {
            return KindField {
                codes: Vec::new(),
                smoothed_cells: 0,
            };
        }

        let per_tile: Vec<Vec<u8>> = (0..n)
            .into_par_iter()
            .map(|slot| classify_tile(grid, registry, inputs, min_sea_columns, slot))
            .collect();

        let mut codes = Vec::with_capacity(n * CELLS_PER_TILE);
        for t in per_tile {
            codes.extend_from_slice(&t);
        }

        let mut field = KindField {
            codes,
            smoothed_cells: 0,
        };
        for _ in 0..config::KIND_SMOOTHING_PASSES {
            field.smooth(grid);
        }
        field
    }

    /// Classification of one cell, or `None` where there is no water.
    #[inline]
    pub fn code_at(&self, slot: usize, chunk_index: usize, cell: usize) -> u8 {
        match self.codes.get(cell_index(slot, chunk_index, cell)) {
            Some(c) => *c,
            None => NO_WATER,
        }
    }

    /// One round of majority filtering, delegated to the shared implementation.
    /// Cave water does not vote and is never reclassified: whether water can see
    /// the sky is measured, not inferred, and no neighbourhood should overturn it.
    fn smooth(&mut self, grid: &WorldGrid) {
        self.smoothed_cells += majority_filter(grid, &mut self.codes, NO_WATER, is_cave);
    }
}

#[inline]
fn cell_index(slot: usize, chunk_index: usize, cell: usize) -> usize {
    let gx = (chunk_index % REGION_CHUNKS) * 4 + (cell % 4);
    let gz = (chunk_index / REGION_CHUNKS) * 4 + (cell / 4);
    slot * CELLS_PER_TILE + gz * TILE_CELLS + gx
}

/// Builds the raw, unsmoothed classification of one region file.
fn classify_tile(
    grid: &WorldGrid,
    registry: &BiomeRegistry,
    inputs: &KindInputs<'_>,
    min_sea_columns: u32,
    slot: usize,
) -> Vec<u8> {
    let mut codes = vec![NO_WATER; CELLS_PER_TILE];
    let region = &grid.regions[slot];

    let mut water_keys = vec![NONE; TILE * TILE];
    if !grid.water_keys(slot, &mut water_keys) {
        return codes;
    }
    // Deterministic: the same key grid yields exactly the hydrological labels.
    let labels = label_grid(TILE, TILE, &water_keys);
    let base = inputs.hydro_bases[slot];
    let proximity = WaterProximity::build(
        grid,
        region.region_x,
        region.region_z,
        registry,
        inputs.sheets,
        inputs.families,
    );

    for cz in 0..REGION_CHUNKS {
        for cx in 0..REGION_CHUNKS {
            let Some(chunk) = region.chunk(cz * REGION_CHUNKS + cx) else {
                continue;
            };
            for (i, cell) in chunk.cells.iter().enumerate() {
                if cell.water_cols == 0 {
                    continue;
                }
                let Some(info) = registry.info(cell.biome) else {
                    continue;
                };
                // The family that decides `kind` is the denoised one; `info` above
                // still supplies the per-cell temperature, which is not the noise
                // source that produces speckled regions.
                let Some(family) = inputs.families.at(slot, cz * REGION_CHUNKS + cx, i) else {
                    continue;
                };
                let cell_x = (i % 4) * 4;
                let cell_z = (i / 4) * 4;

                // A cell's water columns all belong to the same body in all but
                // pathological cases; the first one stands for the cell.
                let mut dense = NONE;
                'find: for lz in 0..4usize {
                    for lx in 0..4usize {
                        let (px, pz) = (cell_x + lx, cell_z + lz);
                        if !chunk.get(px, pz) {
                            continue;
                        }
                        let tile_i = (cz * 16 + pz) * TILE + cx * 16 + px;
                        dense = inputs.hydro.resolve(base, labels.labels[tile_i]);
                        break 'find;
                    }
                }
                if dense == NONE {
                    continue; // body of water too small to matter
                }

                let tile_x = cx * 16 + cell_x;
                let tile_z = cz * 16 + cell_z;
                let around = Surroundings {
                    near_ocean: proximity.near_ocean(tile_x, tile_z),
                    ocean_temperature: proximity.ocean_temperature(tile_x, tile_z),
                    fringe: proximity
                        .fringe(tile_x, tile_z)
                        .map(|f| (f.family, f.temperature)),
                };
                let cave = cell.cave_cols > 0 && cell.cave_cols * 2 >= cell.water_cols;
                let in_sea_sheet =
                    inputs
                        .sheets
                        .is_sea_cell(slot, cz * REGION_CHUNKS + cx, i);
                let kind = classifier::column_kind(
                    family,
                    &inputs.hydro_infos[dense as usize],
                    &around,
                    cave,
                    in_sea_sheet,
                    min_sea_columns,
                );
                let temperature = classifier::column_temperature(
                    family,
                    kind,
                    info.water_temperature,
                    &around,
                );
                let icy = cell.ice_cols > 0 && cell.ice_cols * 2 >= cell.water_cols;

                let gx = cx * 4 + (i % 4);
                let gz = cz * 4 + (i / 4);
                codes[gz * TILE_CELLS + gx] = pack(kind, temperature, icy, cave);
            }
        }
    }
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for kind in [
            WaterKind::Sea,
            WaterKind::River,
            WaterKind::Lake,
            WaterKind::Swamp,
        ] {
            for t in [Temperature::Warm, Temperature::Medium, Temperature::Cold] {
                for icy in [false, true] {
                    for cave in [false, true] {
                        let c = pack(kind, t, icy, cave);
                        assert_ne!(c, NO_WATER);
                        assert_eq!(unpack(c), Some((kind, t, icy, cave)));
                        assert_eq!(is_cave(c), cave);
                    }
                }
            }
        }
        assert_eq!(unpack(NO_WATER), None);
        assert!(!is_cave(NO_WATER));
    }
}
