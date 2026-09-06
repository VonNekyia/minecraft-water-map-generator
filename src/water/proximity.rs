//! Water proximity maps for one tile.
//!
//! Two nearest-feature distance transforms over the 4x4 biome cell grid of a
//! region file:
//!
//! * **ocean** - how far the nearest *sea-sized* sheet of ocean-biome water is,
//!   and how warm it is.
//!   Water in a land biome (beach, plains, stony shore, ...) only counts as `Sea`
//!   when real ocean water is close by; otherwise a lake at the far end of a river
//!   network - which *is* connected to the ocean - would be classified as sea.
//! * **fringe** - how far the nearest river or swamp water is. Minecraft stores
//!   biomes at 4x4 resolution, so a cell straddling a river bank reports the land
//!   biome for water that is plainly part of the river. Without this repair every
//!   river grows a fringe of tiny lake regions along its banks.
//!
//! Both are Chebyshev distance transforms computed with a two-pass chamfer sweep,
//! built with a halo read from the neighbouring region files so tile borders
//! behave exactly like the interior.

use crate::config;
use rayon::prelude::*;
use crate::water::grid::WorldGrid;
use crate::water::kinds::FamilyField;
use crate::water::model::Temperature;
use crate::water::oceans::OceanSheets;
use crate::world::biome::{BiomeFamily, BiomeRegistry};

/// Cells along one side of a region file (512 blocks / 4 blocks per cell).
pub const TILE_CELLS: usize = 128;

const NO_VALUE: u8 = 0xFF;
const FAR: u8 = 0xFF;

/// Attribute-only proximity to arid surface land. This never changes water
/// masks, classification signatures, smoothing votes or connected components.
pub struct DrylandProximity {
    tiles: Vec<Vec<bool>>,
}

impl DrylandProximity {
    pub fn build(grid: &WorldGrid) -> Self {
        let halo = config::DRYLAND_CELL_RADIUS as usize;
        let side = TILE_CELLS + 2 * halo;
        let tiles = grid.regions.par_iter().map(|region| {
            let mut field = Field::new(side * side);
            let mut any = false;
            for gz in 0..side {
                for gx in 0..side {
                    let wx = region.region_x * 512 + (gx as i32 - halo as i32) * 4;
                    let wz = region.region_z * 512 + (gz as i32 - halo as i32) * 4;
                    let Some(land) = grid.region(wx >> 9, wz >> 9) else { continue };
                    let chunk = (((wz & 511) >> 4) * 32 + ((wx & 511) >> 4)) as usize;
                    let cell = (((wz & 15) >> 2) * 4 + ((wx & 15) >> 2)) as usize;
                    if land.dryland_cell(chunk, cell) {
                        field.seed(gz * side + gx, 1, 0);
                        any = true;
                    }
                }
            }
            if any { field.transform(side); }
            let mut near = vec![false; TILE_CELLS * TILE_CELLS];
            for z in 0..TILE_CELLS {
                for x in 0..TILE_CELLS {
                    near[z * TILE_CELLS + x] = field.dist[(z + halo) * side + x + halo]
                        <= config::DRYLAND_CELL_RADIUS;
                }
            }
            near
        }).collect();
        Self { tiles }
    }

    pub fn at_world(&self, grid: &WorldGrid, x: i32, z: i32) -> bool {
        let Some(slot) = grid.region_slot(x >> 9, z >> 9) else { return false };
        self.tiles[slot][(((z & 511) >> 2) * 128 + ((x & 511) >> 2)) as usize]
    }
}

/// What a land-biome column found next to it, if anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fringe {
    pub family: BiomeFamily,
    pub temperature: Temperature,
}

/// A nearest-feature Chebyshev distance transform carrying two payload bytes.
struct Field {
    dist: Vec<u8>,
    a: Vec<u8>,
    b: Vec<u8>,
}

impl Field {
    fn new(n: usize) -> Self {
        Field {
            dist: vec![FAR; n],
            a: vec![NO_VALUE; n],
            b: vec![NO_VALUE; n],
        }
    }

