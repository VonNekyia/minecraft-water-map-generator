use super::raster::NONE;
use super::{Connection, LakeAnalysis, LakeCandidate, LakeOptions, Raster};
use crate::water::grid::WorldGrid;
use crate::water::model::WaterRegion;
use std::collections::VecDeque;

pub fn analyze(grid: &WorldGrid, regions: &[WaterRegion], options: &LakeOptions) -> LakeAnalysis {
    let started = std::time::Instant::now();
    let raster = Raster::from_regions(regions);
    println!(
        "    lake raster          {} allocated cells ({:.1}s)",
        raster.len(),
        started.elapsed().as_secs_f64()
    );
    let distance = raster.distances();
    println!(
        "    shore distance       ready ({:.1}s)",
        started.elapsed().as_secs_f64()
    );
    let density = raster.densities();
    println!(
        "    local densities      ready ({:.1}s)",
        started.elapsed().as_secs_f64()
    );
    let (cores, mut candidates) = lake_cores(&raster, &distance, &density, options);
    println!(
        "    lake cores           {} ({:.1}s)",
        candidates.len(),
        started.elapsed().as_secs_f64()
    );
    let (provisional, core_steps) =
        channel_watershed(&raster, &distance, &cores, &candidates, options);
    println!(
        "    channel watershed    ready ({:.1}s)",
        started.elapsed().as_secs_f64()
    );
    let (mut necks, connections) = find_connections(
        &raster,
        &distance,
        &provisional,
        &core_steps,
        &candidates,
        options,
    );
    drop(core_steps);
    for (candidate, connections) in candidates.iter_mut().zip(connections) {
        candidate.connections = connections;
    }
    // Reconstruction is a flood of original water, stopped by the watershed's
    // lake/channel cuts. Shoreline cells, islands and short bays remain intact.
    let mut reconstructed = reconstruct(&raster, &cores, &provisional, &necks, &candidates);
    recover_bank_fragments(grid, &raster, &mut reconstructed, options.min_core_area);
    for (i, neck) in necks.iter_mut().enumerate() {
        *neck &= reconstructed[i] == 0;
    }
    println!(
        "    lake reconstruction  ready ({:.1}s)",
        started.elapsed().as_secs_f64()
    );
    score_candidates(
        grid,
        regions,
        &raster,
        &distance,
        &density,
        &reconstructed,
        &mut candidates,
        options,
    );
    LakeAnalysis {
        raster,
        distance,
        density,
        cores,
        reconstructed,
        necks,
        candidates,
        options: options.clone(),
    }
}

fn lake_cores(
    r: &Raster,
    distance: &[u16],
    density: &[[u8; 3]],
    opts: &LakeOptions,
) -> (Vec<u32>, Vec<LakeCandidate>) {
    let mut labels = vec![0; r.len()];
    let mut candidates = Vec::new();
    let thresholds = [opts.density_8, opts.density_16, opts.density_32];
    let qualifies = |i: usize| {
        r.is_water(i)
            && distance[i] >= opts.core_radius
            && (0..3).all(|k| density[i][k] as f32 / 255.0 >= thresholds[k])
    };
    let mut queue = VecDeque::new();
    for i in 0..r.len() {
        if labels[i] != 0 || !qualifies(i) {
            continue;
        }
        let id = candidates.len() as u32 + 1;
        labels[i] = id;
        queue.push_back(i);
        let mut area = 0;
        let mut max_radius = 0;
        let mut radius_sum = 0u64;
        let mut bounds = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
        while let Some(p) = queue.pop_front() {
            let (x, z) = r.coords(p);
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(z);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(z);
            area += 1;
            max_radius = max_radius.max(distance[p]);
            radius_sum += u64::from(distance[p]);
            for n in r.neighbors(p).into_iter().flatten() {
                if labels[n] == 0 && qualifies(n) {
                    labels[n] = id;
                    queue.push_back(n);
                }
            }
        }
        // A blended mean/maximum resists one unusually deep interior pixel.
        let radius = (radius_sum as f32 / area as f32 + max_radius as f32) * 0.5;
        candidates.push(LakeCandidate {
            id: id - 1,
            core_area: area,
            bounds,
            characteristic_width: 2.0 * radius,
            rejection: (area < opts.min_core_area).then(|| "core_too_small".into()),
            ..LakeCandidate::default()
        });
    }
    (labels, candidates)
}

