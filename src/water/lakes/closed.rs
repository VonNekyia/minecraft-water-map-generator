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

/// Mark River/Lake regions in sea-disconnected physical bodies with total area
/// in the inclusive range min_area..=max_area. Swamp area counts toward the body
/// size but its label stays protected. A zero maximum disables all work; an
/// inverted range similarly matches no body. This function never mutates input.
pub fn classify(regions: &[WaterRegion], min_area: u32, max_area: u32) -> ClosedWater {
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

    for component in &mut result.components {
        component.forced_lake = !component.reaches_sea
            && component.inland_area > 0
            && (min_area as u64..=max_area as u64).contains(&component.area);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water::model::{Bathymetry, Depth, RegionGeometry, Run, Temperature, Vegetation};

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
}
