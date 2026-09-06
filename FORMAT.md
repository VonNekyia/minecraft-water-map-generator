# `water_regions.bin` - format specification

Version 2.

Changes from version 1: `temperature` lost its `hot` level and renumbered, and
`depth` is now filled in for every kind of water rather than only for `sea`. A
version 1 reader would misread both, so `format_version` must be checked.

Everything is **big endian**, which is what Java's `DataInputStream` and a default
`ByteBuffer` already use. Every field is a fixed-width integer at a documented
offset - there are no varints, no serialization framework and nothing
Rust-specific. The file is meant to be memory-mapped or read once into a byte
array and queried in place.

## Layout

```text
+---------------------+ 0
| header              |  80 bytes
+---------------------+ region_table_offset
| region table        |  region_count * 48 bytes
+---------------------+ geometry_offset
| geometry runs       |  12 bytes per run
+---------------------+ spatial_index_offset
| spatial index       |  cell grid + overflow lists
+---------------------+ file_size
```

Sections are addressed through offsets in the header, so a later format version
can grow or reorder them without breaking a reader that only uses the offsets.

## Header (80 bytes)

| off | type    | field                    | notes |
|-----|---------|--------------------------|-------|
| 0   | u8[8]   | `magic`                  | `4D 43 57 41 54 45 52 00` (`"MCWATER\0"`) |
| 8   | u16     | `format_version`         | 1 |
| 10  | u16     | `header_size`            | 80; skip to this offset instead of assuming |
| 12  | i32     | `minecraft_data_version` | `DataVersion` from `level.dat`, 0 if unknown |
| 16  | i16     | `sea_level`              | detected from the world; top water block is at `sea_level - 1` |
| 18  | u8      | `spatial_cell_shift`     | log2 of the index cell size in blocks (6 = 64x64) |
| 19  | u8      | `flags`                  | reserved, 0 |
| 20  | u32     | `region_count`           | |
| 24  | i32     | `world_min_x`            | inclusive world bounds of the scanned area |
| 28  | i32     | `world_min_z`            | |
| 32  | i32     | `world_max_x`            | |
| 36  | i32     | `world_max_z`            | |
| 40  | u64     | `region_table_offset`    | |
| 48  | u64     | `geometry_offset`        | |
| 56  | u64     | `spatial_index_offset`   | |
| 64  | u64     | `file_size`              | must equal the file length |
| 72  | u8[8]   | padding                  | zero |

## Region table (48 bytes per entry)

Entries are stored in id order, so entry `i` describes region `i`.

| off | type    | field             | notes |
|-----|---------|-------------------|-------|
| 0   | u32     | `id`              | equals the entry index |
| 4   | u8      | `kind`            | 0 sea, 1 river, 2 lake, 3 swamp |
| 5   | u8      | `temperature`     | 0 warm, 1 medium, 2 cold |
| 6   | u8      | `vegetation`      | 0 none, 1 sparse, 2 normal, 3 jungle |
| 7   | u8      | `depth`           | 0 shallow, 1 normal, 2 deep, `0xFF` unknown |
| 8   | u16     | `modifiers`       | bit 0 ice, 1 corals, 2 desert, 3 mangrove, 4 cave |
| 10  | i16     | `surface_y`       | mean water surface Y |
| 12  | i32     | `min_x`           | bounding box, inclusive |
| 16  | i32     | `min_z`           | |
| 20  | i32     | `max_x`           | |
| 24  | i32     | `max_z`           | |
| 28  | u32     | `column_count`    | number of water columns |
| 32  | u32     | `first_run_index` | index into the geometry run array |
| 36  | u32     | `run_count`       | |
| 40  | u16     | `mean_depth`      | bathymetry, in blocks |
| 42  | u16     | `max_depth`       | |
| 44  | u8[4]   | `contour_shares`  | share of columns deeper than 10/20/30/40 blocks, 0..255 |

