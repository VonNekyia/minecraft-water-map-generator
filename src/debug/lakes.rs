//! Optional lake-analysis diagnostics. These images only downsample retained
//! one-block analysis fields; they never feed geometry or labels back into the
//! normal map. A maximum per display pixel keeps thin cores/necks visible.

use std::io::{BufWriter, Write};
use std::path::Path;

use serde::Serialize;

use crate::water::grid::WorldGrid;
use crate::water::lakes::{LakeAnalysis, LakeCandidate, LakeOptions};

use super::map::Canvas;

const VOID: [u8; 3] = [14, 14, 20];
const LAND: [u8; 3] = [52, 54, 58];
const PANEL: [u8; 3] = [8, 8, 12];
const TEXT: [u8; 3] = [232, 232, 236];
const DIM: [u8; 3] = [152, 156, 166];
const WATER: [u8; 3] = [62, 105, 120];
const LAKE: [u8; 3] = [58, 225, 205];
const CORE: [u8; 3] = [252, 227, 88];
const NECK: [u8; 3] = [255, 143, 60];
const REJECTED: [u8; 3] = [234, 80, 112];
const INFLOW: [u8; 3] = [116, 246, 121];
const OUTFLOW: [u8; 3] = [130, 165, 255];
const UNKNOWN: [u8; 3] = [245, 245, 245];

#[derive(Clone, Copy)]
enum Layer {
    Raw,
    Width,
    Density(usize),
    Cores,
    Reconstructed,
    Necks,
    Connections,
    Rejected,
}

const LAYERS: [(Layer, &str, &str); 10] = [
    (Layer::Raw, "lake_raw_inland.png", "RAW INLAND WATER"),
    (
        Layer::Width,
        "lake_distance_width.png",
        "DISTANCE TO SHORE / WIDTH",
    ),
    (
        Layer::Density(0),
        "lake_density_8.png",
        "WATER DENSITY: RADIUS 8",
    ),
    (
        Layer::Density(1),
        "lake_density_16.png",
        "WATER DENSITY: RADIUS 16",
    ),
    (
        Layer::Density(2),
        "lake_density_32.png",
        "WATER DENSITY: RADIUS 32",
    ),
    (Layer::Cores, "lake_cores.png", "LAKE CORES"),
    (
        Layer::Reconstructed,
        "lake_reconstructed.png",
        "RECONSTRUCTED LAKES",
    ),
    (Layer::Necks, "lake_necks.png", "RIVER / LAKE NECKS"),
    (
        Layer::Connections,
        "lake_connections.png",
        "INFLOWS / OUTFLOWS",
    ),
    (Layer::Rejected, "lake_rejected.png", "REJECTED CANDIDATES"),
];

#[derive(Serialize)]
struct Diagnostics<'a> {
    analysis_blocks_per_cell: u32,
    render_blocks_per_pixel: u32,
    world_bounds: [i32; 4],
    inland_water_columns: u64,
    accepted_candidate_count: usize,
    river_promotion_candidate_count: usize,
    rejected_candidate_count: usize,
    distance_metric: &'static str,
    density_windows: &'static str,
    render_reduction: &'static str,
    terrain_note: &'static str,
    flow_note: &'static str,
    options: &'a LakeOptions,
    candidates: &'a [LakeCandidate],
    closed_water: &'a crate::water::lakes::ClosedWater,
    source_kinds: &'a [crate::water::model::WaterKind],
    candidate_runs_format: &'static str,
}

