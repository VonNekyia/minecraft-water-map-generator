//! Biome registry and biome derived environment classification.
//!
//! The registry is built once, before scanning, from
//!   1. a built-in vanilla overworld table, and
//!   2. every biome JSON found in the world's `datapacks` folder
//!      (both `.zip` packs and unpacked folders).
//!
//! Datapack entries override built-ins, which is what makes worlds generated with
//! Terralith, Incendium, William Wythers, ... classify correctly without any code
//! change: their biomes ship `temperature` and `downfall` in the pack itself.
//!
//! Everything the scanner needs at runtime is looked up through an immutable map,
//! so biome lookups during the parallel scan are lock free.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::RwLock;

use crate::config;
use crate::water::model::{Temperature, Vegetation};

/// Sentinel used when a biome name is not present in the registry.
pub const UNKNOWN_BIOME: u16 = u16::MAX;

/// Coarse hydrological hint carried by a biome name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum BiomeFamily {
    Land = 0,
    Ocean = 1,
    River = 2,
    Swamp = 3,
}

bitflags::bitflags! {
    /// Environmental traits derived from biome names.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub struct BiomeTraits: u16 {
        const FROZEN   = 1 << 0;
        const DESERT   = 1 << 1;
        const MANGROVE = 1 << 2;
        const JUNGLE   = 1 << 3;
        const CAVE     = 1 << 4;
    }
}

/// Everything the classifier needs to know about one biome.
#[derive(Clone, Debug)]
pub struct BiomeInfo {
    pub name: String,
    #[allow(dead_code)]
    pub temperature: f32,
    #[allow(dead_code)]
    pub downfall: f32,
    pub family: BiomeFamily,
    pub traits: BiomeTraits,
    /// Temperature class used for water in this biome.
    pub water_temperature: Temperature,
    /// Vegetation class implied by the biome alone (block evidence refines it).
    pub vegetation: Vegetation,
}

impl BiomeInfo {
    pub fn derive(name: &str, temperature: f32, downfall: f32) -> Self {
        let id = crate::world::blocks::strip_namespace(name);
        let family = family_of(id);
        let traits = traits_of(id);
        let water_temperature = water_temperature_of(id, family, traits, temperature);
        let vegetation = vegetation_of(id, family, traits, downfall);
        BiomeInfo {
            name: name.to_string(),
            temperature,
            downfall,
            family,
            traits,
            water_temperature,
            vegetation,
        }
    }
}

fn contains_any(id: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| id.contains(n))
}

fn family_of(id: &str) -> BiomeFamily {
    if contains_any(id, &["swamp", "marsh", "bayou", "mangrove"]) {
        return BiomeFamily::Swamp;
    }
    if id.contains("river") {
        return BiomeFamily::River;
    }
    if id.contains("ocean") {
        return BiomeFamily::Ocean;
    }
    BiomeFamily::Land
}

fn traits_of(id: &str) -> BiomeTraits {
    let mut t = BiomeTraits::empty();
    if contains_any(
        id,
        &[
            "frozen", "snowy", "glacial", "wintry", "siberian", "polar", "frost", "ice_", "_ice",
            "icy",
        ],
    ) || id == "ice"
    {
        t |= BiomeTraits::FROZEN;
    }
    // The map's sand category is explicitly desert and canyon, not every
    // low-rainfall biome or terrain type that might look dry.
    if contains_any(id, &["desert", "canyon"]) {
        t |= BiomeTraits::DESERT;
    }
    if id.contains("mangrove") {
        t |= BiomeTraits::MANGROVE;
    }
    if contains_any(id, &["jungle", "rainforest", "bamboo"]) {
        t |= BiomeTraits::JUNGLE;
    }
    if id.contains("cave") || id.contains("deep_dark") {
        t |= BiomeTraits::CAVE;
    }
    t
}

fn water_temperature_of(
    id: &str,
    family: BiomeFamily,
    traits: BiomeTraits,
    temperature: f32,
) -> Temperature {
    // Ocean biomes carry their water temperature in the name, not in the
    // `temperature` field (vanilla `deep_frozen_ocean` has temperature 0.5).
    if family == BiomeFamily::Ocean {
        if id.contains("warm") {
            // Covers both `warm_ocean` and `lukewarm_ocean`.
            return Temperature::Warm;
        }
        if id.contains("frozen") || id.contains("cold") {
            return Temperature::Cold;
        }
        return Temperature::Medium;
    }
    if traits.contains(BiomeTraits::FROZEN) {
        return Temperature::Cold;
    }
    if family == BiomeFamily::River && id.contains("warm") {
        return Temperature::Warm;
    }
    if temperature >= config::TEMP_WARM_MIN {
        Temperature::Warm
    } else if temperature >= config::TEMP_MEDIUM_MIN {
        Temperature::Medium
    } else {
        Temperature::Cold
    }
}

