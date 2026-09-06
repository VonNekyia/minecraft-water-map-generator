//! Chunk decoding.
//!
//! Only the parts of a chunk the water scanner actually needs are decoded:
//! position, status, heightmaps and, per section, the palettes plus the byte
//! offset of the packed block/biome data. Nothing is copied out of the
//! decompressed buffer - palettes are recorded as `(offset, length)` ranges and
//! the packed `long[]` payloads are addressed in place.
//!
//! Palette entries are deliberately *not* classified during parsing. A chunk has
//! ~25 sections but the scanner usually touches two or three of them, so
//! classification happens lazily in [`SectionReader`].

use super::nbt::*;
use crate::world::blocks::{self, BlockClass};

/// A palette entry recorded as a range into the decompressed chunk buffer.
#[derive(Clone, Copy, Debug)]
pub struct PaletteEntry {
    pub off: u32,
    pub len: u16,
    /// `Properties.waterlogged == "true"`, i.e. the block state contains water.
    pub waterlogged: bool,
}

impl PaletteEntry {
    #[inline]
    pub fn name<'a>(&self, buf: &'a [u8]) -> &'a str {
        let s = self.off as usize;
        let e = s + self.len as usize;
        if e > buf.len() {
            return "";
        }
        std::str::from_utf8(&buf[s..e]).unwrap_or("")
    }
}

/// Packed `long[]` payload addressed by byte offset into the chunk buffer.
#[derive(Clone, Copy, Debug, Default)]
pub struct PackedRef {
    pub off: u32,
    pub longs: u32,
}