/// Marker-controlled maximum-clearance flood. Narrow channels distant from all
/// cores are river markers; short shoreline indentations have no river marker.
/// Descending distance buckets find saddles in near-linear time without a heap.
fn channel_watershed(
    r: &Raster,
    distance: &[u16],
    cores: &[u32],
    candidates: &[LakeCandidate],
    opts: &LakeOptions,
) -> (Vec<u32>, Vec<u32>) {
    let mut nearest = vec![0u32; r.len()];
    let mut travel = vec![u32::MAX; r.len()];
    let mut queue = VecDeque::new();
    for i in 0..r.len() {
        if cores[i] > 0 && candidates[(cores[i] - 1) as usize].rejection.is_none() {
            nearest[i] = cores[i];
            travel[i] = 0;
            queue.push_back(i as u32);
        }
    }
    while let Some(p) = queue.pop_front() {
        let p = p as usize;
        for n in r.neighbors(p).into_iter().flatten() {
            if r.is_water(n) && travel[n] == u32::MAX {
                travel[n] = travel[p] + 1;
                nearest[n] = nearest[p];
                queue.push_back(n as u32);
            }
        }
    }
    let mut owner = vec![NONE; r.len()];
    for i in 0..r.len() {
        if !r.is_water(i) {
            owner[i] = 0;
            continue;
        }
        let id = nearest[i];
        if id == 0 {
            owner[i] = 0;
            continue;
        }
        let candidate = &candidates[(id - 1) as usize];
        let reach = (candidate.characteristic_width * 0.5 * opts.river_seed_distance_factor)
            .max(opts.min_channel_length as f32) as u32;
        let d = distance[i];
        let (x, z) = r.coords(i);
        let sample = |dx: i32, dz: i32| r.index_at(x + dx, z + dz).map_or(0, |n| distance[n]);
        // Both sides must descend across a two-block cross-section. Testing
        // only one immediate descent mistakes stair-stepped banks for medial
        // channel ridges. Two blocks also admit even-width medial plateaus.
        let ridge = |dx, dz| {
            d >= sample(dx, dz)
                && d >= sample(-dx, -dz)
                && d > sample(2 * dx, 2 * dz)
                && d > sample(-2 * dx, -2 * dz)
        };
        // Only persistently narrow medial water seeds a river. A far-away
        // shoreline is not a river marker: that would strip the bays and ends
        // of long lakes whose confident core occupies only their widest part.
        let narrow = 2.0 * d as f32 <= candidate.characteristic_width * opts.neck_width_ratio;
        if travel[i] > reach && narrow && (ridge(1, 0) || ridge(0, 1)) {
            owner[i] = NONE - 1;
        } else if cores[i] == id {
            owner[i] = id;
        }
    }
    drop(nearest);
    // A genuine channel has a sustained medial ridge. Isolated shore corners
    // and small indented bays must not create competing river markers.
    let mut points = Vec::new();
    for i in 0..r.len() {
        if owner[i] != NONE - 1 {
            continue;
        }
        points.clear();
        owner[i] = NONE;
        queue.push_back(i as u32);
        let (x, z) = r.coords(i);
        let mut bounds = [x, z, x, z];
        while let Some(p) = queue.pop_front() {
            let p = p as usize;
            points.push(p);
            let (x, z) = r.coords(p);
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(z);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(z);
            for dz in -1..=1 {
                for dx in -1..=1 {
                    if let Some(n) = r.index_at(x + dx, z + dz) {
                        if owner[n] == NONE - 1 {
                            owner[n] = NONE;
                            queue.push_back(n as u32);
                        }
                    }
                }
            }
        }
        let span = (bounds[2] - bounds[0]).max(bounds[3] - bounds[1]) as u32 + 1;
        if span >= u32::from(opts.min_channel_length) {
            for &p in &points {
                owner[p] = 0;
            }
        }
    }
    let max_distance = distance.iter().copied().max().unwrap_or(0) as usize;
    let mut buckets: Vec<VecDeque<u32>> = (0..=max_distance).map(|_| VecDeque::new()).collect();
    for i in 0..r.len() {
        if r.is_water(i)
            && owner[i] != NONE
            && r.neighbors(i)
                .into_iter()
                .flatten()
                .any(|n| owner[n] == NONE)
        {
            buckets[distance[i] as usize].push_back(i as u32);
        }
    }
    for level in (0..=max_distance).rev() {
        while let Some(p) = buckets[level].pop_front() {
            let p = p as usize;
            for n in r.neighbors(p).into_iter().flatten() {
                if !r.is_water(n) || owner[n] != NONE {
                    continue;
                }
                owner[n] = owner[p];
                buckets[level.min(distance[n] as usize)].push_back(n as u32);
            }
        }
    }
    for o in &mut owner {
        if *o == NONE {
            *o = 0;
        }
    }
    (owner, travel)
}

