//! Synthetic water networks exercise geographic behavior independently of the
//! input biome labels. Fixtures start as rivers so lake acceptance is observable.

use std::collections::BTreeMap;

use super::{analyze, apply, LakeAnalysis, LakeOptions};
use crate::water::grid::{ChunkWater, RegionWater, WorldGrid};
use crate::water::model::{
    Bathymetry, Depth, Modifiers, RegionGeometry, Run, Temperature, Vegetation, WaterKind,
    WaterRegion,
};

fn shape(bounds: [i32; 4], water: impl Fn(i32, i32) -> bool) -> WaterRegion {
    let [x0, z0, x1, z1] = bounds;
    let mut runs = Vec::new();
    let mut count = 0;
    for z in z0..=z1 {
        let mut x = x0;
        while x <= x1 {
            if !water(x, z) {
                x += 1;
                continue;
            }
            let start = x;
            while x < x1 && water(x + 1, z) {
                x += 1;
            }
            runs.push(Run {
                z,
                x0: start,
                x1: x,
            });
            count += (x - start + 1) as u32;
            x += 1;
        }
    }
    WaterRegion {
        id: 42,
        geometry: RegionGeometry {
            min_x: runs.iter().map(|r| r.x0).min().unwrap(),
            min_z: runs.first().unwrap().z,
            max_x: runs.iter().map(|r| r.x1).max().unwrap(),
            max_z: runs.last().unwrap().z,
            runs,
            column_count: count,
        },
        kind: WaterKind::River,
        temperature: Temperature::Medium,
        vegetation: Vegetation::Normal,
        depth: Some(Depth::Normal),
        modifiers: Modifiers::empty(),
        surface_y: 62,
        bathymetry: Bathymetry::default(),
        dominant_biome: "minecraft:river".into(),
    }
}

fn rectangle(bounds: [i32; 4]) -> WaterRegion {
    shape(bounds, |_, _| true)
}

fn canonical_mask(regions: &[WaterRegion]) -> Vec<Run> {
    let mut runs: Vec<_> = regions
        .iter()
        .flat_map(|r| r.geometry.runs.iter().copied())
        .collect();
    runs.sort_unstable_by_key(|r| (r.z, r.x0));
    let mut merged: Vec<Run> = Vec::new();
    for run in runs {
        if let Some(previous) = merged.last_mut().filter(|p| p.z == run.z) {
            assert!(
                previous.x1 < run.x0,
                "a water column has more than one owner"
            );
            if previous.x1 + 1 == run.x0 {
                previous.x1 = run.x1;
                continue;
            }
        }
        merged.push(run);
    }
    merged
}

fn run_analysis(
    grid: &WorldGrid,
    regions: Vec<WaterRegion>,
    options: &LakeOptions,
) -> (LakeAnalysis, Vec<WaterRegion>) {
    options.validate().unwrap();
    let original_mask = canonical_mask(&regions);
    let original_area: u64 = regions.iter().map(|r| r.geometry.column_count as u64).sum();
    let analysis = analyze(grid, &regions, options);
    let mut output = regions;
    apply(&mut output, &analysis);
    assert_eq!(
        canonical_mask(&output),
        original_mask,
        "lake labeling changed the original shoreline"
    );
    assert_eq!(
        output
            .iter()
            .map(|r| r.geometry.column_count as u64)
            .sum::<u64>(),
        original_area
    );
    for region in &output {
        assert_eq!(
            region.geometry.runs.iter().map(Run::len).sum::<u32>(),
            region.geometry.column_count
        );
    }
    (analysis, output)
}

fn detect(region: WaterRegion) -> (LakeAnalysis, Vec<WaterRegion>) {
    run_analysis(
        &WorldGrid::build(Vec::new()),
        vec![region],
        &LakeOptions::default(),
    )
}

fn kind_at(regions: &[WaterRegion], x: i32, z: i32) -> Option<WaterKind> {
    regions
        .iter()
        .find(|r| r.geometry.contains(x, z))
        .map(|r| r.kind)
}

