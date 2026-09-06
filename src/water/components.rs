//! Connected component labelling with a tile-local pass and a boundary merge.
//!
//! Labelling the whole world in one go would need a `u32` label per block column
//! (2.6 GiB for the world this was built against) and would be strictly serial.
//! Instead every region file is labelled independently and in parallel, and the
//! resulting local components are stitched together afterwards with a union-find
//! over the tile boundaries. That is the standard "local CCL + boundary merge"
//! decomposition and it keeps the expensive part embarrassingly parallel.

/// Marker for "no component" / "no key" in label and key grids.
pub const NONE: u32 = u32::MAX;

/// Side length of a labelling tile in block columns (one region file).
pub const TILE: usize = 512;

/// Disjoint set with union by rank and path halving.
#[derive(Clone, Debug, Default)]
pub struct UnionFind {
    parent: Vec<u32>,
    rank: Vec<u8>,
}

#[allow(dead_code)]
impl UnionFind {
    pub fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
        }
    }

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Adds a new singleton set and returns its id.
    pub fn push(&mut self) -> u32 {
        let id = self.parent.len() as u32;
        self.parent.push(id);
        self.rank.push(0);
        id
    }

    pub fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let grand = self.parent[self.parent[x as usize] as usize];
            self.parent[x as usize] = grand;
            x = grand;
        }
        x
    }

    pub fn union(&mut self, a: u32, b: u32) -> u32 {
        let mut ra = self.find(a);
        let mut rb = self.find(b);
        if ra == rb {
            return ra;
        }
        if self.rank[ra as usize] < self.rank[rb as usize] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb as usize] = ra;
        if self.rank[ra as usize] == self.rank[rb as usize] {
            self.rank[ra as usize] += 1;
        }
        ra
    }

    pub fn connected(&mut self, a: u32, b: u32) -> bool {
        self.find(a) == self.find(b)
    }
}

/// Result of labelling one tile.
pub struct TileLabels {
    pub width: usize,
    pub height: usize,
    /// One entry per cell, `NONE` where the cell is not part of any component.
    pub labels: Vec<u32>,
    /// Number of local components.
    pub count: u32,
    /// Cell count per local component.
    pub sizes: Vec<u32>,
}

/// 4-connected component labelling over a key grid.
///
/// Two neighbouring cells belong to the same component when both keys differ from
/// [`NONE`] *and* are equal. Passing a constant key gives plain water
/// connectivity; passing a classification signature splits a connected body of
/// water into homogeneous regions.
pub fn label_grid(width: usize, height: usize, keys: &[u32]) -> TileLabels {
    debug_assert_eq!(keys.len(), width * height);
    let mut labels = vec![NONE; width * height];
    let mut uf = UnionFind::new(0);

    for z in 0..height {
        for x in 0..width {
            let i = z * width + x;
            let k = keys[i];
            if k == NONE {
                continue;
            }
            let mut label = NONE;
            if x > 0 && keys[i - 1] == k {
                label = labels[i - 1];
            }
            if z > 0 && keys[i - width] == k {
                let north = labels[i - width];
                if label == NONE {
                    label = north;
                } else if north != label {
                    label = uf.union(label, north);
                }
            }
            if label == NONE {
                label = uf.push();
            }
            labels[i] = label;
        }
    }

    // Compact roots to a dense 0..count range and total up the sizes.
    let mut remap = vec![NONE; uf.len()];
    let mut count = 0u32;
    let mut sizes: Vec<u32> = Vec::new();
    for l in labels.iter_mut() {
        if *l == NONE {
            continue;
        }
        let root = uf.find(*l);
        let dense = if remap[root as usize] == NONE {
            let d = count;
            remap[root as usize] = d;
            count += 1;
            sizes.push(0);
            d
        } else {
            remap[root as usize]
        };
        *l = dense;
        sizes[dense as usize] += 1;
    }

    TileLabels {
        width,
        height,
        labels,
        count,
        sizes,
    }
}

/// One boundary cell of a tile: its local label and its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeCell {
    pub label: u32,
    pub key: u32,
}

impl Default for EdgeCell {
    fn default() -> Self {
        EdgeCell {
            label: NONE,
            key: NONE,
        }
    }
}