fn find_connections(
    r: &Raster,
    distance: &[u16],
    owner: &[u32],
    core_steps: &[u32],
    candidates: &[LakeCandidate],
    opts: &LakeOptions,
) -> (Vec<bool>, Vec<Vec<Connection>>) {
    let mut boundary = vec![false; r.len()];
    for i in 0..r.len() {
        boundary[i] = owner[i] > 0
            && r.neighbors(i)
                .into_iter()
                .flatten()
                .any(|n| r.is_water(n) && owner[n] != owner[i]);
    }
    let mut visited = vec![false; r.len()];
    let mut necks = vec![false; r.len()];
    let mut connections: Vec<Vec<Connection>> = vec![Vec::new(); candidates.len()];
    let mut queue = VecDeque::new();
    let mut points = Vec::new();
    for i in 0..r.len() {
        if !boundary[i] || visited[i] {
            continue;
        }
        let id = owner[i];
        visited[i] = true;
        queue.push_back(i);
        points.clear();
        let mut peak = i;
        while let Some(p) = queue.pop_front() {
            points.push(p);
            if distance[p] > distance[peak] {
                peak = p;
            }
            let (x, z) = r.coords(p);
            // A diagonal staircase is one cross-section, not many outlets.
            for dz in -1..=1 {
                for dx in -1..=1 {
                    if let Some(n) = r.index_at(x + dx, z + dz) {
                        if boundary[n] && !visited[n] && owner[n] == id {
                            visited[n] = true;
                            queue.push_back(n);
                        }
                    }
                }
            }
        }
        let width = 2.0 * distance[peak] as f32;
        let ratio = width / candidates[(id - 1) as usize].characteristic_width.max(1.0);
        let confirmed = ratio <= opts.neck_width_ratio;
        // Nearby cores can meet before there is room for a river marker. Their
        // narrow saddle is still a channel; a broad shared interior is merely
        // an internal core partition, not a hydrological connection.
        let touches_river = points.iter().any(|&p| {
            r.neighbors(p)
                .into_iter()
                .flatten()
                .any(|n| r.is_water(n) && owner[n] == 0)
        });
        if !touches_river && !confirmed {
            continue;
        }
        if confirmed {
            // A distance plateau along a constant-width channel has no unique
            // watershed saddle. Trace its medial high-clearance path toward the
            // core and place the cut at the first real widening, not halfway
            // between arbitrary markers far down the channel.
            // The interface can hit one side of a diagonal medial ridge. Use
            // its immediate local maximum so normal one-block alternation is
            // not mistaken for an entrance into a wider basin.
            let channel_radius = r
                .neighbors(peak)
                .into_iter()
                .flatten()
                .filter(|&n| owner[n] == id)
                .map(|n| distance[n])
                .max()
                .unwrap_or(distance[peak])
                .max(distance[peak]);
            let mut direction = None;
            let mut trail = vec![peak];
            loop {
                let next = r
                    .neighbors(peak)
                    .into_iter()
                    .flatten()
                    .filter(|&n| owner[n] == id && core_steps[n] < core_steps[peak])
                    .max_by_key(|&n| (distance[n], std::cmp::Reverse(core_steps[n])));
                let Some(next) = next else {
                    break;
                };
                let a = r.coords(peak);
                let b = r.coords(next);
                direction = Some((b.0 - a.0, b.1 - a.1));
                if distance[next] > channel_radius {
                    break;
                }
                peak = next;
                trail.push(peak);
            }
            if let Some((mut dx, mut dz)) = direction {
                if trail.len() > 1 {
                    let a = r.coords(trail[0]);
                    let b = r.coords(trail[8.min(trail.len() - 1)]);
                    dx = b.0 - a.0;
                    dz = b.1 - a.1;
                }
                let initial = cross_section(r, owner, trail[0], dx, dz, usize::MAX);
                // Manhattan clearance can stay flat even after a diagonal
                // cross-section has entered the lake. Check the actual water
                // transect too, so that the cut cannot shave off a lake corner.
                let cut_limit = initial.len() + 2;
                for t in (0..trail.len()).rev() {
                    if t > 0 {
                        let a = r.coords(trail[t.saturating_sub(8)]);
                        let b = r.coords(trail[t]);
                        dx = b.0 - a.0;
                        dz = b.1 - a.1;
                    }
                    let cut = cross_section(r, owner, trail[t], dx, dz, cut_limit);
                    if cut.len() <= cut_limit {
                        peak = trail[t];
                        for p in cut {
                            necks[p] = true;
                        }
                        break;
                    }
                }
            } else {
                for &p in &points {
                    necks[p] = true;
                }
            }
        }
        let (x, z) = r.coords(peak);
        connections[(id - 1) as usize].push(Connection {
            x,
            z,
            width,
            width_ratio: ratio,
            confirmed_neck: confirmed,
            direction: "unknown".into(),
            water_head_difference: None,
        });
    }
    (necks, connections)
}

