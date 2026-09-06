//! Reader for Minecraft Anvil region files (`region/r.<x>.<z>.mca`).
//!
//! A region file is a 4 KiB location table, a 4 KiB timestamp table and then the
//! chunk payloads, each padded to 4 KiB sectors. The whole file is read into one
//! buffer because a region file is at most a few dozen MiB and we then touch
//! every chunk in it anyway - one sequential read beats 1024 random seeks.

use std::io::Read;
use std::path::{Path, PathBuf};

pub const SECTOR: usize = 4096;
pub const CHUNKS_PER_REGION: usize = 1024;

pub const COMPRESSION_GZIP: u8 = 1;
pub const COMPRESSION_ZLIB: u8 = 2;
pub const COMPRESSION_NONE: u8 = 3;
/// Bit set in the compression byte when the payload lives in an external
/// `c.<x>.<z>.mcc` file instead of inside the region file.
pub const COMPRESSION_EXTERNAL: u8 = 0x80;

#[derive(Debug)]
pub struct RegionFile {
    pub region_x: i32,
    pub region_z: i32,
    pub path: PathBuf,
    data: Vec<u8>,
}

/// One raw, still compressed chunk payload.
pub struct RawChunk<'a> {
    /// Index inside the region, `(z & 31) * 32 + (x & 31)`.
    pub index: usize,
    pub compression: u8,
    pub payload: &'a [u8],
    /// Set when the payload had to be loaded from an external `.mcc` file.
    pub external: Option<PathBuf>,
}

/// Parses `r.<x>.<z>.mca` into region coordinates.
pub fn parse_region_coords(path: &Path) -> Option<(i32, i32)> {
    let stem = path.file_name()?.to_str()?;
    let mut it = stem.split('.');
    if it.next()? != "r" {
        return None;
    }
    let x = it.next()?.parse().ok()?;
    let z = it.next()?.parse().ok()?;
    if it.next()? != "mca" {
        return None;
    }
    Some((x, z))
}

impl RegionFile {
    pub fn open(path: &Path) -> std::io::Result<Option<Self>> {
        let Some((region_x, region_z)) = parse_region_coords(path) else {
            return Ok(None);
        };
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
        let mut data = Vec::with_capacity(len.max(2 * SECTOR));
        file.read_to_end(&mut data)?;
        if data.len() < 2 * SECTOR {
            // Empty or truncated region file: no chunks, not an error.
            return Ok(Some(RegionFile {
                region_x,
                region_z,
                path: path.to_path_buf(),
                data: Vec::new(),
            }));
        }
        Ok(Some(RegionFile {
            region_x,
            region_z,
            path: path.to_path_buf(),
            data,
        }))
    }

    /// Chunk coordinate of the chunk at `index` inside this region.
    #[inline]
    pub fn chunk_coords(&self, index: usize) -> (i32, i32) {
        (
            self.region_x * 32 + (index % 32) as i32,
            self.region_z * 32 + (index / 32) as i32,
        )
    }

    /// Yields every present chunk payload. Corrupt entries are skipped silently -
    /// a single broken chunk must never abort a multi-hour world scan.
    pub fn chunks(&self) -> impl Iterator<Item = RawChunk<'_>> + '_ {
        (0..CHUNKS_PER_REGION).filter_map(move |i| self.chunk(i))
    }

    pub fn chunk(&self, index: usize) -> Option<RawChunk<'_>> {
        if self.data.len() < 2 * SECTOR {
            return None;
        }
        let e = index * 4;
        let b = &self.data[e..e + 4];
        let offset = ((b[0] as usize) << 16) | ((b[1] as usize) << 8) | b[2] as usize;
        let sectors = b[3] as usize;
        if offset < 2 || sectors == 0 {
            return None;
        }
        let start = offset * SECTOR;
        if start + 5 > self.data.len() {
            return None;
        }
        let length = u32::from_be_bytes([
            self.data[start],
            self.data[start + 1],
            self.data[start + 2],
            self.data[start + 3],
        ]) as usize;
        if length == 0 {
            return None;
        }
        let compression = self.data[start + 4];
        let body_start = start + 5;
        let body_end = body_start + (length - 1);
        if body_end > self.data.len() {
            return None;
        }

