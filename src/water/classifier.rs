//! Classification of water columns and of finished regions.
//!
//! Classification happens in two steps, and the split matters:
//!
//! 1. **Hydrological components** - plain water connectivity. A body of water that
//!    reaches ocean-biome water is "connected to the sea", which is what separates
//!    a coastal bay from a landlocked lake.
//! 2. **Classification regions** - the same water, but split again so that every
//!    emitted region is homogeneous in `kind` and `temperature`. Without that step
//!    a world's entire ocean would collapse into one region and a single
//!    "temperature" for a frozen ocean and a warm reef alike.
//!
//! Everything here is environmental. No gameplay concept is derived.

use crate::config;
use crate::water::model::*;
use crate::world::biome::{BiomeFamily, BiomeRegistry, BiomeTraits};

/// Aggregated facts about one hydrological body.
#[derive(Clone, Copy, Debug, Default)]
pub struct HydroInfo {
    pub columns: u64,
    pub ocean_columns: u64,
    pub river_columns: u64,
    pub swamp_columns: u64,
    pub land_columns: u64,
}

impl HydroInfo {
    pub fn add_column(&mut self, family: BiomeFamily, n: u64) {
        self.columns += n;
        match family {
            BiomeFamily::Ocean => self.ocean_columns += n,
            BiomeFamily::River => self.river_columns += n,
            BiomeFamily::Swamp => self.swamp_columns += n,
            BiomeFamily::Land => self.land_columns += n,
        }
    }

    pub fn merge(&mut self, other: &HydroInfo) {
        self.columns += other.columns;
        self.ocean_columns += other.ocean_columns;
        self.river_columns += other.river_columns;
        self.swamp_columns += other.swamp_columns;
        self.land_columns += other.land_columns;
    }

    /// Whether this body of water reaches ocean-biome water anywhere.
    pub fn touches_ocean(&self) -> bool {
        self.ocean_columns >= 16
    }
}

/// What the surroundings of a land-biome water column look like.
#[derive(Clone, Copy, Debug, Default)]
pub struct Surroundings {
    /// Ocean-biome water within [`config::COASTAL_CELL_RADIUS`].
    pub near_ocean: bool,
    /// Temperature of that ocean water.
    pub ocean_temperature: Option<Temperature>,
    /// River or swamp water within [`config::FRINGE_CELL_RADIUS`].
    pub fringe: Option<(BiomeFamily, Temperature)>,
}

/// Basic water type of a single column.
///
/// Nothing becomes a `Sea` unless it belongs to a sheet of ocean-biome water that
/// reaches [`config::SEA_MIN_COLUMNS`] - see
/// [`OceanSheets`](crate::water::oceans::OceanSheets). An ocean biome is not
/// proof of an ocean: Minecraft paints one onto any large sheet of water below
/// sea level, so a big inland lake gets one too. Size is the check that the biome
/// cannot fake - measured on the ocean water itself, because rivers glue whole
/// continents into one body.
///
/// Land-biome water - a beach, a river bank, a plains pond - is then resolved in
/// this order:
///
/// 1. **Sea** when the column is both connected to ocean water *and* close to it.
///    Both are needed: connectivity alone turns every river-fed lake into sea
///    because rivers reach the ocean, and proximity alone does the same for a pond
///    sitting just behind a beach.
/// 2. **River / Swamp** when river or swamp water is right next to it. Minecraft's
///    4x4 biome cells cut across river banks, so the outer strip of a river often
///    reports the land biome; without this it would break off into tiny lakes.
/// 3. **Lake** otherwise.
pub fn column_kind(
    family: BiomeFamily,
    hydro: &HydroInfo,
    around: &Surroundings,
    cave: bool,
    in_sea_sheet: bool,
    min_sea_columns: u32,
) -> WaterKind {
    // Water under a roof is an underground pool, whatever the biome above says.
    // It is never sea, river or swamp: none of those exist without a sky.
    if cave {
        return WaterKind::Lake;
    }
    match family {
        BiomeFamily::Ocean => {
            if in_sea_sheet {
                WaterKind::Sea
            } else {
                WaterKind::Lake
            }
        }
        BiomeFamily::River => WaterKind::River,
        BiomeFamily::Swamp => WaterKind::Swamp,
        BiomeFamily::Land => {
            // `near_ocean` is seeded from sea-sized sheets only, so being close
            // to one already means being close to a real sea.
            let coastal = around.near_ocean && hydro.touches_ocean();
            // Fallback for worlds that have no ocean biomes at all: a landlocked
            // sheet of water this size is a sea whatever it is called.
            let inland_sea =
                !hydro.touches_ocean() && hydro.columns >= min_sea_columns as u64;
            if coastal || inland_sea {
                return WaterKind::Sea;
            }
            match around.fringe {
                Some((BiomeFamily::River, _)) => WaterKind::River,
                Some((BiomeFamily::Swamp, _)) => WaterKind::Swamp,
                _ => WaterKind::Lake,
            }
        }
    }
}