    #[inline]
    fn seed(&mut self, i: usize, a: u8, b: u8) {
        self.dist[i] = 0;
        self.a[i] = a;
        self.b[i] = b;
    }

    #[inline]
    fn relax(&mut self, i: usize, j: usize) {
        let candidate = self.dist[j].saturating_add(1);
        if candidate < self.dist[i] {
            self.dist[i] = candidate;
            self.a[i] = self.a[j];
            self.b[i] = self.b[j];
        }
    }

    /// Two-pass chamfer sweep over an 8-connected neighbourhood.
    fn transform(&mut self, side: usize) {
        for z in 0..side {
            for x in 0..side {
                let i = z * side + x;
                if z > 0 {
                    self.relax(i, i - side);
                    if x > 0 {
                        self.relax(i, i - side - 1);
                    }
                    if x + 1 < side {
                        self.relax(i, i - side + 1);
                    }
                }
                if x > 0 {
                    self.relax(i, i - 1);
                }
            }
        }
        for z in (0..side).rev() {
            for x in (0..side).rev() {
                let i = z * side + x;
                if z + 1 < side {
                    self.relax(i, i + side);
                    if x > 0 {
                        self.relax(i, i + side - 1);
                    }
                    if x + 1 < side {
                        self.relax(i, i + side + 1);
                    }
                }
                if x + 1 < side {
                    self.relax(i, i + 1);
                }
            }
        }
    }
}

pub struct WaterProximity {
    halo: usize,
    side: usize,
    ocean: Field,
    fringe: Field,
}

impl WaterProximity {
    pub fn build(
        grid: &WorldGrid,
        region_x: i32,
        region_z: i32,
        registry: &BiomeRegistry,
        sheets: &OceanSheets,
        families: &FamilyField,
    ) -> Self {
        // One halo wide enough for both radii keeps the two fields in one grid.
        let halo = config::COASTAL_CELL_RADIUS.max(config::FRINGE_CELL_RADIUS) as usize;
        let side = TILE_CELLS + 2 * halo;
        let mut ocean = Field::new(side * side);
        let mut fringe = Field::new(side * side);

        let base_x = region_x * 512;
        let base_z = region_z * 512;
        for gz in 0..side {
            for gx in 0..side {
                let wx = base_x + (gx as i32 - halo as i32) * 4;
                let wz = base_z + (gz as i32 - halo as i32) * 4;
                let Some(cell) = grid.cell_at(wx, wz) else {
                    continue;
                };
                if cell.water_cols == 0 {
                    continue;
                }
                // Seeded from the *denoised* family, not the raw per-cell biome.
                // The fringe rule below dilates whatever it is seeded with, and
                // dilating a stray cell of noise only makes the noise bigger - a
                // single river cell inside a lake used to grow into a band five
                // cells wide cutting the lake in two.
                let Some(family) = families.at_world(grid, wx, wz) else {
                    continue;
                };
                let Some(info) = registry.info(cell.biome) else {
                    continue;
                };
                let i = gz * side + gx;
                match family {
                    BiomeFamily::Ocean => {
                        // Only sheets big enough to be a sea seed the field, so
                        // "near ocean" cannot mean "near an ocean-biome pond".
                        if sheets.is_sea_at(grid, wx, wz) {
                            ocean.seed(i, info.water_temperature as u8, 0);
                        }
                    }
                    BiomeFamily::River | BiomeFamily::Swamp => {
                        fringe.seed(i, family as u8, info.water_temperature as u8);
                    }
                    BiomeFamily::Land => {}
                }
            }
        }

        ocean.transform(side);
        fringe.transform(side);

        WaterProximity {
            halo,
            side,
            ocean,
            fringe,
        }
    }

    #[inline]
    fn index(&self, tile_x: usize, tile_z: usize) -> usize {
        let gx = (tile_x >> 2) + self.halo;
        let gz = (tile_z >> 2) + self.halo;
        gz * self.side + gx
    }

