//! Phase 1: turn region files into per-chunk water masks and 4x4 cell summaries.
//!
//! The scan is deliberately heightmap driven. For every column we read
//! `MOTION_BLOCKING` (highest fluid or motion blocking block) and `OCEAN_FLOOR`
//! (highest motion blocking non-fluid block). Their difference already tells us
//! whether a column holds surface fluid. The surface block is also checked when
//! the heights match, because raw mud counts as swamp water despite being solid
//! to Minecraft's heightmaps. Deeper probing handles mud beds and thin covers
//! such as ice, snow and lily pads.

use std::path::Path;

use crate::config;
use crate::water::grid::{CellInfo, ChunkFlags, ChunkWater, RegionWater};
use crate::world::biome::{BiomeFamily, BiomeRegistry, BiomeTraits, UNKNOWN_BIOME};
use crate::world::blocks::BlockClass;
use crate::world::chunk::{
    self, ChunkScratch, PaletteEntry, ParsedChunk, SectionFacts, SectionMeta, SectionReader,
};
use crate::world::region_reader::{Inflater, RegionFile};

/// Y offset used to index the surface histograms, covering y in `-128..384`.
pub const HIST_OFFSET: i32 = 128;
pub const HIST_LEN: usize = 512;

#[derive(Clone)]
pub struct ScanStats {
    pub region_files: u64,
    pub chunks_seen: u64,
    pub chunks_full: u64,
    pub chunks_failed: u64,
    pub water_chunks: u64,
    pub water_columns: u64,
    pub ice_columns: u64,
    /// Water columns with no view of the sky.
    pub cave_columns: u64,
    /// Water surface Y histogram restricted to ocean biomes (sea level detection).
    pub ocean_surface_hist: Box<[u64; HIST_LEN]>,
    /// Water surface Y histogram over all water.
    pub surface_hist: Box<[u64; HIST_LEN]>,
}

impl Default for ScanStats {
    fn default() -> Self {
        ScanStats {
            region_files: 0,
            chunks_seen: 0,
            chunks_full: 0,
            chunks_failed: 0,
            water_chunks: 0,
            water_columns: 0,
            ice_columns: 0,
            cave_columns: 0,
            ocean_surface_hist: Box::new([0; HIST_LEN]),
            surface_hist: Box::new([0; HIST_LEN]),
        }
    }
}

impl ScanStats {
    pub fn merge(mut self, other: ScanStats) -> ScanStats {
        self.region_files += other.region_files;
        self.chunks_seen += other.chunks_seen;
        self.chunks_full += other.chunks_full;
        self.chunks_failed += other.chunks_failed;
        self.water_chunks += other.water_chunks;
        self.water_columns += other.water_columns;
        self.ice_columns += other.ice_columns;
        self.cave_columns += other.cave_columns;
        for i in 0..HIST_LEN {
            self.ocean_surface_hist[i] += other.ocean_surface_hist[i];
            self.surface_hist[i] += other.surface_hist[i];
        }
        self
    }

    #[inline]
    fn record_surface(&mut self, y: i32, count: u64, ocean: bool) {
        let idx = (y + HIST_OFFSET).clamp(0, HIST_LEN as i32 - 1) as usize;
        self.surface_hist[idx] += count;
        if ocean {
            self.ocean_surface_hist[idx] += count;
        }
    }
}

/// Per-section block class tables, rebuilt for every chunk.
#[derive(Default)]
pub struct SectionCache {
    classes: Vec<Vec<u8>>,
    facts: Vec<SectionFacts>,
    built: Vec<bool>,
    /// Section slot by `section_y - min_section_y`, `u16::MAX` when absent.
    slot_by_y: Vec<u16>,
    min_section_y: i32,
}

impl SectionCache {
    fn reset(&mut self, sections: &[SectionMeta]) {
        if self.classes.len() < sections.len() {
            self.classes.resize_with(sections.len(), Vec::new);
            self.facts.resize(sections.len(), SectionFacts::default());
            self.built.resize(sections.len(), false);
        }
        for b in self.built.iter_mut().take(sections.len()) {
            *b = false;
        }
        self.min_section_y = sections.iter().map(|s| s.y).min().unwrap_or(0);
        let max_y = sections.iter().map(|s| s.y).max().unwrap_or(0);
        let span = (max_y - self.min_section_y + 1).max(1) as usize;
        self.slot_by_y.clear();
        self.slot_by_y.resize(span, u16::MAX);
        for (i, s) in sections.iter().enumerate() {
            self.slot_by_y[(s.y - self.min_section_y) as usize] = i as u16;
        }
    }

