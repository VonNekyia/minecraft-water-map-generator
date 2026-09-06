//! The data model written to `water_regions.bin`.
//!
//! Everything here is purely geographic / environmental. There is deliberately no
//! notion of fish, loot, ships or any other gameplay concept - consumers decide
//! that themselves from `kind`, `temperature`, `vegetation`, `depth` and
//! `modifiers`.

use serde::Serialize;

/// The basic type of a body of water. Special properties live in [`Modifiers`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum WaterKind {
    Sea = 0,
    River = 1,
    Lake = 2,
    Swamp = 3,
}

impl WaterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WaterKind::Sea => "sea",
            WaterKind::River => "river",
            WaterKind::Lake => "lake",
            WaterKind::Swamp => "swamp",
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => WaterKind::Sea,
            1 => WaterKind::River,
            2 => WaterKind::Lake,
            3 => WaterKind::Swamp,
            _ => return None,
        })
    }
}

/// How warm the water is. Derived from the biome, which is the only thing in a
/// Minecraft world that says anything about water temperature.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Temperature {
    Warm = 0,
    Medium = 1,
    Cold = 2,
}

impl Temperature {
    pub fn as_str(self) -> &'static str {
        match self {
            Temperature::Warm => "warm",
            Temperature::Medium => "medium",
            Temperature::Cold => "cold",
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Temperature::Warm,
            1 => Temperature::Medium,
            2 => Temperature::Cold,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Vegetation {
    None = 0,
    Sparse = 1,
    Normal = 2,
    Jungle = 3,
}

impl Vegetation {
    pub fn as_str(self) -> &'static str {
        match self {
            Vegetation::None => "none",
            Vegetation::Sparse => "sparse",
            Vegetation::Normal => "normal",
            Vegetation::Jungle => "jungle",
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Vegetation::None,
            1 => Vegetation::Sparse,
            2 => Vegetation::Normal,
            3 => Vegetation::Jungle,
            _ => return None,
        })
    }

    /// Moves one step towards `Jungle`, saturating.
    pub fn promote(self) -> Self {
        match self {
            Vegetation::None => Vegetation::Sparse,
            Vegetation::Sparse => Vegetation::Normal,
            _ => Vegetation::Jungle,
        }
    }

    /// Moves one step towards `None`, saturating.
    pub fn demote(self) -> Self {
        match self {
            Vegetation::Jungle => Vegetation::Normal,
            Vegetation::Normal => Vegetation::Sparse,
            _ => Vegetation::None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Depth {
    Shallow = 0,
    Normal = 1,
    Deep = 2,
}

impl Depth {
    pub fn as_str(self) -> &'static str {
        match self {
            Depth::Shallow => "shallow",
            Depth::Normal => "normal",
            Depth::Deep => "deep",
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Depth::Shallow,
            1 => Depth::Normal,
            2 => Depth::Deep,
            _ => return None,
        })
    }

    /// Classifies a *measured* mean water depth in blocks using the configured
    /// thresholds. Nothing about the biome enters here - this is the distance
    /// from the water surface down to the floor, and nothing else.
    pub fn classify(blocks: i32) -> Depth {
        if blocks <= crate::config::DEPTH_SHALLOW_MAX {
            Depth::Shallow
        } else if blocks <= crate::config::DEPTH_NORMAL_MAX {
            Depth::Normal
        } else {
            Depth::Deep
        }
    }
}

bitflags::bitflags! {
    /// Special properties of a water region. Several may be set at once.
    ///
    /// The encoding is stable: new modifiers must only ever be appended, never
    /// renumbered, so old readers keep working.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub struct Modifiers: u16 {
        const ICE      = 1 << 0;
        const CORALS   = 1 << 1;
        const DESERT   = 1 << 2;
        const MANGROVE = 1 << 3;
        /// Water with no view of the sky: an underground pool or aquifer.
        const CAVE     = 1 << 4;
    }
}

#[allow(dead_code)]
impl Modifiers {
    pub const NAMES: [(Modifiers, &'static str); 5] = [
        (Modifiers::ICE, "ice"),
        (Modifiers::CORALS, "corals"),
        (Modifiers::DESERT, "desert"),
        (Modifiers::MANGROVE, "mangrove"),
        (Modifiers::CAVE, "cave"),
    ];

    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(f, _)| self.contains(*f))
            .map(|(_, n)| *n)
            .collect()
    }
}

/// One horizontal run of water columns belonging to a region: `z`, `x0..=x1`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Run {
    pub z: i32,
    pub x0: i32,
    pub x1: i32,
}

#[allow(dead_code)]
impl Run {
    #[inline]
    pub fn len(&self) -> u32 {
        (self.x1 - self.x0 + 1) as u32
    }
}