/// Temperature of a single water column.
///
/// A beach or plains column that is part of the open sea must not read "warm"
/// just because the land around it is warm - it takes the temperature of the
/// water it actually belongs to.
pub fn column_temperature(
    family: BiomeFamily,
    kind: WaterKind,
    biome_temperature: Temperature,
    around: &Surroundings,
) -> Temperature {
    if family != BiomeFamily::Land {
        return biome_temperature;
    }
    match kind {
        WaterKind::Sea => around.ocean_temperature.unwrap_or(biome_temperature),
        WaterKind::River | WaterKind::Swamp => {
            around.fringe.map(|(_, t)| t).unwrap_or(biome_temperature)
        }
        WaterKind::Lake => biome_temperature,
    }
}

/// Packs the region signature that connected-component labelling splits on.
///
/// Ice is part of the signature so that a frozen ocean does not dissolve into the
/// cold ocean it borders: both are `Sea` + `Cold`, and merging them would dilute
/// the ice share until the `ICE` modifier disappeared. Cave is in there for the
/// same reason, and because an underground pool and the lake above it are not the
/// same body of water even where they share an x/z column.
#[inline]
pub fn signature(kind: WaterKind, temperature: Temperature, icy: bool, cave: bool) -> u32 {
    ((kind as u32) << 16) | ((cave as u32) << 9) | ((icy as u32) << 8) | temperature as u32
}

/// Returns `(kind, temperature, icy, cave)`.
#[inline]
pub fn unpack_signature(sig: u32) -> (WaterKind, Temperature, bool, bool) {
    (
        WaterKind::from_u8((sig >> 16) as u8).unwrap_or(WaterKind::Lake),
        Temperature::from_u8((sig & 0xFF) as u8).unwrap_or(Temperature::Medium),
        (sig >> 8) & 1 == 1,
        (sig >> 9) & 1 == 1,
    )
}

/// Rounds a sum/count mean to the nearest integer instead of truncating.
#[inline]
fn mean(sum: i64, count: u64) -> i64 {
    let n = count.max(1) as i64;
    let half = n / 2;
    if sum >= 0 {
        (sum + half) / n
    } else {
        (sum - half) / n
    }
}

/// Running totals for one classification region.
#[derive(Clone, Debug)]
pub struct RegionAccum {
    pub signature: u32,
    pub columns: u64,
    pub ice_columns: u64,
    pub surface_sum: i64,
    pub depth_sum: i64,
    pub depth_max: u32,
    pub contour_columns: [u64; 4],
    pub vegetation_sum: u64,
    pub desert_columns: u64,
    pub mangrove_columns: u64,
    pub coral_columns: u64,
    pub plant_columns: u64,
    pub cave_columns: u64,
    /// Small linear-probed biome histogram, only used to name the region.
    pub biomes: Vec<(u16, u32)>,
    pub min_x: i32,
    pub min_z: i32,
    pub max_x: i32,
    pub max_z: i32,
}

const BIOME_HIST_CAP: usize = 24;

impl Default for RegionAccum {
    fn default() -> Self {
        RegionAccum {
            signature: 0,
            columns: 0,
            ice_columns: 0,
            surface_sum: 0,
            depth_sum: 0,
            depth_max: 0,
            contour_columns: [0; 4],
            vegetation_sum: 0,
            desert_columns: 0,
            mangrove_columns: 0,
            coral_columns: 0,
            plant_columns: 0,
            cave_columns: 0,
            biomes: Vec::new(),
            min_x: i32::MAX,
            min_z: i32::MAX,
            max_x: i32::MIN,
            max_z: i32::MIN,
        }
    }
}