    #[inline]
    fn slot_for_block_y(&self, y: i32) -> Option<usize> {
        let sy = y >> 4;
        let d = sy - self.min_section_y;
        if d < 0 || d as usize >= self.slot_by_y.len() {
            return None;
        }
        let slot = self.slot_by_y[d as usize];
        if slot == u16::MAX {
            None
        } else {
            Some(slot as usize)
        }
    }

    /// Classifies section palettes. `y_lo`/`y_hi` bound the range; pass the whole
    /// chunk when cave water has to be found too.
    ///
    /// Only the *palette* is walked here - a few string comparisons per entry -
    /// so covering every section of a chunk instead of just the surface band is
    /// cheap. Decoding the packed block data is what costs, and that still only
    /// happens for sections the scan actually looks into.
    fn build_band(
        &mut self,
        buf: &[u8],
        palette: &[PaletteEntry],
        sections: &[SectionMeta],
        y_lo: i32,
        y_hi: i32,
    ) -> SectionFacts {
        let mut combined = SectionFacts::default();
        let lo = y_lo >> 4;
        let hi = y_hi >> 4;
        for (i, meta) in sections.iter().enumerate() {
            if meta.y < lo || meta.y > hi {
                continue;
            }
            let facts = chunk::classify_palette(buf, palette, meta, &mut self.classes[i]);
            self.facts[i] = facts;
            self.built[i] = true;
            combined.has_water |= facts.has_water;
            combined.has_ice |= facts.has_ice;
            combined.has_coral |= facts.has_coral;
            combined.has_aquatic_plant |= facts.has_aquatic_plant;
        }
        combined
    }

    /// Whether this section's palette contains water at all.
    #[inline]
    fn section_has_water(&self, slot: usize) -> bool {
        self.built[slot] && self.facts[slot].has_water
    }

    /// Section slots ordered from the top of the chunk downwards.
    fn slots_top_down(&self) -> impl Iterator<Item = usize> + '_ {
        self.slot_by_y
            .iter()
            .rev()
            .copied()
            .filter(|s| *s != u16::MAX)
            .map(|s| s as usize)
    }

    /// A reader for one section, for scanning many blocks of it in a row.
    #[inline]
    fn reader<'a>(
        &'a self,
        buf: &'a [u8],
        sections: &'a [SectionMeta],
        slot: usize,
    ) -> SectionReader<'a> {
        SectionReader::new(buf, &self.classes[slot], &sections[slot])
    }

    #[inline]
    fn class_at(
        &self,
        buf: &[u8],
        sections: &[SectionMeta],
        x: usize,
        y: i32,
        z: usize,
    ) -> BlockClass {
        let Some(slot) = self.slot_for_block_y(y) else {
            return BlockClass::Air;
        };
        if !self.built[slot] {
            // Outside the decoded band: treat as solid so probes stop instead of
            // silently walking into undecoded territory.
            return BlockClass::Solid;
        }
        SectionReader::new(buf, &self.classes[slot], &sections[slot]).class_at(
            x,
            (y & 15) as usize,
            z,
        )
    }
}

/// Reusable per-thread state. Rayon creates one of these per worker.
pub struct ScanContext {
    inflater: Inflater,
    buf: Vec<u8>,
    scratch: ChunkScratch,
    cache: SectionCache,
    mb: [u16; 256],
    of: [u16; 256],
}

impl Default for ScanContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanContext {
    pub fn new() -> Self {
        ScanContext {
            inflater: Inflater::new(),
            buf: Vec::with_capacity(1 << 20),
            scratch: ChunkScratch::new(),
            cache: SectionCache::default(),
            mb: [0; 256],
            of: [0; 256],
        }
    }
}

/// Result of scanning one region file.
pub struct RegionScan {
    pub water: RegionWater,
    pub stats: ScanStats,
}

/// Scan policy. A height cutoff replaces sky visibility as the exclusion rule:
/// accepted roofed water participates in normal sea/river/lake classification.
#[derive(Clone, Copy, Default)]
pub struct ScanOptions {
    pub caves: bool,
    pub min_surface_y: Option<i32>,
}

impl ScanOptions {
    fn accepts(self, surface_y: i32) -> bool {
        self.min_surface_y.is_none_or(|minimum| surface_y >= minimum)
    }
}

