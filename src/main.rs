//! `water-analyzer` - offline Minecraft Java world scanner.
//!
//! Reads a world's region files and writes `water_regions.bin`: a compact,
//! versioned, Java-friendly description of every classified body of water in the
//! world. The tool is a pure geographic preprocessor - it answers *where is
//! water, what kind is it, how warm, how vegetated, how deep and what is special
//! about it*, and nothing else. Gameplay decisions belong to the consumers.

mod config;
mod debug;
mod format;
mod water;
mod world;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use clap::Parser;
use rayon::prelude::*;

use crate::format::spatial_index::SpatialIndex;
use crate::water::grid::WorldGrid;
use crate::water::model::WaterKind;
use crate::water::scanner::{self, ScanContext, ScanStats};
use crate::world::biome::BiomeRegistry;

#[derive(Parser, Debug)]
#[command(
    name = "water-analyzer",
    version,
    about = "Scans a Minecraft Java world and writes classified water regions to water_regions.bin"
)]
struct Cli {
    /// Path to the world folder (the one containing `region/` and `level.dat`).
    #[arg(long)]
    world: PathBuf,

    /// Output directory. `water_regions.bin` is written here.
    #[arg(long, default_value = "./generated")]
    output: PathBuf,

    /// Worker threads. Defaults to the number of logical CPUs.
    #[arg(long)]
    threads: Option<usize>,

    /// Extra technical logging.
    #[arg(long)]
    debug: bool,

    /// Also write `debug/water_regions.json`.
    #[arg(long)]
    export_json: bool,

    /// Limit the JSON export to the N largest regions.
    #[arg(long)]
    json_limit: Option<usize>,

    /// Also write the six debug PNGs, including the combined ocean/inland map.
    #[arg(long)]
    export_map: bool,

    /// Blocks per pixel in the debug maps.
    #[arg(long, default_value_t = 8)]
    map_scale: u32,

    /// Override the detected sea level instead of deriving it from the world.
    #[arg(long)]
    sea_level: Option<i16>,

    /// Smallest body of water to report, in columns. Bodies below this are
    /// dropped, and classification pieces below it always merge into a neighbour.
    #[arg(long, default_value_t = config::MIN_WATER_BODY_COLUMNS)]
    min_water_body: u32,

    /// Skip water that cannot see the sky. Without this, underground pools and
    /// aquifers are reported as regions carrying the `cave` modifier.
    #[arg(long)]
    no_caves: bool,

    /// Smallest cave pool to report, in columns. Defaults to `--min-water-body`.
    /// Aquifers are far more numerous than lakes, so this usually wants to be
    /// higher than the surface threshold.
    #[arg(long)]
    min_cave_body: Option<u32>,

