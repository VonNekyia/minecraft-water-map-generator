//! Area-based consolidation after shape correction. This merges real regions,
//! including repaired riverbanks, without eroding or dilating their outlines.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use super::absorb::{adjacency, Adjacency};
use super::classifier::{self, RegionAccum};
use super::model::*;
use crate::world::biome::BiomeRegistry;

#[derive(Clone, Copy)]
pub struct MergeOptions {
    pub river_min_area: u32,
    pub sea_min_area: u32,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            river_min_area: crate::config::RIVER_MERGE_MIN_COLUMNS,
            sea_min_area: crate::config::SEA_MERGE_MIN_COLUMNS,
        }
    }
}

impl MergeOptions {
    fn threshold(self, kind: WaterKind) -> u64 {
        match kind {
            WaterKind::River => self.river_min_area as u64,
            WaterKind::Sea => self.sea_min_area as u64,
            _ => 0,
        }
    }
}

#[derive(Default)]
pub struct MergeStats {
    pub merged_rivers: u32,
    pub merged_seas: u32,
    /// Small connected components with no same-kind neighbour left to merge.
    pub small_rivers: u32,
    pub small_seas: u32,
}

/// Root is also the original region supplying the merged group's categorical
/// attributes. The larger current group wins; equal target scores favour lower IDs.
fn plan(
    sizes: &[u32],
    kinds: &[WaterKind],
    cave: &[bool],
    adj: &Adjacency,
    opts: MergeOptions,
) -> Vec<usize> {
    let n = sizes.len();
    let mut parent: Vec<_> = (0..n).collect();
    let mut areas: Vec<_> = sizes.iter().map(|s| *s as u64).collect();
    let mut edges = vec![BTreeMap::<usize, u64>::new(); n];
    for &(a, b, contact) in adj {
        let (a, b) = (a as usize, b as usize);
        if kinds[a] != kinds[b] || cave[a] || cave[b] || opts.threshold(kinds[a]) == 0 {
            continue;
        }
        *edges[a].entry(b).or_default() += contact as u64;
        *edges[b].entry(a).or_default() += contact as u64;
    }
    let mut queue: BinaryHeap<_> = areas
        .iter()
        .enumerate()
        .filter(|(i, a)| !cave[*i] && **a < opts.threshold(kinds[*i]))
        .map(|(i, a)| Reverse((*a, i)))
        .collect();
    while let Some(Reverse((area, id))) = queue.pop() {
        if parent[id] != id || areas[id] != area {
            continue;
        }
        let target = edges[id]
            .iter()
            .filter(|(other, _)| areas[**other] >= area)
            .max_by_key(|(other, w)| (**w, areas[**other], Reverse(**other)))
            .map(|(other, _)| *other);
        let Some(target) = target else {
            continue;
        };
        parent[id] = target;
        areas[target] += area;
        let old_edges = std::mem::take(&mut edges[id]);
        edges[target].remove(&id);
        for (other, contact) in old_edges {
            if other == target {
                continue;
            }
            edges[other].remove(&id);
            *edges[other].entry(target).or_default() += contact;
            *edges[target].entry(other).or_default() += contact;
        }
        if areas[target] < opts.threshold(kinds[target]) {
            queue.push(Reverse((areas[target], target)));
        }
    }
    for id in 0..n {
        let mut root = id;
        while parent[root] != root {
            root = parent[root];
        }
        let mut node = id;
        while parent[node] != node {
            let next = parent[node];
            parent[node] = root;
            node = next;
        }
    }
    parent
}