/// Eight-connected digital transect, hence a barrier for the four-connected
/// water flood. Stop early when a proposed cut is already too wide.
fn cross_section(
    r: &Raster,
    owner: &[u32],
    p: usize,
    dx: i32,
    dz: i32,
    limit: usize,
) -> Vec<usize> {
    let (x, z) = r.coords(p);
    let span = dx.abs().max(dz.abs()).max(1) as f64;
    let mut cut = vec![p];
    for sign in [-1, 1] {
        let mut step = 1;
        loop {
            let nx = x + (-dz as f64 * sign as f64 * step as f64 / span).round() as i32;
            let nz = z + (dx as f64 * sign as f64 * step as f64 / span).round() as i32;
            let Some(n) = r.index_at(nx, nz) else {
                break;
            };
            if !r.is_water(n) || owner[n] != owner[p] {
                break;
            }
            cut.push(n);
            if cut.len() > limit {
                return cut;
            }
            step += 1;
        }
    }
    cut
}

fn reconstruct(
    r: &Raster,
    cores: &[u32],
    provisional: &[u32],
    necks: &[bool],
    candidates: &[LakeCandidate],
) -> Vec<u32> {
    let mut out = vec![0; r.len()];
    let mut queue = VecDeque::new();
    for i in 0..r.len() {
        if cores[i] > 0 && candidates[(cores[i] - 1) as usize].rejection.is_none() {
            out[i] = cores[i];
            queue.push_back(i as u32);
        }
    }
    while let Some(p) = queue.pop_front() {
        let p = p as usize;
        for n in r.neighbors(p).into_iter().flatten() {
            if r.is_water(n) && !necks[n] && out[n] == 0 && provisional[n] == out[p] {
                out[n] = out[p];
                queue.push_back(n as u32);
            }
        }
    }
    out
}

fn surface(grid: &WorldGrid, regions: &[WaterRegion], r: &Raster, i: usize) -> f32 {
    let (x, z) = r.coords(i);
    grid.cell_at(x, z)
        .map(|c| c.surface_y)
        .unwrap_or(regions[r.source[i] as usize].surface_y) as f32
}

