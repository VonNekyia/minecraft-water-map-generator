//! Absorbing small regions into the neighbour they belong to.
//!
//! Splitting water by classification signature is exact but noisy: a river
//! widening, a shallow shelf, a patch of ice at a lake shore each break off into
//! their own region even though nobody would call them one. This pass grows the
//! big regions back over the small ones.
//!
//! A region is absorbed into the adjacent region it shares the longest boundary
//! with, and the merged region keeps the attributes of its *largest* member - so
//! the lake fragments along a river become river, not the other way round. Two
//! comparably sized bodies of water are never merged: the neighbour has to be
//! [`config::ABSORB_MIN_RATIO`] times larger, unless the region is too small to
//! describe anything on its own.
//!
//! Absorption reassembles pieces; it never reclassifies them. Only a piece too
//! small to describe anything on its own ([`min_columns`](absorb)) may join a
//! neighbour of a different kind.
//!
//! That restriction is not decoration. Rivers touch lakes everywhere, and letting
//! `Lake` yield to any larger neighbour meant every pond a river ran through was
//! swallowed by it: the river class ended up with a median width of 27 blocks,
//! wider than the lakes. Repairing the 4x4 biome quantisation along a river bank
//! is the fringe rule's job, not absorption's.
//!
//! Cave water never merges with water that can see the sky either: an underground
//! pool and the lake above it share x/z coordinates but are not the same body of
//! water.

use std::collections::HashMap;

use crate::config;
use crate::water::components::UnionFind;
use crate::water::model::Run;

/// Shared boundary length between two regions, in columns.
pub type Adjacency = Vec<(u32, u32, u32)>;