`temperature` comes from the biome - the only thing in a Minecraft world that
says anything about how warm water is. Ocean biomes are read by name
(`warm_ocean` and `lukewarm_ocean` are warm, `cold_ocean` and `frozen_ocean` are
cold, plain `ocean` is medium); everything else falls back to the biome's
`temperature` value.

`depth` is measured, not inferred: the mean distance from the water surface down
to the floor, banded at 10 and 30 blocks. Every kind of water carries one, so a
shallow sea reads `shallow` and a deep lake reads `deep`. `0xFF` is reserved for
"not measured" and is not written by the current analyzer.

`cave` means the water cannot see the sky: an underground pool or aquifer. Such a
region sits *below* the surface, so at the same `x`/`z` the world may well look
like dry land. Cave regions are always `kind == lake`.

New modifier bits are only ever appended, so an old reader can mask off bits it
does not know and keep working.

## Geometry

Run-length encoded scanlines. One run is:

| off | type | field |
|-----|------|-------|
| 0   | i32  | `z`   |
| 4   | i32  | `x0`  |
| 8   | i32  | `x1`  |

A run covers the water columns `x0..=x1` on row `z`. A region's runs start at
`geometry_offset + first_run_index * 12` and are sorted by `(z, x0)`, so a reader
can binary-search them. Runs never overlap and are already merged across region
file boundaries.

Storing water as runs rather than per-block bitmaps is what keeps the file small:
a 41-million-column ocean costs a few tens of thousands of runs.

## Spatial index

```text
u32   cells_x
u32   cells_z
i32   origin_cell_x        (world_min_x >> spatial_cell_shift)
i32   origin_cell_z
u32   cells[cells_x * cells_z]
u32   list_length
u32   lists[list_length]
```

`cells` is indexed by `cell_z * cells_x + cell_x`, where
`cell_x = (x >> shift) - origin_cell_x`. A cell value is one of:

* `0xFFFFFFFF` - no region touches this cell.
* high bit set (`& 0x80000000`) - exactly one region, its id is `value & 0x7FFFFFFF`.
* anything else - an index into `lists`, where `lists[value]` is a count `n`
  followed by `n` region ids.

Note that `lists` starts *after* the `list_length` field, so element `i` sits at
file offset `spatial_index_offset + 16 + 4 * cells_x * cells_z + 4 + 4 * i`.

Most cells resolve with a single array read; only cells where several regions meet
need the candidate list. A candidate still has to be confirmed against the
region's runs, because a cell is 64x64 blocks and a region may only cover part of
it.

## Reading a position

```text
x/z -> spatial index cell -> candidate region ids -> run test -> region
```

No match means no classified water at that position: not a bug, just dry land,
water too small to be a region, or water the analyzer did not classify. Consumers
decide for themselves what that means.

## Minimal Java reader

