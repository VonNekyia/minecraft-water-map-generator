//! Human readable JSON export of the analysis result.
//!
//! Development and validation aid only - the runtime format is
//! `water_regions.bin`.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::water::model::*;

#[derive(Serialize)]
struct JsonGeometry {
    min_x: i32,
    min_z: i32,
    max_x: i32,
    max_z: i32,
    columns: u32,
    runs: usize,
}

#[derive(Serialize)]
struct JsonBathymetry {
    mean_depth: u16,
    max_depth: u16,
    /// Share of columns deeper than the configured contour levels, 0.0 - 1.0.
    contours: Vec<JsonContour>,
}

#[derive(Serialize)]
struct JsonContour {
    depth: u8,
    share: f32,
}

#[derive(Serialize)]
struct JsonRegion {
    id: u32,
    kind: &'static str,
    temperature: &'static str,
    vegetation: &'static str,
    depth: Option<&'static str>,
    modifiers: Vec<&'static str>,
    surface_y: i16,
    dominant_biome: String,
    geometry: JsonGeometry,
    bathymetry: JsonBathymetry,
}

#[derive(Serialize)]
struct JsonMeta {
    world: String,
    minecraft_data_version: i32,
    sea_level: i16,
    sea_level_source: String,
    region_count: usize,
    water_columns: u64,
    scan_seconds: f64,
}

#[derive(Serialize)]
struct JsonRoot {
    meta: JsonMeta,
    regions: Vec<JsonRegion>,
}

fn to_json(region: &WaterRegion) -> JsonRegion {
    JsonRegion {
        id: region.id,
        kind: region.kind.as_str(),
        temperature: region.temperature.as_str(),
        vegetation: region.vegetation.as_str(),
        depth: region.depth.map(|d| d.as_str()),
        modifiers: region.modifiers.names(),
        surface_y: region.surface_y,
        dominant_biome: region.dominant_biome.clone(),
        geometry: JsonGeometry {
            min_x: region.geometry.min_x,
            min_z: region.geometry.min_z,
            max_x: region.geometry.max_x,
            max_z: region.geometry.max_z,
            columns: region.geometry.column_count,
            runs: region.geometry.runs.len(),
        },
        bathymetry: JsonBathymetry {
            mean_depth: region.bathymetry.mean_depth,
            max_depth: region.bathymetry.max_depth,
            contours: crate::config::BATHYMETRY_CONTOURS
                .iter()
                .enumerate()
                .map(|(i, d)| JsonContour {
                    depth: *d,
                    share: region.bathymetry.contour_shares[i] as f32 / 255.0,
                })
                .collect(),
        },
    }
}

pub struct JsonMetaInput<'a> {
    pub world: &'a str,
    pub minecraft_data_version: i32,
    pub sea_level: i16,
    pub sea_level_source: &'a str,
    pub water_columns: u64,
    pub scan_seconds: f64,
    /// Cap on the number of exported regions, largest first. `None` exports all.
    pub limit: Option<usize>,
}

/// Writes `water_regions.json`. Returns the file size in bytes.
pub fn write(path: &Path, regions: &[WaterRegion], meta: JsonMetaInput<'_>) -> std::io::Result<u64> {
    let mut sorted: Vec<&WaterRegion> = regions.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.geometry.column_count));
    if let Some(limit) = meta.limit {
        sorted.truncate(limit);
    }

    let root = JsonRoot {
        meta: JsonMeta {
            world: meta.world.to_string(),
            minecraft_data_version: meta.minecraft_data_version,
            sea_level: meta.sea_level,
            sea_level_source: meta.sea_level_source.to_string(),
            region_count: regions.len(),
            water_columns: meta.water_columns,
            scan_seconds: (meta.scan_seconds * 1000.0).round() / 1000.0,
        },
        regions: sorted.into_iter().map(to_json).collect(),
    };

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
    serde_json::to_writer_pretty(&mut w, &root)?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(std::fs::metadata(path)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(id: u32, columns: u32, kind: WaterKind) -> WaterRegion {
        WaterRegion {
            id,
            geometry: RegionGeometry {
                min_x: 0,
                min_z: 0,
                max_x: 10,
                max_z: 10,
                runs: vec![Run { z: 0, x0: 0, x1: 10 }],
                column_count: columns,
            },
            kind,
            temperature: Temperature::Warm,
            vegetation: Vegetation::Jungle,
            depth: Some(Depth::Normal),
            modifiers: Modifiers::CORALS,
            surface_y: 62,
            bathymetry: Bathymetry {
                mean_depth: 20,
                max_depth: 51,
                contour_shares: [255, 128, 0, 0],
            },
            dominant_biome: "minecraft:warm_ocean".into(),
        }
    }

    #[test]
    fn json_shape_matches_the_documented_example() {
        let r = region(184, 5000, WaterKind::Sea);
        let j = serde_json::to_value(to_json(&r)).unwrap();
        assert_eq!(j["id"], 184);
        assert_eq!(j["kind"], "sea");
        assert_eq!(j["temperature"], "warm");
        assert_eq!(j["vegetation"], "jungle");
        assert_eq!(j["depth"], "normal");
        assert_eq!(j["modifiers"][0], "corals");
        assert_eq!(j["bathymetry"]["max_depth"], 51);
        assert_eq!(j["bathymetry"]["contours"][0]["depth"], 10);
    }

    #[test]
    fn regions_without_depth_serialise_as_null() {
        let mut r = region(1, 40, WaterKind::Lake);
        r.depth = None;
        r.modifiers = Modifiers::empty();
        let j = serde_json::to_value(to_json(&r)).unwrap();
        assert!(j["depth"].is_null());
        assert_eq!(j["modifiers"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn export_is_sorted_by_size_and_honours_the_limit() {
        let dir = std::env::temp_dir().join("water-analyzer-json-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("water_regions.json");
        let regions = vec![
            region(0, 10, WaterKind::Lake),
            region(1, 9000, WaterKind::Sea),
            region(2, 500, WaterKind::River),
        ];
        write(
            &path,
            &regions,
            JsonMetaInput {
                world: "test",
                minecraft_data_version: 4671,
                sea_level: 63,
                sea_level_source: "ocean-histogram",
                water_columns: 9510,
                scan_seconds: 1.5,
                limit: Some(2),
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["meta"]["region_count"], 3);
        assert_eq!(v["meta"]["sea_level"], 63);
        assert_eq!(v["regions"].as_array().unwrap().len(), 2);
        assert_eq!(v["regions"][0]["id"], 1);
        assert_eq!(v["regions"][1]["id"], 2);
        let _ = std::fs::remove_file(&path);
    }
}
