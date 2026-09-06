//! Phase 2: turn the scanned water grid into classified water regions.
//!
//! Tile-parallel passes, each stitched together with a union-find:
//!
//! | pass | key per column                       | produces                      |
//! |------|--------------------------------------|-------------------------------|
//! | 1    | "is water"                           | hydrological bodies           |
//! | 1b   | "is ocean-biome water" (per cell)    | sheets of ocean and their size|
//! | 1c   | -                                    | the denoised classification   |
//! | 2    | `(kind, temperature, ice)` signature | classification regions + runs |
//! | 3    | -                                    | attributes and RLE geometry   |
//!
//! Pass 1 exists so the classification can tell a landlocked lake from a bay:
//! whether a body of water reaches the sea at all is not a local property. Pass 1c
//! turns that, the ocean sheets and the biomes into one classification per 4x4
//! cell and *smooths it* - see [`KindField`] for why that has to happen before the
//! labelling rather than after.
//!
//! [`absorb`] then puts back together the pieces the signature split produced, and
//! a final shape correction overrules the biome where the geometry is
//! unambiguous.

use std::collections::HashMap;

use rayon::prelude::*;

use crate::water::absorb;
use crate::water::classifier::{self, HydroInfo, RegionAccum};
use crate::water::components::{
    label_grid, merge_tiles, TileEdges, TileResult, NONE, TILE,
};
use crate::water::grid::{ChunkFlags, WorldGrid, REGION_CHUNKS};
use crate::water::kinds::{self, KindField};
use crate::water::model::*;
use crate::water::oceans::OceanSheets;
use crate::water::proximity::DrylandProximity;
use crate::world::biome::{BiomeRegistry, BiomeTraits};

#[derive(Clone, Debug, Default)]
pub struct RegionStats {
    pub hydro_bodies: u32,
    /// Bodies of water large enough, and oceanic enough, to be called a sea.
    pub sea_bodies: u32,
    /// `(columns, ocean_columns)` of the largest bodies, biggest first.
    pub largest_bodies: Vec<(u64, u64)>,
    /// Column counts of the largest connected sheets of ocean-biome water.
    pub largest_sheets: Vec<u32>,
    /// Classification regions before small ones were absorbed into neighbours.
    pub regions_before_absorption: u32,
    /// How many of those were absorbed.
    pub absorbed: u32,
    pub regions: u32,
    pub by_kind: [u32; 4],
    pub with_ice: u32,
    pub with_corals: u32,
    pub with_desert: u32,
    pub with_mangrove: u32,
    pub with_cave: u32,
    /// Regions whose shape overruled the biome's idea of what they are.
    pub reshaped_to_lake: u32,
    pub reshaped_to_river: u32,
    pub bank_groups: u32,
    pub bank_columns: u64,
    pub merged_rivers: u32,
    pub merged_seas: u32,
    pub small_rivers: u32,
    pub small_seas: u32,
    /// Cells the biome-family majority filter reclassified, before anything else.
    pub smoothed_family_cells: u64,
    /// Cells the full-classification majority filter reclassified afterwards.
    pub smoothed_cells: u64,
    pub geometry_runs: u64,
}

/// Attributes and geometry produced by pass 3 for one tile.
type TileAttributes = (HashMap<u32, RegionAccum>, Vec<(u32, Run)>);

/// One horizontal run in world coordinates, tagged with a tile-local label.
#[derive(Clone, Copy, Debug)]
struct LocalRun {
    z: i32,
    x0: i32,
    x1: i32,
    label: u32,
}

/// Pass 1 result for one tile.
struct HydroTile {
    tile: TileResult,
    per_local: Vec<HydroInfo>,
}

fn hydro_pass(grid: &WorldGrid, slot: usize, registry: &BiomeRegistry) -> Option<HydroTile> {
    let mut keys = vec![NONE; TILE * TILE];
    if !grid.water_keys(slot, &mut keys) {
        return None;
    }
    let labels = label_grid(TILE, TILE, &keys);
    if labels.count == 0 {
        return None;
    }

    let region = &grid.regions[slot];
    let mut per_local = vec![HydroInfo::default(); labels.count as usize];
    for cz in 0..REGION_CHUNKS {
        for cx in 0..REGION_CHUNKS {
            let Some(chunk) = region.chunk(cz * REGION_CHUNKS + cx) else {
                continue;
            };
            let bx = cx * 16;
            let bz = cz * 16;
            for lz in 0..16usize {
                for lx in 0..16usize {
                    if !chunk.get(lx, lz) {
                        continue;
                    }
                    let label = labels.labels[(bz + lz) * TILE + bx + lx];
                    if label == NONE {
                        continue;
                    }
                    let cell = chunk.cell(lx, lz);
                    let family = registry
                        .info(cell.biome)
                        .map(|b| b.family)
                        .unwrap_or(crate::world::biome::BiomeFamily::Land);
                    per_local[label as usize].add_column(family, 1);
                }
            }
        }
    }

    Some(HydroTile {
        tile: TileResult {
            rx: region.region_x,
            rz: region.region_z,
            base: 0,
            count: labels.count,
            sizes: labels.sizes.clone(),
            edges: TileEdges::from_labels(&labels, &keys),
        },
        per_local,
    })
}

