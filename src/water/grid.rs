//! In-memory result of the chunk scan.
//!
//! The scan never keeps a per-block array of the whole world. Instead every chunk
//! contributes a 256 bit water mask plus 16 aggregated 4x4 cells (the resolution
//! Minecraft itself stores biomes at). For the 30k x 25k world this project was
//! built against that is a few hundred MiB instead of several hundred GiB.

pub const REGION_CHUNKS: usize = 32;
pub const CHUNKS_PER_REGION: usize = REGION_CHUNKS * REGION_CHUNKS;
/// Side length of a region in blocks.
pub const REGION_BLOCKS: i32 = 512;

bitflags::bitflags! {
    /// Block evidence collected per chunk while scanning.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub struct ChunkFlags: u8 {
        const CORAL  = 1 << 0;
        const PLANTS = 1 << 1;
    }
}

/// Aggregated water information for one 4x4 column cell of a chunk.
#[derive(Clone, Copy, Debug, Default)]
pub struct CellInfo {
    /// Biome registry id at the water surface of this cell.
    pub biome: u16,
    /// Mean water surface Y of the cell's water columns.
    pub surface_y: i16,
    /// Mean `surface_y - floor_y` of the cell's water columns, clamped.
    pub depth: u16,
    pub water_cols: u8,
    pub ice_cols: u8,
    /// Water columns in this cell that cannot see the sky.
    pub cave_cols: u8,
}

/// Water information for one chunk.
#[derive(Clone, Debug)]
pub struct ChunkWater {
    /// One bit per column, bit index `z * 16 + x`.
    pub mask: [u64; 4],
    pub cells: [CellInfo; 16],
    pub flags: ChunkFlags,
    pub water_cols: u16,
}

impl Default for ChunkWater {
    fn default() -> Self {
        ChunkWater {
            mask: [0; 4],
            cells: [CellInfo::default(); 16],
            flags: ChunkFlags::empty(),
            water_cols: 0,
        }
    }
}

impl ChunkWater {
    #[inline]
    pub fn set(&mut self, x: usize, z: usize) {
        let i = z * 16 + x;
        self.mask[i >> 6] |= 1u64 << (i & 63);
    }

    #[inline]
    pub fn get(&self, x: usize, z: usize) -> bool {
        let i = z * 16 + x;
        self.mask[i >> 6] & (1u64 << (i & 63)) != 0
    }

    #[inline]
    pub fn cell(&self, x: usize, z: usize) -> &CellInfo {
        &self.cells[(z >> 2) * 4 + (x >> 2)]
    }
}

/// Water data of one region file, indexed by the region-local chunk index.
pub struct RegionWater {
    pub region_x: i32,
    pub region_z: i32,
    /// Index into `chunks`, `u16::MAX` when the chunk holds no water.
    index: Box<[u16; CHUNKS_PER_REGION]>,
    chunks: Vec<ChunkWater>,
}

#[allow(dead_code)]
impl RegionWater {
    pub fn new(region_x: i32, region_z: i32) -> Self {
        RegionWater {
            region_x,
            region_z,
            index: Box::new([u16::MAX; CHUNKS_PER_REGION]),
            chunks: Vec::new(),
        }
    }

    pub fn insert(&mut self, chunk_index: usize, data: ChunkWater) {
        debug_assert!(self.chunks.len() < u16::MAX as usize);
        self.index[chunk_index] = self.chunks.len() as u16;
        self.chunks.push(data);
    }