fn assert_kind(regions: &[WaterRegion], points: &[(i32, i32)], kind: WaterKind) {
    for &(x, z) in points {
        assert_eq!(
            kind_at(regions, x, z),
            Some(kind),
            "unexpected kind at ({x}, {z})"
        );
    }
}

/// Cell heights deliberately differ from the input region's flat fallback Y.
fn water_height_grid(region: &WaterRegion, height: impl Fn(i32, i32) -> i16) -> WorldGrid {
    let mut chunks: BTreeMap<(i32, i32), ChunkWater> = BTreeMap::new();
    for run in &region.geometry.runs {
        for x in run.x0..=run.x1 {
            let chunk = chunks.entry((x >> 4, run.z >> 4)).or_default();
            let (lx, lz) = ((x & 15) as usize, (run.z & 15) as usize);
            chunk.set(lx, lz);
            chunk.water_cols += 1;
            let cell = &mut chunk.cells[(lz >> 2) * 4 + (lx >> 2)];
            cell.water_cols += 1;
            cell.surface_y = height((x & !3) + 1, (run.z & !3) + 1);
            cell.depth = 12;
        }
    }
    let mut regions: BTreeMap<(i32, i32), RegionWater> = BTreeMap::new();
    for ((cx, cz), chunk) in chunks {
        let region = regions
            .entry((cx >> 5, cz >> 5))
            .or_insert_with(|| RegionWater::new(cx >> 5, cz >> 5));
        region.insert(((cz & 31) * 32 + (cx & 31)) as usize, chunk);
    }
    WorldGrid::build(regions.into_values().collect())
}

fn terrain_grid(bounds: [i32; 4], height: i16) -> WorldGrid {
    let [x0, z0, x1, z1] = bounds;
    let mut regions: BTreeMap<(i32, i32), RegionWater> = BTreeMap::new();
    for cz in (z0 >> 4)..=(z1 >> 4) {
        for cx in (x0 >> 4)..=(x1 >> 4) {
            let region = regions
                .entry((cx >> 5, cz >> 5))
                .or_insert_with(|| RegionWater::new(cx >> 5, cz >> 5));
            region.set_terrain(((cz & 31) * 32 + (cx & 31)) as usize, [height; 16]);
        }
    }
    WorldGrid::build(regions.into_values().collect())
}

#[test]
fn straight_narrow_river_remains_river_despite_large_total_area() {
    let (analysis, output) = detect(rectangle([-300, -3, 300, 4]));
    assert!(output.iter().all(|r| r.kind == WaterKind::River));
    assert!(analysis.candidates.iter().all(|c| !c.accepted));
}

#[test]
fn large_isolated_lake_is_detected_from_a_river_label() {
    let (analysis, output) = detect(shape([-60, -60, 60, 60], |x, z| x * x + z * z <= 60 * 60));
    assert_eq!(analysis.candidates.iter().filter(|c| c.accepted).count(), 1);
    assert!(output.iter().all(|r| r.kind == WaterKind::Lake));
}

#[test]
fn river_lake_river_separates_the_widened_body_from_channels() {
    let (analysis, output) = detect(shape([-240, -60, 240, 60], |x, z| {
        x * x + z * z <= 60 * 60 || (-3..=4).contains(&z)
    }));
    assert_kind(
        &output,
        &[(0, 0), (0, 50), (-45, 0), (45, 0)],
        WaterKind::Lake,
    );
    assert_kind(
        &output,
        &[(-160, 0), (160, 0), (-95, 0), (95, 0)],
        WaterKind::River,
    );
    let lake = analysis.candidates.iter().find(|c| c.accepted).unwrap();
    assert_eq!(lake.connections.len(), 2);
    assert!(lake.connections.iter().all(|c| c.confirmed_neck));
    assert_eq!(
        lake.unknown_connection_count, 2,
        "flat water must not fabricate flow directions"
    );
}