/// Pass 2 result for one tile.
struct SignatureTile {
    tile: TileResult,
    runs: Vec<LocalRun>,
    /// Signature key per local label - constant within a component by construction.
    sig_by_local: Vec<u32>,
}

fn signature_pass(
    grid: &WorldGrid,
    slot: usize,
    kinds: &KindField,
) -> Option<SignatureTile> {
    let mut water_keys = vec![NONE; TILE * TILE];
    if !grid.water_keys(slot, &mut water_keys) {
        return None;
    }

    let region = &grid.regions[slot];
    let mut sig_keys = vec![NONE; TILE * TILE];
    for cz in 0..REGION_CHUNKS {
        for cx in 0..REGION_CHUNKS {
            let chunk_index = cz * REGION_CHUNKS + cx;
            let Some(chunk) = region.chunk(chunk_index) else {
                continue;
            };
            let bx = cx * 16;
            let bz = cz * 16;
            for lz in 0..16usize {
                for lx in 0..16usize {
                    if !chunk.get(lx, lz) {
                        continue;
                    }
                    // The classification was decided per cell and denoised in
                    // `KindField`; here it is only read back out.
                    let cell = (lz >> 2) * 4 + (lx >> 2);
                    let Some((kind, temperature, icy, cave)) =
                        kinds::unpack(kinds.code_at(slot, chunk_index, cell))
                    else {
                        continue;
                    };
                    sig_keys[(bz + lz) * TILE + bx + lx] =
                        classifier::signature(kind, temperature, icy, cave);
                }
            }
        }
    }

    let labels = label_grid(TILE, TILE, &sig_keys);
    if labels.count == 0 {
        return None;
    }

    // Emit run-length encoded scanlines while the labels are still around.
    let x0 = region.region_x * TILE as i32;
    let z0 = region.region_z * TILE as i32;
    let mut runs = Vec::new();
    let mut sig_by_local = vec![0u32; labels.count as usize];
    for lz in 0..TILE {
        let row = lz * TILE;
        let mut x = 0usize;
        while x < TILE {
            let label = labels.labels[row + x];
            if label == NONE {
                x += 1;
                continue;
            }
            let start = x;
            while x + 1 < TILE && labels.labels[row + x + 1] == label {
                x += 1;
            }
            sig_by_local[label as usize] = sig_keys[row + start];
            runs.push(LocalRun {
                z: z0 + lz as i32,
                x0: x0 + start as i32,
                x1: x0 + x as i32,
                label,
            });
            x += 1;
        }
    }

    Some(SignatureTile {
        tile: TileResult {
            rx: region.region_x,
            rz: region.region_z,
            base: 0,
            count: labels.count,
            sizes: labels.sizes.clone(),
            edges: TileEdges::from_labels(&labels, &sig_keys),
        },
        runs,
        sig_by_local,
    })
}

/// Accumulates the attributes of one run into its region.
fn accumulate_run(
    grid: &WorldGrid,
    registry: &BiomeRegistry,
    dryland: &DrylandProximity,
    accum: &mut RegionAccum,
    run: &LocalRun,
) {
    accum.extend_bounds(run.x0, run.x1, run.z);
    let mut x = run.x0;
    while x <= run.x1 {
        let Some(chunk) = grid.chunk_at(x, run.z) else {
            x += 1;
            continue;
        };
        let lx = (x & 15) as usize;
        let lz = (run.z & 15) as usize;
        let cell = chunk.cell(lx, lz);
        // Advance to the end of the current 4x4 cell (and of the current chunk).
        let cell_end_x = (x & !3) + 3;
        let end = cell_end_x.min(run.x1);
        let n = (end - x + 1) as u64;
        let fraction = |count: u8| {
            if cell.water_cols > 0 {
                count as f32 / cell.water_cols as f32
            } else {
                0.0
            }
        };
        accum.add_cell(
            registry,
            cell.biome,
            cell.surface_y,
            cell.depth,
            fraction(cell.ice_cols),
            fraction(cell.cave_cols),
            chunk.flags.contains(ChunkFlags::CORAL),
            chunk.flags.contains(ChunkFlags::PLANTS),
            n,
        );
        // Direct dry-biome evidence was already counted by add_cell. For a
        // neutral river biome, nearby arid banks contribute once, without
        // changing the river's kind, temperature or geometry.
        if dryland.at_world(grid, x, run.z)
            && !registry.info(cell.biome)
                .is_some_and(|b| b.traits.contains(BiomeTraits::DESERT))
        {
            accum.desert_columns += n;
        }
        x = end + 1;
    }
}