impl PackedRef {
    #[inline]
    pub fn long_at(&self, buf: &[u8], i: usize) -> u64 {
        let o = self.off as usize + i * 8;
        if i >= self.longs as usize || o + 8 > buf.len() {
            return 0;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[o..o + 8]);
        u64::from_be_bytes(b)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SectionMeta {
    pub y: i32,
    pub block_pal: (u32, u32),
    pub block_data: PackedRef,
    pub biome_pal: (u32, u32),
    pub biome_data: PackedRef,
}

/// Reusable per-thread scratch storage. Keeping the palette vectors alive across
/// chunks means a full world scan performs essentially no allocations.
#[derive(Default)]
pub struct ChunkScratch {
    pub sections: Vec<SectionMeta>,
    pub palette: Vec<PaletteEntry>,
}

impl ChunkScratch {
    pub fn new() -> Self {
        Self::default()
    }

    fn clear(&mut self) {
        self.sections.clear();
        self.palette.clear();
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Heightmaps {
    pub motion_blocking: PackedRef,
    pub ocean_floor: PackedRef,
    pub world_surface: PackedRef,
}

/// Everything decoded out of one chunk.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ParsedChunk {
    pub x: i32,
    pub z: i32,
    /// Lowest section index; `min_y = min_section_y * 16`.
    pub min_section_y: i32,
    pub data_version: i32,
    pub full: bool,
    pub heightmaps: Heightmaps,
}

impl ParsedChunk {
    #[inline]
    pub fn min_y(&self) -> i32 {
        self.min_section_y * 16
    }
}

/// `ceil(log2(n))`, with `ceil_log2(1) == 0`.
#[inline]
pub fn ceil_log2(n: usize) -> u32 {
    if n <= 1 {
        0
    } else {
        usize::BITS - (n - 1).leading_zeros()
    }
}

/// Bits per entry of a Minecraft heightmap given its `long[]` length.
///
/// Heightmaps store 256 values without straddling long boundaries, so the array
/// length uniquely determines the packing.
#[inline]
pub fn heightmap_bits(longs: usize) -> u32 {
    if longs == 0 {
        return 0;
    }
    let values_per_long = 256usize.div_ceil(longs);
    if values_per_long == 0 || values_per_long > 64 {
        return 0;
    }
    (64 / values_per_long) as u32
}

/// Decodes a 16x16 heightmap into `out`, indexed by `z * 16 + x`.
///
/// Values are raw heightmap values: the block Y is `min_y + value - 1`, and a
/// value of 0 means "nothing here".
pub fn decode_heightmap(buf: &[u8], packed: PackedRef, out: &mut [u16; 256]) -> bool {
    let bits = heightmap_bits(packed.longs as usize);
    if bits == 0 {
        out.fill(0);
        return false;
    }
    let per_long = (64 / bits) as usize;
    let mask = (1u64 << bits) - 1;
    let mut i = 0usize;
    for li in 0..packed.longs as usize {
        let word = packed.long_at(buf, li);
        for k in 0..per_long {
            if i >= 256 {
                break;
            }
            out[i] = ((word >> (k as u32 * bits)) & mask) as u16;
            i += 1;
        }
        if i >= 256 {
            break;
        }
    }
    while i < 256 {
        out[i] = 0;
        i += 1;
    }
    true
}

/// Reads one entry out of a packed palette-indexed array (block states / biomes).
#[inline]
pub fn packed_index(buf: &[u8], packed: PackedRef, bits: u32, i: usize) -> usize {
    if bits == 0 {
        return 0;
    }
    let per_long = (64 / bits) as usize;
    let li = i / per_long;
    let shift = ((i % per_long) as u32) * bits;
    let word = packed.long_at(buf, li);
    ((word >> shift) & ((1u64 << bits) - 1)) as usize
}

/// Parses a decompressed chunk. Returns `None` for chunks that are not fully
/// generated - those have no usable heightmaps.
pub fn parse_chunk(
    buf: &[u8],
    scratch: &mut ChunkScratch,
) -> Result<Option<ParsedChunk>> {
    scratch.clear();

    let mut r = NbtReader::new(buf);
    r.open_root()?;

    let mut x = 0i32;
    let mut z = 0i32;
    let mut min_section_y = -4i32;
    let mut data_version = 0i32;
    let mut full = false;
    let mut heightmaps = Heightmaps::default();

    while let Some((tag, name)) = r.next_entry()? {
        match (tag, name) {
            (TAG_INT, "xPos") => x = r.i32()?,
            (TAG_INT, "zPos") => z = r.i32()?,
            (TAG_INT, "yPos") => min_section_y = r.i32()?,
            (TAG_INT, "DataVersion") => data_version = r.i32()?,
            (TAG_STRING, "Status") => {
                let s = r.string()?;
                full = s == "minecraft:full" || s == "full";
            }
            (TAG_COMPOUND, "Heightmaps") => read_heightmaps(&mut r, &mut heightmaps)?,
            (TAG_LIST, "sections") => read_sections(&mut r, scratch)?,
            _ => r.skip_payload(tag)?,
        }
    }

    if !full {
        return Ok(None);
    }

    Ok(Some(ParsedChunk {
        x,
        z,
        min_section_y,
        data_version,
        full,
        heightmaps,
    }))
}

fn read_heightmaps(r: &mut NbtReader<'_>, out: &mut Heightmaps) -> Result<()> {
    while let Some((tag, name)) = r.next_entry()? {
        if tag != TAG_LONG_ARRAY {
            r.skip_payload(tag)?;
            continue;
        }
        let off = r.position() as u32 + 4; // skip the int length prefix
        let arr = r.long_array()?;
        let packed = PackedRef {
            off,
            longs: arr.len() as u32,
        };
        match name {
            "MOTION_BLOCKING" => out.motion_blocking = packed,
            "OCEAN_FLOOR" => out.ocean_floor = packed,
            "WORLD_SURFACE" => out.world_surface = packed,
            _ => {}
        }
    }
    Ok(())
}

fn read_sections(r: &mut NbtReader<'_>, scratch: &mut ChunkScratch) -> Result<()> {
    let (etag, n) = r.list_header()?;
    if etag != TAG_COMPOUND {
        for _ in 0..n {
            r.skip_payload(etag)?;
        }
        return Ok(());
    }
    for _ in 0..n {
        let mut meta = SectionMeta {
            y: i32::MIN,
            block_pal: (0, 0),
            block_data: PackedRef::default(),
            biome_pal: (0, 0),
            biome_data: PackedRef::default(),
        };
        while let Some((tag, name)) = r.next_entry()? {
            match (tag, name) {
                (TAG_BYTE, "Y") => meta.y = r.i8()? as i32,
                (TAG_INT, "Y") => meta.y = r.i32()?,
                (TAG_COMPOUND, "block_states") => {
                    let (pal, data) = read_block_states(r, scratch)?;
                    meta.block_pal = pal;
                    meta.block_data = data;
                }
                (TAG_COMPOUND, "biomes") => {
                    let (pal, data) = read_biomes(r, scratch)?;
                    meta.biome_pal = pal;
                    meta.biome_data = data;
                }
                _ => r.skip_payload(tag)?,
            }
        }
        if meta.y != i32::MIN {
            scratch.sections.push(meta);
        }
    }
    Ok(())
}

fn read_block_states(
    r: &mut NbtReader<'_>,
    scratch: &mut ChunkScratch,
) -> Result<((u32, u32), PackedRef)> {
    let pal_start = scratch.palette.len() as u32;
    let mut pal_len = 0u32;
    let mut data = PackedRef::default();

    while let Some((tag, name)) = r.next_entry()? {
        match (tag, name) {
            (TAG_LIST, "palette") => {
                let (etag, n) = r.list_header()?;
                if etag != TAG_COMPOUND {
                    for _ in 0..n {
                        r.skip_payload(etag)?;
                    }
                    continue;
                }
                for _ in 0..n {
                    let mut entry = PaletteEntry {
                        off: 0,
                        len: 0,
                        waterlogged: false,
                    };
                    while let Some((t2, n2)) = r.next_entry()? {
                        match (t2, n2) {
                            (TAG_STRING, "Name") => {
                                let off = r.position() as u32 + 2; // skip u16 length
                                let s = r.string()?;
                                entry.off = off;
                                entry.len = s.len() as u16;
                            }
                            (TAG_COMPOUND, "Properties") => {
                                entry.waterlogged = read_waterlogged(r)?;
                            }
                            _ => r.skip_payload(t2)?,
                        }
                    }
                    scratch.palette.push(entry);
                    pal_len += 1;
                }
            }
            (TAG_LONG_ARRAY, "data") => {
                let off = r.position() as u32 + 4;
                let arr = r.long_array()?;
                data = PackedRef {
                    off,
                    longs: arr.len() as u32,
                };
            }
            _ => r.skip_payload(tag)?,
        }
    }
    Ok(((pal_start, pal_len), data))
}

fn read_waterlogged(r: &mut NbtReader<'_>) -> Result<bool> {
    let mut wl = false;
    while let Some((tag, name)) = r.next_entry()? {
        if tag == TAG_STRING && name == "waterlogged" {
            wl = r.string()? == "true";
        } else {
            r.skip_payload(tag)?;
        }
    }
    Ok(wl)
}

fn read_biomes(
    r: &mut NbtReader<'_>,
    scratch: &mut ChunkScratch,
) -> Result<((u32, u32), PackedRef)> {
    let pal_start = scratch.palette.len() as u32;
    let mut pal_len = 0u32;
    let mut data = PackedRef::default();

    while let Some((tag, name)) = r.next_entry()? {
        match (tag, name) {
            (TAG_LIST, "palette") => {
                let (etag, n) = r.list_header()?;
                if etag != TAG_STRING {
                    for _ in 0..n {
                        r.skip_payload(etag)?;
                    }
                    continue;
                }
                for _ in 0..n {
                    let off = r.position() as u32 + 2;
                    let s = r.string()?;
                    scratch.palette.push(PaletteEntry {
                        off,
                        len: s.len() as u16,
                        waterlogged: false,
                    });
                    pal_len += 1;
                }
            }
            (TAG_LONG_ARRAY, "data") => {
                let off = r.position() as u32 + 4;
                let arr = r.long_array()?;
                data = PackedRef {
                    off,
                    longs: arr.len() as u32,
                };
            }
            _ => r.skip_payload(tag)?,
        }
    }
    Ok(((pal_start, pal_len), data))
}

/// Facts about a section's palette, collected in a single pass over its names.
#[derive(Clone, Copy, Default, Debug)]
pub struct SectionFacts {
    pub has_water: bool,
    pub has_ice: bool,
    pub has_coral: bool,
    pub has_aquatic_plant: bool,
}

/// Random access into one section's block data.
pub struct SectionReader<'a> {
    buf: &'a [u8],
    classes: &'a [u8],
    data: PackedRef,
    bits: u32,
}

/// Builds the block class table for one section and collects its palette facts.
pub fn classify_palette(
    buf: &[u8],
    palette: &[PaletteEntry],
    meta: &SectionMeta,
    out: &mut Vec<u8>,
) -> SectionFacts {
    let (start, len) = meta.block_pal;
    out.clear();
    out.reserve(len as usize);
    let mut facts = SectionFacts::default();
    for i in 0..len as usize {
        let entry = palette[start as usize + i];
        let name = entry.name(buf);
        let class = blocks::classify(name, entry.waterlogged);
        match class {
            BlockClass::Water => facts.has_water = true,
            BlockClass::Ice => facts.has_ice = true,
            _ => {}
        }
        if blocks::is_coral(name) {
            facts.has_coral = true;
        }
        if blocks::is_aquatic_plant(name) {
            facts.has_aquatic_plant = true;
        }
        out.push(class as u8);
    }
    facts
}

impl<'a> SectionReader<'a> {
    /// Builds a reader over a class table produced by [`classify_palette`].
    pub fn new(buf: &'a [u8], classes: &'a [u8], meta: &SectionMeta) -> Self {
        let bits = if classes.len() <= 1 {
            0
        } else {
            ceil_log2(classes.len()).max(4)
        };
        SectionReader {
            buf,
            classes,
            data: meta.block_data,
            bits,
        }
    }

    /// Class of the block at the given in-section coordinates (each 0..16).
    #[inline]
    pub fn class_at(&self, x: usize, y: usize, z: usize) -> BlockClass {
        if self.classes.is_empty() {
            return BlockClass::Air;
        }
        let i = (y << 8) | (z << 4) | x;
        let pi = if self.bits == 0 {
            0
        } else {
            packed_index(self.buf, self.data, self.bits, i)
        };
        match self.classes.get(pi).copied().unwrap_or(0) {
            1 => BlockClass::Air,
            2 => BlockClass::Water,
            3 => BlockClass::Ice,
            4 => BlockClass::Cover,
            5 => BlockClass::Lava,
            _ => BlockClass::Solid,
        }
    }
}

/// Reads the biome resource location for a 4x4x4 cell of a section.
pub fn biome_name_at<'a>(
    buf: &'a [u8],
    palette: &[PaletteEntry],
    meta: &SectionMeta,
    x: usize,
    y: usize,
    z: usize,
) -> Option<&'a str> {
    let (start, len) = meta.biome_pal;
    if len == 0 {
        return None;
    }
    let bits = ceil_log2(len as usize);
    let i = ((y >> 2) << 4) | ((z >> 2) << 2) | (x >> 2);
    let pi = if bits == 0 {
        0
    } else {
        packed_index(buf, meta.biome_data, bits, i)
    };
    let pi = pi.min(len as usize - 1);
    Some(palette[start as usize + pi].name(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_log2_matches_minecraft_palette_sizing() {
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(16), 4);
        assert_eq!(ceil_log2(17), 5);
    }

    #[test]
    fn heightmap_bit_width_is_derived_from_array_length() {
        assert_eq!(heightmap_bits(37), 9); // 384 high overworld
        assert_eq!(heightmap_bits(32), 8);
        assert_eq!(heightmap_bits(16), 4);
        assert_eq!(heightmap_bits(0), 0);
    }

    #[test]
    fn heightmap_decodes_nine_bit_values() {
        // Seven 9-bit values per long, no straddling.
        let mut bytes = Vec::new();
        let mut word = 0u64;
        for k in 0..7u32 {
            word |= ((k as u64) + 100) << (9 * k);
        }
        bytes.extend_from_slice(&word.to_be_bytes());
        for _ in 1..37 {
            bytes.extend_from_slice(&0u64.to_be_bytes());
        }
        let packed = PackedRef { off: 0, longs: 37 };
        let mut out = [0u16; 256];
        assert!(decode_heightmap(&bytes, packed, &mut out));
        for (k, value) in out.iter().take(7).enumerate() {
            assert_eq!(*value, 100 + k as u16);
        }
        assert_eq!(out[7], 0);
    }
}