/// Writes candidates/options JSON and ten optional PNG layers into `dir`.
/// Input fields retain one-block resolution. Only PNG output uses `scale`.
pub fn write(
    dir: &Path,
    grid: &WorldGrid,
    analysis: &LakeAnalysis,
    scale: u32,
) -> anyhow::Result<()> {
    anyhow::ensure!(scale > 0, "lake debug map scale must be positive");
    std::fs::create_dir_all(dir)?;
    let bounds = if grid.regions.is_empty() {
        // Synthetic stage checks may supply geometry without a scanned world.
        analysis
            .raster
            .source
            .iter()
            .enumerate()
            .filter(|(_, source)| **source != u32::MAX)
            .map(|(i, _)| analysis.raster.coords(i))
            .fold(None, |bounds, (x, z)| {
                Some(match bounds {
                    None => (x, z, x, z),
                    Some((x0, z0, x1, z1)) => (x.min(x0), z.min(z0), x.max(x1), z.max(z1)),
                })
            })
            .unwrap_or((0, 0, 0, 0))
    } else {
        grid.world_bounds()
    };
    let accepted = analysis.candidates.iter().filter(|c| c.accepted).count();
    let metadata = Diagnostics {
        analysis_blocks_per_cell: 1,
        render_blocks_per_pixel: scale,
        world_bounds: [bounds.0, bounds.1, bounds.2, bounds.3],
        inland_water_columns: analysis.raster.source.iter().filter(|&&s| s != u32::MAX).count() as u64,
        accepted_candidate_count: accepted,
        river_promotion_candidate_count: analysis.candidates.iter().filter(|c| c.accepts_river_water).count(),
        rejected_candidate_count: analysis.candidates.len() - accepted,
        distance_metric: "Manhattan blocks to nearest dry cell; shore water = 1; approximate width = 2 * distance",
        density_windows: "Square windows at radii 8, 16, 32 blocks (17, 33, 65 blocks per side)",
        render_reduction: "Maximum distance/density per PNG pixel; any highlighted cell preserves a categorical feature; connection symbols are enlarged debug markers",
        terrain_note: "Lowest valid heightmap surface per 4x4 cell; canopy and roofs may bias terrain upward; missing samples remain unknown",
        flow_note: "Direction is inferred from water-surface head differences, not simulated flow; flat or unavailable head remains unknown",
        options: &analysis.options,
        candidates: &analysis.candidates,
        closed_water: &analysis.closed_water,
        source_kinds: &analysis.source_kinds,
        candidate_runs_format: "lake_candidate_runs.bin: LKRUNS01 magic, then little-endian 20-byte records: u32 source index, u32 candidate id+1 (0 = channel), i32 z, i32 x0, i32 x1 inclusive. Original one-block water only; runs are in raster tile order.",
    };
    let mut json = BufWriter::new(std::fs::File::create(dir.join("lake_candidates.json"))?);
    serde_json::to_writer_pretty(&mut json, &metadata)?;
    json.write_all(b"\n")?;
    json.flush()?;
    write_candidate_runs(&dir.join("lake_candidate_runs.bin"), analysis)?;

    let projection = Projection::new(bounds, scale)?;
    let mut values = vec![0u16; projection.w * projection.h];
    for (layer, filename, title) in LAYERS {
        reduce(analysis, &projection, layer, &mut values);
        let canvas = render(grid, analysis, &projection, layer, title, &values);
        canvas.write_png(&dir.join(filename))?;
    }
    Ok(())
}

/// Optional exact labels support threshold experiments without rescanning a world.
/// Limit each run to its raster row; tile boundaries may split otherwise equal runs.
fn write_candidate_runs(path: &Path, analysis: &LakeAnalysis) -> anyhow::Result<()> {
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    out.write_all(b"LKRUNS01")?;
    let r = &analysis.raster;
    let mut i = 0;
    while i < r.len() {
        let source = r.source[i];
        if source == u32::MAX {
            i += 1;
            continue;
        }
        let candidate = analysis.reconstructed[i];
        let (x0, z) = r.coords(i);
        let row_end = (i / 32 + 1) * 32;
        let start = i;
        i += 1;
        while i < row_end && r.source[i] == source && analysis.reconstructed[i] == candidate {
            i += 1;
        }
        let x1 = x0 + (i - start) as i32 - 1;
        out.write_all(&source.to_le_bytes())?;
        out.write_all(&candidate.to_le_bytes())?;
        out.write_all(&z.to_le_bytes())?;
        out.write_all(&x0.to_le_bytes())?;
        out.write_all(&x1.to_le_bytes())?;
    }
    out.flush()?;
    Ok(())
}

struct Projection {
    min_x: i32,
    min_z: i32,
    scale: u32,
    w: usize,
    h: usize,
}

impl Projection {
    fn new(bounds: (i32, i32, i32, i32), scale: u32) -> anyhow::Result<Self> {
        anyhow::ensure!(scale > 0, "lake debug map scale must be positive");
        let w = (i64::from(bounds.2) - i64::from(bounds.0)) / i64::from(scale) + 1;
        let h = (i64::from(bounds.3) - i64::from(bounds.1)) / i64::from(scale) + 1;
        anyhow::ensure!(
            w > 0 && h > 0 && w < u32::MAX as i64 && h < u32::MAX as i64,
            "invalid lake debug map bounds"
        );
        anyhow::ensure!(
            w.checked_mul(h).is_some_and(|n| n <= 100_000_000),
            "lake debug map exceeds 100 million pixels; increase --map-scale"
        );
        Ok(Self {
            min_x: bounds.0,
            min_z: bounds.1,
            scale,
            w: w as usize,
            h: h as usize,
        })
    }