pub const EDGE_WEST: usize = 0;
pub const EDGE_EAST: usize = 1;
pub const EDGE_NORTH: usize = 2;
pub const EDGE_SOUTH: usize = 3;

/// The four boundary strips of a labelled tile, used for the merge step.
pub struct TileEdges {
    pub edges: [Vec<EdgeCell>; 4],
}

impl TileEdges {
    pub fn from_labels(labels: &TileLabels, keys: &[u32]) -> Self {
        let w = labels.width;
        let h = labels.height;
        let mut west = vec![EdgeCell::default(); h];
        let mut east = vec![EdgeCell::default(); h];
        let mut north = vec![EdgeCell::default(); w];
        let mut south = vec![EdgeCell::default(); w];
        for z in 0..h {
            west[z] = EdgeCell {
                label: labels.labels[z * w],
                key: keys[z * w],
            };
            east[z] = EdgeCell {
                label: labels.labels[z * w + (w - 1)],
                key: keys[z * w + (w - 1)],
            };
        }
        for x in 0..w {
            north[x] = EdgeCell {
                label: labels.labels[x],
                key: keys[x],
            };
            south[x] = EdgeCell {
                label: labels.labels[(h - 1) * w + x],
                key: keys[(h - 1) * w + x],
            };
        }
        TileEdges {
            edges: [west, east, north, south],
        }
    }
}

/// A labelled tile as it enters the global merge.
pub struct TileResult {
    pub rx: i32,
    pub rz: i32,
    /// Offset of this tile's local component 0 in the global id space.
    pub base: u32,
    pub count: u32,
    pub sizes: Vec<u32>,
    pub edges: TileEdges,
}

/// Global component ids after merging all tiles.
pub struct MergedComponents {
    #[allow(dead_code)]
    pub uf: UnionFind,
    /// Dense id per global (pre-merge) component, `NONE` when filtered away.
    pub dense: Vec<u32>,
    /// Column count per dense component.
    #[allow(dead_code)]
    pub sizes: Vec<u32>,
    pub count: u32,
}

impl MergedComponents {
    /// Dense component id for a tile-local label.
    #[inline]
    pub fn resolve(&self, base: u32, local: u32) -> u32 {
        if local == NONE {
            return NONE;
        }
        self.dense[(base + local) as usize]
    }
}