    /// How large a connected body of water must be, in columns, before it counts
    /// as a sea. Smaller bodies become lakes whatever their biome says.
    #[arg(long, default_value_t = config::SEA_MIN_COLUMNS)]
    min_sea_body: u32,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(&cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> anyhow::Result<()> {
    let started = Instant::now();

    let region_dir = cli.world.join("region");
    if !region_dir.is_dir() {
        anyhow::bail!(
            "{} does not look like a world folder (no region/ directory)",
            cli.world.display()
        );
    }

    if let Some(t) = cli.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(t)
            .build_global()
            .ok();
    }
    let threads = rayon::current_num_threads();

    let level = world::level::read(&cli.world);
    let registry = BiomeRegistry::build(&cli.world);

    let mut files: Vec<PathBuf> = std::fs::read_dir(&region_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mca"))
        .collect();
    files.sort();

    println!("water-analyzer {}", env!("CARGO_PKG_VERSION"));
    println!("  world            {}", cli.world.display());
    if !level.level_name.is_empty() {
        println!("  level name       {}", level.level_name);
    }
    if !level.version_name.is_empty() {
        println!("  minecraft        {}", level.version_name);
    }
    println!("  data version     {}", level.data_version);
    println!(
        "  biomes known     {} ({} from datapacks)",
        registry.len(),
        registry.datapack_biomes
    );
    println!("  region files     {}", files.len());
    println!("  min water body   {} columns", cli.min_water_body);
    println!("  min sea body     {} columns", cli.min_sea_body);
    if cli.no_caves {
        println!("  cave water       skipped");
    } else {
        println!(
            "  cave water       included, min {} columns",
            cli.min_cave_body.unwrap_or(cli.min_water_body)
        );
    }
    println!("  threads          {threads}");
    println!();

    // ---- phase 1: scan -----------------------------------------------------
    let scan_started = Instant::now();
    let done = AtomicU64::new(0);
    let total = files.len() as u64;
    let scans: Vec<scanner::RegionScan> = files
        .par_iter()
        .map_init(ScanContext::new, |ctx, path| {
            let result = scanner::scan_region_file(path, &registry, ctx, !cli.no_caves);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 100 == 0 || n == total {
                let pct = n as f64 * 100.0 / total as f64;
                print!("\r  scanning regions {n}/{total} ({pct:.0}%)   ");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            match result {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("\n  warning: {} - {e}", path.display());
                    None
                }
            }
        })
        .filter_map(|x| x)
        .collect();
    println!();

    let mut stats = ScanStats::default();
    let mut region_waters = Vec::with_capacity(scans.len());
    for s in scans {
        stats = stats.merge(s.stats);
        // Water-free regions are kept: they cost 2 KiB each and they are what
        // makes the world bounds (and therefore the spatial index) cover the
        // whole generated world rather than just its wet parts.
        region_waters.push(s.water);
    }
    let scan_seconds = scan_started.elapsed().as_secs_f64();

    // ---- sea level ---------------------------------------------------------
    let detected = water::sea_level::detect(&stats.ocean_surface_hist, &stats.surface_hist);
    let sea_level = cli.sea_level.unwrap_or(detected.value);
    let sea_level_source = if cli.sea_level.is_some() {
        "cli-override"
    } else {
        detected.source.as_str()
    };

    // ---- phase 2: regions --------------------------------------------------
    let min_cave_body = cli.min_cave_body.unwrap_or(cli.min_water_body);
    let regions_started = Instant::now();
    let grid = WorldGrid::build(region_waters);
    let (regions, region_stats) =
        water::regions::build_regions(
            &grid,
            &registry,
            cli.min_water_body,
            min_cave_body,
            cli.min_sea_body,
        );
    let regions_seconds = regions_started.elapsed().as_secs_f64();

    // ---- output ------------------------------------------------------------
    let bounds = if grid.regions.is_empty() {
        (0, 0, 0, 0)
    } else {
        grid.world_bounds()
    };
    let index = SpatialIndex::build(&regions, bounds);
    std::fs::create_dir_all(&cli.output)?;
    let bin_path = cli.output.join("water_regions.bin");
    let bin_size = format::writer::write_file(
        &bin_path,
        &regions,
        &index,
        level.data_version,
        sea_level,
        bounds,
    )?;

    let verified = verify_output(&bin_path, &regions, sea_level)?;

    let debug_dir = cli.output.join("debug");
    let mut json_size = None;
    if cli.export_json {
        json_size = Some(debug::json::write(
            &debug_dir.join("water_regions.json"),
            &regions,
            debug::json::JsonMetaInput {
                world: &cli.world.display().to_string(),
                minecraft_data_version: level.data_version,
                sea_level,
                sea_level_source,
                water_columns: stats.water_columns,
                scan_seconds,
                limit: cli.json_limit,
            },
        )?);
    }
    let mut map_sizes = None;
    if cli.export_map {
        map_sizes = Some(debug::map::render(
            &debug_dir,
            &grid,
            &regions,
            sea_level,
            &debug::map::MapOptions {
                scale: cli.map_scale,
            },
        )?);
    }

    // ---- statistics --------------------------------------------------------
    let elapsed = started.elapsed().as_secs_f64();
    println!();
    println!("scan");
    println!("  region files scanned   {}", stats.region_files);
    println!("  chunks seen            {}", stats.chunks_seen);
    println!("  chunks fully generated {}", stats.chunks_full);
    if stats.chunks_failed > 0 {
        println!("  chunks unreadable      {}", stats.chunks_failed);
    }
    println!("  chunks with water      {}", stats.water_chunks);
    println!("  water columns detected {}", stats.water_columns);
    println!("  ice covered columns    {}", stats.ice_columns);
    println!("  cave (no sky) columns  {}", stats.cave_columns);
    println!(
        "  sea level              {sea_level} ({sea_level_source}, {} samples, {:.1}% agreement)",
        detected.samples,
        detected.confidence * 100.0
    );
    println!();
    println!("regions");
    println!("  hydrological bodies    {}", region_stats.hydro_bodies);
    println!("  of those seas          {}", region_stats.sea_bodies);
    println!(
        "  classification pieces  {} ({} absorbed into neighbours)",
        region_stats.regions_before_absorption, region_stats.absorbed
    );
    println!("  water regions          {}", regions.len());
    println!("    sea                  {}", region_stats.by_kind[WaterKind::Sea as usize]);
    println!("    river                {}", region_stats.by_kind[WaterKind::River as usize]);
    println!("    lake                 {}", region_stats.by_kind[WaterKind::Lake as usize]);
    println!("    swamp                {}", region_stats.by_kind[WaterKind::Swamp as usize]);
    println!("  with ice               {}", region_stats.with_ice);
    println!("  with corals            {}", region_stats.with_corals);
    println!("  with desert            {}", region_stats.with_desert);
    println!("  with mangrove          {}", region_stats.with_mangrove);
    println!("  with cave              {}", region_stats.with_cave);
    println!(
        "  cells denoised         {} family, {} classification",
        region_stats.smoothed_family_cells, region_stats.smoothed_cells
    );
    println!(
        "  reshaped by geometry   {} pools -> lake, {} strands -> river",
        region_stats.reshaped_to_lake, region_stats.reshaped_to_river
    );
    println!("  riverbank repair       {} groups, {} columns", region_stats.bank_groups, region_stats.bank_columns);
    println!("  geometry runs          {}", region_stats.geometry_runs);
    println!();
    println!("output");
    println!("  {}  {}", bin_path.display(), human_size(bin_size));
    println!("    verified: {verified} spot-checked lookups round-tripped");
    if let Some(size) = json_size {
        println!(
            "  {}  {}",
            debug_dir.join("water_regions.json").display(),
            human_size(size)
        );
    }
    if let Some((a, b, c, d, e, f)) = map_sizes {
        for (name, size) in [
            ("water_map.png", a),
            ("water_regions_map.png", b),
            ("water_depth_map.png", c),
            ("water_ocean_map.png", d),
            ("water_inland_map.png", e),
            ("water_combined_map.png", f),
        ] {
            println!("  {}  {}", debug_dir.join(name).display(), human_size(size));
        }
    }
    println!();
    println!(
        "timing  scan {scan_seconds:.1}s  regions {regions_seconds:.1}s  total {elapsed:.1}s"
    );

    if cli.debug {
        print_debug(&grid, &registry, &regions, &region_stats, &index, bounds);
    }

    Ok(())
}

/// Reads the file back and spot-checks that a consumer would resolve the same
/// regions we just wrote. Cheap insurance against a silent format regression.
fn verify_output(
    path: &Path,
    regions: &[water::model::WaterRegion],
    sea_level: i16,
) -> anyhow::Result<usize> {
    let bytes = std::fs::read(path)?;
    let file = format::reader::WaterRegionFile::parse(&bytes)
        .map_err(|e| anyhow::anyhow!("written file does not parse: {e}"))?;

    anyhow::ensure!(
        file.regions.len() == regions.len(),
        "region count mismatch: wrote {}, read {}",
        regions.len(),
        file.regions.len()
    );
    anyhow::ensure!(
        file.header.sea_level == sea_level,
        "sea level mismatch in header"
    );

    // Sample regions spread across the whole table rather than just the first few.
    let step = (regions.len() / 64).max(1);
    let mut checked = 0usize;
    for region in regions.iter().step_by(step) {
        let Some(run) = region.geometry.runs.first() else {
            continue;
        };
        let x = run.x0 + (run.x1 - run.x0) / 2;
        let found = file.region_at(x, run.z);
        anyhow::ensure!(
            found.map(|r| r.id) == Some(region.id),
            "lookup at {x}/{} returned {:?}, expected region {}",
            run.z,
            found.map(|r| r.id),
            region.id
        );
        checked += 1;
    }

    // Somewhere far outside the world there must be no region at all.
    let outside_x = file.header.world_min_x.saturating_sub(1_000_000);
    let outside_z = file.header.world_min_z.saturating_sub(1_000_000);
    anyhow::ensure!(
        file.region_at(outside_x, outside_z).is_none(),
        "found a region outside the world bounds"
    );

    Ok(checked)
}

fn print_debug(
    grid: &WorldGrid,
    registry: &BiomeRegistry,
    regions: &[water::model::WaterRegion],
    region_stats: &water::regions::RegionStats,
    index: &SpatialIndex,
    bounds: (i32, i32, i32, i32),
) {
    println!();
    println!("debug");
    println!(
        "  world bounds           x {}..{}  z {}..{}",
        bounds.0, bounds.2, bounds.1, bounds.3
    );
    println!(
        "  region grid            {}x{} tiles, {} with data",
        grid.rw,
        grid.rh,
        grid.regions.len()
    );
    println!(
        "  spatial index          {}x{} cells of {} blocks, {}",
        index.cells_x,
        index.cells_z,
        config::SPATIAL_CELL_SIZE,
        human_size(index.byte_size() as u64)
    );
    println!("  largest bodies of water (columns, of which ocean biome)");
    println!("    rivers connect these, so a body is far larger than its ocean");
    for (i, (columns, ocean)) in region_stats.largest_bodies.iter().take(8).enumerate() {
        let share = if *columns > 0 {
            *ocean as f64 / *columns as f64 * 100.0
        } else {
            0.0
        };
        println!("      {:>2}. {:>12}  {:>12} ocean ({share:.0}%)", i + 1, columns, ocean);
    }
    println!("  largest connected sheets of ocean water (columns)");
    println!("    this is what the sea threshold is measured against");
    for (i, columns) in region_stats.largest_sheets.iter().enumerate() {
        println!("      {:>2}. {:>12}", i + 1, columns);
    }

    let unknown = registry.unknown_names();
    if unknown.is_empty() {
        println!("  unknown biomes         none");
    } else {
        println!("  unknown biomes         {}", unknown.len());
        for n in unknown.iter().take(20) {
            println!("      {n}");
        }
    }

    let mut largest: Vec<&water::model::WaterRegion> = regions.iter().collect();
    largest.sort_by_key(|r| std::cmp::Reverse(r.geometry.column_count));
    println!("  largest regions");
    for r in largest.iter().take(15) {
        let modifiers = r.modifiers.names().join("+");
        println!(
            "      #{:<6} {:<6} {:<7} {:<7} {:<8} {:>12} cols  y={:<4} {:<24} {}",
            r.id,
            r.kind.as_str(),
            r.temperature.as_str(),
            r.vegetation.as_str(),
            r.depth.map(|d| d.as_str()).unwrap_or("-"),
            r.geometry.column_count,
            r.surface_y,
            r.dominant_biome,
            modifiers
        );
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_sensibly() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn world_path_must_contain_a_region_directory() {
        let cli = Cli {
            world: PathBuf::from("definitely-not-a-world"),
            output: PathBuf::from("out"),
            threads: None,
            debug: false,
            export_json: false,
            json_limit: None,
            export_map: false,
            map_scale: 8,
            sea_level: None,
            min_water_body: config::MIN_WATER_BODY_COLUMNS,
            no_caves: false,
            min_cave_body: None,
            min_sea_body: config::SEA_MIN_COLUMNS,
        };
        assert!(run(&cli).is_err());
    }
}