    fn pixel(&self, x: i32, z: i32) -> Option<(usize, usize)> {
        let dx = i64::from(x) - i64::from(self.min_x);
        let dz = i64::from(z) - i64::from(self.min_z);
        if dx < 0 || dz < 0 {
            return None;
        }
        let px = (dx / i64::from(self.scale)) as usize;
        let pz = (dz / i64::from(self.scale)) as usize;
        (px < self.w && pz < self.h).then_some((px, pz))
    }
}

fn reduce(analysis: &LakeAnalysis, projection: &Projection, layer: Layer, values: &mut [u16]) {
    values.fill(0);
    for (i, &source) in analysis.raster.source.iter().enumerate() {
        if source == u32::MAX {
            continue;
        }
        let value = match layer {
            Layer::Raw => 1,
            Layer::Width => analysis.distance[i].min(64) + 1,
            Layer::Density(k) => u16::from(analysis.density[i][k]) + 1,
            Layer::Cores => {
                let core = analysis.cores[i];
                if core == 0 {
                    1
                } else if analysis.candidates[(core - 1) as usize].accepted {
                    3
                } else {
                    2
                }
            }
            Layer::Reconstructed | Layer::Connections => 1 + u16::from(analysis.is_lake(i)),
            Layer::Necks => 1 + u16::from(analysis.necks[i]),
            Layer::Rejected => {
                // Tiny cores are rejected before reconstruction. Keep their
                // original core pixels visible in this rejection layer too.
                let candidate = if analysis.reconstructed[i] > 0 {
                    analysis.reconstructed[i]
                } else {
                    analysis.cores[i]
                };
                1 + u16::from(
                    candidate > 0 && !analysis.is_lake(i)
                        && {
                            let c = &analysis.candidates[(candidate - 1) as usize];
                            !c.accepted || (analysis.source_kinds[source as usize] == crate::water::model::WaterKind::River && !c.accepts_river_water)
                        },
                )
            }
        };
        let (x, z) = analysis.raster.coords(i);
        if let Some((px, pz)) = projection.pixel(x, z) {
            let pixel = &mut values[pz * projection.w + px];
            *pixel = (*pixel).max(value);
        }
    }
}

fn ramp(value: f32) -> [u8; 3] {
    const STOPS: [[u8; 3]; 5] = [
        [55, 83, 157],
        [52, 186, 218],
        [66, 220, 155],
        [248, 216, 91],
        [236, 94, 81],
    ];
    let position = value.clamp(0.0, 1.0) * 4.0;
    let lo = (position as usize).min(3);
    let fraction = position - lo as f32;
    std::array::from_fn(|i| {
        (STOPS[lo][i] as f32 * (1.0 - fraction) + STOPS[lo + 1][i] as f32 * fraction).round() as u8
    })
}

fn color(layer: Layer, value: u16) -> [u8; 3] {
    match layer {
        Layer::Raw => LAKE,
        Layer::Width => ramp((value.saturating_sub(1)) as f32 / 64.0),
        Layer::Density(_) => ramp((value.saturating_sub(1)) as f32 / 255.0),
        Layer::Cores => match value {
            3 => CORE,
            2 => REJECTED,
            _ => WATER,
        },
        Layer::Reconstructed | Layer::Connections => {
            if value > 1 {
                LAKE
            } else {
                WATER
            }
        }
        Layer::Necks => {
            if value > 1 {
                NECK
            } else {
                WATER
            }
        }
        Layer::Rejected => {
            if value > 1 {
                REJECTED
            } else {
                WATER
            }
        }
    }
}