#[test]
fn diagonal_channels_are_cut_at_the_lake_mouth_not_at_a_medial_step() {
    let (_, output) = detect(shape([-240, -240, 240, 240], |x, z| {
        x * x + z * z <= 60 * 60 || (x - z).abs() <= 4
    }));
    assert_kind(
        &output,
        &[
            (0, 0),
            (35, 35),
            (-35, -35),
            (42, 42),
            (-42, -42),
            (50, 32),
            (-50, -32),
        ],
        WaterKind::Lake,
    );
    assert_kind(
        &output,
        &[(55, 55), (-55, -55), (95, 95), (-95, -95)],
        WaterKind::River,
    );
}

#[test]
fn irregular_bays_islands_and_peninsulas_keep_their_exact_shoreline() {
    let (analysis, output) = detect(shape([-90, -78, 80, 48], |x, z| {
        let body = (-64..=64).contains(&x) && (-48..=48).contains(&z);
        let west_bay = (-90..=-65).contains(&x) && (-18..=18).contains(&z);
        let north_bay = (-25..=20).contains(&x) && (-78..=-49).contains(&z);
        let east_bay = (65..=80).contains(&x) && (5..=32).contains(&z);
        let island = (-8..=8).contains(&x) && (-7..=7).contains(&z);
        let peninsula = x >= 25 && z <= -20;
        (body || west_bay || north_bay || east_bay) && !island && !peninsula
    }));
    assert!(analysis.candidates.iter().any(|c| c.accepted));
    assert!(
        output.iter().all(|r| r.kind == WaterKind::Lake),
        "short bays must be reconstructed as part of the lake"
    );
    assert_kind(
        &output,
        &[(-90, 0), (0, -78), (80, 20), (-64, 48)],
        WaterKind::Lake,
    );
    assert_eq!(kind_at(&output, 0, 0), None, "island was filled");
    assert_eq!(kind_at(&output, 40, -30), None, "peninsula was filled");
}

#[test]
fn elongated_lake_with_genuine_widening_is_allowed() {
    let (_, output) = detect(shape([-320, -28, 320, 28], |x, z| {
        i64::from(x).pow(2) * 28 * 28 + i64::from(z).pow(2) * 160 * 160 <= 160 * 160 * 28 * 28
            || (-2..=3).contains(&z)
    }));
    assert_kind(
        &output,
        &[(0, 0), (-110, 0), (110, 0), (0, 25)],
        WaterKind::Lake,
    );
    assert_kind(&output, &[(-260, 0), (260, 0)], WaterKind::River);
}

#[test]
fn short_channel_between_two_lakes_remains_river() {
    let (analysis, output) = detect(shape([-145, -60, 145, 60], |x, z| {
        (x + 85).pow(2) + z * z <= 60 * 60
            || (x - 85).pow(2) + z * z <= 60 * 60
            || ((-85..=85).contains(&x) && (-3..=4).contains(&z))
    }));
    assert_eq!(analysis.candidates.iter().filter(|c| c.accepted).count(), 2);
    assert_kind(
        &output,
        &[(-85, 0), (85, 0), (-35, 0), (35, 0)],
        WaterKind::Lake,
    );
    assert_kind(&output, &[(-15, 0), (0, 0), (15, 0)], WaterKind::River);
    assert!(analysis.candidates.iter().all(|c| c.connections.len() == 1));
}

#[test]
fn uniform_wide_river_is_not_a_lake_just_because_it_has_a_core() {
    let (analysis, output) = detect(rectangle([-800, -31, 800, 32]));
    assert!(
        analysis.candidates.iter().any(|c| c.core_area > 64),
        "fixture must exercise a wide channel with a real core"
    );
    assert!(output.iter().all(|r| r.kind == WaterKind::River));
    assert!(analysis.candidates.iter().all(|c| !c.accepted));
}

#[test]
fn a_small_tributary_does_not_make_a_uniform_wide_river_a_lake() {
    let (_, output) = detect(shape([-800, -220, 800, 32], |x, z| {
        (-31..=32).contains(&z) || (-3..=4).contains(&x)
    }));
    assert!(output.iter().all(|r| r.kind == WaterKind::River));
}