/// Builds the region adjacency graph from run-length encoded geometry.
///
/// `runs` must be sorted by `(z, x0)`. Two regions are adjacent when a column of
/// one is orthogonally next to a column of the other.
pub fn adjacency(runs: &[(u32, Run)]) -> Adjacency {
    let mut edges: HashMap<(u32, u32), u32> = HashMap::new();
    let mut add = |a: u32, b: u32, w: u32| {
        if a == b {
            return;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        *edges.entry(key).or_insert(0) += w;
    };

    // Row boundaries in `runs`, so consecutive rows can be walked in parallel.
    let mut rows: Vec<(i32, usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < runs.len() {
        let z = runs[i].1.z;
        let start = i;
        while i < runs.len() && runs[i].1.z == z {
            i += 1;
        }
        rows.push((z, start, i));
    }

    for (r, &(z, start, end)) in rows.iter().enumerate() {
        // Side by side in the same row.
        for k in start + 1..end {
            let prev = &runs[k - 1];
            let cur = &runs[k];
            if prev.1.x1 + 1 == cur.1.x0 {
                add(prev.0, cur.0, 1);
            }
        }

        // Overlapping with the row below, if that row is directly below.
        let Some(&(nz, nstart, nend)) = rows.get(r + 1) else {
            continue;
        };
        if nz != z + 1 {
            continue;
        }
        let (mut a, mut b) = (start, nstart);
        while a < end && b < nend {
            let (ra, rb) = (&runs[a].1, &runs[b].1);
            let lo = ra.x0.max(rb.x0);
            let hi = ra.x1.min(rb.x1);
            if lo <= hi {
                add(runs[a].0, runs[b].0, (hi - lo + 1) as u32);
            }
            if ra.x1 < rb.x1 {
                a += 1;
            } else {
                b += 1;
            }
        }
    }

    edges.into_iter().map(|((a, b), w)| (a, b, w)).collect()
}

/// Result of the absorption pass.
pub struct Absorbed {
    /// Group root per region id.
    pub group: Vec<u32>,
    /// Region whose classification the group inherits (its largest member).
    pub donor: Vec<u32>,
    /// Number of regions that were absorbed into another.
    pub absorbed: u32,
}

/// Merges small regions into their dominant neighbour.
///
/// `kinds` holds each region's [`WaterKind`] as its wire value and `cave` says
/// whether it is roofed over; together they decide which merges are allowed.
/// `min_columns` is the size below which a piece cannot stand on its own and
/// merges unconditionally.
pub fn absorb(
    sizes: &[u32],
    kinds: &[u8],
    cave: &[bool],
    adj: &Adjacency,
    min_columns: u32,
) -> Absorbed {
    let n = sizes.len();
    debug_assert_eq!(kinds.len(), n);
    debug_assert_eq!(cave.len(), n);
    let mut uf = UnionFind::new(n);
    let mut group_size: Vec<u64> = sizes.iter().map(|s| *s as u64).collect();
    let mut group_kind: Vec<u8> = kinds.to_vec();
    let mut group_cave: Vec<bool> = cave.to_vec();
    let mut donor: Vec<u32> = (0..n as u32).collect();
    let mut absorbed = 0u32;

    for _ in 0..config::ABSORB_MAX_ROUNDS {
        // Aggregate the boundary weights onto the current groups.
        let mut by_root: HashMap<(u32, u32), u32> = HashMap::new();
        for (a, b, w) in adj {
            let (ra, rb) = (uf.find(*a), uf.find(*b));
            if ra == rb {
                continue;
            }
            let key = if ra < rb { (ra, rb) } else { (rb, ra) };
            *by_root.entry(key).or_insert(0) += w;
        }
        if by_root.is_empty() {
            break;
        }
        let mut neighbours: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        for ((a, b), w) in by_root {
            neighbours.entry(a).or_default().push((b, w));
            neighbours.entry(b).or_default().push((a, w));
        }

        // Smallest regions first, so a fragment settles before its neighbour grows.
        let mut candidates: Vec<u32> = (0..n as u32)
            .filter(|i| uf.find(*i) == *i)
            .filter(|i| group_size[*i as usize] < config::ABSORB_MAX_COLUMNS as u64)
            .collect();
        candidates.sort_by_key(|i| group_size[*i as usize]);

        let mut merged_any = false;
        for candidate in candidates {
            if uf.find(candidate) != candidate {
                continue; // already absorbed earlier this round
            }
            let size = group_size[candidate as usize];
            if size >= config::ABSORB_MAX_COLUMNS as u64 {
                continue;
            }
            let tiny = size < min_columns as u64;

            let Some(list) = neighbours.get(&candidate) else {
                continue;
            };
            let candidate_kind = group_kind[candidate as usize];
            let mut best: Option<(u32, u32, u64)> = None; // (root, weight, size)
            for (other, weight) in list {
                let root = uf.find(*other);
                if root == candidate {
                    continue;
                }
                let other_size = group_size[root as usize];
                // An underground pool and the water on the surface above it are
                // never one body of water, whichever of them is bigger and however
                // small the piece is. This one holds even for tiny pieces: letting
                // them across would produce regions that are part cave, part not,
                // and a `swamp` that carries the `cave` modifier is nonsense.
                if group_cave[root as usize] != group_cave[candidate as usize] {
                    continue;
                }
                if !tiny {
                    if other_size < size.saturating_mul(config::ABSORB_MIN_RATIO) {
                        continue;
                    }
                    // Absorption puts pieces back together, it does not rename
                    // them. A pond beside a river stays a pond.
                    if group_kind[root as usize] != candidate_kind {
                        continue;
                    }
                }
                let better = match best {
                    None => true,
                    Some((_, bw, bs)) => (*weight, other_size) > (bw, bs),
                };
                if better {
                    best = Some((root, *weight, other_size));
                }
            }

            let Some((target, _, target_size)) = best else {
                continue;
            };
            let root = uf.union(candidate, target);
            group_size[root as usize] = size + target_size;
            // The bigger side decides what the merged region is.
            let winner = if target_size >= size { target } else { candidate };
            donor[root as usize] = donor[winner as usize];
            group_kind[root as usize] = group_kind[winner as usize];
            group_cave[root as usize] = group_cave[winner as usize];
            absorbed += 1;
            merged_any = true;
        }

        if !merged_any {
            break;
        }
    }

    let group: Vec<u32> = (0..n as u32).map(|i| uf.find(i)).collect();
    let donor = group.iter().map(|g| donor[*g as usize]).collect();
    Absorbed {
        group,
        donor,
        absorbed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water::model::WaterKind;

    fn run(id: u32, z: i32, x0: i32, x1: i32) -> (u32, Run) {
        (id, Run { z, x0, x1 })
    }

    fn sorted(mut runs: Vec<(u32, Run)>) -> Vec<(u32, Run)> {
        runs.sort_by_key(|(_, r)| (r.z, r.x0));
        runs
    }

    fn weight(adj: &Adjacency, a: u32, b: u32) -> u32 {
        adj.iter()
            .find(|(x, y, _)| (*x == a && *y == b) || (*x == b && *y == a))
            .map(|(_, _, w)| *w)
            .unwrap_or(0)
    }

    #[test]
    fn side_by_side_runs_are_adjacent() {
        let runs = sorted(vec![run(0, 0, 0, 4), run(1, 0, 5, 9)]);
        let adj = adjacency(&runs);
        assert_eq!(weight(&adj, 0, 1), 1);
    }

    #[test]
    fn a_gap_breaks_adjacency() {
        let runs = sorted(vec![run(0, 0, 0, 4), run(1, 0, 6, 9)]);
        assert!(adjacency(&runs).is_empty());
    }

    #[test]
    fn stacked_rows_share_their_overlap() {
        let runs = sorted(vec![run(0, 0, 0, 9), run(1, 1, 4, 20)]);
        let adj = adjacency(&runs);
        assert_eq!(weight(&adj, 0, 1), 6); // x 4..9
    }

    #[test]
    fn rows_that_are_not_neighbours_do_not_touch() {
        let runs = sorted(vec![run(0, 0, 0, 9), run(1, 2, 0, 9)]);
        assert!(adjacency(&runs).is_empty());
    }

    const LAKE: u8 = WaterKind::Lake as u8;
    const RIVER: u8 = WaterKind::River as u8;
    const SEA: u8 = WaterKind::Sea as u8;

    const MIN: u32 = config::MIN_WATER_BODY_COLUMNS;

    fn lakes(n: usize) -> Vec<u8> {
        vec![LAKE; n]
    }

    fn open(n: usize) -> Vec<bool> {
        vec![false; n]
    }

    #[test]
    fn a_fragment_joins_the_neighbour_it_shares_most_boundary_with() {
        // Region 0 is big, regions 1 and 2 are small; 0 touches 1 over 10 columns,
        // 2 touches 1 over 1 column.
        let sizes = vec![10_000, 30, 40];
        let adj = vec![(0u32, 1u32, 10u32), (1, 2, 1)];
        let out = absorb(&sizes, &lakes(3), &open(3), &adj, MIN);
        assert_eq!(out.group[1], out.group[0]);
        assert_eq!(out.donor[1], 0, "the big region decides the classification");
        assert_eq!(out.absorbed, 2);
        // 2 reaches 0 through 1 in a later round.
        assert_eq!(out.group[2], out.group[0]);
        assert_eq!(out.donor[2], 0);
    }

    #[test]
    fn comparable_regions_are_left_alone() {
        let sizes = vec![1000, 900];
        let adj = vec![(0u32, 1u32, 50u32)];
        let out = absorb(&sizes, &lakes(2), &open(2), &adj, MIN);
        assert_ne!(out.group[0], out.group[1]);
        assert_eq!(out.absorbed, 0);
    }

    #[test]
    fn very_small_regions_merge_even_without_the_size_ratio() {
        // 10 and 15 columns: neither is 4x the other, but both are below
        // the minimum body size and must not survive on their own.
        let sizes = vec![10, 15];
        let adj = vec![(0u32, 1u32, 3u32)];
        let out = absorb(&sizes, &lakes(2), &open(2), &adj, MIN);
        assert_eq!(out.group[0], out.group[1]);
        assert_eq!(out.donor[0], 1, "the larger of the two decides");
    }

    #[test]
    fn big_regions_are_never_absorbed() {
        let sizes = vec![config::ABSORB_MAX_COLUMNS, 10_000_000];
        let adj = vec![(0u32, 1u32, 500u32)];
        let out = absorb(&sizes, &lakes(2), &open(2), &adj, MIN);
        assert_ne!(out.group[0], out.group[1]);
    }

    #[test]
    fn a_real_river_is_not_swallowed_by_the_sea() {
        // A 500 column river reaching a huge ocean stays a river ...
        let sizes = vec![50_000_000, 500];
        let adj = vec![(0u32, 1u32, 12u32)];
        let out = absorb(&sizes, &[SEA, RIVER], &open(2), &adj, MIN);
        assert_ne!(out.group[0], out.group[1]);

        // A lake of the same size beside it keeps its own kind too: absorption
        // reassembles pieces, it does not rename them.
        let out = absorb(&sizes, &[SEA, LAKE], &open(2), &adj, MIN);
        assert_ne!(out.group[0], out.group[1]);
    }

    #[test]
    fn a_pond_beside_a_river_stays_a_pond() {
        // Rivers touch lakes everywhere. Letting the bigger one swallow the
        // smaller made the river class wider than the lake class.
        let sizes = vec![200_000, 3_000];
        let adj = vec![(0u32, 1u32, 20u32)];
        let out = absorb(&sizes, &[RIVER, LAKE], &open(2), &adj, MIN);
        assert_ne!(out.group[0], out.group[1]);

        // Two river pieces of the same kind still merge.
        let out = absorb(&sizes, &[RIVER, RIVER], &open(2), &adj, MIN);
        assert_eq!(out.group[0], out.group[1]);
    }

    #[test]
    fn same_kind_neighbours_still_merge() {
        let sizes = vec![50_000_000, 500];
        let adj = vec![(0u32, 1u32, 12u32)];
        let out = absorb(&sizes, &[RIVER, RIVER], &open(2), &adj, MIN);
        assert_eq!(out.group[0], out.group[1]);
    }

    #[test]
    fn a_tiny_fragment_merges_across_kinds_anyway() {
        // Below the minimum body size nothing can be described on its own.
        let sizes = vec![50_000, MIN - 1];
        let adj = vec![(0u32, 1u32, 4u32)];
        let out = absorb(&sizes, &[SEA, RIVER], &open(2), &adj, MIN);
        assert_eq!(out.group[0], out.group[1]);
    }

    #[test]
    fn a_cave_pool_is_not_absorbed_into_the_water_above_it() {
        let adj = vec![(0u32, 1u32, 30u32)];

        // A small cave pool beside a big surface lake stays separate ...
        let sizes = vec![400_000, 800];
        let out = absorb(&sizes, &lakes(2), &[false, true], &adj, MIN);
        assert_ne!(out.group[0], out.group[1]);

        // ... and so does a small surface pool beside a big cave lake, even though
        // `Lake` would otherwise be free to give up its kind.
        let out = absorb(&sizes, &lakes(2), &[true, false], &adj, MIN);
        assert_ne!(out.group[0], out.group[1]);

        // Two cave pools next to each other do merge.
        let out = absorb(&sizes, &lakes(2), &[true, true], &adj, MIN);
        assert_eq!(out.group[0], out.group[1]);
    }

    #[test]
    fn even_a_tiny_cave_piece_stays_out_of_surface_water() {
        // The "too small to stand alone" rule lets pieces cross kinds, but never
        // the cave boundary: a region that is part cave and part not could not be
        // described by one set of attributes.
        let sizes = vec![50_000, MIN - 1];
        let adj = vec![(0u32, 1u32, 6u32)];
        let out = absorb(&sizes, &lakes(2), &[false, true], &adj, MIN);
        assert_ne!(out.group[0], out.group[1]);
    }

    #[test]
    fn isolated_regions_survive() {
        let sizes = vec![25, 10_000];
        let out = absorb(&sizes, &lakes(2), &open(2), &Vec::new(), MIN);
        assert_ne!(out.group[0], out.group[1]);
        assert_eq!(out.absorbed, 0);
    }

    #[test]
    fn a_chain_of_fragments_collapses_into_the_big_region() {
        // 0 is a river; 1..=4 are little bank fragments hanging off each other.
        let sizes = vec![100_000, 30, 25, 20, 22];
        let kinds = vec![RIVER, LAKE, LAKE, LAKE, LAKE];
        let adj = vec![(0u32, 1u32, 8u32), (1, 2, 6), (2, 3, 5), (3, 4, 4)];
        let out = absorb(&sizes, &kinds, &open(5), &adj, MIN);
        for id in 1..5 {
            assert_eq!(out.group[id], out.group[0], "fragment {id} not absorbed");
            assert_eq!(out.donor[id], 0);
        }
    }
}