/// Compact geometry of a water region: run-length encoded scanlines plus a
/// bounding box for cheap rejection.
#[derive(Clone, Debug, Default)]
pub struct RegionGeometry {
    pub min_x: i32,
    pub min_z: i32,
    pub max_x: i32,
    pub max_z: i32,
    /// Sorted by `(z, x0)`.
    pub runs: Vec<Run>,
    pub column_count: u32,
}

impl RegionGeometry {
    /// Outline length of the shape in block edges.
    ///
    /// Walks the run-length rows and counts the edges that face something other
    /// than this region: the two ends of every run, plus whatever is not covered
    /// by the rows above and below.
    pub fn perimeter(&self) -> u64 {
        let mut per: u64 = 0;
        let mut i = 0usize;
        // Row boundaries, so the rows above and below can be found by index.
        let mut rows: Vec<(i32, usize, usize)> = Vec::new();
        while i < self.runs.len() {
            let z = self.runs[i].z;
            let start = i;
            while i < self.runs.len() && self.runs[i].z == z {
                i += 1;
            }
            rows.push((z, start, i));
        }

        for (r, &(z, start, end)) in rows.iter().enumerate() {
            for run in &self.runs[start..end] {
                per += 2; // the two ends
                let len = run.len() as u64;
                for neighbour in [
                    r.checked_sub(1).and_then(|k| rows.get(k)).filter(|n| n.0 == z - 1),
                    rows.get(r + 1).filter(|n| n.0 == z + 1),
                ] {
                    let Some(&(_, ns, ne)) = neighbour else {
                        per += len; // nothing on that side at all
                        continue;
                    };
                    let mut covered: u64 = 0;
                    for other in &self.runs[ns..ne] {
                        let lo = run.x0.max(other.x0);
                        let hi = run.x1.min(other.x1);
                        if lo <= hi {
                            covered += (hi - lo + 1) as u64;
                        }
                    }
                    per += len - covered;
                }
            }
        }
        per
    }

    /// Characteristic width of the shape in blocks, `2 * area / perimeter`.
    ///
    /// Orientation independent, unlike a bounding box: a long strip of width `w`
    /// gives `w`, a disc of radius `r` gives `r`, and a river that meanders all
    /// over its bounding box still gives the width of the river.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn mean_width(&self) -> f32 {
        let per = self.perimeter();
        if per == 0 {
            return 0.0;
        }
        2.0 * self.column_count as f32 / per as f32
    }

    /// How many times longer than wide the shape is.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn elongation(&self) -> f32 {
        let w = self.mean_width();
        if w <= 0.0 {
            return 0.0;
        }
        (self.column_count as f32 / w) / w
    }

    pub fn contains(&self, x: i32, z: i32) -> bool {
        if x < self.min_x || x > self.max_x || z < self.min_z || z > self.max_z {
            return false;
        }
        // Runs are sorted by (z, x0): binary search for the first run of this row
        // that could contain x.
        let idx = self
            .runs
            .partition_point(|r| (r.z, r.x1) < (z, x));
        match self.runs.get(idx) {
            Some(r) => r.z == z && x >= r.x0 && x <= r.x1,
            None => false,
        }
    }
}

/// Bathymetry summary for sea regions (secondary data, see spec section 22).
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Bathymetry {
    /// Mean depth in blocks.
    pub mean_depth: u16,
    pub max_depth: u16,
    /// Share of columns deeper than `config::BATHYMETRY_CONTOURS[i]`, 0..=255.
    pub contour_shares: [u8; 4],
}

/// A classified body of water.
#[derive(Clone, Debug)]
pub struct WaterRegion {
    pub id: u32,
    pub geometry: RegionGeometry,
    pub kind: WaterKind,
    pub temperature: Temperature,
    pub vegetation: Vegetation,
    pub depth: Option<Depth>,
    pub modifiers: Modifiers,
    /// Mean water surface Y of the region.
    pub surface_y: i16,
    pub bathymetry: Bathymetry,
    /// Dominant biome, for debugging and for consumers that want it.
    pub dominant_biome: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_thresholds_follow_the_configured_bands() {
        assert_eq!(Depth::classify(0), Depth::Shallow);
        assert_eq!(Depth::classify(10), Depth::Shallow);
        assert_eq!(Depth::classify(11), Depth::Normal);
        assert_eq!(Depth::classify(30), Depth::Normal);
        assert_eq!(Depth::classify(31), Depth::Deep);
        assert_eq!(Depth::classify(200), Depth::Deep);
    }

