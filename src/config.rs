//! Central configuration: every tunable threshold of the analyzer lives here.
//!
//! Nothing in this file encodes gameplay rules - only geographic / environmental
//! classification thresholds used while scanning the world.

/// Vertical search window used when probing downwards from the surface block of a
/// column to find water hidden below a thin cover (ice, snow, lily pads).
pub const SURFACE_COVER_PROBE: i32 = 8;

/// Maximum number of blocks we walk down from a water surface to locate the floor
/// when the `OCEAN_FLOOR` heightmap cannot be used (ice covered columns).
pub const MAX_FLOOR_PROBE: i32 = 160;

// Note: there is deliberately no "ignore water below sea level" threshold. Open
// water at the bottom of a deep canyon is genuine surface water and must be kept,
// and roofed-over water is separated by the sky test rather than by its height.

/// Fallback sea level when the histogram based detection has too little evidence.
pub const FALLBACK_SEA_LEVEL: i16 = 63;

/// Minimum number of ocean-biome water surface samples required before the
/// automatically detected sea level is trusted.
pub const SEA_LEVEL_MIN_SAMPLES: u64 = 10_000;

/// The detected sea level must be supported by at least this fraction of samples.
pub const SEA_LEVEL_MIN_SHARE: f64 = 0.25;

// ---------------------------------------------------------------------------
// Region size filters
// ---------------------------------------------------------------------------

/// Default minimum size of a body of water, in columns. Overridable with
/// `--min-water-body`.
///
/// This is the single biggest lever on how many regions come out. Most regions in
/// a large world are isolated puddles: they touch nothing, so nothing can absorb
/// them, and only this threshold removes them. Raising it costs very little
/// coverage - a thousand puddles of 50 columns are 0.02% of a 300 million column
/// world - and removes thousands of regions.
///
/// The same number is the "too small to stand on its own" mark during absorption:
/// a piece below it always merges into its best neighbour, whatever its kind.
pub const MIN_WATER_BODY_COLUMNS: u32 = 200;

/// How much *connected ocean-biome water* it takes to be a `Sea`, in columns.
/// Overridable with `--min-sea-body`.
///
/// The biome on its own is not enough - Minecraft paints an ocean biome onto any
/// large sheet of water below sea level, so big inland lakes get one too - and
/// neither is the size of the connected body of water, because rivers glue a
/// whole continent into one. What is measured is the sheet of ocean water itself;
/// see [`OceanSheets`](crate::water::oceans::OceanSheets).
///
/// A world has a handful of oceans, not hundreds, so the value is best read off
/// the actual distribution, which `--debug` prints. On the world this was built
/// against the sheets fall off like this:
///
/// ```text
///   143 936 264      12 821 160       4 943 150
///    43 970 349       9 316 896   ---------------  factor 3 gap
///    26 260 981       7 745 990       1 579 048
///    20 655 615       5 901 783       1 410 430  ...
/// ```
///
/// Two million columns sits in that gap and yields nine oceans, each at least a
/// 1414x1414 sheet of water. Thirteen million would yield five.
pub const SEA_MIN_COLUMNS: u32 = 2_000_000;

/// How far coastal water in a land biome (beach, plains, ...) may sit from actual
/// ocean-biome water and still count as `Sea`, in 4x4 biome cells.
///
/// Connectivity alone is not enough: rivers connect inland lakes to the ocean, so
/// without a distance limit every river-fed lake in the world would become sea.
pub const COASTAL_CELL_RADIUS: u8 = 6;

/// How far water in a land biome may sit from river or swamp water and still be
/// counted as part of it, in 4x4 biome cells.
///
/// Minecraft stores biomes at 4x4 resolution, so a cell straddling a river bank
/// reports the *land* biome for water that is plainly part of the river. Without
/// this, every river grows a fringe of tiny lake regions along its banks. The
/// radius is deliberately small - it repairs the quantisation, it does not
/// annex neighbouring water.
pub const FRINGE_CELL_RADIUS: u8 = 2;

/// Rounds of majority filtering applied to the raw per-cell biome *family*
/// (ocean/river/swamp/land), before anything else touches it.
///
/// This runs first because the fringe rule dilates whatever family it is given by
/// [`FRINGE_CELL_RADIUS`] to repair the 4x4 quantisation along a bank - and
/// dilating a stray cell of noise only makes the noise bigger. A single river
/// cell inside a lake, left uncorrected, grows into a band five cells wide
/// cutting the lake in two. Filtering the seed removes the stray cell before it
/// can be grown at all.
pub const FAMILY_SMOOTHING_PASSES: usize = 1;

/// Rounds of majority filtering applied to the full per-cell classification
/// (kind, temperature, ice, cave) after it is built, before components are
/// labelled.
///
/// Even with the family denoised first, the classification can still disagree
/// with its neighbours at the edges - a coastal cell just inside or outside
/// [`COASTAL_CELL_RADIUS`], for instance. One round of a 3x3 majority filter
/// absorbs anything narrower than about two cells into its surroundings, which is
/// the width of the remaining artefacts. Only water cells vote, and cave water
/// never changes: whether water can see the sky is measured, not inferred.
pub const KIND_SMOOTHING_PASSES: usize = 1;