    #[inline]
    pub fn chunk(&self, chunk_index: usize) -> Option<&ChunkWater> {
        let i = self.index[chunk_index];
        if i == u16::MAX {
            None
        } else {
            Some(&self.chunks[i as usize])
        }
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn water_columns(&self) -> u64 {
        self.chunks.iter().map(|c| c.water_cols as u64).sum()
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Marks every cell of a chunk as roofed over. Test helper.
    #[cfg(test)]
    pub fn set_cave_for_test(&mut self, chunk_index: usize) {
        let i = self.index[chunk_index];
        if i != u16::MAX {
            for cell in self.chunks[i as usize].cells.iter_mut() {
                cell.cave_cols = cell.water_cols;
            }
        }
    }
}

/// All scanned regions plus a dense lookup grid over the world bounding box.
pub struct WorldGrid {
    pub regions: Vec<RegionWater>,
    pub min_rx: i32,
    pub min_rz: i32,
    pub rw: i32,
    pub rh: i32,
    /// `rw * rh` slots, `usize::MAX` where no region file exists.
    lookup: Vec<usize>,
}

#[allow(dead_code)]
impl WorldGrid {
    pub fn build(regions: Vec<RegionWater>) -> Self {
        if regions.is_empty() {
            return WorldGrid {
                regions,
                min_rx: 0,
                min_rz: 0,
                rw: 0,
                rh: 0,
                lookup: Vec::new(),
            };
        }
        let min_rx = regions.iter().map(|r| r.region_x).min().unwrap();
        let max_rx = regions.iter().map(|r| r.region_x).max().unwrap();
        let min_rz = regions.iter().map(|r| r.region_z).min().unwrap();
        let max_rz = regions.iter().map(|r| r.region_z).max().unwrap();
        let rw = max_rx - min_rx + 1;
        let rh = max_rz - min_rz + 1;
        let mut lookup = vec![usize::MAX; (rw * rh) as usize];
        for (i, r) in regions.iter().enumerate() {
            let ix = (r.region_z - min_rz) * rw + (r.region_x - min_rx);
            lookup[ix as usize] = i;
        }
        WorldGrid {
            regions,
            min_rx,
            min_rz,
            rw,
            rh,
            lookup,
        }
    }

    #[inline]
    pub fn region_slot(&self, rx: i32, rz: i32) -> Option<usize> {
        if rx < self.min_rx || rz < self.min_rz {
            return None;
        }
        let dx = rx - self.min_rx;
        let dz = rz - self.min_rz;
        if dx >= self.rw || dz >= self.rh {
            return None;
        }
        let v = self.lookup[(dz * self.rw + dx) as usize];
        if v == usize::MAX {
            None
        } else {
            Some(v)
        }
    }

    #[inline]
    pub fn region(&self, rx: i32, rz: i32) -> Option<&RegionWater> {
        self.region_slot(rx, rz).map(|i| &self.regions[i])
    }

    /// Water lookup by world block coordinates. Used for cross-region boundary
    /// merging, so it must work for arbitrary coordinates.
    pub fn is_water(&self, x: i32, z: i32) -> bool {
        self.chunk_at(x, z)
            .map(|c| c.get((x & 15) as usize, (z & 15) as usize))
            .unwrap_or(false)
    }

    pub fn chunk_at(&self, x: i32, z: i32) -> Option<&ChunkWater> {
        let rx = x >> 9;
        let rz = z >> 9;
        let region = self.region(rx, rz)?;
        let cx = ((x >> 4) & 31) as usize;
        let cz = ((z >> 4) & 31) as usize;
        region.chunk(cz * REGION_CHUNKS + cx)
    }

    pub fn cell_at(&self, x: i32, z: i32) -> Option<&CellInfo> {
        self.chunk_at(x, z)
            .map(|c| c.cell((x & 15) as usize, (z & 15) as usize))
    }

    /// Fills `keys` with `0` for every water column of a region file and
    /// [`NONE`](crate::water::components::NONE) elsewhere, ready for labelling.
    /// Returns whether the tile holds any water at all.
    pub fn water_keys(&self, slot: usize, keys: &mut [u32]) -> bool {
        use crate::water::components::{NONE, TILE};
        keys.fill(NONE);
        let region = &self.regions[slot];
        let mut any = false;
        for cz in 0..REGION_CHUNKS {
            for cx in 0..REGION_CHUNKS {
                let Some(chunk) = region.chunk(cz * REGION_CHUNKS + cx) else {
                    continue;
                };
                any = true;
                let bx = cx * 16;
                let bz = cz * 16;
                for lz in 0..16usize {
                    let row = (bz + lz) * TILE + bx;
                    for lx in 0..16usize {
                        if chunk.get(lx, lz) {
                            keys[row + lx] = 0;
                        }
                    }
                }
            }
        }
        any
    }

    pub fn total_water_columns(&self) -> u64 {
        self.regions.iter().map(|r| r.water_columns()).sum()
    }

    pub fn world_bounds(&self) -> (i32, i32, i32, i32) {
        (
            self.min_rx * REGION_BLOCKS,
            self.min_rz * REGION_BLOCKS,
            (self.min_rx + self.rw) * REGION_BLOCKS - 1,
            (self.min_rz + self.rh) * REGION_BLOCKS - 1,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_mask_round_trips() {
        let mut c = ChunkWater::default();
        assert!(!c.get(0, 0));
        c.set(0, 0);
        c.set(15, 15);
        c.set(7, 9);
        assert!(c.get(0, 0));
        assert!(c.get(15, 15));
        assert!(c.get(7, 9));
        assert!(!c.get(9, 7));
        assert_eq!(c.mask.iter().map(|m| m.count_ones()).sum::<u32>(), 3);
    }

    #[test]
    fn cells_map_four_by_four_columns() {
        let mut c = ChunkWater::default();
        c.cells[0].biome = 11;
        c.cells[5].biome = 22;
        c.cells[15].biome = 33;
        assert_eq!(c.cell(0, 0).biome, 11);
        assert_eq!(c.cell(3, 3).biome, 11);
        assert_eq!(c.cell(4, 4).biome, 22);
        assert_eq!(c.cell(7, 7).biome, 22);
        assert_eq!(c.cell(15, 15).biome, 33);
    }

    #[test]
    fn world_grid_resolves_negative_coordinates() {
        let mut a = RegionWater::new(-1, -1);
        let mut cw = ChunkWater::default();
        cw.set(15, 15);
        cw.water_cols = 1;
        // last chunk of region (-1,-1) covers blocks -16..-1
        a.insert(31 * REGION_CHUNKS + 31, cw);

        let grid = WorldGrid::build(vec![a]);
        assert!(grid.is_water(-1, -1));
        assert!(!grid.is_water(-2, -1));
        assert!(!grid.is_water(0, 0));
        assert_eq!(grid.total_water_columns(), 1);
    }

    #[test]
    fn missing_regions_are_not_water() {
        let grid = WorldGrid::build(vec![RegionWater::new(0, 0)]);
        assert!(!grid.is_water(5000, 5000));
        assert!(!grid.is_water(-5000, -5000));
        assert!(grid.region(1, 0).is_none());
    }
}