```java
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;

public final class WaterRegions {
    private static final int CELL_EMPTY = 0xFFFFFFFF;
    private static final int CELL_INLINE = 0x80000000;

    private final ByteBuffer buf;
    private final int regionCount, cellShift, cellsX, cellsZ, originCellX, originCellZ;
    private final long regionTableOffset, geometryOffset;
    private final int cellsOffset, listsOffset;
    public final int seaLevel, minecraftDataVersion;

    public WaterRegions(Path path) throws IOException {
        buf = ByteBuffer.wrap(Files.readAllBytes(path)).order(ByteOrder.BIG_ENDIAN);
        if (buf.getLong(0) != 0x4D43574154455200L) throw new IOException("bad magic");
        if (buf.getShort(8) != 2) throw new IOException("unsupported version");
        minecraftDataVersion = buf.getInt(12);
        seaLevel = buf.getShort(16);
        cellShift = buf.get(18) & 0xFF;
        regionCount = buf.getInt(20);
        regionTableOffset = buf.getLong(40);
        geometryOffset = buf.getLong(48);
        int idx = (int) buf.getLong(56);
        cellsX = buf.getInt(idx);
        cellsZ = buf.getInt(idx + 4);
        originCellX = buf.getInt(idx + 8);
        originCellZ = buf.getInt(idx + 12);
        cellsOffset = idx + 16;
        // + 4 skips the list_length field, so lists[i] is at listsOffset + 4 * i
        listsOffset = cellsOffset + 4 * cellsX * cellsZ + 4;
    }

    public int regionCount() { return regionCount; }

    private int entry(int id) { return (int) regionTableOffset + id * 48; }

    public int kind(int id)        { return buf.get(entry(id) + 4) & 0xFF; }
    public int temperature(int id) { return buf.get(entry(id) + 5) & 0xFF; }
    public int vegetation(int id)  { return buf.get(entry(id) + 6) & 0xFF; }
    /** 0 shallow, 1 normal, 2 deep, 0xFF unknown. Measured, not biome derived. */
    public int depth(int id)       { return buf.get(entry(id) + 7) & 0xFF; }
    public int modifiers(int id)   { return buf.getShort(entry(id) + 8) & 0xFFFF; }
    public boolean isCave(int id)  { return (modifiers(id) & (1 << 4)) != 0; }
    public int surfaceY(int id)    { return buf.getShort(entry(id) + 10); }
    public int columnCount(int id) { return buf.getInt(entry(id) + 28); }
    public int meanDepth(int id)   { return buf.getShort(entry(id) + 40) & 0xFFFF; }

    private boolean contains(int id, int x, int z) {
        int e = entry(id);
        if (x < buf.getInt(e + 12) || x > buf.getInt(e + 20)) return false;
        if (z < buf.getInt(e + 16) || z > buf.getInt(e + 24)) return false;
        int lo = buf.getInt(e + 32);
        int hi = lo + buf.getInt(e + 36) - 1;
        while (lo <= hi) {                       // runs are sorted by (z, x0)
            int mid = (lo + hi) >>> 1;
            int o = (int) geometryOffset + mid * 12;
            int rz = buf.getInt(o), x0 = buf.getInt(o + 4), x1 = buf.getInt(o + 8);
            if (rz < z || (rz == z && x1 < x)) lo = mid + 1;
            else if (rz > z || x0 > x) hi = mid - 1;
            else return true;
        }
        return false;
    }

    /** Region id at the given block position, or -1 for no classified water. */
    public int regionAt(int x, int z) {
        int cx = (x >> cellShift) - originCellX;
        int cz = (z >> cellShift) - originCellZ;
        if (cx < 0 || cz < 0 || cx >= cellsX || cz >= cellsZ) return -1;
        int v = buf.getInt(cellsOffset + 4 * (cz * cellsX + cx));
        if (v == CELL_EMPTY) return -1;
        if ((v & CELL_INLINE) != 0) {
            int id = v & ~CELL_INLINE;
            return contains(id, x, z) ? id : -1;
        }
        int n = buf.getInt(listsOffset + 4 * v);
        for (int i = 1; i <= n; i++) {
            int id = buf.getInt(listsOffset + 4 * (v + i));
            if (contains(id, x, z)) return id;
        }
        return -1;
    }
}
```

## Optional region consolidation

`--min-river-merge` (default 5000) and `--min-sea-merge` (default 0) consolidate
small adjoining regions of the same water kind after shape correction. The file
layout/version is unchanged. Geometry is the exact union of the constituent runs;
no water is filled or erased by consolidation. Temperature, vegetation, modifiers
and the diagnostic dominant biome represent the larger group. Measured depth,
maximum depth, surface height and depth contour shares are recomputed from column
statistics. Region IDs are regenerated and must not be persisted across scans.

The minimum retention size is independent: isolated small components can survive
a merge threshold and are removed only below `--min-water-body` (or the separate
cave minimum). The ocean PNG sieve has its own `--ocean-map-min-area` option and
never changes the binary region count by itself.