/// Scans one `.mca` file.
pub fn scan_region_file(
    path: &Path,
    registry: &BiomeRegistry,
    ctx: &mut ScanContext,
    options: ScanOptions,
) -> std::io::Result<Option<RegionScan>> {
    let Some(region) = RegionFile::open(path)? else {
        return Ok(None);
    };
    let mut stats = ScanStats {
        region_files: 1,
        ..Default::default()
    };
    let mut water = RegionWater::new(region.region_x, region.region_z);

    let mut buf = std::mem::take(&mut ctx.buf);
    for raw in region.chunks() {
        stats.chunks_seen += 1;
        if ctx.inflater.inflate(&raw, &mut buf).is_err() {
            stats.chunks_failed += 1;
            continue;
        }

        let parsed = match chunk::parse_chunk(&buf, &mut ctx.scratch) {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(_) => {
                stats.chunks_failed += 1;
                continue;
            }
        };
        stats.chunks_full += 1;

        // Surface-land metadata is independent of water. Dry banks in a wholly
        // dry chunk must still be available to a river in the neighbouring chunk.
        water.set_dryland(raw.index, scan_dryland(&buf, &parsed, &ctx.scratch, registry, &mut ctx.mb));

        let scanned = scan_chunk(
            &buf,
            &parsed,
            registry,
            &ctx.scratch,
            &mut ctx.cache,
            &mut ctx.mb,
            &mut ctx.of,
            &mut stats,
            options,
        );
        if let Some(cw) = scanned {
            stats.water_chunks += 1;
            stats.water_columns += cw.water_cols as u64;
            water.insert(raw.index, cw);
        }
    }
    ctx.buf = buf;

    Ok(Some(RegionScan { water, stats }))
}

/// Samples only surface biome metadata; this never sets a water-mask bit.
fn scan_dryland(
    buf: &[u8], parsed: &ParsedChunk, scratch: &ChunkScratch,
    registry: &BiomeRegistry, heights: &mut [u16; 256],
) -> u16 {
    let hm = &parsed.heightmaps;
    let surface = if hm.motion_blocking.longs > 0 { hm.motion_blocking } else { hm.world_surface };
    if !chunk::decode_heightmap(buf, surface, heights) { return 0; }
    let mut mask = 0;
    for cell in 0..16 {
        let x = (cell % 4) * 4;
        let z = (cell / 4) * 4;
        let h = heights[z * 16 + x];
        if h == 0 { continue; }
        let y = parsed.min_y() + i32::from(h) - 1;
        let Some(section) = scratch.sections.iter().find(|s| s.y == (y >> 4)) else { continue };
        let Some(name) = chunk::biome_name_at(buf, &scratch.palette, section, x, (y & 15) as usize, z) else { continue };
        if registry.info(registry.id_of(name)).is_some_and(|info|
            info.family == BiomeFamily::Land && info.traits.contains(BiomeTraits::DESERT))
        {
            mask |= 1 << cell;
        }
    }
    mask
}

/// Water column found by one column probe.
struct WaterColumn {
    surface_y: i32,
    floor_y: i32,
    ice: bool,
}

