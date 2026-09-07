use super::raster::NONE;
use super::LakeAnalysis;
use crate::water::model::{Modifiers, RegionGeometry, Run, WaterKind, WaterRegion};
use std::collections::VecDeque;

/// Split only existing inland runs. Protected regions are cloned verbatim and
/// every input water column has exactly one output owner; no erosion or filling.
pub fn apply(regions: &mut Vec<WaterRegion>, analysis: &LakeAnalysis) {
    let r = &analysis.raster;
    let mut labels = vec![NONE; r.len()];
    let mut output = Vec::new();
    for region in regions.iter() {
        if !matches!(region.kind, WaterKind::River | WaterKind::Lake)
            || region.modifiers.contains(Modifiers::CAVE)
        {
            output.push(region.clone());
        }
    }
    let mut queue = VecDeque::new();
    for i in 0..r.len() {
        if !r.is_water(i) || labels[i] != NONE {
            continue;
        }
        let source = r.source[i];
        let lake = analysis.is_lake(i);
        let id = output.len() as u32;
        let parent = &regions[source as usize];
        // Copy attributes only. A large parent can split into many pieces;
        // repeatedly cloning its complete geometry would be quadratic work.
        let mut region = WaterRegion {
            id,
            kind: if lake {
                WaterKind::Lake
            } else {
                WaterKind::River
            },
            geometry: RegionGeometry {
                min_x: i32::MAX,
                min_z: i32::MAX,
                max_x: i32::MIN,
                max_z: i32::MIN,
                column_count: 0,
                runs: Vec::new(),
            },
            temperature: parent.temperature,
            vegetation: parent.vegetation,
            depth: parent.depth,
            modifiers: parent.modifiers,
            surface_y: parent.surface_y,
            bathymetry: parent.bathymetry,
            dominant_biome: parent.dominant_biome.clone(),
        };
        labels[i] = id;
        queue.push_back(i as u32);
        while let Some(p) = queue.pop_front() {
            let p = p as usize;
            region.geometry.column_count += 1;
            let (x, z) = r.coords(p);
            region.geometry.min_x = region.geometry.min_x.min(x);
            region.geometry.max_x = region.geometry.max_x.max(x);
            region.geometry.min_z = region.geometry.min_z.min(z);
            region.geometry.max_z = region.geometry.max_z.max(z);
            for n in r.neighbors(p).into_iter().flatten() {
                if labels[n] == NONE && r.source[n] == source && analysis.is_lake(n) == lake {
                    labels[n] = id;
                    queue.push_back(n as u32);
                }
            }
        }
        output.push(region);
    }
    // Scan the input RLE instead of sorting millions of individual columns.
    for region in regions.iter() {
        if !matches!(region.kind, WaterKind::River | WaterKind::Lake)
            || region.modifiers.contains(Modifiers::CAVE)
        {
            continue;
        }
        for run in &region.geometry.runs {
            let mut x = run.x0;
            while x <= run.x1 {
                let id = labels[r.index_at(x, run.z).expect("input water in raster")] as usize;
                let start = x;
                while x < run.x1 && labels[r.index_at(x + 1, run.z).unwrap()] as usize == id {
                    x += 1;
                }
                let runs = &mut output[id].geometry.runs;
                if let Some(last) = runs
                    .last_mut()
                    .filter(|last| last.z == run.z && last.x1 + 1 == start)
                {
                    last.x1 = x;
                } else {
                    runs.push(Run {
                        z: run.z,
                        x0: start,
                        x1: x,
                    });
                }
                x += 1;
            }
        }
    }
    for (id, region) in output.iter_mut().enumerate() {
        region.id = id as u32;
    }
    *regions = output;
}