#[test]
fn tiny_cut_fragments_rejoin_one_basin_but_short_interlake_channels_survive() {
    let raster = super::Raster::from_regions(&[rectangle([-5, -1, 5, 1])]);
    let mut labels = vec![0; raster.len()];
    for x in -5..=5 {
        for z in -1..=1 {
            labels[raster.index_at(x, z).unwrap()] = if x == 0 { 0 } else { 1 };
        }
    }
    let grid = WorldGrid::build(Vec::new());
    let mut two_basins = labels.clone();
    for x in 1..=5 {
        for z in -1..=1 {
            two_basins[raster.index_at(x, z).unwrap()] = 2;
        }
    }
    super::detection::recover_bank_fragments(&grid, &raster, &mut labels, 64);
    super::detection::recover_bank_fragments(&grid, &raster, &mut two_basins, 64);
    for z in -1..=1 {
        let i = raster.index_at(0, z).unwrap();
        assert_eq!(labels[i], 1, "single-basin cut debris should recover");
        assert_eq!(
            two_basins[i], 0,
            "even a tiny real connection must remain river"
        );
        assert!(raster.is_water(i));
    }
}

#[test]
fn several_raised_inflows_and_one_lower_outlet_have_high_plausibility() {
    let region = shape([-220, -220, 220, 220], |x, z| {
        (x.abs() <= 64 && z.abs() <= 64) || (-4..=3).contains(&x) || (-4..=3).contains(&z)
    });
    let grid = water_height_grid(&region, |x, z| {
        let rise = ((x.abs().max(z.abs()) - 64).max(0) / 8) as i16;
        if z > 64 {
            62 - rise
        } else {
            62 + rise
        }
    });
    let (analysis, output) = run_analysis(&grid, vec![region], &LakeOptions::default());
    assert_kind(&output, &[(0, 0)], WaterKind::Lake);
    assert_kind(
        &output,
        &[(-180, 0), (180, 0), (0, -180), (0, 180)],
        WaterKind::River,
    );
    let lake = analysis.candidates.iter().find(|c| c.accepted).unwrap();
    assert_eq!(lake.inflow_count, 3, "{:#?}", lake.connections);
    assert_eq!(lake.outflow_count, 1, "{:#?}", lake.connections);
    assert_eq!(lake.unknown_connection_count, 0);
    assert!(lake.confidence >= 0.75, "confidence {}", lake.confidence);
    assert_eq!(lake.flow_score, 1.0);
}

#[test]
fn abrupt_channel_height_steps_are_compared_with_the_lake_side() {
    let mut regions = vec![
        rectangle([-64, -64, 64, 64]),
        rectangle([-220, -4, -65, 3]),
        rectangle([65, -4, 220, 3]),
        rectangle([-4, -220, 3, -65]),
        rectangle([-4, 65, 3, 220]),
    ];
    for (region, height) in regions.iter_mut().zip([62, 66, 66, 66, 58]) {
        region.surface_y = height;
    }
    let (analysis, output) = run_analysis(
        &WorldGrid::build(Vec::new()),
        regions,
        &LakeOptions::default(),
    );
    assert_kind(&output, &[(0, 0)], WaterKind::Lake);
    assert_kind(
        &output,
        &[(-100, 0), (100, 0), (0, -100), (0, 100)],
        WaterKind::River,
    );
    let lake = analysis.candidates.iter().find(|c| c.accepted).unwrap();
    assert_eq!(lake.inflow_count, 3, "{:#?}", lake.connections);
    assert_eq!(lake.outflow_count, 1, "{:#?}", lake.connections);
    assert_eq!(lake.unknown_connection_count, 0);
    for connection in &lake.connections {
        let head = connection.water_head_difference.unwrap();
        let expected = if connection.direction == "inflow" {
            4.0
        } else {
            -4.0
        };
        assert!(
            (head - expected).abs() < 0.1,
            "unexpected head {head}: {connection:?}"
        );
    }
}