#[allow(clippy::too_many_arguments)]
fn scan_chunk(
    buf: &[u8],
    parsed: &ParsedChunk,
    registry: &BiomeRegistry,
    scratch: &ChunkScratch,
    cache: &mut SectionCache,
    mb: &mut [u16; 256],
    of: &mut [u16; 256],
    stats: &mut ScanStats,
    options: ScanOptions,
) -> Option<ChunkWater> {
    let sections = scratch.sections.as_slice();
    if sections.is_empty() {
        return None;
    }

    let hm = &parsed.heightmaps;
    let surface_map = if hm.motion_blocking.longs > 0 {
        hm.motion_blocking
    } else {
        hm.world_surface
    };
    if surface_map.longs == 0 {
        return None;
    }
    chunk::decode_heightmap(buf, surface_map, mb);
    let has_floor_map = chunk::decode_heightmap(buf, hm.ocean_floor, of);

    let min_y = parsed.min_y();

    // Vertical band that has to be decoded.
    let mut max_top = i32::MIN;
    let mut min_fluid_floor = i32::MAX;
    let mut any_fluid = false;
    for i in 0..256 {
        let mv = mb[i];
        if mv == 0 {
            continue;
        }
        let top = min_y + mv as i32 - 1;
        if top > max_top {
            max_top = top;
        }
        if has_floor_map && mv > of[i] {
            any_fluid = true;
            let floor = min_y + of[i] as i32 - 1;
            if floor < min_fluid_floor {
                min_fluid_floor = floor;
            }
        }
    }
    if max_top == i32::MIN {
        return None;
    }

    // From the deepest fluid floor (capped) up to the highest surface. Without any
    // fluid we still look at the top few blocks so ice covered water is not missed.
    let band_lo = if any_fluid {
        min_fluid_floor.max(max_top - config::MAX_FLOOR_PROBE)
    } else {
        max_top - config::SURFACE_COVER_PROBE
    };

    cache.reset(sections);
    let has_mud = scratch.palette.iter().any(|entry|
        crate::world::blocks::strip_namespace(entry.name(buf)) == "mud");
    let (classify_lo, classify_hi) = if options.caves || has_mud {
        // Cave water can sit anywhere below the surface. Mud also needs all
        // palettes: solid heightmaps cannot bound its surface or bed, especially
        // beside taller terrain. This only classifies palettes; without caves
        // enabled the column probes still start at exposed surfaces only.
        (i32::MIN / 2, i32::MAX / 2)
    } else {
        (band_lo, max_top)
    };
    let facts = cache.build_band(buf, &scratch.palette, sections, classify_lo, classify_hi);
    if !facts.has_water {
        return None;
    }

    let mut cw = ChunkWater::default();
    if facts.has_coral {
        cw.flags |= ChunkFlags::CORAL;
    }
    if facts.has_aquatic_plant {
        cw.flags |= ChunkFlags::PLANTS;
    }

    let mut cell_cols = [0u32; 16];
    let mut cell_ice = [0u32; 16];
    let mut cell_cave = [0u32; 16];
    let mut cell_surface_sum = [0i64; 16];
    let mut cell_depth_sum = [0i64; 16];
    let mut cell_ice_depth = [i32::MIN; 16];

    for z in 0..16usize {
        for x in 0..16usize {
            let i = z * 16 + x;
            let mv = mb[i];
            if mv == 0 {
                continue;
            }
            let top = min_y + mv as i32 - 1;
            let floor_hint = if has_floor_map && of[i] > 0 {
                Some(min_y + of[i] as i32 - 1)
            } else {
                None
            };
            let cell = (z >> 2) * 4 + (x >> 2);
            let Some(col) = probe_column(
                buf,
                sections,
                cache,
                x,
                z,
                top,
                floor_hint,
                &mut cell_ice_depth[cell],
            ) else {
                continue;
            };

            if !options.accepts(col.surface_y) {
                continue;
            }
            cw.set(x, z);
            cw.water_cols += 1;
            cell_cols[cell] += 1;
            cell_surface_sum[cell] += col.surface_y as i64;
            cell_depth_sum[cell] += (col.surface_y - col.floor_y).max(0) as i64;
            if col.ice {
                cell_ice[cell] += 1;
                stats.ice_columns += 1;
            }
        }
    }

    if options.caves {
        scan_caves(
            buf,
            sections,
            cache,
            mb,
            min_y,
            &mut cw,
            &mut cell_cols,
            &mut cell_cave,
            &mut cell_surface_sum,
            &mut cell_depth_sum,
            stats,
            options,
        );
    }

    if cw.water_cols == 0 {
        return None;
    }

    // One biome per 4x4 cell, sampled at the cell's mean water surface.
    for cell in 0..16usize {
        if cell_cols[cell] == 0 {
            continue;
        }
        let n = cell_cols[cell] as i64;
        let surface_y = (cell_surface_sum[cell] / n) as i32;
        let depth = (cell_depth_sum[cell] / n).clamp(0, u16::MAX as i64) as u16;
        let cx = (cell % 4) * 4;
        let cz = (cell / 4) * 4;

        let biome = biome_at(buf, scratch, cache, registry, cx, surface_y, cz);

        cw.cells[cell] = CellInfo {
            biome,
            surface_y: surface_y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            depth,
            water_cols: cell_cols[cell].min(16) as u8,
            ice_cols: cell_ice[cell].min(16) as u8,
            cave_cols: cell_cave[cell].min(16) as u8,
        };

        // Cave water must not pull the sea level histogram down: it sits at
        // whatever height its aquifer happens to be.
        if cell_cave[cell] * 2 < cell_cols[cell] {
            let ocean = registry
                .info(biome)
                .map(|b| b.family == BiomeFamily::Ocean)
                .unwrap_or(false);
            stats.record_surface(surface_y, cell_cols[cell] as u64, ocean);
        }
    }

    Some(cw)
}

