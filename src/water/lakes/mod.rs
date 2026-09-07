//! Inland lake segmentation on the original one-block water mask.
//!
//! Marker-controlled flooding of the shore-distance field separates wide cores
//! from persistent narrow channels. Only River/Lake labels are changed; ocean,
//! swamp, cave water and the union of retained water columns are protected.

mod detection;
mod geometry;
mod output;
pub mod raster;

use crate::water::grid::WorldGrid;
use crate::water::model::WaterRegion;
pub use raster::Raster;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LakeOptions {
    pub core_radius: u16,
    pub density_8: f32,
    pub density_16: f32,
    pub density_32: f32,
    pub min_core_area: u32,
    pub min_lake_area: u32,
    /// Minimum water occupancy of the candidate's axis/PCA-oriented footprint.
    pub min_basin_fill: f32,
    pub neck_width_ratio: f32,
    pub river_seed_distance_factor: f32,
    pub min_channel_length: u16,
    pub max_channel_elongation: f32,
    pub min_confidence: f32,
    pub terrain_rise: i16,
    pub flow_head_difference: f32,
    pub connection_sample_distance: u16,
}

impl Default for LakeOptions {
    fn default() -> Self {
        Self {
            core_radius: 18,
            density_8: 0.90,
            density_16: 0.82,
            density_32: 0.80,
            min_core_area: 64,
            min_lake_area: 2000,
            min_basin_fill: 0.45,
            neck_width_ratio: 0.55,
            river_seed_distance_factor: 2.0,
            min_channel_length: 32,
            max_channel_elongation: 8.0,
            min_confidence: 0.58,
            terrain_rise: 1,
            flow_head_difference: 1.0,
            connection_sample_distance: 24,
        }
    }
}

impl LakeOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.core_radius > 0 && self.core_radius <= 4096,
            "core_radius must be 1..4096"
        );
        for (name, value) in [
            ("density_8", self.density_8),
            ("density_16", self.density_16),
            ("density_32", self.density_32),
            ("min_confidence", self.min_confidence),
            ("min_basin_fill", self.min_basin_fill),
        ] {
            anyhow::ensure!(
                value.is_finite() && (0.0..=1.0).contains(&value),
                "{name} must be 0..1"
            );
        }
        anyhow::ensure!(
            self.neck_width_ratio.is_finite()
                && self.neck_width_ratio > 0.0
                && self.neck_width_ratio < 1.0,
            "neck_width_ratio must be between 0 and 1"
        );
        anyhow::ensure!(
            self.river_seed_distance_factor.is_finite() && self.river_seed_distance_factor >= 1.0,
            "river_seed_distance_factor must be finite and at least 1"
        );
        anyhow::ensure!(
            self.max_channel_elongation.is_finite() && self.max_channel_elongation >= 1.0,
            "max_channel_elongation must be finite and at least 1"
        );
        anyhow::ensure!(
            self.flow_head_difference.is_finite() && self.flow_head_difference > 0.0,
            "flow_head_difference must be positive"
        );
        anyhow::ensure!(self.min_core_area > 0 && self.min_lake_area > 0 && self.min_channel_length > 0
            && self.connection_sample_distance > 0 && self.connection_sample_distance <= 256
            && self.terrain_rise >= 0, "areas, lengths and terrain_rise are invalid (connection_sample_distance must be 1..256)");
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Connection {
    pub x: i32,
    pub z: i32,
    pub width: f32,
    pub width_ratio: f32,
    pub confirmed_neck: bool,
    pub direction: String,
    pub water_head_difference: Option<f32>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LakeCandidate {
    pub id: u32,
    pub accepted: bool,
    pub rejection: Option<String>,
    pub core_area: u32,
    pub area: u32,
    pub bounds: [i32; 4],
    pub characteristic_width: f32,
    pub max_distance_to_shore: u16,
    pub mean_distance_to_shore: f32,
    pub density_8: f32,
    pub density_16: f32,
    pub density_32: f32,
    pub channel_elongation: f32,
    pub axis_fill_ratio: f32,
    pub basin_fill_ratio: f32,
    pub terrain_basin_score: f32,
    pub terrain_sample_coverage: f32,
    pub inflow_count: u32,
    pub outflow_count: u32,
    pub unknown_connection_count: u32,
    pub connections: Vec<Connection>,
    pub width_score: f32,
    pub core_score: f32,
    pub density_score: f32,
    pub widening_score: f32,
    pub area_score: f32,
    pub shape_score: f32,
    pub flow_score: f32,
    pub confidence: f32,
}

pub struct LakeAnalysis {
    pub raster: Raster,
    pub distance: Vec<u16>,
    pub density: Vec<[u8; 3]>,
    /// Candidate id + 1; zero means no core.
    pub cores: Vec<u32>,
    /// Candidate id + 1; zero means river. Includes rejected candidates for QA.
    pub reconstructed: Vec<u32>,
    pub necks: Vec<bool>,
    pub candidates: Vec<LakeCandidate>,
    pub options: LakeOptions,
}

impl LakeAnalysis {
    pub fn is_lake(&self, i: usize) -> bool {
        let id = self.reconstructed[i];
        id > 0 && self.candidates[(id - 1) as usize].accepted
    }
}

pub fn analyze(grid: &WorldGrid, regions: &[WaterRegion], options: &LakeOptions) -> LakeAnalysis {
    detection::analyze(grid, regions, options)
}

pub fn apply(regions: &mut Vec<WaterRegion>, analysis: &LakeAnalysis) {
    output::apply(regions, analysis);
}

#[cfg(test)]
mod tests;