/// Digital neck cuts can strand tiny bank tips. Recover those belonging to a
/// single basin, without erasing a short channel between two basins or a handoff
/// to protected water. This changes labels only, never the original water mask.
pub(super) fn recover_bank_fragments(grid: &WorldGrid, r: &Raster, labels: &mut [u32], limit: u32) {
    let mut seen = vec![false; r.len()];
    let mut queue = VecDeque::new();
    let mut points = Vec::new();
    for i in 0..r.len() {
        if !r.is_water(i) || labels[i] != 0 || seen[i] {
            continue;
        }
        seen[i] = true;
        queue.push_back(i as u32);
        points.clear();
        let mut area = 0u32;
        let mut basin = 0;
        let mut several_or_protected = false;
        while let Some(p) = queue.pop_front() {
            let p = p as usize;
            area += 1;
            if area <= limit {
                points.push(p);
            }
            let (x, z) = r.coords(p);
            for (n, (dx, dz)) in r
                .neighbors(p)
                .into_iter()
                .zip([(-1, 0), (1, 0), (0, -1), (0, 1)])
            {
                if let Some(n) = n.filter(|&n| r.is_water(n)) {
                    if labels[n] > 0 {
                        if basin != 0 && basin != labels[n] {
                            several_or_protected = true;
                        }
                        basin = labels[n];
                    } else if !seen[n] {
                        seen[n] = true;
                        queue.push_back(n as u32);
                    }
                } else if grid.is_water(x + dx, z + dz) {
                    several_or_protected = true;
                }
            }
        }
        if area <= limit && basin != 0 && !several_or_protected {
            for &p in &points {
                labels[p] = basin;
            }
        }
    }
}