#[test]
fn closed_lake_with_high_banks_and_no_outlet_is_valid() {
    let lake = shape([-60, -50, 60, 50], |x, z| {
        x * x * 50 * 50 + z * z * 60 * 60 <= 60 * 60 * 50 * 50
    });
    let grid = terrain_grid([-65, -55, 65, 55], 70);
    let (analysis, output) = run_analysis(&grid, vec![lake], &LakeOptions::default());
    assert!(output.iter().all(|r| r.kind == WaterKind::Lake));
    let candidate = analysis.candidates.iter().find(|c| c.accepted).unwrap();
    assert!(candidate.connections.is_empty());
    assert_eq!(candidate.inflow_count, 0);
    assert_eq!(candidate.outflow_count, 0);
    assert_eq!(candidate.terrain_sample_coverage, 1.0);
    assert_eq!(candidate.terrain_basin_score, 1.0);
    assert!(candidate.confidence > 0.75);
}

#[test]
fn many_narrow_branches_remain_rivers_around_a_lake_body() {
    let (analysis, output) = detect(shape([-250, -250, 250, 250], |x, z| {
        let band = |v| (-36..=-31).contains(&v) || (31..=36).contains(&v);
        (x.abs() <= 64 && z.abs() <= 64) || (x.abs() > 64 && band(z)) || (z.abs() > 64 && band(x))
    }));
    assert_kind(&output, &[(0, 0), (-50, -50), (50, 50)], WaterKind::Lake);
    assert_kind(
        &output,
        &[
            (-200, -33),
            (-200, 33),
            (200, -33),
            (200, 33),
            (-33, -200),
            (33, -200),
            (-33, 200),
            (33, 200),
        ],
        WaterKind::River,
    );
    let lake = analysis.candidates.iter().find(|c| c.accepted).unwrap();
    assert_eq!(lake.connections.len(), 8);
    assert!(lake.connections.iter().all(|c| c.confirmed_neck));
}

#[test]
fn tiny_pool_requires_explicit_small_lake_configuration() {
    let pool = rectangle([0, 0, 7, 7]);
    let (_, normal) = detect(pool.clone());
    assert!(normal.iter().all(|r| r.kind == WaterKind::River));
    let options = LakeOptions {
        core_radius: 2,
        density_8: 0.15,
        density_16: 0.05,
        density_32: 0.01,
        min_core_area: 4,
        min_lake_area: 32,
        ..LakeOptions::default()
    };
    let (_, configured) = run_analysis(&WorldGrid::build(Vec::new()), vec![pool], &options);
    assert!(configured.iter().all(|r| r.kind == WaterKind::Lake));
}

#[test]
fn lake_application_preserves_protected_regions_and_all_water_attributes() {
    let lake = rectangle([-60, -60, 60, 60]);
    let mut sea = rectangle([200, 0, 230, 30]);
    sea.kind = WaterKind::Sea;
    sea.temperature = Temperature::Warm;
    sea.depth = Some(Depth::Deep);
    sea.modifiers = Modifiers::CORALS;
    sea.bathymetry = Bathymetry {
        mean_depth: 37,
        max_depth: 61,
        contour_shares: [255, 250, 200, 100],
    };
    let mut swamp = rectangle([240, 0, 270, 30]);
    swamp.kind = WaterKind::Swamp;
    swamp.modifiers = Modifiers::MANGROVE;
    let mut cave = rectangle([280, 0, 310, 30]);
    cave.kind = WaterKind::Lake;
    cave.modifiers = Modifiers::CAVE | Modifiers::ICE;
    let protected = [sea.clone(), swamp.clone(), cave.clone()];
    let (_, output) = run_analysis(
        &WorldGrid::build(Vec::new()),
        vec![lake, sea, swamp, cave],
        &LakeOptions::default(),
    );
    for mut before in protected {
        let mut after = output
            .iter()
            .find(|r| r.geometry.min_x == before.geometry.min_x)
            .unwrap()
            .clone();
        // IDs are regenerated by the established output pipeline.
        before.id = 0;
        after.id = 0;
        assert_eq!(format!("{before:?}"), format!("{after:?}"));
    }
    let inland = output.iter().find(|r| r.geometry.contains(0, 0)).unwrap();
    assert_eq!(inland.kind, WaterKind::Lake);
    assert_eq!(inland.surface_y, 62);
    assert_eq!(inland.temperature, Temperature::Medium);
    assert_eq!(inland.depth, Some(Depth::Normal));
    assert_eq!(inland.dominant_biome, "minecraft:river");
}

