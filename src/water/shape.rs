//! Shape correction on connected water kinds, independent of temperature/ice
//! region boundaries. A bank shares a long side with a river; a lake usually
//! meets one at a narrow mouth. Use that contact as well as shape, rather than
//! increasing the biome fringe radius and flooding into river-fed lakes.

use super::absorb;
use super::components::UnionFind;
use super::model::*;
use crate::config;

#[derive(Default)]
pub struct ShapeStats {
    pub to_lake: u32,
    pub to_river: u32,
    pub bank_groups: u32,
    pub bank_columns: u64,
}

pub fn correct(regions: &mut [WaterRegion]) -> ShapeStats {
    let n = regions.len();
    let mut runs: Vec<_> = regions
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.modifiers.contains(Modifiers::CAVE))
        .flat_map(|(i, r)| r.geometry.runs.iter().map(move |run| (i as u32, *run)))
        .collect();
    runs.sort_unstable_by_key(|(_, r)| (r.z, r.x0));
    let adj = absorb::adjacency(&runs);
    let mut uf = UnionFind::new(n);
    for &(a, b, _) in &adj {
        if regions[a as usize].kind == regions[b as usize].kind {
            uf.union(a, b);
        }
    }
    let roots: Vec<_> = (0..n).map(|i| uf.find(i as u32) as usize).collect();
    let mut areas = vec![0u64; n];
    let mut perimeters = vec![0u64; n];
    for (i, r) in regions.iter().enumerate() {
        if r.modifiers.contains(Modifiers::CAVE) {
            continue;
        }
        areas[roots[i]] += r.geometry.column_count as u64;
        perimeters[roots[i]] += r.geometry.perimeter();
    }
    // Cancel internal region borders, including temperature and ice seams.
    for &(a, b, contact) in &adj {
        if roots[a as usize] == roots[b as usize] {
            perimeters[roots[a as usize]] -= 2 * contact as u64;
        }
    }
    let mut kinds: Vec<_> = regions.iter().map(|r| r.kind).collect();
    for i in 0..n {
        if roots[i] != i || areas[i] == 0 || perimeters[i] == 0 {
            continue;
        }
        let width = 2.0 * areas[i] as f64 / perimeters[i] as f64;
        let elongation = areas[i] as f64 / (width * width);
        kinds[i] = match kinds[i] {
            WaterKind::River
                if width >= config::POOL_MIN_WIDTH as f64
                    && elongation <= config::POOL_MAX_ELONGATION as f64 =>
            {
                WaterKind::Lake
            }
            WaterKind::Lake
                if width <= config::STRAND_MAX_WIDTH as f64
                    && elongation >= config::STRAND_MIN_ELONGATION as f64 =>
            {
                WaterKind::River
            }
            k => k,
        };
    }
    let mut river_contact = vec![0u64; n];
    for &(a, b, contact) in &adj {
        let (a, b) = (roots[a as usize], roots[b as usize]);
        if kinds[a] == WaterKind::Lake && kinds[b] == WaterKind::River {
            river_contact[a] += contact as u64;
        }
        if kinds[b] == WaterKind::Lake && kinds[a] == WaterKind::River {
            river_contact[b] += contact as u64;
        }
    }
    let mut stats = ShapeStats::default();
    // Decide simultaneously: a repaired bank cannot seed another round of
    // expansion into a pond. All tests use the original connected lake outline.
    for i in 0..n {
        if roots[i] == i
            && kinds[i] == WaterKind::Lake
            && is_bank(areas[i], perimeters[i], river_contact[i])
        {
            kinds[i] = WaterKind::River;
            stats.bank_groups += 1;
            stats.bank_columns += areas[i];
        }
    }
    for (i, r) in regions.iter_mut().enumerate() {
        if r.modifiers.contains(Modifiers::CAVE) {
            continue;
        }
        let kind = kinds[roots[i]];
        if kind != r.kind {
            if kind == WaterKind::Lake {
                stats.to_lake += 1;
            }
            if kind == WaterKind::River {
                stats.to_river += 1;
            }
            r.kind = kind;
        }
    }
    stats
}

fn is_bank(area: u64, perimeter: u64, river_contact: u64) -> bool {
    if perimeter == 0 || river_contact == 0 {
        return false;
    }
    let width = 2.0 * area as f64 / perimeter as f64;
    let elongation = area as f64 / (width * width);
    width <= config::BANK_MAX_WIDTH
        && elongation >= config::BANK_MIN_ELONGATION
        && river_contact as f64 / perimeter as f64 >= config::BANK_MIN_CONTACT_SHARE
        && area as f64 / river_contact as f64 <= config::BANK_MAX_CONTACT_WIDTH
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(kind: WaterKind, x: i32, z: i32, w: i32, h: i32) -> WaterRegion {
        WaterRegion {
            id: 0,
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
                column_count: (w * h) as u32,
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
    fn long_bank_joins_river_but_a_lake_at_its_mouth_stays() {
        let mut rs = vec![
            rect(WaterKind::River, 0, 0, 30, 500),
            rect(WaterKind::Lake, 30, 0, 18, 500),
            rect(WaterKind::Lake, -40, 500, 110, 110),
        ];
        let stats = correct(&mut rs);
        // The bank touches this lake at its end, so they are one lake component:
        // the broad lake must protect the whole component from bank absorption.
        assert_eq!(rs[2].kind, WaterKind::Lake);
        assert_eq!(stats.bank_groups, 0);
        let mut rs = vec![
            rect(WaterKind::River, 0, 0, 30, 500),
            rect(WaterKind::Lake, 30, 0, 18, 400),
            rect(WaterKind::Lake, -40, 500, 110, 110),
        ];
        assert_eq!(correct(&mut rs).bank_groups, 1);
        assert_eq!(rs[1].kind, WaterKind::River);
        assert_eq!(rs[2].kind, WaterKind::Lake);
    }
    #[test]
    fn narrow_mouth_and_isolated_ponds_do_not_count_as_banks() {
        assert!(!is_bank(1600, 160, 8));
        assert!(!is_bank(1600, 160, 0));
        assert!(is_bank(3600, 436, 200));
        assert!(!is_bank(40_000, 800, 200));
    }
    #[test]
    fn temperature_and_ice_seams_do_not_turn_pieces_of_a_lake_into_rivers() {
        let mut rs: Vec<_> = (0..12)
            .map(|i| rect(WaterKind::Lake, i * 5, 0, 5, 200))
            .collect();
        rs[0].temperature = Temperature::Cold;
        rs[0].modifiers = Modifiers::ICE;
        correct(&mut rs);
        assert!(rs.iter().all(|r| r.kind == WaterKind::Lake));
    }
    #[test]
    fn cave_banks_never_change() {
        let mut rs = vec![
            rect(WaterKind::River, 0, 0, 30, 500),
            rect(WaterKind::Lake, 30, 0, 8, 500),
        ];
        rs[1].modifiers = Modifiers::CAVE;
        correct(&mut rs);
        assert_eq!(rs[1].kind, WaterKind::Lake);
    }
}
