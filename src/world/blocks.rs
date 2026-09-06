//! Block classification.
//!
//! The scanner never needs a real block registry - it only needs to answer a few
//! questions about a palette entry ("is this water?", "does this cover water?",
//! "is this coral?"). Palette entries are classified once per section into a
//! compact [`BlockClass`] byte, and all column probing then works on those bytes.
//!
//! The block name lists are kept here so they can be maintained centrally.

/// Compact per-palette-entry classification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum BlockClass {
    /// Anything that is neither water nor a thin cover above water.
    Solid = 0,
    Air = 1,
    /// Source water, waterlogged blocks and blocks that always contain water
    /// (kelp, seagrass, bubble columns, ...), plus raw mud used as swamp water.
    Water = 2,
    /// Ice variants. Above water these produce the `ICE` modifier.
    Ice = 3,
    /// Thin, non-water cover that may sit on top of a water surface
    /// (snow layers, lily pads, ...).
    Cover = 4,
    Lava = 5,
}

impl BlockClass {
    #[inline]
    pub fn is_water(self) -> bool {
        self == BlockClass::Water
    }

    /// Blocks that may legitimately sit between the heightmap surface and the
    /// actual water surface.
    #[inline]
    pub fn is_cover(self) -> bool {
        matches!(self, BlockClass::Air | BlockClass::Ice | BlockClass::Cover)
    }
}

/// Ice variants. Frosted ice is included because it is genuinely ice over water.
const ICE_BLOCKS: [&str; 4] = ["ice", "packed_ice", "blue_ice", "frosted_ice"];

/// Blocks that always contain water even though the block itself is not
/// `minecraft:water`.
const WATERY_BLOCKS: [&str; 6] = [
    "bubble_column",
    "kelp",
    "kelp_plant",
    "seagrass",
    "tall_seagrass",
    "sea_pickle",
];

/// Aquatic vegetation used for the vegetation classification.
const AQUATIC_PLANTS: [&str; 5] = [
    "kelp",
    "kelp_plant",
    "seagrass",
    "tall_seagrass",
    "sea_pickle",
];

/// Thin covers that can sit directly on a water surface.
const COVER_BLOCKS: [&str; 4] = ["snow", "lily_pad", "snow_block", "powder_snow"];

/// Strips a `namespace:` prefix. Unknown namespaces are handled by name only,
/// which is what modded / datapack blocks need.
#[inline]
pub fn strip_namespace(name: &str) -> &str {
    match name.as_bytes().iter().position(|b| *b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Classifies a palette entry using its block id, waterlogging and fluid level.
/// Flowing/falling water and water with an unknown level are excluded.
#[inline]
pub fn classify(name: &str, waterlogged: bool, water_level: Option<u8>) -> BlockClass {
    let id = strip_namespace(name);
    if id == "flowing_water" {
        return BlockClass::Solid;
    }
    if id == "water" {
        return if water_level == Some(0) {
            BlockClass::Water
        } else {
            BlockClass::Solid
        };
    }
    if id == "air" || id == "cave_air" || id == "void_air" {
        return BlockClass::Air;
    }
    if id == "lava" || id == "flowing_lava" {
        return BlockClass::Lava;
    }
    if ICE_BLOCKS.contains(&id) {
        return BlockClass::Ice;
    }
    if id == "mud" || WATERY_BLOCKS.contains(&id) {
        return BlockClass::Water;
    }
    if waterlogged {
        return BlockClass::Water;
    }
    if COVER_BLOCKS.contains(&id) {
        return BlockClass::Cover;
    }
    BlockClass::Solid
}

/// Living coral in any of its forms. Dead coral is deliberately excluded - a dead
/// reef is not a coral reef any more.
#[inline]
pub fn is_coral(name: &str) -> bool {
    let id = strip_namespace(name);
    if id.starts_with("dead_") {
        return false;
    }
    id.contains("coral")
}

#[inline]
pub fn is_aquatic_plant(name: &str) -> bool {
    AQUATIC_PLANTS.contains(&strip_namespace(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_source_fluid_counts_even_with_waterlogged_property() {
        for level in 1..=15 {
            assert!(!classify("minecraft:water", false, Some(level)).is_water());
            assert!(!classify("minecraft:water", true, Some(level)).is_water());
        }
        assert!(!classify("minecraft:water", false, None).is_water());
        assert!(!classify("minecraft:flowing_water", false, Some(0)).is_water());
        assert!(classify("minecraft:water", false, Some(0)).is_water());
        assert!(classify("minecraft:oak_stairs", true, None).is_water());
        assert!(classify("minecraft:kelp", false, None).is_water());
    }

    #[test]
    fn raw_mud_counts_but_dry_mud_building_blocks_do_not() {
        assert!(classify("minecraft:mud", false, None).is_water());
        for name in [
            "minecraft:packed_mud",
            "minecraft:mud_bricks",
            "minecraft:muddy_mangrove_roots",
        ] {
            assert!(!classify(name, false, None).is_water(), "{name}");
            assert!(classify(name, true, None).is_water(), "waterlogged {name}");
        }
    }

    #[test]
    fn water_family_is_recognised() {
        assert_eq!(
            classify("minecraft:water", false, Some(0)),
            BlockClass::Water
        );
        assert_eq!(
            classify("minecraft:kelp_plant", false, Some(0)),
            BlockClass::Water
        );
        assert_eq!(
            classify("minecraft:tall_seagrass", false, Some(0)),
            BlockClass::Water
        );
        assert_eq!(
            classify("minecraft:oak_stairs", true, Some(0)),
            BlockClass::Water
        );
        assert_eq!(
            classify("minecraft:oak_stairs", false, Some(0)),
            BlockClass::Solid
        );
    }

    #[test]
    fn ice_and_cover_are_distinct_from_solid() {
        assert_eq!(classify("minecraft:ice", false, Some(0)), BlockClass::Ice);
        assert_eq!(
            classify("minecraft:blue_ice", false, Some(0)),
            BlockClass::Ice
        );
        assert_eq!(
            classify("minecraft:snow", false, Some(0)),
            BlockClass::Cover
        );
        assert_eq!(
            classify("minecraft:lily_pad", false, Some(0)),
            BlockClass::Cover
        );
        assert_eq!(
            classify("minecraft:stone", false, Some(0)),
            BlockClass::Solid
        );
        assert!(BlockClass::Ice.is_cover());
        assert!(!BlockClass::Solid.is_cover());
    }

    #[test]
    fn coral_detection_ignores_dead_coral() {
        assert!(is_coral("minecraft:brain_coral_block"));
        assert!(is_coral("minecraft:fire_coral_fan"));
        assert!(is_coral("minecraft:tube_coral"));
        assert!(!is_coral("minecraft:dead_brain_coral_block"));
        assert!(!is_coral("minecraft:stone"));
    }

    #[test]
    fn namespace_is_optional() {
        assert_eq!(strip_namespace("terralith:foo"), "foo");
        assert_eq!(strip_namespace("water"), "water");
        assert_eq!(classify("water", false, Some(0)), BlockClass::Water);
    }
}