fn legend(
    analysis: &LakeAnalysis,
    layer: Layer,
    title: &str,
    scale: u32,
) -> Vec<(String, Option<[u8; 3]>)> {
    let mut rows = vec![
        ("LAKE ANALYSIS".into(), None),
        (title.into(), None),
        (String::new(), None),
        ("SOURCE: 1 BLOCK PER CELL".into(), None),
        (format!("RENDER: {scale} BLOCKS PER PIXEL"), None),
        ("MAX / ANY FEATURE PER RENDER PIXEL".into(), None),
        (String::new(), None),
    ];
    let mut swatch = |label: &str, rgb| rows.push((label.into(), Some(rgb)));
    match layer {
        Layer::Raw => swatch("ELIGIBLE INLAND WATER", LAKE),
        Layer::Width => {
            for (distance, label) in [
                (1, "DISTANCE 1 / WIDTH 2"),
                (8, "DISTANCE 8 / WIDTH 16"),
                (16, "DISTANCE 16 / WIDTH 32"),
                (32, "DISTANCE 32 / WIDTH 64"),
                (64, "DISTANCE 64+ / WIDTH 128+"),
            ] {
                swatch(label, ramp(distance as f32 / 64.0));
            }
        }
        Layer::Density(_) => {
            for percent in [0, 25, 50, 75, 100] {
                swatch(
                    &format!("{percent}% LOCAL WATER COVERAGE"),
                    ramp(percent as f32 / 100.0),
                );
            }
        }
        Layer::Cores => {
            swatch("ACCEPTED CANDIDATE CORE", CORE);
            swatch("REJECTED CANDIDATE CORE", REJECTED);
            swatch("OTHER INLAND WATER", WATER);
        }
        Layer::Reconstructed => {
            swatch("ACCEPTED RECONSTRUCTED LAKE", LAKE);
            swatch("REMAINING RIVER WATER", WATER);
        }
        Layer::Necks => {
            swatch("CONFIRMED NARROW CONNECTION", NECK);
            swatch("OTHER INLAND WATER", WATER);
        }
        Layer::Connections => {
            swatch("INFERRED INFLOW", INFLOW);
            swatch("INFERRED OUTFLOW", OUTFLOW);
            swatch("UNKNOWN / FLAT WATER HEAD", UNKNOWN);
            swatch("ACCEPTED LAKE", LAKE);
            swatch("OTHER INLAND WATER", WATER);
        }
        Layer::Rejected => {
            swatch("REJECTED BODY / TINY CORE", REJECTED);
            swatch("OTHER INLAND WATER", WATER);
        }
    }
    rows.push((String::new(), None));
    rows.push(("LAND / OUTSIDE ANALYSIS".into(), Some(LAND)));
    rows.push(("NO SCANNED REGION".into(), Some(VOID)));
    rows.push((String::new(), None));
    let accepted = analysis.candidates.iter().filter(|c| c.accepted).count();
    rows.push((format!("CANDIDATES: {}", analysis.candidates.len()), None));
    rows.push((format!("ACCEPTED: {accepted}"), None));
    rows.push((
        format!("REJECTED: {}", analysis.candidates.len() - accepted),
        None,
    ));
    rows.push((String::new(), None));
    rows.push(("DETAILS: LAKE_CANDIDATES.JSON".into(), None));
    if matches!(layer, Layer::Connections) {
        rows.push(("ALL CANDIDATE CONNECTION POINTS".into(), None));
        rows.push(("ENLARGED DEBUG MARKERS ONLY".into(), None));
        rows.push(("DIRECTION IS APPROXIMATE".into(), None));
    }
    rows
}