// ---------------------------------------------------------------------------
// Shape correction
//
// Minecraft's river biome is a rough proxy for a river. Terralith paints it over
// pools 60 blocks across, and leaves narrow watercourses along a shore with no
// river biome at all - so `minecraft:river` regions came out with a median width
// of 27 blocks, wider than the lakes. Where the shape is unambiguous it overrides
// the biome; the two conditions together are what keeps a genuinely wide river
// (they are wide in this world) from being turned into a lake.
// ---------------------------------------------------------------------------

/// A `River` region at least this wide *and* at most [`POOL_MAX_ELONGATION`]
/// times longer than wide is a pool, and becomes a `Lake`.
pub const POOL_MIN_WIDTH: f32 = 30.0;
pub const POOL_MAX_ELONGATION: f32 = 8.0;

/// A `Lake` region at most this wide *and* at least [`STRAND_MIN_ELONGATION`]
/// times longer than wide is a watercourse, and becomes a `River`.
pub const STRAND_MAX_WIDTH: f32 = 10.0;
pub const STRAND_MIN_ELONGATION: f32 = 30.0;

/// Riverbank repair is based on the entire connected lake-kind component.
/// A narrow, elongated strip must share at least a fifth of its outline with
/// river water; its area/contact also limits how far the bank can extend.
pub const BANK_MAX_WIDTH: f64 = 24.0;
pub const BANK_MIN_ELONGATION: f64 = 8.0;
pub const BANK_MIN_CONTACT_SHARE: f64 = 0.20;
pub const BANK_MAX_CONTACT_WIDTH: f64 = 32.0;

/// Minimum connected colour-patch area in the ocean overview, in actual water
/// columns represented by the pixels. Does not alter runtime depth or temperature.
pub const OCEAN_MAP_MIN_AREA: u64 = 10_000;

// ---------------------------------------------------------------------------
// Absorption of small regions into their neighbours
// ---------------------------------------------------------------------------

/// Regions at or above this size are never absorbed into a neighbour.
pub const ABSORB_MAX_COLUMNS: u32 = 4096;

/// A neighbour must be at least this many times larger before it may absorb a
/// region. Two comparably sized bodies of water stay separate.
///
/// Regions below the minimum water body size ignore this and always merge into
/// their best neighbour - they are too small to describe anything on their own.
pub const ABSORB_MIN_RATIO: u64 = 4;

/// Upper bound on absorption rounds. Each round lets a merged group absorb one
/// more ring of neighbours; the loop stops early once nothing changes.
pub const ABSORB_MAX_ROUNDS: usize = 12;

// ---------------------------------------------------------------------------
// Depth classification, in blocks: surface_y - floor_y
//
// Applied to every kind of water. The number is measured off the world, so a
// river reads shallow because it *is* shallow, not because it is a river.
// ---------------------------------------------------------------------------

pub const DEPTH_SHALLOW_MAX: i32 = 10;
pub const DEPTH_NORMAL_MAX: i32 = 30;

/// Optional bathymetry contour levels exported as region metadata.
pub const BATHYMETRY_CONTOURS: [u8; 4] = [10, 20, 30, 40];

// ---------------------------------------------------------------------------
// Temperature thresholds on the biome `temperature` value.
// Ocean biomes are classified by name first, these are the generic fallback.
// ---------------------------------------------------------------------------

pub const TEMP_WARM_MIN: f32 = 0.75;
pub const TEMP_MEDIUM_MIN: f32 = 0.25;

// ---------------------------------------------------------------------------
// Vegetation thresholds on the biome `downfall` value.
// ---------------------------------------------------------------------------

pub const VEG_NORMAL_MIN: f32 = 0.55;
pub const VEG_SPARSE_MIN: f32 = 0.2;

/// Share of a region's chunks that must contain aquatic plants (kelp / seagrass /
/// sea pickles) before the biome derived vegetation level is promoted one step.
pub const VEG_PLANT_PROMOTE_SHARE: f32 = 0.25;

/// Below this share of chunks containing aquatic plants a `Sea` region is demoted
/// one vegetation step (barren water).
pub const VEG_PLANT_DEMOTE_SHARE: f32 = 0.02;

// ---------------------------------------------------------------------------
// Modifier thresholds
// ---------------------------------------------------------------------------

/// Share of a region's water columns that must be covered by ice for `ICE`.
pub const ICE_MIN_SHARE: f32 = 0.15;

/// Share of a region's chunks that must contain coral blocks for `CORALS`.
pub const CORAL_MIN_SHARE: f32 = 0.02;

/// Share of a region's water cells whose biome is desert-like for `DESERT`.
pub const DESERT_MIN_SHARE: f32 = 0.4;

/// Share of a region's water cells whose biome is mangrove-like for `MANGROVE`.
pub const MANGROVE_MIN_SHARE: f32 = 0.25;

/// Share of a region's water columns that must be roofed over for `CAVE`.
///
/// Cave and open water are already kept apart by the region signature, so a
/// region is normally either all cave or none of it; the share only decides
/// borderline cases such as a pool right under a cave mouth.
pub const CAVE_MIN_SHARE: f32 = 0.5;

// ---------------------------------------------------------------------------
// Spatial index
// ---------------------------------------------------------------------------

/// log2 of the spatial index cell size in blocks. 6 => 64x64 blocks per cell.
pub const SPATIAL_CELL_SHIFT: u32 = 6;
pub const SPATIAL_CELL_SIZE: i32 = 1 << SPATIAL_CELL_SHIFT;