    /// Whether ocean-biome water lies within the coastal radius of this column.
    /// `tile_x` / `tile_z` are block coordinates inside the region file (0..512).
    #[inline]
    pub fn near_ocean(&self, tile_x: usize, tile_z: usize) -> bool {
        self.ocean.dist[self.index(tile_x, tile_z)] <= config::COASTAL_CELL_RADIUS
    }

    /// Temperature of the nearest ocean water, if there is any within reach.
    #[inline]
    pub fn ocean_temperature(&self, tile_x: usize, tile_z: usize) -> Option<Temperature> {
        let i = self.index(tile_x, tile_z);
        if self.ocean.dist[i] > config::COASTAL_CELL_RADIUS {
            return None;
        }
        Temperature::from_u8(self.ocean.a[i])
    }

    /// The river or swamp this column is a fringe of, if any is close enough.
    #[inline]
    pub fn fringe(&self, tile_x: usize, tile_z: usize) -> Option<Fringe> {
        let i = self.index(tile_x, tile_z);
        if self.fringe.dist[i] > config::FRINGE_CELL_RADIUS {
            return None;
        }
        let family = match self.fringe.a[i] {
            x if x == BiomeFamily::River as u8 => BiomeFamily::River,
            x if x == BiomeFamily::Swamp as u8 => BiomeFamily::Swamp,
            _ => return None,
        };
        Some(Fringe {
            family,
            temperature: Temperature::from_u8(self.fringe.b[i])?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water::grid::{CellInfo, ChunkWater, RegionWater, WorldGrid, REGION_CHUNKS};

    /// Fills a whole region with water, using `biome_at(cx)` per chunk column.
    fn region_with<F: Fn(usize) -> u16>(rx: i32, rz: i32, biome_at: F) -> RegionWater {
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
                let biome = biome_at(cx);
                for cell in cw.cells.iter_mut() {
                    *cell = CellInfo {
                        biome,
                        surface_y: 62,
                        depth: 5,
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

    /// Sheets with a threshold low enough that any ocean water in the fixture
    /// counts, so these tests exercise the distance transform, not the sizing.
    fn sheets(grid: &WorldGrid, registry: &BiomeRegistry) -> OceanSheets {
        OceanSheets::build(grid, registry, 1)
    }

    /// A `FamilyField` with smoothing turned off, so these tests exercise the
    /// distance transform on the raw family grid, not the denoising.
    fn raw_families(grid: &WorldGrid, registry: &BiomeRegistry) -> FamilyField {
        FamilyField::build_with_passes(grid, registry, 0)
    }

    fn ocean_world(ocean_chunks: usize, registry: &BiomeRegistry) -> WorldGrid {
        let ocean = registry.id_of("minecraft:cold_ocean");
        let land = registry.id_of("minecraft:plains");
        WorldGrid::build(vec![region_with(0, 0, |cx| {
            if cx < ocean_chunks {
                ocean
            } else {
                land
            }
        })])
    }

    #[test]
    fn dry_only_neighbour_is_seen_across_negative_tile_boundary_without_water() {
        let mut land = RegionWater::new(-1, -1);
        // Last 4x4 cell of the dry region: world (-4, -4).
        land.set_dryland(1023, 1 << 15);
        let grid = WorldGrid::build(vec![land, RegionWater::new(0, 0)]);
        let map = DrylandProximity::build(&grid);
        assert!(map.at_world(&grid, -4, -4));
        assert!(map.at_world(&grid, 0, 0));
        assert!(map.at_world(&grid, 12, 12), "four cells from the dry bank");
        assert!(!map.at_world(&grid, 16, 12), "beyond the configured radius");
        assert!(!map.at_world(&grid, 12, 16));
        assert!(!map.at_world(&grid, 512, 512), "missing region");
        assert!(grid.regions.iter().all(|r| r.is_empty() && r.water_columns() == 0));
    }

    #[test]
    fn distance_grows_away_from_the_ocean() {
        let registry = BiomeRegistry::vanilla_only();
        let grid = ocean_world(4, &registry);
        let map = WaterProximity::build(&grid, 0, 0, &registry, &sheets(&grid, &registry), &raw_families(&grid, &registry));

        assert!(map.near_ocean(0, 0));
        assert!(map.near_ocean(63, 100));
        // Just past the ocean edge (ocean ends at x = 63), still coastal.
        assert!(map.near_ocean(70, 100));
        // COASTAL_CELL_RADIUS cells is 24 blocks; well beyond that is not.
        assert!(!map.near_ocean(200, 100));
        assert!(!map.near_ocean(500, 500));
    }

    #[test]
    fn nearest_temperature_comes_from_the_ocean_cells() {
        let registry = BiomeRegistry::vanilla_only();
        let grid = ocean_world(4, &registry);
        let map = WaterProximity::build(&grid, 0, 0, &registry, &sheets(&grid, &registry), &raw_families(&grid, &registry));
        assert_eq!(map.ocean_temperature(70, 100), Some(Temperature::Cold));
        assert_eq!(map.ocean_temperature(400, 100), None);
    }

    #[test]
    fn a_world_without_oceans_is_never_coastal() {
        let registry = BiomeRegistry::vanilla_only();
        let land = registry.id_of("minecraft:plains");
        let grid = WorldGrid::build(vec![region_with(0, 0, |_| land)]);
        let map = WaterProximity::build(&grid, 0, 0, &registry, &sheets(&grid, &registry), &raw_families(&grid, &registry));
        for x in (0..512).step_by(37) {
            for z in (0..512).step_by(41) {
                assert!(!map.near_ocean(x, z));
                assert_eq!(map.ocean_temperature(x, z), None);
                assert_eq!(map.fringe(x, z), None);
            }
        }
    }

    #[test]
    fn the_halo_sees_ocean_in_the_neighbouring_region() {
        let registry = BiomeRegistry::vanilla_only();
        let ocean = registry.id_of("minecraft:warm_ocean");
        let land = registry.id_of("minecraft:plains");
        let grid = WorldGrid::build(vec![
            region_with(-1, 0, |_| ocean),
            region_with(0, 0, |_| land),
        ]);

        let map = WaterProximity::build(&grid, 0, 0, &registry, &sheets(&grid, &registry), &raw_families(&grid, &registry));
        // The first columns of the land region border the ocean region.
        assert!(map.near_ocean(0, 0));
        assert_eq!(map.ocean_temperature(0, 0), Some(Temperature::Warm));
        assert!(!map.near_ocean(300, 0));
    }

    #[test]
    fn land_water_next_to_a_river_is_recognised_as_river_fringe() {
        let registry = BiomeRegistry::vanilla_only();
        let river = registry.id_of("minecraft:river");
        let land = registry.id_of("minecraft:plains");
        // River biome on chunk column 4 only (blocks 64..79).
        let grid = WorldGrid::build(vec![region_with(0, 0, |cx| {
            if cx == 4 {
                river
            } else {
                land
            }
        })]);
        let map = WaterProximity::build(&grid, 0, 0, &registry, &sheets(&grid, &registry), &raw_families(&grid, &registry));

        let f = map.fringe(60, 100).expect("bank column is river fringe");
        assert_eq!(f.family, BiomeFamily::River);
        assert_eq!(f.temperature, Temperature::Medium);
        assert!(map.fringe(64, 100).is_some());
        // FRINGE_CELL_RADIUS is 2 cells = 8 blocks; far from the river there is none.
        assert!(map.fringe(200, 100).is_none());
        assert!(map.fringe(0, 0).is_none());
    }

    #[test]
    fn swamp_fringes_are_reported_as_swamp() {
        let registry = BiomeRegistry::vanilla_only();
        let swamp = registry.id_of("minecraft:mangrove_swamp");
        let land = registry.id_of("minecraft:plains");
        let grid = WorldGrid::build(vec![region_with(0, 0, |cx| {
            if cx < 2 {
                swamp
            } else {
                land
            }
        })]);
        let map = WaterProximity::build(&grid, 0, 0, &registry, &sheets(&grid, &registry), &raw_families(&grid, &registry));
        let f = map.fringe(36, 10).expect("bank column is swamp fringe");
        assert_eq!(f.family, BiomeFamily::Swamp);
        assert!(map.fringe(300, 10).is_none());
    }
}