fn render(
    grid: &WorldGrid,
    analysis: &LakeAnalysis,
    projection: &Projection,
    layer: Layer,
    title: &str,
    values: &[u16],
) -> Canvas {
    let font_scale = if projection.h < 600 { 1 } else { 2 };
    let padding = 14 * font_scale;
    let line_height = 14 * font_scale;
    let rows = legend(analysis, layer, title, projection.scale);
    let panel_width = rows
        .iter()
        .map(|(text, _)| text.chars().count())
        .max()
        .unwrap_or(1)
        * 6
        * font_scale
        + padding * 2
        + 18 * font_scale;
    let mut canvas = Canvas::new(
        projection.w + panel_width,
        projection.h.max(rows.len() * line_height + padding * 2),
        VOID,
    );
    for region in &grid.regions {
        if let (Some((x0, z0)), Some((x1, z1))) = (
            projection.pixel(region.region_x * 512, region.region_z * 512),
            projection.pixel(region.region_x * 512 + 511, region.region_z * 512 + 511),
        ) {
            canvas.fill_rect(x0, z0, x1 - x0 + 1, z1 - z0 + 1, LAND);
        }
    }
    for (pixel, &value) in values.iter().enumerate() {
        if value != 0 {
            canvas.set(
                pixel % projection.w,
                pixel / projection.w,
                color(layer, value),
            );
        }
    }
    if matches!(layer, Layer::Connections) {
        for candidate in &analysis.candidates {
            for point in &candidate.connections {
                if let Some((x, z)) = projection.pixel(point.x, point.z) {
                    let rgb = match point.direction.as_str() {
                        "inflow" => INFLOW,
                        "outflow" => OUTFLOW,
                        _ => UNKNOWN,
                    };
                    // A clipped cross keeps a connection visible after map
                    // reduction. It belongs only to this diagnostic layer.
                    for dz in -3i32..=3 {
                        for dx in -3i32..=3 {
                            if dx != 0 && dz != 0 {
                                continue;
                            }
                            let (px, pz) = (x as i64 + i64::from(dx), z as i64 + i64::from(dz));
                            if px >= 0
                                && pz >= 0
                                && px < projection.w as i64
                                && pz < projection.h as i64
                            {
                                canvas.set(px as usize, pz as usize, rgb);
                            }
                        }
                    }
                }
            }
        }
    }
    let height = projection.h.max(rows.len() * line_height + padding * 2);
    canvas.fill_rect(projection.w, 0, panel_width, height, PANEL);
    for (row, (label, rgb)) in rows.iter().enumerate() {
        let x = projection.w + padding;
        let y = padding + row * line_height;
        if let Some(rgb) = rgb {
            canvas.fill_rect(x, y, 10 * font_scale, 7 * font_scale, *rgb);
            canvas.text(x + 16 * font_scale, y, font_scale, label, TEXT);
        } else {
            canvas.text(x, y, font_scale, label, if row < 2 { TEXT } else { DIM });
        }
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water::grid::RegionWater;
    use crate::water::lakes::{Connection, Raster};
    use crate::water::model::{
        Bathymetry, Modifiers, RegionGeometry, Run, Temperature, Vegetation, WaterKind, WaterRegion,
    };

    fn fixture() -> (WorldGrid, LakeAnalysis) {
        let region = WaterRegion {
            id: 0,
            geometry: RegionGeometry {
                min_x: -4,
                min_z: -1,
                max_x: 3,
                max_z: 0,
                runs: (-1..=0).map(|z| Run { z, x0: -4, x1: 3 }).collect(),
                column_count: 16,
            },
            kind: WaterKind::River,
            temperature: Temperature::Medium,
            vegetation: Vegetation::Normal,
            depth: None,
            modifiers: Modifiers::empty(),
            surface_y: 63,
            bathymetry: Bathymetry::default(),
            dominant_biome: "minecraft:river".into(),
        };
        let raster = Raster::from_regions(&[region]);
        let n = raster.len();
        let mut analysis = LakeAnalysis {
            closed_water: crate::water::lakes::ClosedWater { forced_lake: vec![false], ..Default::default() },
            source_kinds: vec![WaterKind::River],
            raster,
            distance: vec![0; n],
            density: vec![[0; 3]; n],
            cores: vec![0; n],
            reconstructed: vec![0; n],
            necks: vec![false; n],
            candidates: vec![
                LakeCandidate {
                    id: 0,
                    accepted: true,
                    accepts_river_water: true,
                    confidence: 0.9,
                    connections: vec![Connection {
                        x: -1,
                        z: -1,
                        width: 2.0,
                        width_ratio: 0.2,
                        confirmed_neck: true,
                        direction: "unknown".into(),
                        water_head_difference: None,
                    }],
                    ..Default::default()
                },
                LakeCandidate {
                    id: 1,
                    accepted: false,
                    ..Default::default()
                },
            ],
            options: LakeOptions::default(),
        };
        for i in 0..n {
            if analysis.raster.is_water(i) {
                analysis.distance[i] = 1;
                analysis.density[i] = [100, 50, 25];
            }
        }
        let accepted = analysis.raster.index_at(-1, -1).unwrap();
        analysis.cores[accepted] = 1;
        analysis.reconstructed[accepted] = 1;
        analysis.necks[accepted] = true;
        analysis.distance[accepted] = 20;
        let rejected = analysis.raster.index_at(0, -1).unwrap();
        analysis.cores[rejected] = 2;
        analysis.reconstructed[rejected] = 2;
        let grid = WorldGrid::build(vec![
            RegionWater::new(-1, -1),
            RegionWater::new(0, -1),
            RegionWater::new(-1, 0),
            RegionWater::new(0, 0),
        ]);
        (grid, analysis)
    }

    #[test]
    fn reduced_layers_keep_thin_features_and_distinguish_rejected_candidates() {
        let (_, analysis) = fixture();
        let projection = Projection::new((-4, -4, 3, 3), 4).unwrap();
        let mut values = vec![0; 4];
        reduce(&analysis, &projection, Layer::Cores, &mut values);
        assert_eq!(values, [3, 2, 1, 1]);
        reduce(&analysis, &projection, Layer::Width, &mut values);
        assert_eq!(
            values[0], 21,
            "maximum width remains visible after reduction"
        );
        reduce(&analysis, &projection, Layer::Reconstructed, &mut values);
        assert_eq!(values, [2, 1, 1, 1]);
        reduce(&analysis, &projection, Layer::Rejected, &mut values);
        assert_eq!(values, [1, 2, 1, 1]);
        reduce(&analysis, &projection, Layer::Necks, &mut values);
        assert_eq!(values, [2, 1, 1, 1]);
    }

    #[test]
    fn rejection_layer_shows_tiny_cores_that_were_never_reconstructed() {
        let (_, mut analysis) = fixture();
        let point = analysis.raster.index_at(-2, 0).unwrap();
        analysis.candidates.push(LakeCandidate {
            id: 2,
            accepted: false,
            rejection: Some("core_too_small".into()),
            core_area: 1,
            ..Default::default()
        });
        analysis.cores[point] = 3;
        assert_eq!(analysis.reconstructed[point], 0);
        let projection = Projection::new((-4, -4, 3, 3), 4).unwrap();
        let mut values = vec![0; 4];
        reduce(&analysis, &projection, Layer::Rejected, &mut values);
        assert_eq!(values[2], 2, "a rejected one-block core must survive display reduction");
        assert_eq!(color(Layer::Rejected, values[2]), REJECTED);
        reduce(&analysis, &projection, Layer::Reconstructed, &mut values);
        assert_eq!(values[2], 1, "showing a rejected core must not mark it as a lake");
    }

    #[test]
    fn diagnostic_export_writes_json_layers_and_legends() {
        let (grid, analysis) = fixture();
        let dir =
            std::env::temp_dir().join(format!("water-analyzer-lake-debug-{}", std::process::id()));
        write(&dir, &grid, &analysis, 8).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("lake_candidates.json")).unwrap())
                .unwrap();
        assert_eq!(json["analysis_blocks_per_cell"], 1);
        assert_eq!(json["render_blocks_per_pixel"], 8);
        assert_eq!(json["accepted_candidate_count"], 1);
        let data = std::fs::read(dir.join("lake_candidate_runs.bin")).unwrap();
        assert_eq!(&data[..8], b"LKRUNS01");
        assert_eq!((data.len() - 8) % 20, 0);
        let mut decoded = std::collections::BTreeMap::new();
        for record in data[8..].chunks_exact(20) {
            let source = u32::from_le_bytes(record[0..4].try_into().unwrap());
            let candidate = u32::from_le_bytes(record[4..8].try_into().unwrap());
            let z = i32::from_le_bytes(record[8..12].try_into().unwrap());
            let x0 = i32::from_le_bytes(record[12..16].try_into().unwrap());
            let x1 = i32::from_le_bytes(record[16..20].try_into().unwrap());
            assert!(x0 <= x1);
            for x in x0..=x1 {
                assert!(decoded.insert((x, z), (source, candidate)).is_none());
            }
        }
        assert_eq!(decoded.len(), 16, "export must exclude dry cells and retain all water");
        for z in -1..=0 {
            for x in -4..=3 {
                let candidate = match (x, z) { (-1, -1) => 1, (0, -1) => 2, _ => 0 };
                assert_eq!(decoded[&(x, z)], (0, candidate));
            }
        }
        assert_eq!(
            json["candidates"][0]["connections"][0]["direction"],
            "unknown"
        );
        for (layer, filename, _) in LAYERS {
            let file = std::fs::File::open(dir.join(filename)).unwrap();
            let mut reader = png::Decoder::new(file).read_info().unwrap();
            let mut pixels = vec![0; reader.output_buffer_size()];
            let info = reader.next_frame(&mut pixels).unwrap();
            assert!(info.width > 128, "legend must sit beside the world map");
            assert!(
                pixels.chunks_exact(3).any(|p| p == TEXT),
                "legend text must be drawn"
            );
            // World (-1,-1) projects to pixel (63,63) with the full negative
            // world bounds. It has a one-block core and connection, both of
            // which must survive an eight-block render pixel.
            let index = (63 * info.width as usize + 63) * 3;
            if matches!(layer, Layer::Cores) {
                assert_eq!(&pixels[index..index + 3], &CORE);
            }
            if matches!(layer, Layer::Connections) {
                assert_eq!(&pixels[index..index + 3], &UNKNOWN);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