        if compression & COMPRESSION_EXTERNAL != 0 {
            let (cx, cz) = self.chunk_coords(index);
            let external = self
                .path
                .parent()
                .map(|d| d.join(format!("c.{cx}.{cz}.mcc")))?;
            return Some(RawChunk {
                index,
                compression: compression & !COMPRESSION_EXTERNAL,
                payload: &[],
                external: Some(external),
            });
        }

        Some(RawChunk {
            index,
            compression,
            payload: &self.data[body_start..body_end],
            external: None,
        })
    }
}

/// Reusable inflater. One instance per worker thread keeps the output buffer warm
/// so decompressing millions of chunks does not hammer the allocator.
pub struct Inflater {
    scratch: Vec<u8>,
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Inflater {
    pub fn new() -> Self {
        Inflater {
            scratch: Vec::with_capacity(1 << 20),
        }
    }

    /// Decompresses `chunk` into `out` and returns the decompressed slice.
    pub fn inflate<'o>(
        &mut self,
        chunk: &RawChunk<'_>,
        out: &'o mut Vec<u8>,
    ) -> std::io::Result<&'o [u8]> {
        let src: &[u8] = match &chunk.external {
            Some(path) => {
                self.scratch.clear();
                std::fs::File::open(path)?.read_to_end(&mut self.scratch)?;
                &self.scratch
            }
            None => chunk.payload,
        };

        out.clear();
        match chunk.compression {
            COMPRESSION_ZLIB => {
                flate2::read::ZlibDecoder::new(src).read_to_end(out)?;
            }
            COMPRESSION_GZIP => {
                flate2::read::GzDecoder::new(src).read_to_end(out)?;
            }
            COMPRESSION_NONE => out.extend_from_slice(src),
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unsupported chunk compression {other}"),
                ))
            }
        }
        Ok(out.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_coordinates_are_parsed() {
        assert_eq!(
            parse_region_coords(Path::new("r.0.0.mca")),
            Some((0, 0))
        );
        assert_eq!(
            parse_region_coords(Path::new("/a/b/r.-34.24.mca")),
            Some((-34, 24))
        );
        assert_eq!(parse_region_coords(Path::new("r.0.0.mcr")), None);
        assert_eq!(parse_region_coords(Path::new("level.dat")), None);
    }

    #[test]
    fn chunk_index_maps_to_world_chunk_coordinates() {
        let r = RegionFile {
            region_x: -2,
            region_z: 3,
            path: PathBuf::from("r.-2.3.mca"),
            data: Vec::new(),
        };
        assert_eq!(r.chunk_coords(0), (-64, 96));
        assert_eq!(r.chunk_coords(33), (-63, 97));
        assert_eq!(r.chunk_coords(1023), (-33, 127));
    }

    #[test]
    fn roundtrip_through_a_synthetic_region_file() {
        use std::io::Write;
        let payload = b"hello anvil";
        let mut deflated = Vec::new();
        {
            let mut enc =
                flate2::write::ZlibEncoder::new(&mut deflated, flate2::Compression::fast());
            enc.write_all(payload).unwrap();
            enc.finish().unwrap();
        }

        let mut data = vec![0u8; 3 * SECTOR];
        // Chunk 7 lives in sector 2, one sector long.
        data[7 * 4] = 0;
        data[7 * 4 + 1] = 0;
        data[7 * 4 + 2] = 2;
        data[7 * 4 + 3] = 1;
        let start = 2 * SECTOR;
        let len = (deflated.len() + 1) as u32;
        data[start..start + 4].copy_from_slice(&len.to_be_bytes());
        data[start + 4] = COMPRESSION_ZLIB;
        data[start + 5..start + 5 + deflated.len()].copy_from_slice(&deflated);

        let region = RegionFile {
            region_x: 0,
            region_z: 0,
            path: PathBuf::from("r.0.0.mca"),
            data,
        };
        let chunks: Vec<_> = region.chunks().map(|c| c.index).collect();
        assert_eq!(chunks, vec![7]);

        let raw = region.chunk(7).unwrap();
        let mut inf = Inflater::new();
        let mut out = Vec::new();
        let got = inf.inflate(&raw, &mut out).unwrap();
        assert_eq!(got, payload);
    }
}