pub fn merge(
    regions: Vec<WaterRegion>,
    accums: &[RegionAccum],
    registry: &BiomeRegistry,
    opts: MergeOptions,
) -> (Vec<WaterRegion>, MergeStats) {
    assert_eq!(regions.len(), accums.len());
    if opts.river_min_area == 0 && opts.sea_min_area == 0 {
        return (regions, MergeStats::default());
    }
    let mut runs: Vec<_> = regions
        .iter()
        .enumerate()
        .filter(|(_, r)| opts.threshold(r.kind) > 0 && !r.modifiers.contains(Modifiers::CAVE))
        .flat_map(|(id, r)| r.geometry.runs.iter().map(move |run| (id as u32, *run)))
        .collect();
    runs.sort_unstable_by_key(|(_, r)| (r.z, r.x0));
    let sizes: Vec<_> = regions.iter().map(|r| r.geometry.column_count).collect();
    let kinds: Vec<_> = regions.iter().map(|r| r.kind).collect();
    let cave: Vec<_> = regions
        .iter()
        .map(|r| r.modifiers.contains(Modifiers::CAVE))
        .collect();
    let roots = plan(&sizes, &kinds, &cave, &adjacency(&runs), opts);
    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    for (id, root) in roots.into_iter().enumerate() {
        groups.entry(root).or_default().push(id);
    }
    let mut regions: Vec<_> = regions.into_iter().map(Some).collect();
    let mut output = Vec::with_capacity(groups.len());
    let mut stats = MergeStats::default();
    for (root, members) in groups {
        let mut donor = regions[root].take().unwrap();
        if members.len() > 1 {
            let mut combined = RegionAccum::default();
            for id in &members {
                combined.merge(&accums[*id]);
            }
            combined.signature = classifier::signature(
                donor.kind,
                donor.temperature,
                donor.modifiers.contains(Modifiers::ICE),
                false,
            );
            let mut result = classifier::finalize(0, &combined, registry);
            // Simplify categorical changes, but recompute measured depth,
            // surface height and depth distribution from unrounded column sums.
            result.vegetation = donor.vegetation;
            result.modifiers = donor.modifiers;
            result.dominant_biome = donor.dominant_biome.clone();
            result.geometry.runs = std::mem::take(&mut donor.geometry.runs);
            for id in &members {
                if *id != root {
                    result
                        .geometry
                        .runs
                        .append(&mut regions[*id].take().unwrap().geometry.runs);
                }
            }
            result.geometry.runs.sort_unstable_by_key(|r| (r.z, r.x0));
            let mut compact: Vec<Run> = Vec::with_capacity(result.geometry.runs.len());
            for run in result.geometry.runs {
                match compact.last_mut() {
                    Some(prev) if prev.z == run.z && prev.x1 + 1 == run.x0 => prev.x1 = run.x1,
                    _ => compact.push(run),
                }
            }
            result.geometry.runs = compact;
            match result.kind {
                WaterKind::River => stats.merged_rivers += members.len() as u32 - 1,
                WaterKind::Sea => stats.merged_seas += members.len() as u32 - 1,
                _ => unreachable!(),
            }
            donor = result;
        }
        if (donor.geometry.column_count as u64) < opts.threshold(donor.kind)
            && !donor.modifiers.contains(Modifiers::CAVE)
        {
            match donor.kind {
                WaterKind::River => stats.small_rivers += 1,
                WaterKind::Sea => stats.small_seas += 1,
                _ => {}
            }
        }
        donor.id = output.len() as u32;
        output.push(donor);
    }
    (output, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    const R: WaterKind = WaterKind::River;
    fn opts() -> MergeOptions {
        MergeOptions {
            river_min_area: 5000,
            sea_min_area: 0,
        }
    }

    #[test]
    fn a_4999_block_river_merges_but_a_5000_block_river_stays() {
        assert_eq!(
            plan(
                &[4999, 5000, 100_000],
                &[R; 3],
                &[false; 3],
                &vec![(0, 2, 1), (1, 2, 1)],
                opts()
            ),
            vec![2, 1, 2]
        );
    }
    #[test]
    fn equal_small_pieces_merge_without_the_old_four_to_one_ratio() {
        assert_eq!(
            plan(
                &[3000, 3000],
                &[R; 2],
                &[false; 2],
                &vec![(0, 1, 5)],
                opts()
            ),
            vec![1, 1]
        );
    }
    #[test]
    fn lakes_oceans_caves_and_dry_gaps_are_not_river_merge_targets() {
        assert_eq!(
            plan(
                &[100, 10_000, 10_000, 10_000, 20_000],
                &[R, WaterKind::Lake, WaterKind::Sea, R, R],
                &[false, false, false, true, false],
                &vec![(0, 1, 20), (0, 2, 30), (0, 3, 40)],
                opts()
            ),
            vec![0, 1, 2, 3, 4]
        );
    }
    #[test]
    fn long_chain_finishes_without_a_round_limit() {
        let mut sizes = vec![100; 40];
        sizes.push(100_000);
        let edges = (0..40).map(|i| (i, i + 1, 1)).collect();
        assert_eq!(
            plan(&sizes, &vec![R; 41], &vec![false; 41], &edges, opts()),
            vec![40; 41]
        );
    }
    #[test]
    fn sea_and_river_thresholds_are_independent_and_zero_disables() {
        let kinds = [R, R, WaterKind::Sea, WaterKind::Sea];
        let edges = vec![(0, 1, 1), (2, 3, 1)];
        let result = plan(
            &[8000, 20_000, 8000, 20_000],
            &kinds,
            &[false; 4],
            &edges,
            MergeOptions {
                river_min_area: 5000,
                sea_min_area: 10_000,
            },
        );
        assert_eq!(result, vec![0, 1, 3, 3]);
        assert_eq!(
            plan(
                &[100; 4],
                &kinds,
                &[false; 4],
                &edges,
                MergeOptions {
                    river_min_area: 0,
                    sea_min_area: 0
                }
            ),
            vec![0, 1, 2, 3]
        );
    }
    #[test]
    fn merge_preserves_exact_geometry_and_recomputes_measured_depth() {
        let registry = BiomeRegistry::vanilla_only();
        let accums: Vec<_> = [(1000, 10, 0, 999), (6000, 30, 1000, 6999)]
            .into_iter()
            .map(|(area, depth, x0, x1)| {
                let mut a = RegionAccum::default();
                a.columns = area;
                a.depth_sum = depth * area as i64;
                a.depth_max = depth as u32;
                a.surface_sum = 62 * area as i64;
                a.extend_bounds(x0, x1, 0);
                a.signature = classifier::signature(R, Temperature::Cold, false, false);
                a
            })
            .collect();
        let mut regions: Vec<_> = accums
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let mut r = classifier::finalize(i as u32, a, &registry);
                r.geometry.runs = vec![Run {
                    z: 0,
                    x0: a.min_x,
                    x1: a.max_x,
                }];
                r
            })
            .collect();
        regions[0].temperature = Temperature::Warm;
        regions[1].modifiers = Modifiers::DESERT;
        let (out, stats) = merge(regions, &accums, &registry, opts());
        assert_eq!(out.len(), 1);
        assert_eq!(stats.merged_rivers, 1);
        assert_eq!(out[0].temperature, Temperature::Cold);
        assert!(out[0].modifiers.contains(Modifiers::DESERT));
        assert_eq!(out[0].bathymetry.mean_depth, 27);
        assert_eq!(out[0].bathymetry.max_depth, 30);
        assert_eq!(out[0].geometry.column_count, 7000);
        assert_eq!(
            out[0].geometry.runs,
            vec![Run {
                z: 0,
                x0: 0,
                x1: 6999
            }]
        );
        assert!(!out[0].geometry.contains(7000, 0));
    }
}