fn vegetation_of(
    id: &str,
    family: BiomeFamily,
    traits: BiomeTraits,
    downfall: f32,
) -> Vegetation {
    if traits.intersects(BiomeTraits::JUNGLE | BiomeTraits::MANGROVE) {
        return Vegetation::Jungle;
    }
    if family == BiomeFamily::Ocean {
        if id.contains("frozen") {
            return Vegetation::None;
        }
        if id.contains("cold") {
            return Vegetation::Sparse;
        }
        return Vegetation::Normal;
    }
    if traits.contains(BiomeTraits::DESERT) {
        return Vegetation::None;
    }
    if downfall >= config::VEG_NORMAL_MIN {
        Vegetation::Normal
    } else if downfall >= config::VEG_SPARSE_MIN {
        Vegetation::Sparse
    } else {
        Vegetation::None
    }
}

/// Built-in vanilla overworld biomes: `(name, temperature, downfall)`.
const VANILLA: &[(&str, f32, f32)] = &[
    ("minecraft:ocean", 0.5, 0.5),
    ("minecraft:deep_ocean", 0.5, 0.5),
    ("minecraft:warm_ocean", 0.5, 0.5),
    ("minecraft:lukewarm_ocean", 0.5, 0.5),
    ("minecraft:deep_lukewarm_ocean", 0.5, 0.5),
    ("minecraft:cold_ocean", 0.5, 0.5),
    ("minecraft:deep_cold_ocean", 0.5, 0.5),
    ("minecraft:frozen_ocean", 0.0, 0.5),
    ("minecraft:deep_frozen_ocean", 0.5, 0.5),
    ("minecraft:river", 0.5, 0.5),
    ("minecraft:frozen_river", 0.0, 0.5),
    ("minecraft:beach", 0.8, 0.4),
    ("minecraft:snowy_beach", 0.05, 0.3),
    ("minecraft:stony_shore", 0.2, 0.3),
    ("minecraft:plains", 0.8, 0.4),
    ("minecraft:sunflower_plains", 0.8, 0.4),
    ("minecraft:snowy_plains", 0.0, 0.5),
    ("minecraft:ice_spikes", 0.0, 0.5),
    ("minecraft:desert", 2.0, 0.0),
    ("minecraft:savanna", 2.0, 0.0),
    ("minecraft:savanna_plateau", 2.0, 0.0),
    ("minecraft:windswept_savanna", 2.0, 0.0),
    ("minecraft:badlands", 2.0, 0.0),
    ("minecraft:eroded_badlands", 2.0, 0.0),
    ("minecraft:wooded_badlands", 2.0, 0.0),
    ("minecraft:forest", 0.7, 0.8),
    ("minecraft:flower_forest", 0.7, 0.8),
    ("minecraft:birch_forest", 0.6, 0.6),
    ("minecraft:old_growth_birch_forest", 0.6, 0.6),
    ("minecraft:dark_forest", 0.7, 0.8),
    ("minecraft:pale_garden", 0.7, 0.8),
    ("minecraft:taiga", 0.25, 0.8),
    ("minecraft:snowy_taiga", -0.5, 0.4),
    ("minecraft:old_growth_pine_taiga", 0.3, 0.8),
    ("minecraft:old_growth_spruce_taiga", 0.25, 0.8),
    ("minecraft:jungle", 0.95, 0.9),
    ("minecraft:sparse_jungle", 0.95, 0.8),
    ("minecraft:bamboo_jungle", 0.95, 0.9),
    ("minecraft:swamp", 0.8, 0.9),
    ("minecraft:mangrove_swamp", 0.8, 0.9),
    ("minecraft:mushroom_fields", 0.9, 1.0),
    ("minecraft:meadow", 0.5, 0.8),
    ("minecraft:cherry_grove", 0.5, 0.8),
    ("minecraft:grove", -0.2, 0.8),
    ("minecraft:snowy_slopes", -0.3, 0.9),
    ("minecraft:jagged_peaks", -0.7, 0.9),
    ("minecraft:frozen_peaks", -0.7, 0.9),
    ("minecraft:stony_peaks", 1.0, 0.3),
    ("minecraft:windswept_hills", 0.2, 0.3),
    ("minecraft:windswept_gravelly_hills", 0.2, 0.3),
    ("minecraft:windswept_forest", 0.2, 0.3),
    ("minecraft:dripstone_caves", 0.8, 0.4),
    ("minecraft:lush_caves", 0.5, 0.5),
    ("minecraft:deep_dark", 0.8, 0.4),
    ("minecraft:the_void", 0.5, 0.5),
];

