//! Area sieve for the ocean display. Weights are actual water columns represented
//! by each pixel, not bounding-box area. Geometry and measured runtime data stay
//! untouched. Small isolated sea components cannot merge across dry land and are
//! omitted from this overview.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

pub const EMPTY: u8 = u8::MAX;

#[derive(Debug, Default)]
pub struct SieveStats {
    pub merged: usize,
    pub omitted: usize,
    pub omitted_columns: u64,
    pub remaining_small: usize,
}

pub fn simplify(codes: &mut [u8], weights: &[u64], width: usize, min_area: u64) -> SieveStats {
    assert_eq!(codes.len(), weights.len());
    assert!(width > 0 && codes.len() % width == 0);
    let height = codes.len() / width;
    let mut labels = vec![usize::MAX; codes.len()];
    let mut areas = Vec::new();
    let mut colors = Vec::new();
    let mut stack = Vec::new();
    for start in 0..codes.len() {
        if codes[start] == EMPTY || labels[start] != usize::MAX {
            continue;
        }
        let id = areas.len();
        areas.push(0u64);
        colors.push(codes[start]);
        labels[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            areas[id] += weights[i];
            for j in neighbours(i, width, height).into_iter().flatten() {
                if labels[j] == usize::MAX && codes[j] == colors[id] {
                    labels[j] = id;
                    stack.push(j);
                }
            }
        }
    }
    let mut edges = vec![BTreeMap::<usize, u64>::new(); areas.len()];
    for i in 0..codes.len() {
        let a = labels[i];
        if a == usize::MAX {
            continue;
        }
        for j in [
            ((i % width) + 1 < width).then_some(i + 1),
            (i + width < codes.len()).then_some(i + width),
        ]
        .into_iter()
        .flatten()
        {
            let b = labels[j];
            if b != usize::MAX && a != b {
                *edges[a].entry(b).or_default() += 1;
                *edges[b].entry(a).or_default() += 1;
            }
        }
    }
    let mut parent: Vec<_> = (0..areas.len()).collect();
    let mut queue: BinaryHeap<_> = areas
        .iter()
        .enumerate()
        .filter(|(_, a)| **a < min_area)
        .map(|(id, a)| Reverse((*a, id)))
        .collect();
    let mut stats = SieveStats::default();
    while let Some(Reverse((area, id))) = queue.pop() {
        if parent[id] != id || areas[id] != area {
            continue;
        }
        let target = edges[id]
            .iter()
            .filter(|(j, _)| areas[**j] >= area)
            .max_by_key(|(j, boundary)| (**boundary, areas[**j], Reverse(**j)))
            .map(|(j, _)| *j);
        let Some(target) = target else {
            // The global minimum cannot have a smaller live neighbour. An empty
            // adjacency list therefore means a whole isolated ocean speck.
            assert!(edges[id].is_empty());
            colors[id] = EMPTY;
            stats.omitted += 1;
            stats.omitted_columns += area;
            continue;
        };
        parent[id] = target;
        areas[target] += area;
        let old_edges = std::mem::take(&mut edges[id]);
        edges[target].remove(&id);
        for (other, boundary) in old_edges {
            if other == target {
                continue;
            }
            edges[other].remove(&id);
            *edges[other].entry(target).or_default() += boundary;
            *edges[target].entry(other).or_default() += boundary;
        }
        stats.merged += 1;
        if areas[target] < min_area {
            queue.push(Reverse((areas[target], target)));
        }
    }
    for id in 0..parent.len() {
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
    stats.remaining_small = (0..areas.len())
        .filter(|i| parent[*i] == *i && colors[*i] != EMPTY && areas[*i] < min_area)
        .count();
    for (i, code) in codes.iter_mut().enumerate() {
        if labels[i] != usize::MAX {
            *code = colors[parent[labels[i]]];
        }
    }
    stats
}

fn neighbours(i: usize, w: usize, h: usize) -> [Option<usize>; 4] {
    [
        (i % w > 0).then(|| i - 1),
        (i % w + 1 < w).then_some(i + 1),
        (i >= w).then(|| i - w),
        (i / w + 1 < h).then_some(i + w),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tiny_chain_merges_until_every_group_meets_the_area_floor() {
        let mut codes = vec![0, 1, 2, 3, 4, 5];
        let stats = simplify(&mut codes, &[1, 2, 3, 4, 5, 100], 6, 20);
        assert_eq!(codes, vec![5; 6]);
        assert_eq!(stats.merged, 5);
        assert_eq!(stats.remaining_small, 0);
    }
    #[test]
    fn exact_threshold_survives_and_weights_are_water_area() {
        let mut codes = vec![0, 0, 1, 1];
        simplify(&mut codes, &[1, 1, 5000, 5000], 4, 10000);
        assert_eq!(codes, vec![1; 4]);
        let mut codes = vec![0, 1];
        simplify(&mut codes, &[10000, 20000], 2, 10000);
        assert_eq!(codes, vec![0, 1]);
    }
    #[test]
    fn dry_gaps_and_row_edges_do_not_create_connections() {
        let mut codes = vec![EMPTY, 0, 1, EMPTY];
        let stats = simplify(&mut codes, &[0, 1, 10000, 0], 2, 10000);
        assert_eq!(codes, vec![EMPTY, EMPTY, 1, EMPTY]);
        assert_eq!(stats.omitted, 1);
        assert_eq!(stats.omitted_columns, 1);
    }
    #[test]
    fn equal_small_neighbours_merge_deterministically() {
        let mut codes = vec![0, 1, 2, 3];
        simplify(&mut codes, &[5000; 4], 4, 10000);
        assert_eq!(codes, vec![1; 4]);
    }
}