fn score_candidates(
    grid: &WorldGrid,
    regions: &[WaterRegion],
    r: &Raster,
    distance: &[u16],
    density: &[[u8; 3]],
    labels: &[u32],
    candidates: &mut [LakeCandidate],
    opts: &LakeOptions,
) {
    let mut sums = vec![[0u64; 8]; candidates.len()];
    let mut surface_sums = vec![0.0f64; candidates.len()];
    for i in 0..r.len() {
        let id = labels[i];
        if id == 0 {
            continue;
        }
        let k = (id - 1) as usize;
        let c = &mut candidates[k];
        let s = &mut sums[k];
        let (x, z) = r.coords(i);
        c.area += 1;
        surface_sums[k] += f64::from(surface(grid, regions, r, i));
        c.bounds[0] = c.bounds[0].min(x);
        c.bounds[1] = c.bounds[1].min(z);
        c.bounds[2] = c.bounds[2].max(x);
        c.bounds[3] = c.bounds[3].max(z);
        c.max_distance_to_shore = c.max_distance_to_shore.max(distance[i]);
        s[0] += u64::from(distance[i]);
        for j in 0..3 {
            s[1 + j] += u64::from(density[i][j]);
        }
        for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            if r.index_at(x + dx, z + dz).is_some_and(|n| r.is_water(n)) {
                continue;
            }
            // Ocean/swamp water is a protected handoff, not a terrain bank.
            if grid.is_water(x + dx, z + dz) {
                continue;
            }
            s[4] += 1;
            if let Some(ground) = grid.terrain_y_at(x + dx, z + dz) {
                s[5] += 1;
                if ground as f32 >= surface(grid, regions, r, i) + opts.terrain_rise as f32 {
                    s[6] += 1;
                }
            }
        }
    }
    super::geometry::measure_basin_fill(r, labels, candidates);
    // One bounded channel BFS per connection; sample heads away from the neck.
    // Flat water has no identifiable flow direction and stays explicitly unknown.
    let mut seen = vec![0u32; r.len()];
    let mut stamp = 0u32;
    let mut queue = VecDeque::new();
    for (k, c) in candidates.iter_mut().enumerate() {
        if c.area == 0 {
            continue;
        }
        c.connections.retain(|connection| {
            r.index_at(connection.x, connection.z).is_some_and(|p| {
                r.neighbors(p)
                    .into_iter()
                    .flatten()
                    .any(|n| r.is_water(n) && labels[n] == 0)
            })
        });
        for connection in &mut c.connections {
            stamp += 1;
            let Some(start) = r.index_at(connection.x, connection.z) else {
                continue;
            };
            // The cut itself belongs to the channel and may already be below
            // a drop or above a step. Reference actual reconstructed lake water.
            let lake_y = (surface_sums[k] / c.area as f64) as f32;
            let mut sum = 0.0;
            let mut count = 0;
            for n in r.neighbors(start).into_iter().flatten() {
                if r.is_water(n) && labels[n] == 0 && seen[n] != stamp {
                    seen[n] = stamp;
                    queue.push_back((n, 0u16));
                }
            }
            while let Some((p, d)) = queue.pop_front() {
                if d >= opts.connection_sample_distance / 2 {
                    sum += surface(grid, regions, r, p);
                    count += 1;
                }
                if d >= opts.connection_sample_distance {
                    continue;
                }
                for n in r.neighbors(p).into_iter().flatten() {
                    if r.is_water(n) && labels[n] == 0 && seen[n] != stamp {
                        seen[n] = stamp;
                        queue.push_back((n, d + 1));
                    }
                }
            }
            if count > 0 {
                let head = sum / count as f32 - lake_y;
                connection.water_head_difference = Some(head);
                if head >= opts.flow_head_difference {
                    connection.direction = "inflow".into();
                    c.inflow_count += 1;
                } else if head <= -opts.flow_head_difference {
                    connection.direction = "outflow".into();
                    c.outflow_count += 1;
                } else {
                    c.unknown_connection_count += 1;
                }
            } else {
                c.unknown_connection_count += 1;
            }
        }
        let s = sums[k];
        let area = c.area as f32;
        c.mean_distance_to_shore = s[0] as f32 / area;
        c.density_8 = s[1] as f32 / area / 255.0;
        c.density_16 = s[2] as f32 / area / 255.0;
        c.density_32 = s[3] as f32 / area / 255.0;
        c.terrain_sample_coverage = s[5] as f32 / s[4].max(1) as f32;
        c.terrain_basin_score = if s[5] > 0 {
            s[6] as f32 / s[5] as f32
        } else {
            0.5
        };
        c.channel_elongation = area / (2.0 * c.max_distance_to_shore.max(1) as f32).powi(2);
        c.width_score = (c.characteristic_width / (4.0 * opts.core_radius as f32)).clamp(0.0, 1.0);
        c.core_score = (c.core_area as f32 / (opts.min_core_area as f32 * 4.0)).clamp(0.0, 1.0);
        c.density_score = (0.4 * c.density_16 + 0.6 * c.density_32).clamp(0.0, 1.0);
        c.widening_score = if c.connections.is_empty() {
            0.7
        } else {
            c.connections
                .iter()
                .map(|x| (1.0 - x.width_ratio).clamp(0.0, 1.0))
                .sum::<f32>()
                / c.connections.len() as f32
        };
        c.area_score = (area / (opts.min_lake_area as f32 * 2.0)).clamp(0.0, 1.0);
        c.shape_score =
            (1.0 - c.channel_elongation / (opts.max_channel_elongation * 2.0)).clamp(0.0, 1.0);
        c.flow_score = 1.0 / (1.0 + c.outflow_count.saturating_sub(1) as f32 * 0.5);
        c.confidence = (0.10 * c.width_score
            + 0.12 * c.core_score
            + 0.20 * c.density_score
            + 0.15 * c.widening_score
            + 0.08 * c.area_score
            + 0.20 * c.shape_score
            + 0.10 * c.terrain_basin_score
            + 0.05 * c.flow_score)
            .clamp(0.0, 1.0);
        if c.area < opts.min_lake_area {
            c.rejection = Some("candidate_too_small".into());
        } else if c.basin_fill_ratio < opts.min_basin_fill {
            c.rejection = Some("channel_like_footprint".into());
        } else if c.channel_elongation > opts.max_channel_elongation && !opposed_necks(c) {
            c.rejection = Some("uniform_wide_channel".into());
        } else if c.confidence < opts.min_confidence {
            c.rejection = Some("low_confidence".into());
        }
        c.accepted = c.rejection.is_none();
    }
}

/// A side tributary alone is no evidence that a long uniform river is a lake.
/// Extremely elongated basins need narrower connections at both longitudinal
/// ends. Less elongated, irregular and closed basins need no outlets at all.
fn opposed_necks(c: &LakeCandidate) -> bool {
    let axis = usize::from(c.bounds[3] - c.bounds[1] > c.bounds[2] - c.bounds[0]);
    let low = c.bounds[axis] as f32;
    let high = c.bounds[axis + 2] as f32;
    let quarter = (high - low) * 0.25;
    let positions: Vec<_> = c
        .connections
        .iter()
        .filter(|n| n.confirmed_neck)
        .map(|n| if axis == 0 { n.x as f32 } else { n.z as f32 })
        .collect();
    positions.iter().any(|&v| v <= low + quarter) && positions.iter().any(|&v| v >= high - quarter)
}
