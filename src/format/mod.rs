//! The `water_regions.bin` container format.
//!
//! Design goals, in this order: readable from Java with nothing but a
//! `ByteBuffer`, versioned and extensible, compact, and cheap to query.
//!
//! * **Endianness** - everything is big endian, which is what `DataInputStream`
//!   and a default `ByteBuffer` already use.
//! * **Versioning** - `format_version` is bumped for incompatible changes.
//!   `header_size` lets an old reader skip header fields it does not know, and
//!   the section offsets in the header mean sections can grow without moving
//!   anything a reader already understands.
//! * **No Rust specifics** - no bincode, no serde, no varints. Every field is a
//!   fixed width integer at a documented offset.
//!
//! ```text
//! +--------------------+ 0
//! | header (80 bytes)  |
//! +--------------------+ region_table_offset
//! | region table       |  region_count * 48 bytes
//! +--------------------+ geometry_offset
//! | geometry runs      |  (z, x0, x1) i32 triples per region
//! +--------------------+ spatial_index_offset
//! | spatial index      |  cell grid + overflow lists
//! +--------------------+ file_size
//! ```

pub mod reader;
pub mod spatial_index;
pub mod writer;

/// File magic. Never changes.
pub const MAGIC: [u8; 8] = *b"MCWATER\0";

/// Current format version.
///
/// * **2** - `temperature` lost its `hot` level and is now `0 warm, 1 medium,
///   2 cold`; `depth` is measured for every kind of water instead of only for
///   `sea`, so `0xFF` no longer appears in practice.
/// * **1** - initial release.
pub const FORMAT_VERSION: u16 = 2;

/// Size of the header in bytes.
pub const HEADER_SIZE: u16 = 80;

/// Size of one region table entry in bytes.
pub const REGION_ENTRY_SIZE: u32 = 48;

/// Size of one geometry run in bytes: `z`, `x0`, `x1` as big endian `i32`.
pub const RUN_SIZE: u32 = 12;

/// `depth` value written when a region has no depth (everything but `Sea`).
pub const DEPTH_NONE: u8 = 0xFF;

/// Marks an empty spatial index cell.
pub const CELL_EMPTY: u32 = 0xFFFF_FFFF;

/// Bit set in a spatial index cell that stores a single region id inline.
pub const CELL_INLINE: u32 = 0x8000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeader {
    pub magic: [u8; 8],
    pub format_version: u16,
    pub header_size: u16,
    pub minecraft_data_version: i32,
    pub sea_level: i16,
    pub spatial_cell_shift: u8,
    pub flags: u8,
    pub region_count: u32,
    pub world_min_x: i32,
    pub world_min_z: i32,
    pub world_max_x: i32,
    pub world_max_z: i32,
    pub region_table_offset: u64,
    pub geometry_offset: u64,
    pub spatial_index_offset: u64,
    pub file_size: u64,
}

impl Default for FileHeader {
    fn default() -> Self {
        FileHeader {
            magic: MAGIC,
            format_version: FORMAT_VERSION,
            header_size: HEADER_SIZE,
            minecraft_data_version: 0,
            sea_level: 63,
            spatial_cell_shift: crate::config::SPATIAL_CELL_SHIFT as u8,
            flags: 0,
            region_count: 0,
            world_min_x: 0,
            world_min_z: 0,
            world_max_x: 0,
            world_max_z: 0,
            region_table_offset: HEADER_SIZE as u64,
            geometry_offset: HEADER_SIZE as u64,
            spatial_index_offset: HEADER_SIZE as u64,
            file_size: HEADER_SIZE as u64,
        }
    }
}