/// Runs the whole component + classification pipeline.
///
/// `min_columns` is the smallest body of water that is reported at all, and
/// `min_cave_columns` the same for water that cannot see the sky. Cave water gets
/// its own threshold because aquifers are far more numerous than lakes: a world
/// has a few thousand ponds but tens of thousands of underground pools.
pub fn build_regions(
    grid: &WorldGrid,
    registry: &BiomeRegistry,
    min_columns: u32,
    min_cave_columns: u32,
    min_sea_columns: u32,
    merge_options: super::consolidate::MergeOptions,
) -> (Vec<WaterRegion>, RegionStats) {
    let mut stats = RegionStats::default();
    if grid.regions.is_empty() {
        return (Vec::new(), stats);
    }

    // ---- pass 1: hydrological bodies -------------------------------------
    let mut hydro_tiles: Vec<HydroTile> = (0..grid.regions.len())
        .into_par_iter()
        .filter_map(|slot| hydro_pass(grid, slot, registry))
        .collect();
    if hydro_tiles.is_empty() {
        return (Vec::new(), stats);
    }
    // Keep a deterministic tile order regardless of how rayon scheduled them.
    hydro_tiles.sort_by_key(|t| (t.tile.rz, t.tile.rx));

    let mut hydro_results: Vec<TileResult> =
        hydro_tiles.iter_mut().map(|t| std::mem::replace(&mut t.tile, empty_tile())).collect();
    let hydro_merged = merge_tiles(&mut hydro_results, min_columns);
    stats.hydro_bodies = hydro_merged.count;

    let mut hydro_infos = vec![HydroInfo::default(); hydro_merged.count.max(1) as usize];
    for (t, res) in hydro_tiles.iter().zip(hydro_results.iter()) {
        for (local, info) in t.per_local.iter().enumerate() {
            let dense = hydro_merged.resolve(res.base, local as u32);
            if dense != NONE {
                hydro_infos[dense as usize].merge(info);
            }
        }
    }

    // Sheets of ocean-biome water, judged on their own connectivity rather than
    // on the body they hang off. This is the number that matters when tuning the
    // threshold: a world has a handful of oceans, not hundreds.
    let sheets = OceanSheets::build(grid, registry, min_sea_columns);
    stats.sea_bodies = sheets.sea_sheets;
    stats.largest_sheets = sheets.largest.clone();
    let mut sizes: Vec<(u64, u64)> = hydro_infos
        .iter()
        .map(|h| (h.columns, h.ocean_columns))
        .collect();
    sizes.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    sizes.truncate(24);
    stats.largest_bodies = sizes;

    // Label bases by slot, in the order `grid.regions` has them.
    let by_coord: HashMap<(i32, i32), u32> = hydro_results
        .iter()
        .map(|t| ((t.rx, t.rz), t.base))
        .collect();
    let hydro_bases: Vec<u32> = grid
        .regions
        .iter()
        .map(|r| *by_coord.get(&(r.region_x, r.region_z)).unwrap_or(&0))
        .collect();
    drop(hydro_tiles);

    // ---- pass 1c: the classification field, denoised ---------------------
    // Deciding the kind per cell straight off the biome produces speckle: banks
    // that read as lakes, thin strips of river across a lake. The biome *family*
    // is denoised first (a majority filter, before anything dilates it), and the
    // full classification is then built and denoised again on top of that.
    let families = kinds::FamilyField::build(grid, registry);
    stats.smoothed_family_cells = families.smoothed_cells;
    let kinds = KindField::build(
        grid,
        registry,
        &kinds::KindInputs {
            sheets: &sheets,
            families: &families,
            hydro: &hydro_merged,
            hydro_infos: &hydro_infos,
            hydro_bases: &hydro_bases,
        },
        min_sea_columns,
    );
    stats.smoothed_cells = kinds.smoothed_cells;

    // ---- pass 2: classification regions ----------------------------------
    let mut sig_tiles: Vec<SignatureTile> = (0..grid.regions.len())
        .into_par_iter()
        .filter_map(|slot| signature_pass(grid, slot, &kinds))
        .collect();
    if sig_tiles.is_empty() {
        return (Vec::new(), stats);
    }
    sig_tiles.sort_by_key(|t| (t.tile.rz, t.tile.rx));

    let mut sig_results: Vec<TileResult> = sig_tiles
        .iter_mut()
        .map(|t| std::mem::replace(&mut t.tile, empty_tile()))
        .collect();
    // Nothing is dropped here: even a one-column fragment keeps an id so that the
    // absorption pass can hand it to the region it belongs to instead of leaving
    // a hole in the middle of a river.
    let merged = merge_tiles(&mut sig_results, 1);
    if merged.count == 0 {
        return (Vec::new(), stats);
    }

    // ---- pass 3: attributes and geometry ---------------------------------
    let dryland = DrylandProximity::build(grid);
    let per_tile: Vec<TileAttributes> = sig_tiles
        .par_iter()
        .zip(sig_results.par_iter())
        .map(|(t, res)| {
            let mut accums: HashMap<u32, RegionAccum> = HashMap::new();
            let mut runs: Vec<(u32, Run)> = Vec::with_capacity(t.runs.len());
            for run in &t.runs {
                let id = merged.resolve(res.base, run.label);
                if id == NONE {
                    continue;
                }
                let accum = accums.entry(id).or_default();
                accum.signature = t.sig_by_local[run.label as usize];
                accumulate_run(grid, registry, &dryland, accum, run);
                runs.push((
                    id,
                    Run {
                        z: run.z,
                        x0: run.x0,
                        x1: run.x1,
                    },
                ));
            }
            (accums, runs)
        })
        .collect();

    let mut accums: Vec<RegionAccum> = vec![RegionAccum::default(); merged.count as usize];
    let mut all_runs: Vec<(u32, Run)> = Vec::new();
    for (map, runs) in per_tile {
        for (id, a) in map {
            accums[id as usize].merge(&a);
        }
        all_runs.extend(runs);
    }

    // ---- absorption: let the big regions grow back over the fragments -----
    all_runs.sort_unstable_by_key(|(_, r)| (r.z, r.x0));
    let sizes: Vec<u32> = accums
        .iter()
        .map(|a| a.columns.min(u32::MAX as u64) as u32)
        .collect();
    let kinds: Vec<u8> = accums
        .iter()
        .map(|a| classifier::unpack_signature(a.signature).0 as u8)
        .collect();
    let cave: Vec<bool> = accums
        .iter()
        .map(|a| classifier::unpack_signature(a.signature).3)
        .collect();
    let absorbed = absorb::absorb(
        &sizes,
        &kinds,
        &cave,
        &absorb::adjacency(&all_runs),
        min_columns,
    );
    stats.regions_before_absorption = merged.count;
    stats.absorbed = absorbed.absorbed;

    // Compact the surviving groups to dense ids.
    let mut dense = vec![NONE; merged.count as usize];
    let mut group_count = 0u32;
    for id in 0..merged.count {
        let root = absorbed.group[id as usize];
        if dense[root as usize] == NONE {
            dense[root as usize] = group_count;
            group_count += 1;
        }
        dense[id as usize] = dense[root as usize];
    }

    let mut grouped: Vec<RegionAccum> = vec![RegionAccum::default(); group_count as usize];
    for (id, accum) in accums.iter().enumerate() {
        grouped[dense[id] as usize].merge(accum);
    }
    // The classification comes from the group's largest member, so lake fragments
    // along a river read as river rather than the other way round.
    for id in 0..merged.count as usize {
        let donor = absorbed.donor[id] as usize;
        grouped[dense[id] as usize].signature = accums[donor].signature;
    }
    let accums = grouped;

    for (id, _) in all_runs.iter_mut() {
        *id = dense[*id as usize];
    }
    all_runs.sort_unstable_by_key(|(id, r)| (*id, r.z, r.x0));

    let mut regions: Vec<WaterRegion> = accums
        .iter()
        .enumerate()
        .map(|(i, a)| classifier::finalize(i as u32, a, registry))
        .collect();

    // Attach geometry, merging runs that were split by a tile boundary.
    let mut i = 0usize;
    while i < all_runs.len() {
        let id = all_runs[i].0;
        let mut j = i;
        let runs = &mut regions[id as usize].geometry.runs;
        while j < all_runs.len() && all_runs[j].0 == id {
            let r = all_runs[j].1;
            match runs.last_mut() {
                Some(prev) if prev.z == r.z && prev.x1 + 1 == r.x0 => prev.x1 = r.x1,
                _ => runs.push(r),
            }
            j += 1;
        }
        i = j;
    }

    let shape = super::shape::correct(&mut regions);
    stats.reshaped_to_lake = shape.to_lake;
    stats.reshaped_to_river = shape.to_river;
    stats.bank_groups = shape.bank_groups;
    stats.bank_columns = shape.bank_columns;

    // Consolidate after shape correction: bank fragments now have their final
    // kind. Do this before retention, so tiny connected pieces can be rescued.
    let (mut regions, merged) = super::consolidate::merge(regions, &accums, registry, merge_options);
    stats.merged_rivers = merged.merged_rivers;
    stats.merged_seas = merged.merged_seas;

    regions.retain(|r| {
        let min = if r.modifiers.contains(Modifiers::CAVE) {
            min_cave_columns
        } else {
            min_columns
        };
        r.geometry.column_count >= min
    });
    for (i, r) in regions.iter_mut().enumerate() {
        r.id = i as u32;
        stats.regions += 1;
        stats.by_kind[r.kind as usize] += 1;
        if r.kind == WaterKind::River && r.geometry.column_count < merge_options.river_min_area {
            stats.small_rivers += 1;
        }
        if r.kind == WaterKind::Sea && r.geometry.column_count < merge_options.sea_min_area {
            stats.small_seas += 1;
        }
        if r.modifiers.contains(Modifiers::ICE) {
            stats.with_ice += 1;
        }
        if r.modifiers.contains(Modifiers::CORALS) {
            stats.with_corals += 1;
        }
        if r.modifiers.contains(Modifiers::DESERT) {
            stats.with_desert += 1;
        }
        if r.modifiers.contains(Modifiers::MANGROVE) {
            stats.with_mangrove += 1;
        }
        if r.modifiers.contains(Modifiers::CAVE) {
            stats.with_cave += 1;
        }
        stats.geometry_runs += r.geometry.runs.len() as u64;
    }

    (regions, stats)
}