    #[test]
    fn modifier_flags_combine_and_name_themselves() {
        let m = Modifiers::ICE | Modifiers::CORALS;
        assert!(m.contains(Modifiers::ICE));
        assert!(m.contains(Modifiers::CORALS));
        assert!(!m.contains(Modifiers::DESERT));
        assert_eq!(m.names(), vec!["ice", "corals"]);
        assert_eq!(m.bits(), 0b11);

        let all = Modifiers::all();
        assert_eq!(
            all.names(),
            vec!["ice", "corals", "desert", "mangrove", "cave"]
        );
    }

    #[test]
    fn enum_roundtrips_through_their_wire_values() {
        for v in 0..4u8 {
            assert_eq!(WaterKind::from_u8(v).unwrap() as u8, v);
            assert_eq!(Vegetation::from_u8(v).unwrap() as u8, v);
        }
        assert!(WaterKind::from_u8(4).is_none());
        for v in 0..3u8 {
            assert_eq!(Temperature::from_u8(v).unwrap() as u8, v);
            assert_eq!(Depth::from_u8(v).unwrap() as u8, v);
        }
        assert!(Temperature::from_u8(3).is_none());
        assert!(Depth::from_u8(3).is_none());
    }

    /// A filled rectangle `w` wide and `h` tall.
    fn rect(w: i32, h: i32) -> RegionGeometry {
        RegionGeometry {
            min_x: 0,
            min_z: 0,
            max_x: w - 1,
            max_z: h - 1,
            runs: (0..h)
                .map(|z| Run {
                    z,
                    x0: 0,
                    x1: w - 1,
                })
                .collect(),
            column_count: (w * h) as u32,
        }
    }

    #[test]
    fn perimeter_counts_the_outline() {
        // A 3x2 rectangle has an outline of 10 block edges.
        assert_eq!(rect(3, 2).perimeter(), 10);
        // A single block is surrounded on four sides.
        assert_eq!(rect(1, 1).perimeter(), 4);
    }

    #[test]
    fn mean_width_converges_on_the_width_of_a_strip() {
        // `2 * area / perimeter` reaches the true width only as the strip gets
        // long - the two ends eat into it - but it never overshoots and it does
        // not depend on which way the strip runs.
        let mut last = 0.0f32;
        for length in [50, 200, 800] {
            let w = rect(8, length).mean_width();
            assert!(w > last, "width should grow with length, {w:.2} <= {last:.2}");
            assert!(w <= 8.0, "width {w:.2} overshoots the real 8");
            last = w;
        }
        assert!(last > 7.5, "800 blocks long should be close to 8, got {last:.2}");
    }

    #[test]
    fn elongation_separates_strips_from_blobs() {
        // A square is four times longer than wide by this measure; a long strip
        // is far more, and that gap is what tells a river from a pool.
        let square = rect(100, 100);
        assert!((square.mean_width() - 50.0).abs() < 1.0);
        assert!(square.elongation() < 5.0);

        let strip = rect(8, 400);
        assert!(strip.elongation() > 40.0);
    }

    #[test]
    fn an_empty_geometry_has_no_shape() {
        let empty = RegionGeometry::default();
        assert_eq!(empty.perimeter(), 0);
        assert_eq!(empty.mean_width(), 0.0);
        assert_eq!(empty.elongation(), 0.0);
    }

    #[test]
    fn geometry_point_lookup_uses_the_runs() {
        let geo = RegionGeometry {
            min_x: 0,
            min_z: 0,
            max_x: 10,
            max_z: 2,
            runs: vec![
                Run { z: 0, x0: 0, x1: 3 },
                Run { z: 0, x0: 7, x1: 10 },
                Run { z: 2, x0: 5, x1: 5 },
            ],
            column_count: 9,
        };
        assert!(geo.contains(0, 0));
        assert!(geo.contains(3, 0));
        assert!(!geo.contains(4, 0));
        assert!(!geo.contains(6, 0));
        assert!(geo.contains(7, 0));
        assert!(geo.contains(10, 0));
        assert!(!geo.contains(11, 0));
        assert!(!geo.contains(5, 1));
        assert!(geo.contains(5, 2));
        assert!(!geo.contains(4, 2));
    }

    #[test]
    fn vegetation_promote_and_demote_saturate() {
        assert_eq!(Vegetation::None.promote(), Vegetation::Sparse);
        assert_eq!(Vegetation::Normal.promote(), Vegetation::Jungle);
        assert_eq!(Vegetation::Jungle.promote(), Vegetation::Jungle);
        assert_eq!(Vegetation::Jungle.demote(), Vegetation::Normal);
        assert_eq!(Vegetation::None.demote(), Vegetation::None);
    }
}