/// Immutable biome registry consulted by the scanner.
pub struct BiomeRegistry {
    infos: Vec<BiomeInfo>,
    by_name: HashMap<String, u16>,
    /// Biome names met during the scan that were not in the pre-built registry.
    /// Only touched on a miss, which keeps the hot path lock free.
    overflow: RwLock<Vec<String>>,
    /// Number of biome JSONs loaded from datapacks.
    pub datapack_biomes: usize,
}

#[allow(dead_code)]
impl BiomeRegistry {
    pub fn build(world_dir: &Path) -> Self {
        let mut by_name: HashMap<String, u16> = HashMap::new();
        let mut infos: Vec<BiomeInfo> = Vec::new();
        let push = |infos: &mut Vec<BiomeInfo>,
                        by_name: &mut HashMap<String, u16>,
                        name: &str,
                        t: f32,
                        d: f32| {
            let info = BiomeInfo::derive(name, t, d);
            match by_name.get(name) {
                Some(&i) => infos[i as usize] = info,
                None => {
                    let id = infos.len() as u16;
                    infos.push(info);
                    by_name.insert(name.to_string(), id);
                }
            }
        };

        for (name, t, d) in VANILLA {
            push(&mut infos, &mut by_name, name, *t, *d);
        }

        let mut datapack_biomes = 0;
        for (name, t, d) in load_datapack_biomes(world_dir) {
            push(&mut infos, &mut by_name, &name, t, d);
            datapack_biomes += 1;
        }

        BiomeRegistry {
            infos,
            by_name,
            overflow: RwLock::new(Vec::new()),
            datapack_biomes,
        }
    }

    /// Registry used by unit tests and by synthetic worlds.
    pub fn vanilla_only() -> Self {
        let mut by_name = HashMap::new();
        let mut infos = Vec::new();
        for (name, t, d) in VANILLA {
            let id = infos.len() as u16;
            infos.push(BiomeInfo::derive(name, *t, *d));
            by_name.insert((*name).to_string(), id);
        }
        BiomeRegistry {
            infos,
            by_name,
            overflow: RwLock::new(Vec::new()),
            datapack_biomes: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.infos.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.infos.is_empty()
    }

    /// Looks up a biome id. Unknown names are recorded once and reported at the
    /// end of the scan so world packs with missing metadata become visible.
    #[inline]
    pub fn id_of(&self, name: &str) -> u16 {
        match self.by_name.get(name) {
            Some(&id) => id,
            None => {
                self.record_unknown(name);
                UNKNOWN_BIOME
            }
        }
    }

    #[cold]
    fn record_unknown(&self, name: &str) {
        if let Ok(seen) = self.overflow.read() {
            if seen.iter().any(|n| n == name) {
                return;
            }
        }
        if let Ok(mut seen) = self.overflow.write() {
            if !seen.iter().any(|n| n == name) {
                seen.push(name.to_string());
            }
        }
    }

    /// Classification for a biome id. Unknown ids fall back to neutral values so a
    /// missing biome never invalidates a whole region.
    #[inline]
    pub fn info(&self, id: u16) -> Option<&BiomeInfo> {
        self.infos.get(id as usize)
    }

    pub fn name_of(&self, id: u16) -> &str {
        self.info(id).map(|i| i.name.as_str()).unwrap_or("unknown")
    }

    pub fn unknown_names(&self) -> Vec<String> {
        self.overflow.read().map(|v| v.clone()).unwrap_or_default()
    }
}

/// Reads `temperature` / `downfall` from every biome JSON in the world datapacks.
fn load_datapack_biomes(world_dir: &Path) -> Vec<(String, f32, f32)> {
    let mut out = Vec::new();
    let dp = world_dir.join("datapacks");
    let Ok(entries) = std::fs::read_dir(&dp) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_from_dir(&path, &mut out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("zip") {
            collect_from_zip(&path, &mut out);
        }
    }
    out
}

fn biome_name_from_path(path: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    let rest = norm.strip_suffix(".json")?;
    let idx = rest.find("data/")?;
    let rest = &rest[idx + 5..];
    let slash = rest.find('/')?;
    let ns = &rest[..slash];
    let rest = &rest[slash + 1..];
    let rest = rest.strip_prefix("worldgen/biome/")?;
    Some(format!("{ns}:{rest}"))
}

fn parse_biome_json(bytes: &[u8]) -> Option<(f32, f32)> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let t = v.get("temperature").and_then(|x| x.as_f64()).unwrap_or(0.5) as f32;
    let d = v.get("downfall").and_then(|x| x.as_f64()).unwrap_or(0.5) as f32;
    Some((t, d))
}