impl RegionAccum {
    /// Adds `n` columns that share one 4x4 cell.
    #[allow(clippy::too_many_arguments)]
    pub fn add_cell(
        &mut self,
        registry: &BiomeRegistry,
        biome: u16,
        surface_y: i16,
        depth: u16,
        ice_fraction: f32,
        cave_fraction: f32,
        has_coral: bool,
        has_plants: bool,
        n: u64,
    ) {
        self.columns += n;
        self.surface_sum += surface_y as i64 * n as i64;
        self.depth_sum += depth as i64 * n as i64;
        self.depth_max = self.depth_max.max(depth as u32);
        self.ice_columns += (ice_fraction.clamp(0.0, 1.0) as f64 * n as f64).round() as u64;
        self.cave_columns += (cave_fraction.clamp(0.0, 1.0) as f64 * n as f64).round() as u64;
        for (i, threshold) in config::BATHYMETRY_CONTOURS.iter().enumerate() {
            if depth as u32 > *threshold as u32 {
                self.contour_columns[i] += n;
            }
        }
        if has_coral {
            self.coral_columns += n;
        }
        if has_plants {
            self.plant_columns += n;
        }
        if let Some(info) = registry.info(biome) {
            self.vegetation_sum += info.vegetation as u64 * n;
            if info.traits.contains(BiomeTraits::DESERT) {
                self.desert_columns += n;
            }
            if info.traits.contains(BiomeTraits::MANGROVE) {
                self.mangrove_columns += n;
            }
        } else {
            self.vegetation_sum += Vegetation::Sparse as u64 * n;
        }
        self.add_biome(biome, n as u32);
    }

    #[inline]
    pub fn extend_bounds(&mut self, x0: i32, x1: i32, z: i32) {
        self.min_x = self.min_x.min(x0);
        self.max_x = self.max_x.max(x1);
        self.min_z = self.min_z.min(z);
        self.max_z = self.max_z.max(z);
    }

    fn add_biome(&mut self, biome: u16, n: u32) {
        if let Some(e) = self.biomes.iter_mut().find(|(b, _)| *b == biome) {
            e.1 = e.1.saturating_add(n);
            return;
        }
        if self.biomes.len() < BIOME_HIST_CAP {
            self.biomes.push((biome, n));
        }
    }

    pub fn merge(&mut self, other: &RegionAccum) {
        self.signature = self.signature.max(other.signature);
        self.columns += other.columns;
        self.ice_columns += other.ice_columns;
        self.surface_sum += other.surface_sum;
        self.depth_sum += other.depth_sum;
        self.depth_max = self.depth_max.max(other.depth_max);
        for i in 0..4 {
            self.contour_columns[i] += other.contour_columns[i];
        }
        self.vegetation_sum += other.vegetation_sum;
        self.desert_columns += other.desert_columns;
        self.mangrove_columns += other.mangrove_columns;
        self.coral_columns += other.coral_columns;
        self.plant_columns += other.plant_columns;
        self.cave_columns += other.cave_columns;
        for (b, n) in &other.biomes {
            self.add_biome(*b, *n);
        }
        self.min_x = self.min_x.min(other.min_x);
        self.min_z = self.min_z.min(other.min_z);
        self.max_x = self.max_x.max(other.max_x);
        self.max_z = self.max_z.max(other.max_z);
    }

    pub fn dominant_biome(&self) -> Option<u16> {
        self.biomes.iter().max_by_key(|(_, n)| *n).map(|(b, _)| *b)
    }
}

