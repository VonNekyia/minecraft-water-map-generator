//! A bounded fallback for small retained bodies with no water route to the sea.
//!
//! Input regions are connected components produced by the existing region CCL
//! and adjacent-only merges. Joining their exact RLE contacts therefore gives
//! physical water bodies without allocating another per-column label grid.
//! Lake, River and Swamp all carry connectivity to Sea. Cave regions are omitted
//! because a projected x/z contact need not connect underground and surface water.

use serde::Serialize;

use crate::water::absorb;
use crate::water::components::UnionFind;
use crate::water::model::{Modifiers, WaterKind, WaterRegion};

use super::LakeOptions;

#[derive(Clone, Debug, Serialize)]
pub struct ClosedComponent {
    /// Dense component id, independent of the externally assigned region ids.
    pub id: u32,
    pub area: u64,
    /// Inclusive world bounds: min_x, min_z, max_x, max_z.
    pub bounds: [i32; 4],
    pub region_count: u32,
    pub inland_area: u64,
    pub river_area: u64,
    pub reaches_sea: bool,
    /// Fraction of the smaller axis-aligned/PCA box occupied by the whole body.
    pub fill_ratio: f32,
    /// Long / short side of the PCA box, including complete water blocks.
    pub elongation: f32,
    /// Whole-body area / long side of the PCA box, in blocks.
    pub mean_width: f32,
    /// Whether this body's River/Lake members receive the bounded lake override.
    pub forced_lake: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ClosedWater {
    /// One flag per source slice index, never indexed by WaterRegion::id.
    /// Sea, Swamp and cave entries are always false.
    pub forced_lake: Vec<bool>,
    /// Dense component id per source slice index. None for cave/empty regions,
    /// or for every entry when the override is disabled.
    pub component_by_region: Vec<Option<u32>>,
    pub components: Vec<ClosedComponent>,
    pub forced_component_count: u32,
    pub forced_region_count: u32,
    /// River + Lake area covered by the override, including already-lake water.
    pub forced_area: u64,
    /// Of forced_area, the part currently classified as River.
    pub reclassified_river_area: u64,
}

/// Mark small closed pools, plus larger closed bodies with a lake-like footprint.
/// Swamp area counts toward the complete body's size/shape but its label remains
/// protected. No water geometry or source region is changed by this analysis.
pub fn classify(regions: &[WaterRegion], options: &LakeOptions) -> ClosedWater {
    let min_area = options.min_lake_area;
    let max_area = options.max_closed_lake_area;
    let mut result = ClosedWater {
        forced_lake: vec![false; regions.len()],
        component_by_region: vec![None; regions.len()],
        ..ClosedWater::default()
    };
    if max_area == 0 || min_area > max_area {
        return result;
    }

    let participates = |r: &WaterRegion| {
        !r.modifiers.contains(Modifiers::CAVE)
            && r.geometry.column_count > 0
            && !r.geometry.runs.is_empty()
    };
    let mut runs: Vec<_> = regions
        .iter()
        .enumerate()
        .filter(|(_, r)| participates(r))
        .flat_map(|(i, r)| r.geometry.runs.iter().map(move |run| (i as u32, *run)))
        .collect();
    runs.sort_unstable_by_key(|(_, run)| (run.z, run.x0));
    let mut union = UnionFind::new(regions.len());
    for (a, b, _) in absorb::adjacency(&runs) {
        union.union(a, b);
    }
    drop(runs);

    let mut dense = vec![None; regions.len()];
    for (i, region) in regions.iter().enumerate() {
        if !participates(region) {
            continue;
        }
        let root = union.find(i as u32) as usize;
        let id = *dense[root].get_or_insert_with(|| {
            let id = result.components.len() as u32;
            result.components.push(ClosedComponent {
                id,
                area: 0,
                bounds: [i32::MAX, i32::MAX, i32::MIN, i32::MIN],
                region_count: 0,
                inland_area: 0,
                river_area: 0,
                reaches_sea: false,
                fill_ratio: 0.0,
                elongation: 0.0,
                mean_width: 0.0,
                forced_lake: false,
            });
            id
        });
        result.component_by_region[i] = Some(id);
        let component = &mut result.components[id as usize];
        let geometry = &region.geometry;
        let area = geometry.column_count as u64;
        component.area += area;
        component.bounds[0] = component.bounds[0].min(geometry.min_x);
        component.bounds[1] = component.bounds[1].min(geometry.min_z);
        component.bounds[2] = component.bounds[2].max(geometry.max_x);
        component.bounds[3] = component.bounds[3].max(geometry.max_z);
        component.region_count += 1;
        component.reaches_sea |= region.kind == WaterKind::Sea;
        if matches!(region.kind, WaterKind::River | WaterKind::Lake) {
            component.inland_area += area;
        }
        if region.kind == WaterKind::River {
            component.river_area += area;
        }
    }

    measure_shapes(regions, &mut result);
    for component in &mut result.components {
        let small_pool = component.area <= options.max_small_closed_lake_area as u64;
        let lake_shape = component.fill_ratio >= options.min_closed_lake_fill
            && component.elongation <= options.max_closed_lake_elongation
            && component.mean_width >= options.min_closed_lake_mean_width;
        component.forced_lake = !component.reaches_sea
            && component.inland_area > 0
            && (min_area as u64..=max_area as u64).contains(&component.area)
            && (small_pool || lake_shape);
        if component.forced_lake {
            result.forced_component_count += 1;
            result.forced_area += component.inland_area;
            result.reclassified_river_area += component.river_area;
        }
    }
    for (i, region) in regions.iter().enumerate() {
        if !matches!(region.kind, WaterKind::River | WaterKind::Lake) {
            continue;
        }
        if let Some(id) = result.component_by_region[i] {
            result.forced_lake[i] = result.components[id as usize].forced_lake;
            result.forced_region_count += u32::from(result.forced_lake[i]);
        }
    }
    result
}

/// Accumulate exact block-center moments analytically over runs, then project
/// their endpoints onto the principal axes. This uses O(bodies) additional
/// memory and O(runs) work, rather than visiting each water block individually.
/// Anchoring at each body's bounds avoids large-world-coordinate cancellation.
fn measure_shapes(regions: &[WaterRegion], result: &mut ClosedWater) {
    let n = result.components.len();
    let mut moments = vec![[0.0_f64; 5]; n];
    for (source, region) in regions.iter().enumerate() {
        let Some(id) = result.component_by_region[source] else {
            continue;
        };
        let c = &result.components[id as usize];
        if c.reaches_sea || c.area == 0 {
            continue;
        }
        let sums = &mut moments[id as usize];
        for run in &region.geometry.runs {
            let x0 = (run.x0 as i64 - c.bounds[0] as i64) as f64;
            let x1 = (run.x1 as i64 - c.bounds[0] as i64) as f64;
            let z = (run.z as i64 - c.bounds[1] as i64) as f64;
            let length = x1 - x0 + 1.0;
            let center_x = (x0 + x1) * 0.5;
            sums[0] += length * center_x;
            sums[1] += length * z;
            sums[2] += length * (center_x * center_x + (length * length - 1.0) / 12.0);
            sums[3] += length * z * z;
            sums[4] += length * center_x * z;
        }
    }
    let mut axes = vec![(1.0_f64, 0.0_f64); n];
    for (i, c) in result.components.iter().enumerate() {
        if c.reaches_sea || c.area == 0 {
            continue;
        }
        let area = c.area as f64;
        let m = moments[i];
        let mx = m[0] / area;
        let mz = m[1] / area;
        let vx = m[2] / area - mx * mx;
        let vz = m[3] / area - mz * mz;
        let covariance = m[4] / area - mx * mz;
        let angle = 0.5 * (2.0 * covariance).atan2(vx - vz);
        axes[i] = (angle.cos(), angle.sin());
    }
    let mut low = vec![[f64::INFINITY; 2]; n];
    let mut high = vec![[f64::NEG_INFINITY; 2]; n];
    for (source, region) in regions.iter().enumerate() {
        let Some(id) = result.component_by_region[source] else {
            continue;
        };
        let i = id as usize;
        let c = &result.components[i];
        if c.reaches_sea || c.area == 0 {
            continue;
        }
        let (cosine, sine) = axes[i];
        for run in &region.geometry.runs {
            let z = (run.z as i64 - c.bounds[1] as i64) as f64;
            for x in [run.x0, run.x1] {
                let x = (x as i64 - c.bounds[0] as i64) as f64;
                let projected = [cosine * x + sine * z, -sine * x + cosine * z];
                for axis in 0..2 {
                    low[i][axis] = low[i][axis].min(projected[axis]);
                    high[i][axis] = high[i][axis].max(projected[axis]);
                }
            }
        }
    }
    for (i, c) in result.components.iter_mut().enumerate() {
        if c.reaches_sea || c.area == 0 {
            continue;
        }
        let (cosine, sine) = axes[i];
        let block_span = cosine.abs() + sine.abs();
        let u = high[i][0] - low[i][0] + block_span;
        let v = high[i][1] - low[i][1] + block_span;
        let axis_width = (c.bounds[2] as i64 - c.bounds[0] as i64 + 1) as f64;
        let axis_height = (c.bounds[3] as i64 - c.bounds[1] as i64 + 1) as f64;
        let bounding_area = (axis_width * axis_height).min(u * v);
        c.fill_ratio = (c.area as f64 / bounding_area).clamp(0.0, 1.0) as f32;
        c.elongation = (u.max(v) / u.min(v)) as f32;
        c.mean_width = (c.area as f64 / u.max(v)) as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water::model::{Bathymetry, Depth, RegionGeometry, Run, Temperature, Vegetation};

    // Keep the existing topology/area tests focused on those invariants. New
    // tests below exercise the actual shape defaults and small-pool exception.
    fn classify(regions: &[WaterRegion], min_area: u32, max_area: u32) -> ClosedWater {
        super::classify(
            regions,
            &LakeOptions {
                min_lake_area: min_area,
                max_closed_lake_area: max_area,
                max_small_closed_lake_area: max_area,
                min_closed_lake_fill: 0.0,
                max_closed_lake_elongation: f32::MAX,
                min_closed_lake_mean_width: 0.0,
                ..LakeOptions::default()
            },
        )
    }

    fn rect(kind: WaterKind, x: i32, z: i32, w: i32, h: i32) -> WaterRegion {
        WaterRegion {
            // Deliberately duplicated and unrelated to input slice indices.
            id: 999,
            kind,
            temperature: Temperature::Medium,
            vegetation: Vegetation::Normal,
            depth: Some(Depth::Shallow),
            modifiers: Modifiers::empty(),
            surface_y: 62,
            bathymetry: Bathymetry::default(),
            dominant_biome: String::new(),
            geometry: RegionGeometry {
                min_x: x,
                min_z: z,
                max_x: x + w - 1,
                max_z: z + h - 1,
                column_count: (w as u64 * h as u64) as u32,
                runs: (z..z + h)
                    .map(|z| Run {
                        z,
                        x0: x,
                        x1: x + w - 1,
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn small_narrow_isolated_body_becomes_lake_without_a_wide_core() {
        let region = rect(WaterKind::River, -10, -20, 2, 50);
        let original = region.clone();
        let result = classify(std::slice::from_ref(&region), 100, 100);
        assert_eq!(result.forced_lake, [true]);
        assert_eq!(result.forced_component_count, 1);
        assert_eq!(result.forced_region_count, 1);
        assert_eq!(result.forced_area, 100);
        assert_eq!(result.reclassified_river_area, 100);
        assert_eq!(result.components[0].bounds, [-10, -20, -9, 29]);
        assert_eq!(region.kind, original.kind);
        assert_eq!(region.geometry.runs, original.geometry.runs);
    }

    #[test]
    fn sea_reachability_propagates_through_lakes_and_swamps() {
        let regions = [
            rect(WaterKind::River, 0, 0, 3, 3),
            rect(WaterKind::Lake, 3, 0, 3, 3),
            rect(WaterKind::Swamp, 6, 0, 3, 3),
            rect(WaterKind::Sea, 9, 0, 3, 3),
        ];
        let result = classify(&regions, 1, 100);
        assert_eq!(result.forced_lake, [false; 4]);
        assert_eq!(result.component_by_region, [Some(0); 4]);
        assert_eq!(result.components.len(), 1);
        assert!(result.components[0].reaches_sea);
        assert_eq!(result.components[0].area, 36);
    }

    #[test]
    fn dry_gaps_and_diagonal_only_contacts_do_not_reach_sea() {
        let regions = [
            rect(WaterKind::Sea, 0, 0, 4, 4),
            rect(WaterKind::River, 5, 0, 3, 3),
            rect(WaterKind::River, 4, 4, 4, 4),
        ];
        let result = classify(&regions, 1, 100);
        assert_eq!(result.forced_lake, [false, true, true]);
        assert_eq!(result.components.len(), 3);
    }

    #[test]
    fn temperature_and_ice_seams_use_the_whole_body_area() {
        let first = rect(WaterKind::River, 0, 0, 10, 10);
        let mut second = rect(WaterKind::Lake, 0, 10, 10, 10);
        second.id = 42;
        second.temperature = Temperature::Cold;
        second.modifiers = Modifiers::ICE;
        let regions = [first, second];
        assert_eq!(classify(&regions, 1, 199).forced_lake, [false, false]);
        let result = classify(&regions, 200, 200);
        assert_eq!(result.forced_lake, [true, true]);
        assert_eq!(result.forced_area, 200);
        assert_eq!(result.reclassified_river_area, 100);
        assert_eq!(result.components[0].region_count, 2);
    }

    #[test]
    fn area_limits_are_inclusive_and_large_closed_networks_are_unchanged() {
        let regions = [
            rect(WaterKind::River, 0, 0, 99, 1),
            rect(WaterKind::River, 0, 3, 100, 1),
            rect(WaterKind::River, 0, 6, 200, 1),
            rect(WaterKind::River, 0, 9, 201, 1),
            rect(WaterKind::River, 0, 12, 10000, 1),
        ];
        assert_eq!(
            classify(&regions, 100, 200).forced_lake,
            [false, true, true, false, false]
        );
    }

    #[test]
    fn swamp_area_counts_without_changing_swamp_or_sea_labels() {
        let regions = [
            rect(WaterKind::River, 0, 0, 9, 10),
            rect(WaterKind::Swamp, 9, 0, 2, 10),
            rect(WaterKind::Swamp, 20, 0, 11, 10),
            rect(WaterKind::Sea, 40, 0, 11, 10),
        ];
        assert_eq!(classify(&regions, 1, 100).forced_lake, [false; 4]);
        let result = classify(&regions, 110, 110);
        assert_eq!(result.forced_lake, [true, false, false, false]);
        assert_eq!(result.forced_component_count, 1);
        assert_eq!(result.forced_area, 90);
        assert_eq!(result.components[0].area, 110);
        assert_eq!(result.components[0].inland_area, 90);
    }

    #[test]
    fn cave_regions_are_protected_and_do_not_bridge_projected_surface_bodies() {
        let mut cave = rect(WaterKind::Lake, 3, 0, 3, 3);
        cave.modifiers = Modifiers::CAVE;
        let regions = [
            rect(WaterKind::River, 0, 0, 3, 3),
            cave,
            rect(WaterKind::Sea, 6, 0, 3, 3),
        ];
        let result = classify(&regions, 1, 100);
        assert_eq!(result.forced_lake, [true, false, false]);
        assert_eq!(result.component_by_region, [Some(0), None, Some(1)]);
        assert_eq!(result.components.len(), 2);
    }

    #[test]
    fn zero_disables_and_inverted_ranges_match_nothing() {
        let regions = [rect(WaterKind::River, 0, 0, 10, 10)];
        for result in [classify(&regions, 1, 0), classify(&regions, 101, 100)] {
            assert_eq!(result.forced_lake, [false]);
            assert_eq!(result.component_by_region, [None]);
            assert!(result.components.is_empty());
            assert_eq!(result.forced_area, 0);
        }
        assert!(classify(&[], 1, 100).components.is_empty());
    }

    #[test]
    fn physical_body_areas_do_not_overflow_u32() {
        let regions = [
            rect(WaterKind::River, -1_000_000_000, 0, 2_000_000_000, 2),
            rect(WaterKind::Lake, -1_000_000_000, 2, 2_000_000_000, 2),
        ];
        let result = classify(&regions, 1, u32::MAX);
        assert_eq!(result.components[0].area, 8_000_000_000);
        assert_eq!(result.forced_lake, [false, false]);
    }

    #[test]
    fn defaults_preserve_tiny_closed_pools_without_wide_cores() {
        let regions = [
            rect(WaterKind::River, 0, 0, 200, 10),
            rect(WaterKind::River, 300, 0, 20, 20),
        ];
        let result = super::classify(&regions, &LakeOptions::default());
        assert_eq!(result.forced_lake, [true, false]);
        assert_eq!(result.components[0].area, 2000);
        assert!((result.components[0].mean_width - 10.0).abs() < 0.001);
        assert!(
            result.components[0].elongation > LakeOptions::default().max_closed_lake_elongation
        );
    }

    #[test]
    fn larger_closed_meanders_and_long_channels_need_a_lake_shape() {
        let regions = [
            // A connected U-shaped channel with a mostly empty bounding box.
            rect(WaterKind::River, 0, 0, 500, 20),
            rect(WaterKind::River, 0, 20, 20, 380),
            rect(WaterKind::River, 480, 20, 20, 380),
            // A straight narrow channel has full occupancy but high elongation.
            rect(WaterKind::River, 1000, 0, 1000, 30),
            rect(WaterKind::River, 3000, 0, 100, 100),
        ];
        let result = super::classify(&regions, &LakeOptions::default());
        assert_eq!(result.forced_lake, [false, false, false, false, true]);
        let u = &result.components[result.component_by_region[0].unwrap() as usize];
        assert_eq!(u.region_count, 3);
        assert!(u.fill_ratio < 0.15);
        let straight = &result.components[result.component_by_region[3].unwrap() as usize];
        assert!(straight.fill_ratio > 0.99);
        assert!(straight.elongation > 30.0);
    }

    #[test]
    fn shape_uses_complete_body_across_attribute_seams() {
        let mut regions: Vec<_> = (0..10)
            .map(|i| rect(WaterKind::River, i * 10, 0, 10, 100))
            .collect();
        regions[4].temperature = Temperature::Cold;
        regions[5].modifiers = Modifiers::ICE;
        regions[6].kind = WaterKind::Swamp;
        let result = super::classify(&regions, &LakeOptions::default());
        assert_eq!(result.components.len(), 1);
        assert!((result.components[0].fill_ratio - 1.0).abs() < 0.001);
        assert!((result.components[0].elongation - 1.0).abs() < 0.001);
        assert_eq!(result.forced_region_count, 9);
        assert!(!result.forced_lake[6]);
    }

    #[test]
    fn diagonal_elliptical_lake_uses_a_rotated_footprint() {
        let mut lake = rect(WaterKind::River, -200, -200, 400, 400);
        let mut runs = Vec::new();
        let sine = std::f64::consts::FRAC_1_SQRT_2;
        for z in -200..200 {
            let mut row = None;
            for x in -200..200 {
                let u = (x + z) as f64 * sine;
                let v = (z - x) as f64 * sine;
                if (u / 180.0).powi(2) + (v / 50.0).powi(2) <= 1.0 {
                    let (first, last) = row.get_or_insert((x, x));
                    *first = (*first).min(x);
                    *last = x;
                }
            }
            if let Some((x0, x1)) = row {
                runs.push(Run { z, x0, x1 });
            }
        }
        lake.geometry.min_x = runs.iter().map(|r| r.x0).min().unwrap();
        lake.geometry.max_x = runs.iter().map(|r| r.x1).max().unwrap();
        lake.geometry.min_z = runs.first().unwrap().z;
        lake.geometry.max_z = runs.last().unwrap().z;
        lake.geometry.column_count = runs.iter().map(|r| (r.x1 - r.x0 + 1) as u32).sum();
        lake.geometry.runs = runs;
        let options = LakeOptions {
            min_closed_lake_fill: 0.70,
            ..LakeOptions::default()
        };
        let result = super::classify(&[lake], &options);
        assert_eq!(result.forced_lake, [true]);
        let shape = &result.components[0];
        let axis_area = (shape.bounds[2] - shape.bounds[0] + 1) as f64
            * (shape.bounds[3] - shape.bounds[1] + 1) as f64;
        assert!((shape.area as f64 / axis_area) < 0.5);
        assert!(shape.fill_ratio > 0.75);
        assert!(shape.elongation > 3.0 && shape.elongation < 4.0);
    }

    #[test]
    fn closed_shape_is_stable_at_large_world_coordinates_and_width_is_optional() {
        let near = rect(WaterKind::River, 0, 0, 120, 60);
        let far = rect(WaterKind::River, 1_000_000_000, -1_000_000_000, 120, 60);
        let options = LakeOptions::default();
        let shapes = super::classify(&[near, far], &options);
        assert_eq!(shapes.forced_lake, [true, true]);
        assert_eq!(
            shapes.components[0].fill_ratio,
            shapes.components[1].fill_ratio
        );
        assert_eq!(
            shapes.components[0].elongation,
            shapes.components[1].elongation
        );
        assert_eq!(
            shapes.components[0].mean_width,
            shapes.components[1].mean_width
        );
        let narrow = rect(WaterKind::River, 0, 0, 120, 60);
        let width_gate = LakeOptions {
            min_closed_lake_mean_width: 61.0,
            ..options
        };
        assert_eq!(super::classify(&[narrow], &width_gate).forced_lake, [false]);
    }
}