/// Assigns global bases, unions across tile boundaries and compacts the result.
///
/// `min_size` drops components smaller than the given number of columns, which is
/// how "a single water block is not a region" is enforced.
pub fn merge_tiles(tiles: &mut [TileResult], min_size: u32) -> MergedComponents {
    let mut total = 0u32;
    for t in tiles.iter_mut() {
        t.base = total;
        total += t.count;
    }

    let mut uf = UnionFind::new(total as usize);

    // Index tiles by their region coordinate so neighbours can be found quickly.
    let mut by_coord: std::collections::HashMap<(i32, i32), usize> =
        std::collections::HashMap::with_capacity(tiles.len());
    for (i, t) in tiles.iter().enumerate() {
        by_coord.insert((t.rx, t.rz), i);
    }

    for i in 0..tiles.len() {
        let (rx, rz, base) = (tiles[i].rx, tiles[i].rz, tiles[i].base);

        if let Some(&j) = by_coord.get(&(rx + 1, rz)) {
            let other_base = tiles[j].base;
            // A tile with no components at all carries empty edges, so the shared
            // strip is only as long as the shorter of the two.
            let n = tiles[i].edges.edges[EDGE_EAST]
                .len()
                .min(tiles[j].edges.edges[EDGE_WEST].len());
            for k in 0..n {
                let a = tiles[i].edges.edges[EDGE_EAST][k];
                let b = tiles[j].edges.edges[EDGE_WEST][k];
                if a.label != NONE && b.label != NONE && a.key == b.key {
                    uf.union(base + a.label, other_base + b.label);
                }
            }
        }

        if let Some(&j) = by_coord.get(&(rx, rz + 1)) {
            let other_base = tiles[j].base;
            let n = tiles[i].edges.edges[EDGE_SOUTH]
                .len()
                .min(tiles[j].edges.edges[EDGE_NORTH].len());
            for k in 0..n {
                let a = tiles[i].edges.edges[EDGE_SOUTH][k];
                let b = tiles[j].edges.edges[EDGE_NORTH][k];
                if a.label != NONE && b.label != NONE && a.key == b.key {
                    uf.union(base + a.label, other_base + b.label);
                }
            }
        }
    }

    // Total sizes per root.
    let mut root_size = vec![0u32; total as usize];
    for t in tiles.iter() {
        for (local, size) in t.sizes.iter().enumerate() {
            let root = uf.find(t.base + local as u32);
            root_size[root as usize] = root_size[root as usize].saturating_add(*size);
        }
    }

    let mut dense = vec![NONE; total as usize];
    let mut sizes = Vec::new();
    let mut count = 0u32;
    for id in 0..total {
        let root = uf.find(id);
        if root_size[root as usize] < min_size {
            continue;
        }
        if dense[root as usize] == NONE {
            dense[root as usize] = count;
            sizes.push(root_size[root as usize]);
            count += 1;
        }
        dense[id as usize] = dense[root as usize];
    }

    MergedComponents {
        uf,
        dense,
        sizes,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&str]) -> (usize, usize, Vec<u32>) {
        let h = rows.len();
        let w = rows[0].len();
        let mut keys = vec![NONE; w * h];
        for (z, row) in rows.iter().enumerate() {
            for (x, c) in row.chars().enumerate() {
                keys[z * w + x] = match c {
                    '.' => NONE,
                    other => other as u32,
                };
            }
        }
        (w, h, keys)
    }

    #[test]
    fn union_find_merges_and_compresses() {
        let mut uf = UnionFind::new(6);
        uf.union(0, 1);
        uf.union(1, 2);
        uf.union(4, 5);
        assert!(uf.connected(0, 2));
        assert!(uf.connected(4, 5));
        assert!(!uf.connected(0, 3));
        assert!(!uf.connected(2, 4));
        uf.union(2, 5);
        assert!(uf.connected(0, 5));
    }

    #[test]
    fn union_find_grows_on_demand() {
        let mut uf = UnionFind::new(0);
        let a = uf.push();
        let b = uf.push();
        assert_eq!((a, b), (0, 1));
        assert!(!uf.connected(a, b));
        uf.union(a, b);
        assert!(uf.connected(a, b));
    }

    #[test]
    fn labels_separate_disconnected_blobs() {
        let (w, h, keys) = grid(&[
            "ww.ww",
            "ww.ww",
            ".....",
            "wwwww",
        ]);
        let l = label_grid(w, h, &keys);
        assert_eq!(l.count, 3);
        let mut sizes = l.sizes.clone();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![4, 4, 5]);
        assert_eq!(l.labels[0], l.labels[w + 1]);
        assert_ne!(l.labels[0], l.labels[3]);
    }

    #[test]
    fn labels_split_on_differing_keys() {
        // Same water body, two signatures: must become two components.
        let (w, h, keys) = grid(&["aaabbb", "aaabbb"]);
        let l = label_grid(w, h, &keys);
        assert_eq!(l.count, 2);
        assert_eq!(l.sizes, vec![6, 6]);
    }

    #[test]
    fn u_shape_is_one_component() {
        let (w, h, keys) = grid(&[
            "w...w",
            "w...w",
            "wwwww",
        ]);
        let l = label_grid(w, h, &keys);
        assert_eq!(l.count, 1);
        assert_eq!(l.sizes, vec![9]);
    }

    /// Splits an 8x4 grid down the middle into two 4x4 tiles and checks that the
    /// boundary merge reconstructs the same components as labelling it at once.
    #[test]
    fn boundary_merge_reconnects_tiles() {
        let rows = ["wwwwwwww", "w..ww..w", "w..ww..w", "wwwwwwww"];
        let (w, h, keys) = grid(&rows);
        let whole = label_grid(w, h, &keys);
        assert_eq!(whole.count, 1);

        let mut left = vec![NONE; 4 * 4];
        let mut right = vec![NONE; 4 * 4];
        for z in 0..4 {
            for x in 0..4 {
                left[z * 4 + x] = keys[z * 8 + x];
                right[z * 4 + x] = keys[z * 8 + x + 4];
            }
        }
        let ll = label_grid(4, 4, &left);
        let rl = label_grid(4, 4, &right);
        assert_eq!(ll.count, 1);
        assert_eq!(rl.count, 1);

        let mut tiles = vec![
            TileResult {
                rx: 0,
                rz: 0,
                base: 0,
                count: ll.count,
                sizes: ll.sizes.clone(),
                edges: TileEdges::from_labels(&ll, &left),
            },
            TileResult {
                rx: 1,
                rz: 0,
                base: 0,
                count: rl.count,
                sizes: rl.sizes.clone(),
                edges: TileEdges::from_labels(&rl, &right),
            },
        ];
        let merged = merge_tiles(&mut tiles, 1);
        assert_eq!(merged.count, 1);
        assert_eq!(merged.sizes[0], whole.sizes[0]);
        assert_eq!(
            merged.resolve(tiles[0].base, 0),
            merged.resolve(tiles[1].base, 0)
        );
    }

    #[test]
    fn boundary_merge_keeps_differing_keys_apart() {
        let left = vec!['a' as u32; 4];
        let right = vec!['b' as u32; 4];
        let ll = label_grid(2, 2, &left);
        let rl = label_grid(2, 2, &right);
        let mut tiles = vec![
            TileResult {
                rx: 0,
                rz: 0,
                base: 0,
                count: ll.count,
                sizes: ll.sizes.clone(),
                edges: TileEdges::from_labels(&ll, &left),
            },
            TileResult {
                rx: 1,
                rz: 0,
                base: 0,
                count: rl.count,
                sizes: rl.sizes.clone(),
                edges: TileEdges::from_labels(&rl, &right),
            },
        ];
        let merged = merge_tiles(&mut tiles, 1);
        assert_eq!(merged.count, 2);
    }

    #[test]
    fn an_empty_neighbour_tile_does_not_panic() {
        // A tile with nothing in it has empty edge strips. Merging it next to a
        // full tile must simply find no connection, not index out of bounds.
        let keys = vec!['w' as u32; 4];
        let l = label_grid(2, 2, &keys);
        let mut tiles = vec![
            TileResult {
                rx: 0,
                rz: 0,
                base: 0,
                count: l.count,
                sizes: l.sizes.clone(),
                edges: TileEdges::from_labels(&l, &keys),
            },
            TileResult {
                rx: 1,
                rz: 0,
                base: 0,
                count: 0,
                sizes: Vec::new(),
                edges: TileEdges {
                    edges: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
                },
            },
            TileResult {
                rx: 0,
                rz: 1,
                base: 0,
                count: 0,
                sizes: Vec::new(),
                edges: TileEdges {
                    edges: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
                },
            },
        ];
        let merged = merge_tiles(&mut tiles, 1);
        assert_eq!(merged.count, 1);
        assert_eq!(merged.sizes, vec![4]);
    }

    #[test]
    fn merge_drops_components_below_min_size() {
        let (w, h, keys) = grid(&["ww.w"]);
        let l = label_grid(w, h, &keys);
        assert_eq!(l.count, 2);
        let mut tiles = vec![TileResult {
            rx: 0,
            rz: 0,
            base: 0,
            count: l.count,
            sizes: l.sizes.clone(),
            edges: TileEdges::from_labels(&l, &keys),
        }];
        let merged = merge_tiles(&mut tiles, 2);
        assert_eq!(merged.count, 1);
        assert_eq!(merged.sizes, vec![2]);
    }

    #[test]
    fn vertical_boundary_merge_works_too() {
        let top = vec!['w' as u32; 4];
        let bottom = vec!['w' as u32; 4];
        let tl = label_grid(2, 2, &top);
        let bl = label_grid(2, 2, &bottom);
        let mut tiles = vec![
            TileResult {
                rx: 0,
                rz: 0,
                base: 0,
                count: tl.count,
                sizes: tl.sizes.clone(),
                edges: TileEdges::from_labels(&tl, &top),
            },
            TileResult {
                rx: 0,
                rz: 1,
                base: 0,
                count: bl.count,
                sizes: bl.sizes.clone(),
                edges: TileEdges::from_labels(&bl, &bottom),
            },
        ];
        let merged = merge_tiles(&mut tiles, 1);
        assert_eq!(merged.count, 1);
        assert_eq!(merged.sizes, vec![8]);
    }
}
