//! Rotation-aware basin occupancy. Long curved channels may fit a square world
//! bounding box and have a wide junction, defeating a width/elongation test.
//! A lake must occupy a meaningful two-dimensional part of its overall footprint.

use super::{LakeCandidate, Raster};

pub(super) fn measure_basin_fill(r: &Raster, labels: &[u32], candidates: &mut [LakeCandidate]) {
    // Anchor moments locally to retain precision even near Minecraft's border.
    let anchors: Vec<_> = candidates
        .iter()
        .map(|c| (c.bounds[0], c.bounds[1]))
        .collect();
    let mut moments = vec![[0.0f64; 5]; candidates.len()];
    for (i, &id) in labels.iter().enumerate() {
        if id == 0 {
            continue;
        }
        let k = (id - 1) as usize;
        let (x, z) = r.coords(i);
        let (x, z) = (f64::from(x - anchors[k].0), f64::from(z - anchors[k].1));
        let m = &mut moments[k];
        m[0] += x;
        m[1] += z;
        m[2] += x * x;
        m[3] += x * z;
        m[4] += z * z;
    }
    let axes: Vec<_> = moments
        .iter()
        .zip(candidates.iter())
        .map(|(m, c)| {
            let area = c.area.max(1) as f64;
            let xx = m[2] - m[0] * m[0] / area;
            let xz = m[3] - m[0] * m[1] / area;
            let zz = m[4] - m[1] * m[1] / area;
            let angle = 0.5 * (2.0 * xz).atan2(xx - zz);
            (angle.cos(), angle.sin())
        })
        .collect();
    let mut extents = vec![
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY
        ];
        candidates.len()
    ];
    for (i, &id) in labels.iter().enumerate() {
        if id == 0 {
            continue;
        }
        let k = (id - 1) as usize;
        let (x, z) = r.coords(i);
        let (x, z) = (f64::from(x - anchors[k].0), f64::from(z - anchors[k].1));
        let (cos, sin) = axes[k];
        let (u, v) = (cos * x + sin * z, -sin * x + cos * z);
        let b = &mut extents[k];
        b[0] = b[0].min(u);
        b[1] = b[1].min(v);
        b[2] = b[2].max(u);
        b[3] = b[3].max(v);
    }
    for (k, c) in candidates.iter_mut().enumerate() {
        if c.area == 0 {
            continue;
        }
        let axis_area = (f64::from(c.bounds[2] - c.bounds[0]) + 1.0)
            * (f64::from(c.bounds[3] - c.bounds[1]) + 1.0);
        let b = extents[k];
        // Include the projected extent of a block square, not just its center.
        let block_span = axes[k].0.abs() + axes[k].1.abs();
        let oriented_area = (b[2] - b[0] + block_span) * (b[3] - b[1] + block_span);
        c.axis_fill_ratio = (c.area as f64 / axis_area).clamp(0.0, 1.0) as f32;
        // PCA is an approximation, so retain whichever rectangle fits better.
        c.basin_fill_ratio = (c.area as f64 / axis_area.min(oriented_area)).clamp(0.0, 1.0) as f32;
    }
}