/// Finds roofed-over water for every column that has no surface water.
///
/// "Can this block see the sky?" needs no ray cast: `MOTION_BLOCKING` already is
/// the height of the topmost fluid-or-motion-blocking block in the column, so
/// everything strictly below it is covered. A column that reached this point has
/// no surface water, which means its topmost block is not water - therefore any
/// water below that height is cave water, and the topmost such pool is the one
/// that gets reported.
///
/// Only sections whose palette contains water are decoded, and a column drops out
/// of the scan as soon as its pool is found.
#[allow(clippy::too_many_arguments)]
fn scan_caves(
    buf: &[u8],
    sections: &[SectionMeta],
    cache: &SectionCache,
    mb: &[u16; 256],
    min_y: i32,
    cw: &mut ChunkWater,
    cell_cols: &mut [u32; 16],
    cell_cave: &mut [u32; 16],
    cell_surface_sum: &mut [i64; 16],
    cell_depth_sum: &mut [i64; 16],
    stats: &mut ScanStats,
    options: ScanOptions,
) {
    // Columns that already have surface water, or no blocks at all, are done.
    let mut pending = 0u32;
    let mut resolved = [true; 256];
    for z in 0..16usize {
        for x in 0..16usize {
            let i = z * 16 + x;
            if mb[i] != 0 && !cw.get(x, z) {
                resolved[i] = false;
                pending += 1;
            }
        }
    }
    if pending == 0 {
        return;
    }

    for slot in cache.slots_top_down() {
        if pending == 0 {
            break;
        }
        if !cache.section_has_water(slot) {
            continue;
        }
        let section_y = sections[slot].y * 16;
        let reader = cache.reader(buf, sections, slot);
        for yy in (0..16usize).rev() {
            if pending == 0 {
                break;
            }
            let world_y = section_y + yy as i32;
            if !options.accepts(world_y) {
                continue;
            }
            for z in 0..16usize {
                for x in 0..16usize {
                    let i = z * 16 + x;
                    if resolved[i] {
                        continue;
                    }
                    // Strictly below the topmost block: no view of the sky.
                    if world_y >= min_y + mb[i] as i32 - 1 {
                        continue;
                    }
                    if !reader.class_at(x, yy, z).is_water() {
                        continue;
                    }
                    let floor = probe_floor(buf, sections, cache, x, z, world_y);
                    let cell = (z >> 2) * 4 + (x >> 2);
                    cw.set(x, z);
                    cw.water_cols += 1;
                    cell_cols[cell] += 1;
                    // In height mode a roof is not a cave-classification boundary:
                    // qualifying tunnel water must stay in normal river/sea groups.
                    if options.min_surface_y.is_none() {
                        cell_cave[cell] += 1;
                    }
                    cell_surface_sum[cell] += world_y as i64;
                    cell_depth_sum[cell] += (world_y - floor).max(0) as i64;
                    stats.cave_columns += 1;
                    resolved[i] = true;
                    pending -= 1;
                }
            }
        }
    }
}

/// Finds the water surface of one column, looking through a thin cover if needed.
#[allow(clippy::too_many_arguments)]
fn probe_column(
    buf: &[u8],
    sections: &[SectionMeta],
    cache: &SectionCache,
    x: usize,
    z: usize,
    top: i32,
    floor_hint: Option<i32>,
    cell_ice_depth: &mut i32,
) -> Option<WaterColumn> {
    let class = cache.class_at(buf, sections, x, top, z);

    if class.is_water() {
        let floor_y = match floor_hint {
            // Mud is motion blocking, so OCEAN_FLOOR can point into a bed that
            // belongs to our water mask. Continue through that bed rather than
            // treating its top as the end of the accepted water column.
            Some(f) if f < top && cache.class_at(buf, sections, x, f, z).is_water() => {
                probe_floor(buf, sections, cache, x, z, f)
            }
            Some(f) if f < top => f,
            _ => probe_floor(buf, sections, cache, x, z, top),
        };
        return Some(WaterColumn {
            surface_y: top,
            floor_y,
            ice: false,
        });
    }

    if !class.is_cover() {
        return None;
    }

    // Something thin sits on top - look for water directly beneath it.
    let mut ice = class == BlockClass::Ice;
    for step in 1..=config::SURFACE_COVER_PROBE {
        let y = top - step;
        match cache.class_at(buf, sections, x, y, z) {
            BlockClass::Water => {
                // The ocean floor heightmap points at the cover block here, so the
                // depth has to be probed. It is memoised per 4x4 cell because a
                // frozen ocean would otherwise probe 256 deep columns per chunk.
                let depth = if *cell_ice_depth != i32::MIN {
                    *cell_ice_depth
                } else {
                    let floor = probe_floor(buf, sections, cache, x, z, y);
                    let d = (y - floor).max(0);
                    *cell_ice_depth = d;
                    d
                };
                return Some(WaterColumn {
                    surface_y: y,
                    floor_y: y - depth,
                    ice,
                });
            }
            BlockClass::Ice => ice = true,
            BlockClass::Air | BlockClass::Cover => {}
            _ => return None,
        }
    }
    None
}

/// Walks down from `surface_y` while the column stays water.
fn probe_floor(
    buf: &[u8],
    sections: &[SectionMeta],
    cache: &SectionCache,
    x: usize,
    z: usize,
    surface_y: i32,
) -> i32 {
    let limit = surface_y - config::MAX_FLOOR_PROBE;
    let mut y = surface_y - 1;
    while y > limit {
        if !cache.class_at(buf, sections, x, y, z).is_water() {
            return y;
        }
        y -= 1;
    }
    limit
}