fn collect_from_zip(path: &Path, out: &mut Vec<(String, f32, f32)>) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(mut zip) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return;
    };
    for i in 0..zip.len() {
        let Ok(mut f) = zip.by_index(i) else { continue };
        if !f.name().ends_with(".json") || !f.name().contains("/worldgen/biome/") {
            continue;
        }
        let Some(name) = biome_name_from_path(f.name()) else {
            continue;
        };
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            continue;
        }
        if let Some((t, d)) = parse_biome_json(&buf) {
            out.push((name, t, d));
        }
    }
}

fn collect_from_dir(root: &Path, out: &mut Vec<(String, f32, f32)>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(s) = path.to_str() else { continue };
            if !s.replace('\\', "/").contains("/worldgen/biome/") {
                continue;
            }
            let Some(name) = biome_name_from_path(s) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if let Some((t, d)) = parse_biome_json(&bytes) {
                out.push((name, t, d));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocean_temperature_comes_from_the_name_not_the_value() {
        // Vanilla `deep_frozen_ocean` has temperature 0.5 but is cold water.
        let b = BiomeInfo::derive("minecraft:deep_frozen_ocean", 0.5, 0.5);
        assert_eq!(b.water_temperature, Temperature::Cold);
        assert_eq!(b.family, BiomeFamily::Ocean);

        let b = BiomeInfo::derive("minecraft:warm_ocean", 0.5, 0.5);
        assert_eq!(b.water_temperature, Temperature::Warm);

        let b = BiomeInfo::derive("minecraft:lukewarm_ocean", 0.5, 0.5);
        assert_eq!(b.water_temperature, Temperature::Warm);

        let b = BiomeInfo::derive("minecraft:deep_ocean", 0.5, 0.5);
        assert_eq!(b.water_temperature, Temperature::Medium);
    }

    #[test]
    fn every_vanilla_ocean_biome_maps_to_an_existing_ocean_category() {
        let expected = [
            ("minecraft:ocean", Temperature::Medium),
            ("minecraft:deep_ocean", Temperature::Medium),
            ("minecraft:warm_ocean", Temperature::Warm),
            ("minecraft:lukewarm_ocean", Temperature::Warm),
            ("minecraft:deep_lukewarm_ocean", Temperature::Warm),
            ("minecraft:cold_ocean", Temperature::Cold),
            ("minecraft:deep_cold_ocean", Temperature::Cold),
            ("minecraft:frozen_ocean", Temperature::Cold),
            ("minecraft:deep_frozen_ocean", Temperature::Cold),
        ];
        for (name, temperature) in expected {
            let info = BiomeInfo::derive(name, 0.5, 0.5);
            assert_eq!(info.family, BiomeFamily::Ocean, "{name}");
            assert_eq!(info.water_temperature, temperature, "{name}");
        }
        let terralith = BiomeInfo::derive("terralith:deep_warm_ocean", 0.5, 0.5);
        assert_eq!(terralith.family, BiomeFamily::Ocean);
        assert_eq!(terralith.water_temperature, Temperature::Warm);
    }

    #[test]
    fn families_are_derived_from_names() {
        assert_eq!(
            BiomeInfo::derive("minecraft:river", 0.5, 0.5).family,
            BiomeFamily::River
        );
        assert_eq!(
            BiomeInfo::derive("terralith:warm_river", 0.5, 0.5).family,
            BiomeFamily::River
        );
        assert_eq!(
            BiomeInfo::derive("minecraft:mangrove_swamp", 0.8, 0.9).family,
            BiomeFamily::Swamp
        );
        assert_eq!(
            BiomeInfo::derive("terralith:ice_marsh", 0.14, 0.9).family,
            BiomeFamily::Swamp
        );
        assert_eq!(
            BiomeInfo::derive("minecraft:plains", 0.8, 0.4).family,
            BiomeFamily::Land
        );
    }

    #[test]
    fn traits_pick_up_desert_mangrove_jungle_and_frozen() {
        let d = BiomeInfo::derive("minecraft:desert", 2.0, 0.0);
        assert!(d.traits.contains(BiomeTraits::DESERT));
        assert_eq!(d.water_temperature, Temperature::Warm);
        assert_eq!(d.vegetation, Vegetation::None);

        let m = BiomeInfo::derive("minecraft:mangrove_swamp", 0.8, 0.9);
        assert!(m.traits.contains(BiomeTraits::MANGROVE));
        assert_eq!(m.vegetation, Vegetation::Jungle);

        let j = BiomeInfo::derive("terralith:tropical_jungle", 0.95, 0.9);
        assert!(j.traits.contains(BiomeTraits::JUNGLE));

        let f = BiomeInfo::derive("terralith:wintry_forest", -0.5, 0.4);
        assert!(f.traits.contains(BiomeTraits::FROZEN));
        assert_eq!(f.water_temperature, Temperature::Cold);
    }

    #[test]
    fn desert_category_is_limited_to_desert_and_canyon_names() {
        for (name, temperature, downfall) in [
            ("minecraft:desert", 2.0, 0.0),
            ("terralith:desert_oasis", 2.0, 0.0),
            ("terralith:desert_canyon", 2.0, 0.0),
            ("terralith:bryce_canyon", 2.0, 0.0),
            ("terralith:amethyst_canyon", 0.95, 0.9),
        ] {
            assert!(BiomeInfo::derive(name, temperature, downfall).traits
                .contains(BiomeTraits::DESERT), "{name}");
        }
        for (name, temperature, downfall) in [
            ("minecraft:savanna", 1.2, 0.0),
            ("minecraft:savanna_plateau", 1.2, 0.0),
            ("minecraft:badlands", 2.0, 0.0),
            ("terralith:brushland", 1.2, 0.2),
            ("terralith:steppe", 0.4, -0.5),
            ("terralith:arid_highlands", 1.6, 0.1),
            ("custom:mesa", 2.0, 0.0),
            ("custom:wasteland", 2.0, 0.0),
            ("custom:dry_country", 0.25, 0.2),
            ("minecraft:plains", 0.8, 0.4),
            ("minecraft:forest", 0.7, 0.8),
            ("custom:wet_country", 0.8, 0.21),
            ("terralith:cold_shrubland", 0.14, 0.0),
            ("custom:snowy_country", 0.5, 0.0),
            ("terralith:mantle_caves", 2.0, 0.0),
            ("custom:swamp", 1.2, 0.0),
            ("custom:river", 1.2, 0.0),
            ("custom:ocean", 1.2, 0.0),
        ] {
            assert!(!BiomeInfo::derive(name, temperature, downfall).traits
                .contains(BiomeTraits::DESERT), "{name}");
        }
    }

    #[test]
    fn datapack_paths_resolve_to_resource_locations() {
        assert_eq!(
            biome_name_from_path("data/terralith/worldgen/biome/cave/deep_caves.json").as_deref(),
            Some("terralith:cave/deep_caves")
        );
        assert_eq!(
            biome_name_from_path("pack/data/minecraft/worldgen/biome/desert.json").as_deref(),
            Some("minecraft:desert")
        );
        assert!(biome_name_from_path("data/minecraft/worldgen/noise/foo.json").is_none());
    }

    #[test]
    fn registry_reports_unknown_names_once() {
        let reg = BiomeRegistry::vanilla_only();
        assert_ne!(reg.id_of("minecraft:ocean"), UNKNOWN_BIOME);
        assert_eq!(reg.id_of("weird:nope"), UNKNOWN_BIOME);
        assert_eq!(reg.id_of("weird:nope"), UNKNOWN_BIOME);
        assert_eq!(reg.unknown_names(), vec!["weird:nope".to_string()]);
    }
}