/// Turns accumulated column statistics into a finished region description.
pub fn finalize(id: u32, accum: &RegionAccum, registry: &BiomeRegistry) -> WaterRegion {
    let (kind, temperature, _icy, _cave) = unpack_signature(accum.signature);
    let n = accum.columns.max(1);

    let mean_depth = mean(accum.depth_sum, n) as i32;
    // Depth is measured, not inferred: it is the mean distance from the water
    // surface down to the floor, and every kind of water gets one.
    let depth = Some(Depth::classify(mean_depth));

    // Vegetation: biome baseline, corrected by what actually grows in the water.
    let mut vegetation = Vegetation::from_u8(
        ((accum.vegetation_sum as f64 / n as f64).round() as u8).min(Vegetation::Jungle as u8),
    )
    .unwrap_or(Vegetation::Sparse);
    let plant_share = accum.plant_columns as f32 / n as f32;
    // Kelp and seagrass can lift barren water up to `Normal`, but `Jungle` means
    // dense tropical growth and only the biome can say that.
    if plant_share >= config::VEG_PLANT_PROMOTE_SHARE && vegetation < Vegetation::Normal {
        vegetation = vegetation.promote();
    } else if kind == WaterKind::Sea && plant_share < config::VEG_PLANT_DEMOTE_SHARE {
        vegetation = vegetation.demote();
    }

    let mut modifiers = Modifiers::empty();
    if accum.ice_columns as f32 / n as f32 >= config::ICE_MIN_SHARE {
        modifiers |= Modifiers::ICE;
    }
    if kind == WaterKind::Sea && accum.coral_columns as f32 / n as f32 >= config::CORAL_MIN_SHARE {
        modifiers |= Modifiers::CORALS;
    }
    // `DESERT` describes an inland body of water in desert country; the open sea
    // along a desert coast is still just sea.
    if kind != WaterKind::Sea
        && accum.desert_columns as f32 / n as f32 >= config::DESERT_MIN_SHARE
    {
        modifiers |= Modifiers::DESERT;
    }
    if accum.mangrove_columns as f32 / n as f32 >= config::MANGROVE_MIN_SHARE {
        modifiers |= Modifiers::MANGROVE;
        vegetation = Vegetation::Jungle;
    }
    if accum.cave_columns as f32 / n as f32 >= config::CAVE_MIN_SHARE {
        modifiers |= Modifiers::CAVE;
    }

    let contour_shares = {
        let mut s = [0u8; 4];
        for (slot, count) in s.iter_mut().zip(accum.contour_columns.iter()) {
            *slot = ((*count as f64 / n as f64) * 255.0).round() as u8;
        }
        s
    };

    WaterRegion {
        id,
        geometry: RegionGeometry {
            min_x: accum.min_x,
            min_z: accum.min_z,
            max_x: accum.max_x,
            max_z: accum.max_z,
            runs: Vec::new(),
            column_count: accum.columns.min(u32::MAX as u64) as u32,
        },
        kind,
        temperature,
        vegetation,
        depth,
        modifiers,
        surface_y: mean(accum.surface_sum, n) as i16,
        bathymetry: Bathymetry {
            mean_depth: mean_depth.clamp(0, u16::MAX as i32) as u16,
            max_depth: accum.depth_max.min(u16::MAX as u32) as u16,
            contour_shares,
        },
        dominant_biome: accum
            .dominant_biome()
            .map(|b| registry.name_of(b).to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> BiomeRegistry {
        BiomeRegistry::vanilla_only()
    }

    /// Whether the column sits in a sheet of ocean water big enough to be a sea.
    /// Measuring that is [`crate::water::oceans::OceanSheets`]' job and tested
    /// there; here it is just an input.
    const IN_SEA: bool = true;
    const NO_SEA: bool = false;

    /// Nothing of interest anywhere near this column.
    fn alone() -> Surroundings {
        Surroundings::default()
    }

    fn beside_ocean(t: Temperature) -> Surroundings {
        Surroundings {
            near_ocean: true,
            ocean_temperature: Some(t),
            fringe: None,
        }
    }

    fn beside(family: BiomeFamily, t: Temperature) -> Surroundings {
        Surroundings {
            near_ocean: false,
            ocean_temperature: None,
            fringe: Some((family, t)),
        }
    }

    #[test]
    fn landlocked_water_is_a_lake_and_coastal_water_is_sea() {
        let landlocked = HydroInfo {
            columns: 500,
            land_columns: 500,
            ..Default::default()
        };
        assert_eq!(
            column_kind(BiomeFamily::Land, &landlocked, &alone(), false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Lake
        );

        let coastal = HydroInfo {
            columns: 500_000,
            ocean_columns: 400_000,
            land_columns: 100_000,
            ..Default::default()
        };
        let at_the_coast = beside_ocean(Temperature::Medium);
        assert_eq!(
            column_kind(BiomeFamily::Land, &coastal, &at_the_coast, false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Sea
        );
        assert_eq!(
            column_kind(BiomeFamily::Ocean, &coastal, &alone(), false, IN_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Sea
        );
        assert_eq!(
            column_kind(BiomeFamily::River, &coastal, &at_the_coast, false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::River
        );
        assert_eq!(
            column_kind(BiomeFamily::Swamp, &coastal, &at_the_coast, false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Swamp
        );
    }

    #[test]
    fn ocean_biome_water_outside_a_sea_sized_sheet_is_a_lake() {
        let body = HydroInfo {
            columns: 5_000_000,
            ocean_columns: 4_000_000,
            ..Default::default()
        };

        // The body of water is enormous - rivers see to that - but this column's
        // own sheet of ocean water is not, so it is a lake.
        assert_eq!(
            column_kind(BiomeFamily::Ocean, &body, &alone(), false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Lake
        );
        assert_eq!(
            column_kind(BiomeFamily::Ocean, &body, &alone(), false, IN_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Sea
        );

        // The shore of such a lake is not coastal sea either: `near_ocean` is
        // seeded from sea-sized sheets only, so it is false here.
        let beside_a_pond = Surroundings {
            near_ocean: false,
            ocean_temperature: None,
            fringe: None,
        };
        assert_eq!(
            column_kind(BiomeFamily::Land, &body, &beside_a_pond, false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Lake
        );

        // Rivers and swamps are unaffected - they are small by nature.
        assert_eq!(
            column_kind(BiomeFamily::River, &body, &alone(), false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::River
        );
        assert_eq!(
            column_kind(BiomeFamily::Swamp, &body, &alone(), false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Swamp
        );
    }

    #[test]
    fn a_river_fed_lake_far_from_the_coast_stays_a_lake() {
        // The river network connects this lake to the ocean, so the hydrological
        // body really does touch ocean water - but the lake itself is nowhere
        // near it, and must not become a sea because of that.
        let river_network = HydroInfo {
            columns: 4_000_000,
            ocean_columns: 3_500_000,
            river_columns: 400_000,
            land_columns: 100_000,
            ..Default::default()
        };
        assert!(river_network.touches_ocean());
        assert_eq!(
            column_kind(BiomeFamily::Land, &river_network, &alone(), false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Lake
        );
        assert_eq!(
            column_kind(
                BiomeFamily::Land,
                &river_network,
                &beside_ocean(Temperature::Cold),
                false,
                NO_SEA,
                config::SEA_MIN_COLUMNS,
            ),
            WaterKind::Sea
        );
    }

    #[test]
    fn a_land_biome_column_on_a_river_bank_is_river() {
        // Minecraft's 4x4 biome cells cut across river banks, so the outer strip
        // of a river reports the land biome. It is still river water.
        let inland = HydroInfo {
            columns: 50_000,
            river_columns: 45_000,
            land_columns: 5_000,
            ..Default::default()
        };
        assert_eq!(
            column_kind(
                BiomeFamily::Land,
                &inland,
                &beside(BiomeFamily::River, Temperature::Medium),
                false,
                NO_SEA,
                config::SEA_MIN_COLUMNS,
            ),
            WaterKind::River
        );
        assert_eq!(
            column_kind(
                BiomeFamily::Land,
                &inland,
                &beside(BiomeFamily::Swamp, Temperature::Warm),
                false,
                NO_SEA,
                config::SEA_MIN_COLUMNS,
            ),
            WaterKind::Swamp
        );
        // Away from both it is just a lake.
        assert_eq!(
            column_kind(BiomeFamily::Land, &inland, &alone(), false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Lake
        );
    }

    #[test]
    fn the_coast_wins_over_a_river_bank() {
        let coastal = HydroInfo {
            columns: 500_000,
            ocean_columns: 400_000,
            land_columns: 100_000,
            ..Default::default()
        };
        let river_mouth = Surroundings {
            near_ocean: true,
            ocean_temperature: Some(Temperature::Warm),
            fringe: Some((BiomeFamily::River, Temperature::Medium)),
        };
        assert_eq!(
            column_kind(BiomeFamily::Land, &coastal, &river_mouth, false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Sea
        );
    }

    #[test]
    fn a_huge_body_without_any_ocean_biome_counts_as_an_inland_sea() {
        let huge = HydroInfo {
            columns: config::SEA_MIN_COLUMNS as u64,
            land_columns: config::SEA_MIN_COLUMNS as u64,
            ..Default::default()
        };
        assert_eq!(
            column_kind(BiomeFamily::Land, &huge, &alone(), false, NO_SEA, config::SEA_MIN_COLUMNS),
            WaterKind::Sea
        );
    }

    #[test]
    fn coastal_water_takes_the_temperature_of_the_nearest_sea() {
        // A warm beach column on a cold coast reads cold.
        assert_eq!(
            column_temperature(
                BiomeFamily::Land,
                WaterKind::Sea,
                Temperature::Warm,
                &beside_ocean(Temperature::Cold)
            ),
            Temperature::Cold
        );
        // Ocean columns keep their own biome temperature.
        assert_eq!(
            column_temperature(
                BiomeFamily::Ocean,
                WaterKind::Sea,
                Temperature::Medium,
                &beside_ocean(Temperature::Cold)
            ),
            Temperature::Medium
        );
        // A river bank takes the river's temperature.
        assert_eq!(
            column_temperature(
                BiomeFamily::Land,
                WaterKind::River,
                Temperature::Warm,
                &beside(BiomeFamily::River, Temperature::Medium)
            ),
            Temperature::Medium
        );
        // A landlocked lake is unaffected.
        assert_eq!(
            column_temperature(BiomeFamily::Land, WaterKind::Lake, Temperature::Warm, &alone()),
            Temperature::Warm
        );
    }

    #[test]
    fn means_are_rounded_not_truncated() {
        assert_eq!(mean(0, 0), 0);
        assert_eq!(mean(619, 10), 62); // 61.9 must not report 61
        assert_eq!(mean(614, 10), 61);
        assert_eq!(mean(615, 10), 62);
        assert_eq!(mean(-615, 10), -62);
    }

    #[test]
    fn roofed_water_is_always_a_lake() {
        // The biome above a cave is irrelevant: an ocean biome does not make an
        // underground pool a sea.
        let coastal = HydroInfo {
            columns: 500_000,
            ocean_columns: 400_000,
            ..Default::default()
        };
        for family in [
            BiomeFamily::Ocean,
            BiomeFamily::River,
            BiomeFamily::Swamp,
            BiomeFamily::Land,
        ] {
            assert_eq!(
                column_kind(
                    family,
                    &coastal,
                    &beside_ocean(Temperature::Warm),
                    true,
                    NO_SEA,
                    config::SEA_MIN_COLUMNS,
                ),
                WaterKind::Lake,
                "{family:?} under a roof should be a lake"
            );
        }
    }

    #[test]
    fn cave_and_open_water_get_different_signatures() {
        let open = signature(WaterKind::Lake, Temperature::Medium, false, false);
        let cave = signature(WaterKind::Lake, Temperature::Medium, false, true);
        assert_ne!(open, cave);
        assert_eq!(
            unpack_signature(cave),
            (WaterKind::Lake, Temperature::Medium, false, true)
        );
    }

    #[test]
    fn the_cave_modifier_needs_roofed_columns() {
        let registry = reg();
        let id = registry.id_of("minecraft:lush_caves");

        let mut open = RegionAccum {
            signature: signature(WaterKind::Lake, Temperature::Medium, false, false),
            ..Default::default()
        };
        open.add_cell(&registry, id, 40, 3, 0.0, 0.0, false, false, 5_000);
        assert!(!finalize(1, &open, &registry)
            .modifiers
            .contains(Modifiers::CAVE));

        let mut roofed = RegionAccum {
            signature: signature(WaterKind::Lake, Temperature::Medium, false, true),
            ..Default::default()
        };
        roofed.add_cell(&registry, id, 40, 3, 0.0, 1.0, false, false, 5_000);
        let region = finalize(2, &roofed, &registry);
        assert!(region.modifiers.contains(Modifiers::CAVE));
        assert_eq!(region.kind, WaterKind::Lake);
        assert_eq!(region.depth, Some(Depth::Shallow));
    }

    #[test]
    fn frozen_and_open_water_get_different_signatures() {
        let open = signature(WaterKind::Sea, Temperature::Cold, false, false);
        let frozen = signature(WaterKind::Sea, Temperature::Cold, true, false);
        assert_ne!(open, frozen);
        assert_eq!(
            unpack_signature(frozen),
            (WaterKind::Sea, Temperature::Cold, true, false)
        );
    }

    #[test]
    fn kelp_never_makes_a_region_a_jungle() {
        let registry = reg();
        let id = registry.id_of("minecraft:cold_ocean");
        let mut a = RegionAccum {
            signature: signature(WaterKind::Sea, Temperature::Cold, false, false),
            ..Default::default()
        };
        // Sparse from the biome, every column has plants.
        a.add_cell(&registry, id, 62, 12, 0.0, 0.0, false, true, 10_000);
        let region = finalize(1, &a, &registry);
        assert_eq!(region.vegetation, Vegetation::Normal);
    }

    #[test]
    fn signatures_round_trip() {
        for kind in [
            WaterKind::Sea,
            WaterKind::River,
            WaterKind::Lake,
            WaterKind::Swamp,
        ] {
            for t in [
                Temperature::Warm,
                Temperature::Warm,
                Temperature::Medium,
                Temperature::Cold,
            ] {
                for icy in [false, true] {
                    for cave in [false, true] {
                        let s = signature(kind, t, icy, cave);
                        assert_ne!(s, crate::water::components::NONE);
                        assert_eq!(unpack_signature(s), (kind, t, icy, cave));
                    }
                }
            }
        }
    }

    fn accum_of(sig: u32, biome: &str, n: u64, depth: u16) -> (RegionAccum, BiomeRegistry) {
        let registry = reg();
        let id = registry.id_of(biome);
        let mut a = RegionAccum {
            signature: sig,
            ..Default::default()
        };
        a.add_cell(&registry, id, 62, depth, 0.0, 0.0, false, true, n);
        a.extend_bounds(0, n as i32 - 1, 0);
        (a, registry)
    }

    #[test]
    fn depth_is_measured_for_every_kind_of_water() {
        let (a, r) = accum_of(
            signature(WaterKind::Sea, Temperature::Warm, false, false),
            "minecraft:lukewarm_ocean",
            1000,
            45,
        );
        let region = finalize(1, &a, &r);
        assert_eq!(region.kind, WaterKind::Sea);
        assert_eq!(region.depth, Some(Depth::Deep));
        assert_eq!(region.bathymetry.mean_depth, 45);

        let (a, r) = accum_of(
            signature(WaterKind::Lake, Temperature::Cold, false, false),
            "minecraft:snowy_plains",
            1000,
            5,
        );
        let region = finalize(2, &a, &r);
        assert_eq!(region.kind, WaterKind::Lake);
        // A shallow lake reads shallow because it is 5 blocks deep, not because
        // it is a lake.
        assert_eq!(region.depth, Some(Depth::Shallow));
        assert_eq!(region.bathymetry.mean_depth, 5);

        // The same lake, 45 blocks deep, reads deep.
        let (a, r) = accum_of(
            signature(WaterKind::Lake, Temperature::Cold, false, false),
            "minecraft:snowy_plains",
            1000,
            45,
        );
        assert_eq!(finalize(3, &a, &r).depth, Some(Depth::Deep));

        // And a shallow sea reads shallow, for the same reason.
        let (a, r) = accum_of(
            signature(WaterKind::Sea, Temperature::Warm, false, false),
            "minecraft:warm_ocean",
            1000,
            6,
        );
        assert_eq!(finalize(4, &a, &r).depth, Some(Depth::Shallow));
    }

    #[test]
    fn ice_needs_actual_ice_columns_not_just_a_cold_biome() {
        let registry = reg();
        let id = registry.id_of("minecraft:snowy_plains");

        let mut cold = RegionAccum {
            signature: signature(WaterKind::Lake, Temperature::Cold, false, false),
            ..Default::default()
        };
        cold.add_cell(&registry, id, 62, 4, 0.0, 0.0, false, false, 1000);
        assert!(!finalize(1, &cold, &registry)
            .modifiers
            .contains(Modifiers::ICE));

        let mut frozen = RegionAccum {
            signature: signature(WaterKind::Lake, Temperature::Cold, false, false),
            ..Default::default()
        };
        frozen.add_cell(&registry, id, 62, 4, 0.9, 0.0, false, false, 1000);
        assert!(finalize(2, &frozen, &registry)
            .modifiers
            .contains(Modifiers::ICE));
    }

    #[test]
    fn desert_lake_stays_a_lake_but_gains_the_modifier() {
        let registry = reg();
        let id = registry.id_of("minecraft:desert");
        let mut a = RegionAccum {
            signature: signature(WaterKind::Lake, Temperature::Warm, false, false),
            ..Default::default()
        };
        a.add_cell(&registry, id, 62, 3, 0.0, 0.0, false, false, 400);
        let region = finalize(7, &a, &registry);
        assert_eq!(region.kind, WaterKind::Lake);
        assert_eq!(region.temperature, Temperature::Warm);
        assert!(region.modifiers.contains(Modifiers::DESERT));
        assert_eq!(region.depth, Some(Depth::Shallow));
    }

    #[test]
    fn mangrove_water_is_swamp_with_a_modifier_and_jungle_vegetation() {
        let registry = reg();
        let id = registry.id_of("minecraft:mangrove_swamp");
        let mut a = RegionAccum {
            signature: signature(WaterKind::Swamp, Temperature::Warm, false, false),
            ..Default::default()
        };
        a.add_cell(&registry, id, 62, 2, 0.0, 0.0, false, false, 800);
        let region = finalize(9, &a, &registry);
        assert_eq!(region.kind, WaterKind::Swamp);
        assert!(region.modifiers.contains(Modifiers::MANGROVE));
        assert_eq!(region.vegetation, Vegetation::Jungle);
    }

    #[test]
    fn corals_are_only_reported_for_sea_regions_that_contain_them() {
        let registry = reg();
        let id = registry.id_of("minecraft:warm_ocean");
        let mut with = RegionAccum {
            signature: signature(WaterKind::Sea, Temperature::Warm, false, false),
            ..Default::default()
        };
        with.add_cell(&registry, id, 62, 8, 0.0, 0.0, true, true, 5000);
        assert!(finalize(1, &with, &registry)
            .modifiers
            .contains(Modifiers::CORALS));

        let mut without = RegionAccum {
            signature: signature(WaterKind::Sea, Temperature::Warm, false, false),
            ..Default::default()
        };
        without.add_cell(&registry, id, 62, 8, 0.0, 0.0, false, true, 5000);
        assert!(!finalize(2, &without, &registry)
            .modifiers
            .contains(Modifiers::CORALS));
    }

    #[test]
    fn barren_sea_loses_a_vegetation_step() {
        let registry = reg();
        let id = registry.id_of("minecraft:ocean");
        let mut barren = RegionAccum {
            signature: signature(WaterKind::Sea, Temperature::Medium, false, false),
            ..Default::default()
        };
        barren.add_cell(&registry, id, 62, 20, 0.0, 0.0, false, false, 10_000);
        let region = finalize(1, &barren, &registry);
        assert_eq!(region.vegetation, Vegetation::Sparse);
    }

    #[test]
    fn accumulators_merge_associatively() {
        let registry = reg();
        let ocean = registry.id_of("minecraft:ocean");
        let mut a = RegionAccum::default();
        a.add_cell(&registry, ocean, 62, 10, 0.0, 0.0, false, true, 100);
        a.extend_bounds(0, 9, 0);
        let mut b = RegionAccum::default();
        b.add_cell(&registry, ocean, 62, 30, 0.0, 0.0, false, true, 100);
        b.extend_bounds(20, 29, 5);

        let mut merged = a.clone();
        merged.merge(&b);
        assert_eq!(merged.columns, 200);
        assert_eq!(merged.depth_sum, 4000);
        assert_eq!(merged.depth_max, 30);
        assert_eq!((merged.min_x, merged.max_x), (0, 29));
        assert_eq!((merged.min_z, merged.max_z), (0, 5));
    }
}