fn biome_at(
    buf: &[u8],
    scratch: &ChunkScratch,
    cache: &SectionCache,
    registry: &BiomeRegistry,
    x: usize,
    y: i32,
    z: usize,
) -> u16 {
    let Some(slot) = cache.slot_for_block_y(y) else {
        return UNKNOWN_BIOME;
    };
    let meta = &scratch.sections[slot];
    match chunk::biome_name_at(buf, &scratch.palette, meta, x, (y & 15) as usize, z) {
        Some(name) if !name.is_empty() => registry.id_of(name),
        _ => UNKNOWN_BIOME,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Heightmaps, PackedRef};

    /// A section at Y=48..63 with real packed block data and both heightmaps.
    /// Four strips contain exposed mud, water over mud, water over stone, and
    /// excluded flowing water / packed mud. `mud_only` makes the entire chunk
    /// exposed mud, so MOTION_BLOCKING equals OCEAN_FLOOR everywhere.
    fn swamp_fixture(mud_only: bool) -> (Vec<u8>, ChunkScratch, ParsedChunk) {
        let mut buf = Vec::new();
        let mut scratch = ChunkScratch::new();
        for (name, water_level) in [
            ("minecraft:air", None),
            ("minecraft:stone", None),
            ("minecraft:mud", None),
            ("minecraft:water", Some(0)),
            ("minecraft:water", Some(1)),
            ("minecraft:packed_mud", None),
            ("minecraft:swamp", None),
        ] {
            scratch.palette.push(PaletteEntry {
                off: buf.len() as u32,
                len: name.len() as u16,
                waterlogged: false,
                water_level,
            });
            buf.extend_from_slice(name.as_bytes());
        }
        let block_off = buf.len() as u32;
        for y in 0..16 {
            for _z in 0..16 {
                let mut word = 0u64;
                for x in 0..16 {
                    let strip = if mud_only { 0 } else { x / 4 };
                    let palette_index = match (strip, y) {
                        (_, 0..=3) => 1, // Stone at Y<=51.
                        (0 | 1, 4..=5) => 2, // Mud at Y=52..53.
                        (1 | 2, 6..=7) => 3, // Source water at Y=54..55.
                        (2, 4..=5) => 1,
                        (3, 4..=5) if x < 14 => 1,
                        (3, 6..=7) if x < 14 => 4, // Flowing water.
                        (3, 4..=7) => 5, // Dry packed mud.
                        _ => 0,
                    };
                    word |= palette_index << (x * 4);
                }
                buf.extend_from_slice(&word.to_be_bytes());
            }
        }
        scratch.sections.push(SectionMeta {
            y: 3,
            block_pal: (0, 6),
            block_data: PackedRef {
                off: block_off,
                longs: 256,
            },
            biome_pal: (6, 1),
            biome_data: PackedRef::default(),
        });
        let mut heightmap = |motion: bool| {
            let off = buf.len() as u32;
            for group in 0..37 {
                let mut word = 0u64;
                for k in 0..7 {
                    let i = group * 7 + k;
                    let x = i % 16;
                    let height = if i >= 256 {
                        0
                    } else if mud_only || x < 4 {
                        6
                    } else if motion || x >= 14 {
                        8
                    } else {
                        6
                    };
                    word |= height << (k * 9);
                }
                buf.extend_from_slice(&word.to_be_bytes());
            }
            PackedRef { off, longs: 37 }
        };
        let motion_blocking = heightmap(true);
        let ocean_floor = heightmap(false);
        let parsed = ParsedChunk {
            x: 0,
            z: 0,
            min_section_y: 3,
            data_version: 0,
            full: true,
            heightmaps: Heightmaps {
                motion_blocking,
                ocean_floor,
                world_surface: PackedRef::default(),
            },
        };
        (buf, scratch, parsed)
    }

    #[test]
    fn dry_surface_biomes_are_recorded_even_when_the_chunk_has_no_water() {
        let (mut buf, mut scratch, parsed) = swamp_fixture(true);
        let registry = BiomeRegistry::vanilla_only();
        assert_eq!(scan_dryland(&buf, &parsed, &scratch, &registry, &mut [0; 256]), 0);
        // Reuse the packed heightmap, but make every block solid. Only desert
        // banks count; savanna and other dry terrain must stay unmarked.
        scratch.sections[0].block_pal = (1, 1);
        for (name, expected) in [
            ("minecraft:desert", u16::MAX),
            ("minecraft:savanna", 0),
            ("minecraft:savanna_plateau", 0),
            ("minecraft:badlands", 0),
            ("minecraft:plains", 0),
        ] {
            scratch.palette[6].off = buf.len() as u32;
            scratch.palette[6].len = name.len() as u16;
            buf.extend_from_slice(name.as_bytes());
            assert_eq!(scan_dryland(&buf, &parsed, &scratch, &registry, &mut [0; 256]),
                expected, "{name}");
            assert!(scan_chunk(
                &buf, &parsed, &registry, &scratch, &mut SectionCache::default(),
                &mut [0; 256], &mut [0; 256], &mut ScanStats::default(),
                ScanOptions { caves: true, min_surface_y: Some(53) },
            ).is_none(), "land metadata must not create water geometry for {name}");
        }
    }

    #[test]
    fn exposed_mud_is_found_when_both_heightmaps_match() {
        let (buf, scratch, parsed) = swamp_fixture(true);
        let registry = BiomeRegistry::build(Path::new("__test_world_not_present__"));
        let cw = scan_chunk(
            &buf,
            &parsed,
            &registry,
            &scratch,
            &mut SectionCache::default(),
            &mut [0; 256],
            &mut [0; 256],
            &mut ScanStats::default(),
            ScanOptions { caves: false, min_surface_y: None },
        ).expect("exposed swamp mud must count even without a heightmap fluid gap");
        assert_eq!(cw.water_cols, 256);
        for cell in &cw.cells {
            assert_eq!(cell.surface_y, 53);
            assert_eq!(cell.depth, 2);
            assert_eq!(cell.biome, registry.id_of("minecraft:swamp"));
            assert_eq!(cell.cave_cols, 0);
        }
    }

    #[test]
    fn exposed_mud_beside_tall_terrain_is_probed_across_section_boundaries() {
        let registry = BiomeRegistry::vanilla_only();
        for water_above_mud in [false, true] {
            let (mut buf, mut scratch, mut parsed) = swamp_fixture(true);
            scratch.sections.clear();
            parsed.min_section_y = 0;
            for sy in 0..=6 {
                let off = buf.len() as u32;
                for local_y in 0..16 {
                    let y = sy * 16 + local_y;
                    for _z in 0..16 {
                        let mut word = 0u64;
                        for x in 0..16 {
                            let palette_index = if y <= 4 || (x >= 8 && y == 100)
                                || ((8..12).contains(&x) && y <= 100)
                            {
                                1
                            } else if (5..=16).contains(&y) {
                                2
                            } else if water_above_mud && (4..8).contains(&x) && y == 17 {
                                3
                            } else {
                                0
                            };
                            word |= palette_index << (x * 4);
                        }
                        buf.extend_from_slice(&word.to_be_bytes());
                    }
                }
                scratch.sections.push(SectionMeta {
                    y: sy,
                    block_pal: (0, 6),
                    block_data: PackedRef { off, longs: 256 },
                    biome_pal: (6, 1),
                    biome_data: PackedRef::default(),
                });
            }
            let mut heightmap = |motion: bool| {
                let off = buf.len() as u32;
                for group in 0..37 {
                    let mut word = 0u64;
                    for k in 0..7 {
                        let i = group * 7 + k;
                        let x = i % 16;
                        let height = if i >= 256 { 0 }
                            else if x >= 8 { 101 }
                            else if motion && water_above_mud && x >= 4 { 18 }
                            else { 17 };
                        word |= height << (k * 9);
                    }
                    buf.extend_from_slice(&word.to_be_bytes());
                }
                PackedRef { off, longs: 37 }
            };
            parsed.heightmaps.motion_blocking = heightmap(true);
            parsed.heightmaps.ocean_floor = heightmap(false);
            let mut stats = ScanStats::default();
            let cw = scan_chunk(
                &buf, &parsed, &registry, &scratch, &mut SectionCache::default(),
                &mut [0; 256], &mut [0; 256], &mut stats,
                ScanOptions { caves: false, min_surface_y: None },
            ).expect("exposed mud at Y16 must not be hidden by neighbouring terrain at Y100");
            assert_eq!(cw.water_cols, 128);
            assert_eq!(stats.cave_columns, 0);
            for z in 0..16 {
                for x in 0..16 {
                    assert_eq!(cw.get(x, z), x < 8, "covered mud must remain excluded");
                }
            }
            for row in 0..4 {
                assert_eq!(cw.cells[row * 4].depth, 12, "mud bed continues into section 0");
                assert_eq!(cw.cells[row * 4].surface_y, 16);
                assert_eq!(cw.cells[row * 4 + 1].depth, 12 + u16::from(water_above_mud));
                assert_eq!(cw.cells[row * 4 + 1].surface_y, 16 + i16::from(water_above_mud));
            }
        }
    }

    #[test]
    fn mud_depth_and_height_cutoff_preserve_source_only_water_rule() {
        let (buf, scratch, parsed) = swamp_fixture(false);
        let registry = BiomeRegistry::build(Path::new("__test_world_not_present__"));
        for (minimum, expected_columns) in [(53, 192), (54, 128), (56, 0)] {
            let cw = scan_chunk(
                &buf,
                &parsed,
                &registry,
                &scratch,
                &mut SectionCache::default(),
                &mut [0; 256],
                &mut [0; 256],
                &mut ScanStats::default(),
                ScanOptions { caves: true, min_surface_y: Some(minimum) },
            );
            if expected_columns == 0 {
                assert!(cw.is_none());
                continue;
            }
            let cw = cw.unwrap();
            assert_eq!(cw.water_cols, expected_columns);
            for z in 0..16 {
                for x in 0..16 {
                    assert_eq!(cw.get(x, z), x < 12 && (minimum <= 53 || x >= 4));
                }
            }
            for row in 0..4 {
                assert_eq!(cw.cells[row * 4 + 1].surface_y, 55);
                assert_eq!(cw.cells[row * 4 + 1].depth, 4, "include the mud bed beneath water");
                assert_eq!(cw.cells[row * 4 + 2].depth, 2, "ordinary water depth stays unchanged");
                assert_eq!(cw.cells[row * 4 + 3].water_cols, 0, "flowing water and dry packed mud stay excluded");
            }
        }
    }

    #[test]
    fn height_rule_keeps_covered_sources_and_excludes_flowing_water() {
        let mut buf = Vec::new();
        let mut scratch = ChunkScratch::new();
        for (name, water_level) in [
            ("minecraft:air", None),
            ("minecraft:stone", None),
            ("minecraft:water", Some(0)),
            ("minecraft:water", Some(1)),
        ] {
            scratch.palette.push(PaletteEntry {
                off: buf.len() as u32,
                len: name.len() as u16,
                waterlogged: false,
                water_level,
            });
            buf.extend_from_slice(name.as_bytes());
        }
        let block_off = buf.len() as u32;
        for y in 0..16 {
            for z in 0..16 {
                let mut word = 0u64;
                for x in 0..16 {
                    let class = if y < 4 || (y == 12 && x == 0 && z == 0) {
                        1
                    } else if y == 4 {
                        if x == 1 && z == 0 {
                            3
                        } else {
                            2
                        }
                    } else {
                        0
                    };
                    word |= class << (x * 4);
                }
                buf.extend_from_slice(&word.to_be_bytes());
            }
        }
        scratch.sections.push(SectionMeta {
            y: 0,
            block_pal: (0, 4),
            block_data: PackedRef {
                off: block_off,
                longs: 256,
            },
            biome_pal: (0, 0),
            biome_data: PackedRef::default(),
        });
        let mut heightmap = |roof: bool| {
            let off = buf.len() as u32;
            for group in 0..37 {
                let mut word = 0u64;
                for k in 0..7 {
                    let i = group * 7 + k;
                    let height = if i >= 256 {
                        0
                    } else if roof && i == 0 {
                        13
                    } else if roof {
                        5
                    } else {
                        4
                    };
                    word |= height << (k * 9);
                }
                buf.extend_from_slice(&word.to_be_bytes());
            }
            PackedRef { off, longs: 37 }
        };
        let motion_blocking = heightmap(true);
        let ocean_floor = heightmap(false);
        let parsed = ParsedChunk {
            x: 0,
            z: 0,
            min_section_y: 0,
            data_version: 0,
            full: true,
            heightmaps: Heightmaps {
                motion_blocking,
                ocean_floor,
                world_surface: PackedRef::default(),
            },
        };
        let registry = BiomeRegistry::build(Path::new("__test_world_not_present__"));
        for (options, expected_columns, roof_retained, cave_modifier) in [
            (ScanOptions { caves: false, min_surface_y: None }, 254, false, false),
            (ScanOptions { caves: true, min_surface_y: None }, 255, true, true),
            (ScanOptions { caves: true, min_surface_y: Some(4) }, 255, true, false),
            (ScanOptions { caves: true, min_surface_y: Some(5) }, 0, false, false),
        ] {
            let mut stats = ScanStats::default();
            let cw = scan_chunk(
                &buf,
                &parsed,
                &registry,
                &scratch,
                &mut SectionCache::default(),
                &mut [0; 256],
                &mut [0; 256],
                &mut stats,
                options,
            );
            if expected_columns == 0 {
                assert!(cw.is_none(), "water surfaces below the cutoff must be excluded");
                continue;
            }
            let cw = cw.unwrap();
            assert_eq!(
                cw.get(0, 0),
                roof_retained,
                "roofed source only enters with cave scanning"
            );
            assert!(
                !cw.get(1, 0),
                "flowing column must never enter the water mask"
            );
            assert!(cw.get(2, 0), "open source below sea level must remain");
            assert_eq!(cw.water_cols, expected_columns);
            assert_eq!(stats.cave_columns, u64::from(roof_retained));
            assert_eq!(cw.cells[0].cave_cols, u8::from(cave_modifier));
        }
    }
}
