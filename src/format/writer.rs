//! Writer for `water_regions.bin`.
//!
//! Region entry layout (48 bytes, big endian):
//!
//! ```text
//!  0  u32   id
//!  4  u8    kind          0 sea, 1 river, 2 lake, 3 swamp
//!  5  u8    temperature   0 hot, 1 warm, 2 medium, 3 cold
//!  6  u8    vegetation    0 none, 1 sparse, 2 normal, 3 jungle
//!  7  u8    depth         0 shallow, 1 normal, 2 deep, 0xFF none
//!  8  u16   modifiers     bit 0 ice, 1 corals, 2 desert, 3 mangrove
//! 10  i16   surface_y
//! 12  i32   min_x
//! 16  i32   min_z
//! 20  i32   max_x
//! 24  i32   max_z
//! 28  u32   column_count
//! 32  u32   first_run_index    index into the geometry run array
//! 36  u32   run_count
//! 40  u16   mean_depth
//! 42  u16   max_depth
//! 44  u8[4] bathymetry contour shares (0..255)
//! ```

use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use crate::water::model::WaterRegion;

use super::spatial_index::SpatialIndex;
use super::*;

struct Out<W: Write> {
    inner: W,
    written: u64,
}

impl<W: Write> Out<W> {
    fn new(inner: W) -> Self {
        Out { inner, written: 0 }
    }
    fn u8(&mut self, v: u8) -> std::io::Result<()> {
        self.raw(&[v])
    }
    fn u16(&mut self, v: u16) -> std::io::Result<()> {
        self.raw(&v.to_be_bytes())
    }
    fn i16(&mut self, v: i16) -> std::io::Result<()> {
        self.raw(&v.to_be_bytes())
    }
    fn u32(&mut self, v: u32) -> std::io::Result<()> {
        self.raw(&v.to_be_bytes())
    }
    fn i32(&mut self, v: i32) -> std::io::Result<()> {
        self.raw(&v.to_be_bytes())
    }
    fn u64(&mut self, v: u64) -> std::io::Result<()> {
        self.raw(&v.to_be_bytes())
    }
    fn raw(&mut self, b: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(b)?;
        self.written += b.len() as u64;
        Ok(())
    }
    fn pad_to(&mut self, target: u64) -> std::io::Result<()> {
        while self.written < target {
            let n = (target - self.written).min(64) as usize;
            let zeros = [0u8; 64];
            self.raw(&zeros[..n])?;
        }
        Ok(())
    }
}

fn write_header<W: Write>(out: &mut Out<W>, h: &FileHeader) -> std::io::Result<()> {
    out.raw(&h.magic)?;
    out.u16(h.format_version)?;
    out.u16(h.header_size)?;
    out.i32(h.minecraft_data_version)?;
    out.i16(h.sea_level)?;
    out.u8(h.spatial_cell_shift)?;
    out.u8(h.flags)?;
    out.u32(h.region_count)?;
    out.i32(h.world_min_x)?;
    out.i32(h.world_min_z)?;
    out.i32(h.world_max_x)?;
    out.i32(h.world_max_z)?;
    out.u64(h.region_table_offset)?;
    out.u64(h.geometry_offset)?;
    out.u64(h.spatial_index_offset)?;
    out.u64(h.file_size)?;
    out.pad_to(HEADER_SIZE as u64)?;
    Ok(())
}

/// Serialises regions plus their spatial index into a byte buffer.
pub fn encode(
    regions: &[WaterRegion],
    index: &SpatialIndex,
    minecraft_data_version: i32,
    sea_level: i16,
    bounds: (i32, i32, i32, i32),
) -> std::io::Result<Vec<u8>> {
    let total_runs: u64 = regions.iter().map(|r| r.geometry.runs.len() as u64).sum();

    let region_table_offset = HEADER_SIZE as u64;
    let geometry_offset = region_table_offset + regions.len() as u64 * REGION_ENTRY_SIZE as u64;
    let spatial_index_offset = geometry_offset + total_runs * RUN_SIZE as u64;
    let file_size = spatial_index_offset + index.byte_size() as u64;

    let header = FileHeader {
        minecraft_data_version,
        sea_level,
        spatial_cell_shift: index.shift as u8,
        region_count: regions.len() as u32,
        world_min_x: bounds.0,
        world_min_z: bounds.1,
        world_max_x: bounds.2,
        world_max_z: bounds.3,
        region_table_offset,
        geometry_offset,
        spatial_index_offset,
        file_size,
        ..Default::default()
    };

    let mut out = Out::new(Vec::with_capacity(file_size as usize));
    write_header(&mut out, &header)?;

    let mut first_run: u32 = 0;
    for r in regions {
        out.u32(r.id)?;
        out.u8(r.kind as u8)?;
        out.u8(r.temperature as u8)?;
        out.u8(r.vegetation as u8)?;
        out.u8(r.depth.map(|d| d as u8).unwrap_or(DEPTH_NONE))?;
        out.u16(r.modifiers.bits())?;
        out.i16(r.surface_y)?;
        out.i32(r.geometry.min_x)?;
        out.i32(r.geometry.min_z)?;
        out.i32(r.geometry.max_x)?;
        out.i32(r.geometry.max_z)?;
        out.u32(r.geometry.column_count)?;
        out.u32(first_run)?;
        out.u32(r.geometry.runs.len() as u32)?;
        out.u16(r.bathymetry.mean_depth)?;
        out.u16(r.bathymetry.max_depth)?;
        out.raw(&r.bathymetry.contour_shares)?;
        first_run += r.geometry.runs.len() as u32;
    }

    for r in regions {
        for run in &r.geometry.runs {
            out.i32(run.z)?;
            out.i32(run.x0)?;
            out.i32(run.x1)?;
        }
    }

    out.u32(index.cells_x)?;
    out.u32(index.cells_z)?;
    out.i32(index.origin_cell_x)?;
    out.i32(index.origin_cell_z)?;
    for c in &index.cells {
        out.u32(*c)?;
    }
    out.u32(index.lists.len() as u32)?;
    for v in &index.lists {
        out.u32(*v)?;
    }

    debug_assert_eq!(out.written, file_size);
    Ok(out.inner)
}

/// Writes `water_regions.bin` to disk.
pub fn write_file(
    path: &Path,
    regions: &[WaterRegion],
    index: &SpatialIndex,
    minecraft_data_version: i32,
    sea_level: i16,
    bounds: (i32, i32, i32, i32),
) -> std::io::Result<u64> {
    let bytes = encode(regions, index, minecraft_data_version, sea_level, bounds)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    w.write_all(&bytes)?;
    w.flush()?;
    let mut f = w.into_inner()?;
    let len = f.seek(SeekFrom::End(0))?;
    Ok(len)
}
