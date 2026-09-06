//! Reference reader for `water_regions.bin`.
//!
//! The analyzer itself never needs to read its own output, but a reader keeps the
//! format honest: it is exercised by the round-trip tests and doubles as the
//! specification a Java consumer can be written against.

use crate::water::model::*;

use super::*;

#[derive(Debug)]
pub enum ReadError {
    TooShort,
    BadMagic,
    UnsupportedVersion(u16),
    Corrupt(&'static str),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::TooShort => write!(f, "file is too short"),
            ReadError::BadMagic => write!(f, "not a water_regions.bin file"),
            ReadError::UnsupportedVersion(v) => write!(f, "unsupported format version {v}"),
            ReadError::Corrupt(w) => write!(f, "corrupt file: {w}"),
        }
    }
}

impl std::error::Error for ReadError {}

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn at(b: &'a [u8], p: usize) -> Self {
        Cur { b, p }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ReadError> {
        let e = self.p.checked_add(n).ok_or(ReadError::TooShort)?;
        if e > self.b.len() {
            return Err(ReadError::TooShort);
        }
        let s = &self.b[self.p..e];
        self.p = e;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ReadError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn i16(&mut self) -> Result<i16, ReadError> {
        Ok(self.u16()? as i16)
    }
    fn u32(&mut self) -> Result<u32, ReadError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i32(&mut self) -> Result<i32, ReadError> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> Result<u64, ReadError> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

/// A decoded `water_regions.bin`.
pub struct WaterRegionFile {
    pub header: FileHeader,
    pub regions: Vec<WaterRegion>,
    pub cells_x: u32,
    pub cells_z: u32,
    pub origin_cell_x: i32,
    pub origin_cell_z: i32,
    pub cells: Vec<u32>,
    pub lists: Vec<u32>,
}

impl WaterRegionFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, ReadError> {
        let mut c = Cur::at(bytes, 0);
        let magic = c.take(8)?;
        if magic != MAGIC {
            return Err(ReadError::BadMagic);
        }
        let format_version = c.u16()?;
        if format_version != FORMAT_VERSION {
            return Err(ReadError::UnsupportedVersion(format_version));
        }
        let header_size = c.u16()?;
        let header = FileHeader {
            magic: MAGIC,
            format_version,
            header_size,
            minecraft_data_version: c.i32()?,
            sea_level: c.i16()?,
            spatial_cell_shift: c.u8()?,
            flags: c.u8()?,
            region_count: c.u32()?,
            world_min_x: c.i32()?,
            world_min_z: c.i32()?,
            world_max_x: c.i32()?,
            world_max_z: c.i32()?,
            region_table_offset: c.u64()?,
            geometry_offset: c.u64()?,
            spatial_index_offset: c.u64()?,
            file_size: c.u64()?,
        };
        if bytes.len() as u64 != header.file_size {
            return Err(ReadError::Corrupt("file size mismatch"));
        }

        // Region table.
        let mut regions = Vec::with_capacity(header.region_count as usize);
        let mut entries = Vec::with_capacity(header.region_count as usize);
        let mut t = Cur::at(bytes, header.region_table_offset as usize);
        for _ in 0..header.region_count {
            let id = t.u32()?;
            let kind = WaterKind::from_u8(t.u8()?).ok_or(ReadError::Corrupt("kind"))?;
            let temperature =
                Temperature::from_u8(t.u8()?).ok_or(ReadError::Corrupt("temperature"))?;
            let vegetation =
                Vegetation::from_u8(t.u8()?).ok_or(ReadError::Corrupt("vegetation"))?;
            let depth_raw = t.u8()?;
            let depth = if depth_raw == DEPTH_NONE {
                None
            } else {
                Some(Depth::from_u8(depth_raw).ok_or(ReadError::Corrupt("depth"))?)
            };
            let modifiers = Modifiers::from_bits_truncate(t.u16()?);
            let surface_y = t.i16()?;
            let min_x = t.i32()?;
            let min_z = t.i32()?;
            let max_x = t.i32()?;
            let max_z = t.i32()?;
            let column_count = t.u32()?;
            let first_run = t.u32()?;
            let run_count = t.u32()?;
            let mean_depth = t.u16()?;
            let max_depth = t.u16()?;
            let shares = t.take(4)?;
            entries.push((first_run, run_count));
            regions.push(WaterRegion {
                id,
                geometry: RegionGeometry {
                    min_x,
                    min_z,
                    max_x,
                    max_z,
                    runs: Vec::with_capacity(run_count as usize),
                    column_count,
                },
                kind,
                temperature,
                vegetation,
                depth,
                modifiers,
                surface_y,
                bathymetry: Bathymetry {
                    mean_depth,
                    max_depth,
                    contour_shares: [shares[0], shares[1], shares[2], shares[3]],
                },
                dominant_biome: String::new(),
            });
        }

        // Geometry.
        for (region, (first_run, run_count)) in regions.iter_mut().zip(entries) {
            let off = header.geometry_offset + first_run as u64 * RUN_SIZE as u64;
            let mut g = Cur::at(bytes, off as usize);
            for _ in 0..run_count {
                let z = g.i32()?;
                let x0 = g.i32()?;
                let x1 = g.i32()?;
                region.geometry.runs.push(Run { z, x0, x1 });
            }
        }

        // Spatial index.
        let mut s = Cur::at(bytes, header.spatial_index_offset as usize);
        let cells_x = s.u32()?;
        let cells_z = s.u32()?;
        let origin_cell_x = s.i32()?;
        let origin_cell_z = s.i32()?;
        let n = (cells_x as usize)
            .checked_mul(cells_z as usize)
            .ok_or(ReadError::Corrupt("index size"))?;
        let mut cells = Vec::with_capacity(n);
        for _ in 0..n {
            cells.push(s.u32()?);
        }
        let list_len = s.u32()? as usize;
        let mut lists = Vec::with_capacity(list_len);
        for _ in 0..list_len {
            lists.push(s.u32()?);
        }

        Ok(WaterRegionFile {
            header,
            regions,
            cells_x,
            cells_z,
            origin_cell_x,
            origin_cell_z,
            cells,
            lists,
        })
    }

    /// `x/z -> region`, exactly the lookup a Java consumer performs.
    pub fn region_at(&self, x: i32, z: i32) -> Option<&WaterRegion> {
        let shift = self.header.spatial_cell_shift as u32;
        let cx = (x >> shift) - self.origin_cell_x;
        let cz = (z >> shift) - self.origin_cell_z;
        if cx < 0 || cz < 0 || cx as u32 >= self.cells_x || cz as u32 >= self.cells_z {
            return None;
        }
        let v = self.cells[(cz as u32 * self.cells_x + cx as u32) as usize];
        if v == CELL_EMPTY {
            return None;
        }
        if v & CELL_INLINE != 0 {
            let r = self.regions.get((v & !CELL_INLINE) as usize)?;
            return if r.geometry.contains(x, z) { Some(r) } else { None };
        }
        let start = v as usize;
        let n = self.lists[start] as usize;
        for id in &self.lists[start + 1..start + 1 + n] {
            if let Some(r) = self.regions.get(*id as usize) {
                if r.geometry.contains(x, z) {
                    return Some(r);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::spatial_index::SpatialIndex;
    use crate::format::writer;

    fn sample_regions() -> Vec<WaterRegion> {
        vec![
            WaterRegion {
                id: 0,
                geometry: RegionGeometry {
                    min_x: -20,
                    min_z: -5,
                    max_x: 100,
                    max_z: 3,
                    runs: vec![
                        Run { z: -5, x0: -20, x1: 100 },
                        Run { z: -4, x0: -20, x1: 100 },
                        Run { z: 3, x0: 0, x1: 10 },
                    ],
                    column_count: 121 * 2 + 11,
                },
                kind: WaterKind::Sea,
                temperature: Temperature::Warm,
                vegetation: Vegetation::Jungle,
                depth: Some(Depth::Normal),
                modifiers: Modifiers::CORALS,
                surface_y: 62,
                bathymetry: Bathymetry {
                    mean_depth: 17,
                    max_depth: 44,
                    contour_shares: [200, 120, 30, 0],
                },
                dominant_biome: "minecraft:warm_ocean".into(),
            },
            WaterRegion {
                id: 1,
                geometry: RegionGeometry {
                    min_x: 500,
                    min_z: 500,
                    max_x: 505,
                    max_z: 500,
                    runs: vec![Run { z: 500, x0: 500, x1: 505 }],
                    column_count: 6,
                },
                kind: WaterKind::Lake,
                temperature: Temperature::Cold,
                vegetation: Vegetation::Sparse,
                depth: None,
                modifiers: Modifiers::ICE,
                surface_y: 90,
                bathymetry: Bathymetry::default(),
                dominant_biome: "minecraft:snowy_plains".into(),
            },
        ]
    }

    #[test]
    fn binary_round_trip_preserves_everything() {
        let regions = sample_regions();
        let bounds = (-512, -512, 1023, 1023);
        let index = SpatialIndex::build(&regions, bounds);
        let bytes = writer::encode(&regions, &index, 4671, 63, bounds).unwrap();

        let parsed = WaterRegionFile::parse(&bytes).unwrap();
        assert_eq!(parsed.header.magic, MAGIC);
        assert_eq!(parsed.header.format_version, FORMAT_VERSION);
        assert_eq!(parsed.header.minecraft_data_version, 4671);
        assert_eq!(parsed.header.sea_level, 63);
        assert_eq!(parsed.header.region_count, 2);
        assert_eq!(parsed.header.world_min_x, -512);
        assert_eq!(parsed.header.world_max_z, 1023);
        assert_eq!(parsed.regions.len(), 2);

        for (a, b) in regions.iter().zip(parsed.regions.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.temperature, b.temperature);
            assert_eq!(a.vegetation, b.vegetation);
            assert_eq!(a.depth, b.depth);
            assert_eq!(a.modifiers, b.modifiers);
            assert_eq!(a.surface_y, b.surface_y);
            assert_eq!(a.geometry.runs, b.geometry.runs);
            assert_eq!(a.geometry.column_count, b.geometry.column_count);
            assert_eq!(a.geometry.min_x, b.geometry.min_x);
            assert_eq!(a.geometry.max_z, b.geometry.max_z);
            assert_eq!(a.bathymetry.mean_depth, b.bathymetry.mean_depth);
            assert_eq!(a.bathymetry.contour_shares, b.bathymetry.contour_shares);
        }
    }

    #[test]
    fn spatial_lookup_survives_the_round_trip() {
        let regions = sample_regions();
        let bounds = (-512, -512, 1023, 1023);
        let index = SpatialIndex::build(&regions, bounds);
        let bytes = writer::encode(&regions, &index, 4671, 63, bounds).unwrap();
        let parsed = WaterRegionFile::parse(&bytes).unwrap();

        assert_eq!(parsed.region_at(0, -5).map(|r| r.id), Some(0));
        assert_eq!(parsed.region_at(100, -4).map(|r| r.id), Some(0));
        assert_eq!(parsed.region_at(101, -4).map(|r| r.id), None);
        assert_eq!(parsed.region_at(503, 500).map(|r| r.id), Some(1));
        assert_eq!(parsed.region_at(503, 501).map(|r| r.id), None);
        // No region == no classified water.
        assert!(parsed.region_at(9_000, 9_000).is_none());
    }

    #[test]
    fn empty_world_round_trips() {
        let regions: Vec<WaterRegion> = Vec::new();
        let bounds = (0, 0, 63, 63);
        let index = SpatialIndex::build(&regions, bounds);
        let bytes = writer::encode(&regions, &index, 0, 63, bounds).unwrap();
        let parsed = WaterRegionFile::parse(&bytes).unwrap();
        assert_eq!(parsed.header.region_count, 0);
        assert!(parsed.regions.is_empty());
        assert!(parsed.region_at(0, 0).is_none());
    }

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        assert!(matches!(
            WaterRegionFile::parse(&[]),
            Err(ReadError::TooShort)
        ));
        assert!(matches!(
            WaterRegionFile::parse(&[0u8; 200]),
            Err(ReadError::BadMagic)
        ));

        let regions = sample_regions();
        let bounds = (-512, -512, 1023, 1023);
        let index = SpatialIndex::build(&regions, bounds);
        let bytes = writer::encode(&regions, &index, 1, 63, bounds).unwrap();
        for cut in 1..bytes.len().min(400) {
            assert!(WaterRegionFile::parse(&bytes[..cut]).is_err());
        }
    }
}