fn empty_tile() -> TileResult {
    TileResult {
        rx: 0,
        rz: 0,
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
    use crate::water::grid::{ChunkWater, RegionWater};

    #[test]
    fn dry_banks_relabel_neutral_rivers_without_double_counting_or_changing_geometry() {
        let registry = BiomeRegistry::vanilla_only();
        for name in ["minecraft:river", "minecraft:desert"] {
            let mut region = RegionWater::new(0, 0);
            let mut chunk = ChunkWater::default();
            for x in 0..8 { chunk.set(x, 0); }
            chunk.water_cols = 8;
            for cell in &mut chunk.cells[..2] {
                cell.biome = registry.id_of(name);
                cell.surface_y = 62;
                cell.depth = 3;
                cell.water_cols = 4;
            }
            let original_mask = chunk.mask;
            region.insert(0, chunk);
            // This neighbouring chunk holds only dry land, no water.
            region.set_dryland(1, u16::MAX);
            let grid = WorldGrid::build(vec![region]);
            let dryland = DrylandProximity::build(&grid);
            let sig = classifier::signature(WaterKind::River, Temperature::Medium, false, false);
            let mut accum = RegionAccum { signature: sig, ..RegionAccum::default() };
            accumulate_run(&grid, &registry, &dryland, &mut accum,
                &LocalRun { z: 0, x0: 0, x1: 7, label: 0 });
            assert_eq!(accum.columns, 8);
            assert_eq!(accum.desert_columns, 8, "count direct and nearby evidence only once");
            assert_eq!(accum.signature, sig);
            assert_eq!(grid.regions[0].chunk(0).unwrap().mask, original_mask);
            let finished = classifier::finalize(0, &accum, &registry);
            assert_eq!(finished.kind, WaterKind::River);
            assert_eq!(finished.temperature, Temperature::Medium);
            assert!(finished.modifiers.contains(Modifiers::DESERT));
            assert_eq!(finished.geometry.column_count, 8);
            assert_eq!((finished.geometry.min_x, finished.geometry.max_x), (0, 7));
        }
    }
}