#[test]
fn terrain_changes_confidence_without_rejecting_an_unusual_closed_basin() {
    let lake = rectangle([-50, -50, 50, 50]);
    let high = terrain_grid([-55, -55, 55, 55], 70);
    let low = terrain_grid([-55, -55, 55, 55], 61);
    let (high_analysis, _) = run_analysis(&high, vec![lake.clone()], &LakeOptions::default());
    let (low_analysis, _) = run_analysis(&low, vec![lake], &LakeOptions::default());
    let high_lake = high_analysis
        .candidates
        .iter()
        .find(|c| c.accepted)
        .unwrap();
    let low_lake = low_analysis.candidates.iter().find(|c| c.accepted).unwrap();
    assert_eq!(high_lake.terrain_basin_score, 1.0);
    assert_eq!(low_lake.terrain_basin_score, 0.0);
    assert!(high_lake.confidence > low_lake.confidence);
}

#[test]
fn lake_configuration_supports_partial_overrides_and_rejects_invalid_thresholds() {
    let defaults = LakeOptions::default();
    let documented: LakeOptions =
        serde_json::from_str(include_str!("../../../docs/lake-defaults.json")).unwrap();
    assert_eq!(
        serde_json::to_value(defaults.clone()).unwrap(),
        serde_json::to_value(documented).unwrap()
    );
    let partial: LakeOptions = serde_json::from_str(r#"{"min_lake_area":500}"#).unwrap();
    assert_eq!(partial.min_lake_area, 500);
    assert_eq!(partial.core_radius, defaults.core_radius);
    assert!(partial.validate().is_ok());
    assert!(serde_json::from_str::<LakeOptions>(r#"{"density_33":0.8}"#).is_err());
    for json in [
        r#"{"density_32":1.1}"#,
        r#"{"core_radius":0}"#,
        r#"{"neck_width_ratio":1.0}"#,
        r#"{"connection_sample_distance":257}"#,
        r#"{"min_core_area":0}"#,
    ] {
        let options: LakeOptions = serde_json::from_str(json).unwrap();
        assert!(
            options.validate().is_err(),
            "accepted invalid options: {json}"
        );
    }
}

/// Manual visual QA: cargo test --offline export_synthetic_lake_diagnostics -- --ignored
#[test]
#[ignore = "writes full-resolution lake-stage PNGs for manual visual inspection"]
fn export_synthetic_lake_diagnostics() {
    let round = shape([-240, -60, 240, 60], |x, z| {
        x * x + z * z <= 60 * 60 || (-3..=4).contains(&z)
    });
    let elongated = shape([-320, -28, 320, 28], |x, z| {
        i64::from(x).pow(2) * 28 * 28 + i64::from(z).pow(2) * 160 * 160 <= 160 * 160 * 28 * 28
            || (-2..=3).contains(&z)
    });
    let diagonal = shape([-120, -120, 120, 120], |x, z| {
        x * x + z * z <= 60 * 60 || (x - z).abs() <= 4
    });
    let linked = shape([-145, -60, 145, 60], |x, z| {
        (x + 85).pow(2) + z * z <= 60 * 60
            || (x - 85).pow(2) + z * z <= 60 * 60
            || ((-85..=85).contains(&x) && (-3..=4).contains(&z))
    });
    let grid = WorldGrid::build(Vec::new());
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("generated-lake-synthetic");
    for (name, region) in [
        ("round", round),
        ("elongated", elongated),
        ("diagonal", diagonal),
        ("linked", linked),
    ] {
        let (analysis, _) = run_analysis(&grid, vec![region], &LakeOptions::default());
        assert!(analysis
            .candidates
            .iter()
            .any(|candidate| candidate.accepted));
        crate::debug::lakes::write(&directory.join(name), &grid, &analysis, 1).unwrap();
    }
}
