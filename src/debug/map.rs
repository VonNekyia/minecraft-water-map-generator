//! Optional PNG debug maps.
//!
//! Six images are produced, all purely for eyeballing the scanner's output -
//! none of them is part of the runtime format:
//!
//! * `water_map.png` - classification map. [`WaterKind`] and [`Temperature`] pick
//!   the hue, [`Depth`] picks the brightness, [`Modifiers`] add a pattern on top.
//! * `water_regions_map.png` - one distinct colour per region id, which is what
//!   actually shows how the world was segmented.
//! * `water_depth_map.png` - the measured depth of every 4x4 cell, read straight
//!   off the scan grid rather than out of the regions. A region only carries one
//!   mean depth, so on the classification map a 40-million-column ocean is a
//!   single flat shade; this one shows the seafloor.
//! * `water_ocean_map.png` - the seas alone, in six colours: violet, blue and pale
//!   blue-grey for warm, medium and cold water, each in a shelf and a basin shade.
//! * `water_inland_map.png` - the mirror image: the sea flattened to one
//!   green-blue backdrop so the rivers, lakes and swamps stand out.
//! * `water_combined_map.png` - cleaned ocean zones with inland water overlaid.
//!
//! All six get a legend panel drawn *beside* the map rather than on top of it,
//! so nothing is ever hidden behind it and the images stay the same size.

use std::path::Path;

use crate::water::grid::WorldGrid;
use crate::water::model::*;

use super::font;

const BG_VOID: [u8; 3] = [14, 14, 20];
const BG_LAND: [u8; 3] = [52, 54, 58];
const PANEL_BG: [u8; 3] = [8, 8, 12];
const TEXT: [u8; 3] = [232, 232, 236];
const TEXT_DIM: [u8; 3] = [150, 152, 160];
const HEADER: [u8; 3] = [126, 198, 255];

pub struct MapOptions {
    /// Blocks per pixel.
    pub scale: u32,
    pub ocean_min_area: u64,
}

impl Default for MapOptions {
    fn default() -> Self {
        MapOptions { scale: 8, ocean_min_area: crate::config::OCEAN_MAP_MIN_AREA }
    }
}

// ---------------------------------------------------------------------------
// Canvas
// ---------------------------------------------------------------------------

struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize, bg: [u8; 3]) -> Self {
        let mut px = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            px.extend_from_slice(&bg);
        }
        Canvas { w, h, px }
    }

    #[inline]
    fn set(&mut self, x: usize, y: usize, c: [u8; 3]) {
        if x >= self.w || y >= self.h {
            return;
        }
        let i = (y * self.w + x) * 3;
        self.px[i] = c[0];
        self.px[i + 1] = c[1];
        self.px[i + 2] = c[2];
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: [u8; 3]) {
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                self.set(xx, yy, c);
            }
        }
    }

    fn text(&mut self, x: usize, y: usize, scale: usize, text: &str, c: [u8; 3]) {
        let mut cx = x;
        for ch in text.chars() {
            let g = font::glyph(ch);
            for (row, bits) in g.iter().enumerate() {
                for col in 0..font::GLYPH_W {
                    if bits & (1 << (font::GLYPH_W - 1 - col)) != 0 {
                        self.fill_rect(cx + col * scale, y + row * scale, scale, scale, c);
                    }
                }
            }
            cx += (font::GLYPH_W + 1) * scale;
        }
    }

    fn write_png(&self, path: &Path) -> std::io::Result<u64> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::File::create(path)?;
        let mut encoder = png::Encoder::new(
            std::io::BufWriter::with_capacity(1 << 20, file),
            self.w as u32,
            self.h as u32,
        );
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        writer
            .write_image_data(&self.px)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        writer
            .finish()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(std::fs::metadata(path)?.len())
    }
}

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

#[inline]
fn blend(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        (a[0] as f32 * (1.0 - t) + b[0] as f32 * t) as u8,
        (a[1] as f32 * (1.0 - t) + b[1] as f32 * t) as u8,
        (a[2] as f32 * (1.0 - t) + b[2] as f32 * t) as u8,
    ]
}

fn kind_color(kind: WaterKind) -> [u8; 3] {
    match kind {
        WaterKind::Sea => [30, 82, 178],
        WaterKind::River => [58, 168, 205],
        WaterKind::Lake => [40, 165, 132],
        WaterKind::Swamp => [86, 108, 60],
    }
}

/// Relative luminance of a colour, 0..1 (Rec. 709 coefficients).
#[inline]
fn luminance(c: [u8; 3]) -> f32 {
    (0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32) / 255.0
}

/// Rescales a colour to a target luminance, keeping its hue.
fn with_luminance(c: [u8; 3], target: f32) -> [u8; 3] {
    let lum = luminance(c);
    if target <= lum {
        // Darken: scale towards black.
        let k = if lum > 0.001 { target / lum } else { 0.0 };
        [
            (c[0] as f32 * k) as u8,
            (c[1] as f32 * k) as u8,
            (c[2] as f32 * k) as u8,
        ]
    } else {
        // Lighten: blend towards white by the amount that hits the target.
        blend(c, [255, 255, 255], (target - lum) / (1.0 - lum).max(0.001))
    }
}

/// Luminance each depth band is pinned to.
fn depth_luminance(depth: Depth) -> f32 {
    match depth {
        Depth::Shallow => 0.62,
        Depth::Normal => 0.38,
        Depth::Deep => 0.18,
    }
}

/// The colour of a classification, before the modifier pattern is overlaid.
///
/// Kind and temperature pick the *hue*, depth picks the *brightness* - and those
/// two axes are kept strictly apart. An earlier version let temperature brighten
/// the colour as well, which made `cold`+`deep` and `medium`+`normal` come out at
/// the same luminance: the map then carried no readable depth at all. Depth now
/// pins the luminance, so a dark patch is deep water whatever its temperature.
pub fn style_color(
    kind: WaterKind,
    temperature: Temperature,
    depth: Option<Depth>,
    vegetation: Vegetation,
) -> [u8; 3] {
    let mut c = kind_color(kind);
    c = match temperature {
        Temperature::Warm => blend(c, [255, 140, 40], 0.42),
        Temperature::Medium => c,
        Temperature::Cold => blend(c, [120, 235, 255], 0.42),
    };
    if vegetation == Vegetation::Jungle {
        c = blend(c, [60, 200, 90], 0.16);
    }
    if let Some(d) = depth {
        c = with_luminance(c, depth_luminance(d));
    }
    c
}

pub fn region_color(region: &WaterRegion) -> [u8; 3] {
    style_color(
        region.kind,
        region.temperature,
        region.depth,
        region.vegetation,
    )
}

/// Pattern overlay colour for a pixel, if any modifier applies there.
///
/// The patterns are keyed off absolute canvas coordinates so a legend swatch
/// shows exactly the same texture as the map does.
fn modifier_overlay(modifiers: Modifiers, x: usize, y: usize) -> Option<[u8; 3]> {
    // Cave first: it says the water is not where the surface is, which changes how
    // everything else about the patch should be read.
    if modifiers.contains(Modifiers::CAVE) && (3 * x + y) % 9 < 2 {
        return Some([24, 20, 32]);
    }
    if modifiers.contains(Modifiers::ICE) && (x + y) % 8 < 2 {
        return Some([235, 248, 255]);
    }
    if modifiers.contains(Modifiers::CORALS) && x % 6 == 2 && y % 6 == 2 {
        return Some([255, 90, 190]);
    }
    if modifiers.contains(Modifiers::DESERT) && (x + 5 * y) % 11 < 2 {
        return Some([226, 200, 120]);
    }
    if modifiers.contains(Modifiers::MANGROVE) && x % 7 == 3 && y % 7 == 5 {
        return Some([150, 80, 40]);
    }
    None
}

/// Deterministic, well separated colour per region id.
pub fn id_color(id: u32) -> [u8; 3] {
    // Golden-ratio hue stepping keeps neighbouring ids far apart in hue.
    let hue = (id as f32 * 0.618_034).fract();
    let sat = 0.55 + 0.35 * (((id / 7) % 3) as f32 / 2.0);
    let val = 0.60 + 0.35 * (((id / 3) % 3) as f32 / 2.0);
    hsv_to_rgb(hue, sat, val)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match (i as i32) % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}

// ---------------------------------------------------------------------------
// Legend
// ---------------------------------------------------------------------------

/// What is drawn in a legend row's swatch column.
enum Swatch {
    /// No swatch: a section heading or a note.
    None,
    Color([u8; 3]),
    /// A base colour with a modifier pattern on top, exactly as on the map.
    Pattern([u8; 3], Modifiers),
    /// A strip of colours, for showing a whole scale at once.
    Ramp(Vec<[u8; 3]>),
}

struct Row {
    text: String,
    swatch: Swatch,
    style: RowStyle,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum RowStyle {
    Heading,
    Entry,
    Note,
}

fn heading(text: &str) -> Row {
    Row {
        text: text.to_string(),
        swatch: Swatch::None,
        style: RowStyle::Heading,
    }
}

fn note(text: &str) -> Row {
    Row {
        text: text.to_string(),
        swatch: Swatch::None,
        style: RowStyle::Note,
    }
}

fn entry(text: &str, swatch: Swatch) -> Row {
    Row {
        text: text.to_string(),
        swatch,
        style: RowStyle::Entry,
    }
}

struct Legend {
    rows: Vec<Row>,
    scale: usize,
}

impl Legend {
    fn line_height(&self) -> usize {
        (font::GLYPH_H + 5) * self.scale
    }

    fn swatch_width(&self) -> usize {
        14 * self.scale
    }

    fn height(&self) -> usize {
        let gap = self.scale * 4; // extra air above each heading
        let headings = self
            .rows
            .iter()
            .filter(|r| r.style == RowStyle::Heading)
            .count();
        self.rows.len() * self.line_height() + headings * gap + 8 * self.scale
    }

    fn width(&self) -> usize {
        let text = self
            .rows
            .iter()
            .map(|r| font::text_width(&r.text, self.scale))
            .max()
            .unwrap_or(0);
        4 * self.scale + self.swatch_width() + 3 * self.scale + text + 6 * self.scale
    }

    fn draw(&self, canvas: &mut Canvas, panel_width: usize) {
        canvas.fill_rect(0, 0, panel_width, canvas.h, PANEL_BG);
        let sw = self.swatch_width();
        let line = self.line_height();
        let text_x = 4 * self.scale + sw + 3 * self.scale;
        let mut y = 4 * self.scale;

        for row in &self.rows {
            if row.style == RowStyle::Heading {
                y += 4 * self.scale;
            }
            match &row.swatch {
                Swatch::None => {}
                Swatch::Color(c) => {
                    canvas.fill_rect(4 * self.scale, y, sw, font::GLYPH_H * self.scale, *c);
                }
                Swatch::Pattern(base, modifiers) => {
                    let x0 = 4 * self.scale;
                    canvas.fill_rect(x0, y, sw, font::GLYPH_H * self.scale, *base);
                    for yy in y..y + font::GLYPH_H * self.scale {
                        for xx in x0..x0 + sw {
                            if let Some(c) = modifier_overlay(*modifiers, xx, yy) {
                                canvas.set(xx, yy, c);
                            }
                        }
                    }
                }
                Swatch::Ramp(colors) => {
                    let x0 = 4 * self.scale;
                    let step = sw / colors.len().max(1);
                    for (i, c) in colors.iter().enumerate() {
                        canvas.fill_rect(
                            x0 + i * step,
                            y,
                            step,
                            font::GLYPH_H * self.scale,
                            *c,
                        );
                    }
                }
            }
            let color = match row.style {
                RowStyle::Heading => HEADER,
                RowStyle::Entry => TEXT,
                RowStyle::Note => TEXT_DIM,
            };
            let x = if row.style == RowStyle::Heading {
                4 * self.scale
            } else {
                text_x
            };
            canvas.text(x, y, self.scale, &row.text, color);
            y += line;
        }
    }
}

/// Picks a legend text size that fits the map height without dwarfing it.
fn legend_scale(map_height: usize, rows: usize) -> usize {
    let per_row = font::GLYPH_H + 5;
    (map_height / (rows * per_row).max(1)).clamp(1, 5)
}

fn classification_rows(regions: &[WaterRegion], sea_level: i16, blocks_per_pixel: u32) -> Vec<Row> {
    use Depth::*;
    use Temperature::*;
    use WaterKind::*;

    let sea_base = |t: Temperature| style_color(Sea, t, Some(Normal), Vegetation::Normal);
    let mut rows = vec![
        heading("WATER KIND - BASE COLOUR"),
        entry(
            "SEA",
            Swatch::Color(style_color(Sea, Medium, Some(Normal), Vegetation::Normal)),
        ),
        entry(
            "RIVER",
            Swatch::Color(style_color(River, Medium, Some(Normal), Vegetation::Normal)),
        ),
        entry(
            "LAKE",
            Swatch::Color(style_color(Lake, Medium, Some(Normal), Vegetation::Normal)),
        ),
        entry(
            "SWAMP",
            Swatch::Color(style_color(Swamp, Medium, Some(Normal), Vegetation::Normal)),
        ),
        heading("TEMPERATURE - TINTS THE COLOUR"),
        note("FROM THE BIOME  SHOWN ON SEA"),
        entry("WARM", Swatch::Color(sea_base(Warm))),
        entry("MEDIUM", Swatch::Color(sea_base(Medium))),
        entry("COLD", Swatch::Color(sea_base(Cold))),
        heading("WATER DEPTH - BRIGHTNESS"),
        note("MEASURED SURFACE TO FLOOR"),
        entry(
            "SHALLOW 0-10",
            Swatch::Color(style_color(Sea, Medium, Some(Shallow), Vegetation::Normal)),
        ),
        entry(
            "NORMAL 11-30",
            Swatch::Color(style_color(Sea, Medium, Some(Normal), Vegetation::Normal)),
        ),
        entry(
            "DEEP 31+",
            Swatch::Color(style_color(Sea, Medium, Some(Deep), Vegetation::Normal)),
        ),
        heading("MODIFIERS - PATTERN ON TOP"),
        entry(
            "ICE",
            Swatch::Pattern(
                style_color(Sea, Cold, Some(Normal), Vegetation::Sparse),
                Modifiers::ICE,
            ),
        ),
        entry(
            "CORALS",
            Swatch::Pattern(
                style_color(Sea, Warm, Some(Shallow), Vegetation::Normal),
                Modifiers::CORALS,
            ),
        ),
        entry(
            "DESERT",
            Swatch::Pattern(
                style_color(Lake, Warm, Some(Shallow), Vegetation::None),
                Modifiers::DESERT,
            ),
        ),
        entry(
            "MANGROVE",
            Swatch::Pattern(
                style_color(Swamp, Warm, Some(Shallow), Vegetation::Jungle),
                Modifiers::MANGROVE,
            ),
        ),
        entry(
            "CAVE",
            Swatch::Pattern(
                style_color(Lake, Medium, Some(Shallow), Vegetation::None),
                Modifiers::CAVE,
            ),
        ),
        note("CAVE = NO VIEW OF THE SKY"),
        note("IT LIES UNDER THE LAND"),
        heading("VEGETATION"),
        note("JUNGLE ADDS A GREEN TINT"),
        note("OTHER LEVELS NOT SHOWN"),
        heading("BACKGROUND"),
        entry("LAND OR NO WATER", Swatch::Color(BG_LAND)),
        entry("NOT GENERATED", Swatch::Color(BG_VOID)),
        heading("THIS MAP"),
    ];

    let counts = kind_counts(regions);
    rows.push(note(&format!("REGIONS {}", regions.len())));
    rows.push(note(&format!(
        "SEA {} RIVER {}",
        counts[Sea as usize], counts[River as usize]
    )));
    rows.push(note(&format!(
        "LAKE {} SWAMP {}",
        counts[Lake as usize], counts[Swamp as usize]
    )));
    rows.push(note(&format!("SEA LEVEL {sea_level}")));
    rows.push(note(&format!("1 PIXEL = {blocks_per_pixel} BLOCKS")));
    rows
}

fn region_rows(regions: &[WaterRegion], blocks_per_pixel: u32) -> Vec<Row> {
    let samples: Vec<[u8; 3]> = (0..6).map(|i| id_color(i * 37 + 3)).collect();
    vec![
        heading("WATER REGIONS"),
        entry("ONE COLOUR = ONE REGION", Swatch::Ramp(samples)),
        note("COLOURS MEAN NOTHING ELSE"),
        note("THEY ONLY SEPARATE REGIONS"),
        heading("READING IT"),
        note("ONE PATCH OF COLOUR IS ONE"),
        note("BODY OF WATER THE SCANNER"),
        note("CLASSIFIED AS A SINGLE UNIT"),
        heading("BACKGROUND"),
        entry("LAND OR NO WATER", Swatch::Color(BG_LAND)),
        entry("NOT GENERATED", Swatch::Color(BG_VOID)),
        heading("THIS MAP"),
        note(&format!("REGIONS {}", regions.len())),
        note(&format!("1 PIXEL = {blocks_per_pixel} BLOCKS")),
    ]
}

fn kind_counts(regions: &[WaterRegion]) -> [usize; 4] {
    let mut c = [0usize; 4];
    for r in regions {
        c[r.kind as usize] += 1;
    }
    c
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

struct Projection {
    min_x: i32,
    min_z: i32,
    scale: i32,
    offset_x: usize,
    w: usize,
    h: usize,
}

impl Projection {
    fn new(bounds: (i32, i32, i32, i32), scale: u32) -> Self {
        let scale = scale.max(1) as i32;
        let (min_x, min_z, max_x, max_z) = bounds;
        Projection {
            min_x,
            min_z,
            scale,
            offset_x: 0,
            w: (((max_x - min_x) / scale) + 1) as usize,
            h: (((max_z - min_z) / scale) + 1) as usize,
        }
    }

    /// World X at the left edge of a canvas pixel column.
    #[inline]
    fn world_x(&self, px: usize) -> i32 {
        self.min_x + (px.saturating_sub(self.offset_x) as i32) * self.scale
    }

    #[inline]
    fn px(&self, x: i32, z: i32) -> (usize, usize) {
        (
            ((x - self.min_x) / self.scale).max(0) as usize + self.offset_x,
            ((z - self.min_z) / self.scale).max(0) as usize,
        )
    }
}

fn paint_land(canvas: &mut Canvas, grid: &WorldGrid, p: &Projection) {
    for rz in grid.min_rz..grid.min_rz + grid.rh {
        for rx in grid.min_rx..grid.min_rx + grid.rw {
            if grid.region(rx, rz).is_none() {
                continue;
            }
            let (x0, y0) = p.px(rx * 512, rz * 512);
            let (x1, y1) = p.px(rx * 512 + 511, rz * 512 + 511);
            canvas.fill_rect(x0, y0, x1 - x0 + 1, y1 - y0 + 1, BG_LAND);
        }
    }
}

fn paint_regions<F>(canvas: &mut Canvas, regions: &[WaterRegion], p: &Projection, color: F)
where
    F: Fn(&WaterRegion, usize, usize) -> [u8; 3],
{
    for region in regions {
        for run in &region.geometry.runs {
            let (xa, y) = p.px(run.x0, run.z);
            let (xb, _) = p.px(run.x1, run.z);
            for x in xa..=xb {
                canvas.set(x, y, color(region, x, y));
            }
        }
    }
}

/// Depth stops of the bathymetry ramp: `(blocks, colour)`.
const DEPTH_STOPS: [(u16, [u8; 3]); 5] = [
    (0, [198, 244, 252]),
    (10, [96, 196, 226]),
    (30, [38, 112, 192]),
    (60, [18, 48, 122]),
    (100, [8, 16, 54]),
];

/// Continuous colour for one measured depth in blocks.
fn depth_ramp(blocks: u16) -> [u8; 3] {
    let last = DEPTH_STOPS[DEPTH_STOPS.len() - 1];
    if blocks >= last.0 {
        return last.1;
    }
    for w in DEPTH_STOPS.windows(2) {
        let (a, b) = (w[0], w[1]);
        if blocks <= b.0 {
            let span = (b.0 - a.0).max(1) as f32;
            return blend(a.1, b.1, (blocks - a.0) as f32 / span);
        }
    }
    last.1
}

/// Paints the measured depth of every open-water 4x4 cell.
///
/// This one does not go through the regions at all - it reads the scan grid, so
/// it shows the depth *per cell* rather than one mean per region. On a map where
/// a single ocean region covers 40 million columns, that is the difference
/// between a flat shade and an actual seafloor.
fn paint_depth(canvas: &mut Canvas, grid: &WorldGrid, p: &Projection) {
    for region in &grid.regions {
        for cz in 0..32usize {
            for cx in 0..32usize {
                let Some(chunk) = region.chunk(cz * 32 + cx) else {
                    continue;
                };
                let base_x = region.region_x * 512 + (cx * 16) as i32;
                let base_z = region.region_z * 512 + (cz * 16) as i32;
                for (i, cell) in chunk.cells.iter().enumerate() {
                    if cell.water_cols == 0 {
                        continue;
                    }
                    // Underground pools would hide the seafloor under the land.
                    if cell.cave_cols * 2 >= cell.water_cols {
                        continue;
                    }
                    let x0 = base_x + ((i % 4) * 4) as i32;
                    let z0 = base_z + ((i / 4) * 4) as i32;
                    let (px0, py0) = p.px(x0, z0);
                    let (px1, py1) = p.px(x0 + 3, z0 + 3);
                    canvas.fill_rect(
                        px0,
                        py0,
                        px1 - px0 + 1,
                        py1 - py0 + 1,
                        depth_ramp(cell.depth),
                    );
                }
            }
        }
    }
}

fn depth_rows(sea_level: i16, blocks_per_pixel: u32) -> Vec<Row> {
    let mut rows = vec![
        heading("MEASURED WATER DEPTH"),
        note("SURFACE DOWN TO THE FLOOR"),
        note("PER 4X4 CELL NOT PER REGION"),
        heading("SCALE - BLOCKS"),
    ];
    for blocks in [0u16, 5, 10, 20, 30, 45, 60, 80, 100] {
        let label = if blocks == 100 {
            "100+".to_string()
        } else {
            blocks.to_string()
        };
        rows.push(entry(&label, Swatch::Color(depth_ramp(blocks))));
    }
    rows.push(heading("CLASSIFICATION BANDS"));
    rows.push(note("SHALLOW 0-10"));
    rows.push(note("NORMAL 11-30"));
    rows.push(note("DEEP 31+"));
    rows.push(heading("BACKGROUND"));
    rows.push(entry("LAND OR NO WATER", Swatch::Color(BG_LAND)));
    rows.push(entry("NOT GENERATED", Swatch::Color(BG_VOID)));
    rows.push(note("USES FILTERED WATER DATA"));
    rows.push(heading("THIS MAP"));
    rows.push(note(&format!("SEA LEVEL {sea_level}")));
    rows.push(note(&format!("1 PIXEL = {blocks_per_pixel} BLOCKS")));
    rows
}

/// The six ocean colours: three temperatures, each in a shelf and a basin shade.
///
/// Only two depth steps here, not the three the data model carries. The measured
/// depth of this world's ocean is sharply bimodal - 93% is deeper than 10 blocks
/// but only 68% is deeper than 40 - so a shelf/basin split says far more than
/// three bands squeezed onto one hue.
fn ocean_color(temperature: Temperature, deep: bool) -> [u8; 3] {
    match (temperature, deep) {
        (Temperature::Warm, false) => [0x4e, 0x5d, 0x7c],
        (Temperature::Warm, true) => [0x2b, 0x37, 0x52],
        (Temperature::Medium, false) => [0x12, 0x32, 0x6e],
        (Temperature::Medium, true) => [0x09, 0x19, 0x37],
        (Temperature::Cold, false) => [0x48, 0x65, 0x9c],
        (Temperature::Cold, true) => [0x2b, 0x3d, 0x5e],
    }
}

/// Depth at which an ocean column stops being shelf and becomes basin, in blocks.
/// Shares the `deep` boundary of the classification so the two agree.
const OCEAN_SHELF_MAX: u16 = crate::config::DEPTH_NORMAL_MAX as u16;

/// Paints `Sea` regions only, hue from the region's temperature and shade from
/// the depth measured at each cell.
fn paint_ocean(
    canvas: &mut Canvas,
    grid: &WorldGrid,
    regions: &[WaterRegion],
    p: &Projection,
    min_area: u64,
) {
    let mut codes = vec![super::sieve::EMPTY; p.w * p.h];
    let mut weights = vec![0u64; p.w * p.h];
    for region in regions.iter().filter(|r| r.kind == WaterKind::Sea) {
        for run in &region.geometry.runs {
            let (xa, y) = p.px(run.x0, run.z);
            let (xb, _) = p.px(run.x1, run.z);
            for px in xa..=xb {
                let left = p.world_x(px);
                let world_x = left.clamp(run.x0, run.x1);
                let depth = grid.cell_at(world_x, run.z).map(|c| c.depth).unwrap_or(0);
                let i = y * p.w + px - p.offset_x;
                codes[i] = region.temperature as u8 * 2 + u8::from(depth > OCEAN_SHELF_MAX);
                // Count each actual water column once, including partial coastal
                // pixels, instead of assuming every pixel contains scale^2 water.
                weights[i] += (run.x1.min(left + p.scale - 1) - run.x0.max(left) + 1) as u64;
            }
        }
    }
    let stats = super::sieve::simplify(&mut codes, &weights, p.w, min_area);
    println!("  ocean map cleanup      {} patches merged, {} isolated omitted ({} columns), {} below minimum remain",
        stats.merged, stats.omitted, stats.omitted_columns, stats.remaining_small);
    for (i, code) in codes.into_iter().enumerate() {
        if code != super::sieve::EMPTY {
            canvas.set(p.offset_x + i % p.w, i / p.w,
                ocean_color(Temperature::from_u8(code / 2).unwrap(), code % 2 != 0));
        }
    }
}

fn ocean_rows(regions: &[WaterRegion], sea_level: i16, blocks_per_pixel: u32, min_area: u64) -> Vec<Row> {
    let seas = regions.iter().filter(|r| r.kind == WaterKind::Sea).count();
    let shelf = format!("0-{OCEAN_SHELF_MAX}");
    let basin = format!("{}+", OCEAN_SHELF_MAX + 1);
    vec![
        heading("OCEAN ZONES"),
        note("SEA REGIONS ONLY"),
        entry(
            &format!("WARM {shelf}"),
            Swatch::Color(ocean_color(Temperature::Warm, false)),
        ),
        entry(
            &format!("WARM {basin}"),
            Swatch::Color(ocean_color(Temperature::Warm, true)),
        ),
        entry(
            &format!("MEDIUM {shelf}"),
            Swatch::Color(ocean_color(Temperature::Medium, false)),
        ),
        entry(
            &format!("MEDIUM {basin}"),
            Swatch::Color(ocean_color(Temperature::Medium, true)),
        ),
        entry(
            &format!("COLD {shelf}"),
            Swatch::Color(ocean_color(Temperature::Cold, false)),
        ),
        entry(
            &format!("COLD {basin}"),
            Swatch::Color(ocean_color(Temperature::Cold, true)),
        ),
        heading("HOW TO READ IT"),
        note("HUE IS THE TEMPERATURE"),
        note("FROM THE OCEAN BIOME"),
        note("SHADE IS THE MEASURED DEPTH"),
        note("PER 4X4 CELL IN BLOCKS"),
        note(&format!("PATCHES BELOW {min_area} MERGED")),
        note(if min_area == 0 { "PATCH FILTER DISABLED" } else { "ISOLATED TINY PATCHES OMITTED" }),
        heading("BACKGROUND"),
        entry("LAND OR OTHER WATER", Swatch::Color(BG_LAND)),
        entry("NOT GENERATED", Swatch::Color(BG_VOID)),
        heading("THIS MAP"),
        note(&format!("SEA REGIONS {seas}")),
        note(&format!("SEA LEVEL {sea_level}")),
        note(&format!("1 PIXEL = {blocks_per_pixel} BLOCKS")),
    ]
}

/// Colours of the inland water map: the sea is one flat green-blue so it recedes
/// into the background, and rivers, lakes and swamps are picked out against it.
fn inland_color(kind: WaterKind) -> [u8; 3] {
    match kind {
        WaterKind::Sea => [36, 112, 120],
        WaterKind::River => [0x3a, 0xe1, 0xcd],
        WaterKind::Lake => [0x99, 0xca, 0xcd],
        WaterKind::Swamp => [190, 175, 80],
    }
}

/// Desert is a measured region modifier, so only the corresponding inland
/// river/lake regions get the sand palette; other kinds keep their base colour.
fn inland_region_color(kind: WaterKind, modifiers: Modifiers) -> [u8; 3] {
    match (kind, modifiers.contains(Modifiers::DESERT)) {
        (WaterKind::River, true) => [0xea, 0xd7, 0xa0],
        (WaterKind::Lake, true) => [0xcb, 0xb9, 0x84],
        _ => inland_color(kind),
    }
}

/// Paints the sea as a backdrop, then the inland water on top of it.
///
/// Drawing order matters here and nowhere else: regions never overlap, but at
/// eight blocks per pixel a river one chunk wide shares its pixel with the coast
/// it runs into. Painting inland water last is what keeps it on the map.
fn paint_inland(canvas: &mut Canvas, regions: &[WaterRegion], p: &Projection, include_sea: bool) {
    // The combined view gives existing ocean pixels priority at shared coastal
    // pixels. The standalone inland view keeps its river-first presentation.
    let ocean_palette: Vec<_> = [Temperature::Warm, Temperature::Medium, Temperature::Cold]
        .into_iter().flat_map(|t| [ocean_color(t, false), ocean_color(t, true)]).collect();
    for sea_pass in [true, false] {
        if sea_pass && !include_sea { continue; }
        for region in regions {
            if (region.kind == WaterKind::Sea) != sea_pass {
                continue;
            }
            // Underground pools would cover the land in lake colour.
            if region.modifiers.contains(Modifiers::CAVE) {
                continue;
            }
            let color = inland_region_color(region.kind, region.modifiers);
            for run in &region.geometry.runs {
                let (xa, y) = p.px(run.x0, run.z);
                let (xb, _) = p.px(run.x1, run.z);
                for x in xa..=xb {
                    let i = (y * canvas.w + x) * 3;
                    if !include_sea && ocean_palette.iter().any(|c| canvas.px[i..i + 3] == c[..]) {
                        continue;
                    }
                    canvas.set(x, y, color);
                }
            }
        }
    }
}

fn inland_rows(regions: &[WaterRegion], blocks_per_pixel: u32) -> Vec<Row> {
    let count = |kind: WaterKind| {
        regions
            .iter()
            .filter(|r| r.kind == kind && !r.modifiers.contains(Modifiers::CAVE))
            .count()
    };
    vec![
        heading("INLAND WATER"),
        note("THE SEA IS A FLAT BACKDROP"),
        note("SO RIVERS AND LAKES SHOW"),
        entry("OCEAN", Swatch::Color(inland_color(WaterKind::Sea))),
        entry("RIVER", Swatch::Color(inland_color(WaterKind::River))),
        entry("LAKE", Swatch::Color(inland_color(WaterKind::Lake))),
        entry("SWAMP", Swatch::Color(inland_color(WaterKind::Swamp))),
        entry("DESERT RIVER", Swatch::Color(inland_region_color(WaterKind::River, Modifiers::DESERT))),
        entry("DESERT LAKE", Swatch::Color(inland_region_color(WaterKind::Lake, Modifiers::DESERT))),
        heading("BACKGROUND"),
        entry("LAND", Swatch::Color(BG_LAND)),
        entry("NOT GENERATED", Swatch::Color(BG_VOID)),
        note("USES FILTERED WATER DATA"),
        heading("THIS MAP"),
        note(&format!("RIVERS {}", count(WaterKind::River))),
        note(&format!("LAKES {}", count(WaterKind::Lake))),
        note(&format!("SWAMPS {}", count(WaterKind::Swamp))),
        note(&format!("1 PIXEL = {blocks_per_pixel} BLOCKS")),
    ]
}

/// The ocean key and inland colours, including desert water, in one legend.
fn combined_rows(regions: &[WaterRegion], sea_level: i16, blocks_per_pixel: u32, min_area: u64) -> Vec<Row> {
    let mut rows = ocean_rows(regions, sea_level, blocks_per_pixel, min_area);
    rows[0] = heading("OCEAN AND INLAND WATER");
    rows[1] = note("OCEAN TEMPERATURE AND DEPTH");
    rows.splice(8..8, [
        heading("INLAND WATER"),
        entry("RIVER", Swatch::Color(inland_color(WaterKind::River))),
        entry("LAKE", Swatch::Color(inland_color(WaterKind::Lake))),
        entry("SWAMP", Swatch::Color(inland_color(WaterKind::Swamp))),
        entry("DESERT RIVER", Swatch::Color(inland_region_color(WaterKind::River, Modifiers::DESERT))),
        entry("DESERT LAKE", Swatch::Color(inland_region_color(WaterKind::Lake, Modifiers::DESERT))),
        note("USES FILTERED WATER DATA"),
        note("OCEAN HAS PRIORITY AT COAST"),
    ]);
    for row in &mut rows {
        if row.text == "LAND OR OTHER WATER" { row.text = "LAND".to_string(); }
    }
    rows
}

/// Renders the debug maps. Returns the byte size of each image.
pub fn render(
    dir: &Path,
    grid: &WorldGrid,
    regions: &[WaterRegion],
    sea_level: i16,
    opts: &MapOptions,
) -> std::io::Result<(u64, u64, u64, u64, u64, u64)> {
    let bounds = grid.world_bounds();
    let mut p = Projection::new(bounds, opts.scale);

    let blocks_per_pixel = opts.scale.max(1);
    let class_rows = classification_rows(regions, sea_level, blocks_per_pixel);
    let map_rows = region_rows(regions, blocks_per_pixel);
    let bathy_rows = depth_rows(sea_level, blocks_per_pixel);
    let sea_rows = ocean_rows(regions, sea_level, blocks_per_pixel, opts.ocean_min_area);
    let land_rows = inland_rows(regions, blocks_per_pixel);
    let combined_rows = combined_rows(regions, sea_level, blocks_per_pixel, opts.ocean_min_area);
    let scale = legend_scale(
        p.h,
        class_rows
            .len()
            .max(map_rows.len())
            .max(bathy_rows.len())
            .max(sea_rows.len())
            .max(land_rows.len())
            .max(combined_rows.len()),
    );
    let class_legend = Legend {
        scale,
        rows: class_rows,
    };
    let region_legend = Legend {
        scale,
        rows: map_rows,
    };
    let depth_legend = Legend {
        scale,
        rows: bathy_rows,
    };
    let ocean_legend = Legend {
        scale,
        rows: sea_rows,
    };
    let inland_legend = Legend {
        scale,
        rows: land_rows,
    };
    let combined_legend = Legend { scale, rows: combined_rows };
    // One panel width for every image so they can be flipped between.
    let panel = class_legend
        .width()
        .max(region_legend.width())
        .max(depth_legend.width())
        .max(ocean_legend.width())
        .max(inland_legend.width())
        .max(combined_legend.width());
    p.offset_x = panel;

    let height = p
        .h
        .max(class_legend.height())
        .max(region_legend.height())
        .max(depth_legend.height())
        .max(ocean_legend.height())
        .max(inland_legend.height())
        .max(combined_legend.height());

    let mut classification = Canvas::new(panel + p.w, height, BG_VOID);
    paint_land(&mut classification, grid, &p);
    paint_regions(&mut classification, regions, &p, |r, x, y| {
        modifier_overlay(r.modifiers, x, y).unwrap_or_else(|| region_color(r))
    });
    class_legend.draw(&mut classification, panel);
    let a = classification.write_png(&dir.join("water_map.png"))?;

    let mut per_region = Canvas::new(panel + p.w, height, BG_VOID);
    paint_land(&mut per_region, grid, &p);
    paint_regions(&mut per_region, regions, &p, |r, _, _| id_color(r.id));
    region_legend.draw(&mut per_region, panel);
    let b = per_region.write_png(&dir.join("water_regions_map.png"))?;

    let mut bathymetry = Canvas::new(panel + p.w, height, BG_VOID);
    paint_land(&mut bathymetry, grid, &p);
    paint_depth(&mut bathymetry, grid, &p);
    depth_legend.draw(&mut bathymetry, panel);
    let c = bathymetry.write_png(&dir.join("water_depth_map.png"))?;

    let mut ocean = Canvas::new(panel + p.w, height, BG_VOID);
    paint_land(&mut ocean, grid, &p);
    paint_ocean(&mut ocean, grid, regions, &p, opts.ocean_min_area);
    ocean_legend.draw(&mut ocean, panel);
    let d = ocean.write_png(&dir.join("water_ocean_map.png"))?;

    let mut inland = Canvas::new(panel + p.w, height, BG_VOID);
    paint_land(&mut inland, grid, &p);
    paint_inland(&mut inland, regions, &p, true);
    inland_legend.draw(&mut inland, panel);
    let e = inland.write_png(&dir.join("water_inland_map.png"))?;

    // Reuse the cleaned ocean layer, preserving its pixels when inland water
    // shares a coastal pixel. The combined view gives ocean zones priority.
    paint_inland(&mut ocean, regions, &p, false);
    combined_legend.draw(&mut ocean, panel);
    let f = ocean.write_png(&dir.join("water_combined_map.png"))?;

    Ok((a, b, c, d, e, f))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(id: u32, kind: WaterKind, t: Temperature, d: Option<Depth>) -> WaterRegion {
        WaterRegion {
            id,
            geometry: RegionGeometry::default(),
            kind,
            temperature: t,
            vegetation: Vegetation::Normal,
            depth: d,
            modifiers: Modifiers::empty(),
            surface_y: 62,
            bathymetry: Bathymetry::default(),
            dominant_biome: String::new(),
        }
    }

    #[test]
    fn every_kind_temperature_depth_combination_has_its_own_colour() {
        let mut seen = std::collections::HashSet::new();
        for kind in [
            WaterKind::Sea,
            WaterKind::River,
            WaterKind::Lake,
            WaterKind::Swamp,
        ] {
            for t in [Temperature::Warm, Temperature::Medium, Temperature::Cold] {
                // Every kind carries a measured depth now, not just the sea.
                for d in [
                    Some(Depth::Shallow),
                    Some(Depth::Normal),
                    Some(Depth::Deep),
                ] {
                    let c = region_color(&region(0, kind, t, d));
                    assert!(seen.insert(c), "duplicate colour {c:?} for {kind:?} {t:?} {d:?}");
                }
            }
        }
    }

    #[test]
    fn depth_is_readable_as_brightness_whatever_the_temperature() {
        // The whole point of splitting the axes: every shallow patch must be
        // brighter than every normal one, and every normal one brighter than
        // every deep one, across all kinds and temperatures.
        let mut bands: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for kind in [
            WaterKind::Sea,
            WaterKind::River,
            WaterKind::Lake,
            WaterKind::Swamp,
        ] {
            for t in [Temperature::Warm, Temperature::Medium, Temperature::Cold] {
                for (i, d) in [Depth::Shallow, Depth::Normal, Depth::Deep]
                    .into_iter()
                    .enumerate()
                {
                    let c = style_color(kind, t, Some(d), Vegetation::Normal);
                    bands[i].push(luminance(c));
                }
            }
        }
        let min = |v: &Vec<f32>| v.iter().cloned().fold(f32::MAX, f32::min);
        let max = |v: &Vec<f32>| v.iter().cloned().fold(f32::MIN, f32::max);
        assert!(
            min(&bands[0]) > max(&bands[1]),
            "shallow {:.3} must stay brighter than normal {:.3}",
            min(&bands[0]),
            max(&bands[1])
        );
        assert!(
            min(&bands[1]) > max(&bands[2]),
            "normal {:.3} must stay brighter than deep {:.3}",
            min(&bands[1]),
            max(&bands[2])
        );
    }

    #[test]
    fn the_inland_map_separates_every_kind_from_the_sea_backdrop() {
        let mut seen = std::collections::HashSet::new();
        for kind in [
            WaterKind::Sea,
            WaterKind::River,
            WaterKind::Lake,
            WaterKind::Swamp,
        ] {
            assert!(seen.insert(inland_color(kind)), "{kind:?} duplicates a colour");
        }
        // The sea has to recede: every inland kind is clearly brighter than it.
        let sea = luminance(inland_color(WaterKind::Sea));
        for kind in [WaterKind::River, WaterKind::Lake, WaterKind::Swamp] {
            let c = luminance(inland_color(kind));
            assert!(
                c - sea > 0.2,
                "{kind:?} at {c:.3} does not stand out from the sea at {sea:.3}"
            );
        }
        // And none of them is confusable with the land underneath.
        for kind in [WaterKind::River, WaterKind::Lake, WaterKind::Swamp] {
            let c = inland_color(kind);
            let d: i32 = (0..3)
                .map(|i| (c[i] as i32 - BG_LAND[i] as i32).abs())
                .sum();
            assert!(d > 200, "{kind:?} is too close to the land colour");
        }
    }

    #[test]
    fn the_inland_legend_names_all_four_kinds() {
        let rows = inland_rows(&[], 8);
        let text: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        for name in ["OCEAN", "RIVER", "LAKE", "SWAMP"] {
            assert!(text.contains(&name), "legend is missing {name}");
        }
    }

    #[test]
    fn the_six_ocean_colours_are_all_distinct() {
        let mut seen = std::collections::HashSet::new();
        for t in [Temperature::Warm, Temperature::Medium, Temperature::Cold] {
            for deep in [false, true] {
                assert!(
                    seen.insert(ocean_color(t, deep)),
                    "{t:?}/{deep} duplicates another ocean colour"
                );
            }
        }
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn every_ocean_basin_shade_is_darker_than_its_shelf() {
        for t in [Temperature::Warm, Temperature::Medium, Temperature::Cold] {
            let shelf = luminance(ocean_color(t, false));
            let basin = luminance(ocean_color(t, true));
            assert!(
                basin < shelf * 0.7,
                "{t:?}: shelf {shelf:.3} and basin {basin:.3} are too close"
            );
        }
    }

    #[test]
    fn the_ocean_map_only_lists_the_six_zones_and_says_where_they_come_from() {
        let rows = ocean_rows(&[], 63, 8, crate::config::OCEAN_MAP_MIN_AREA);
        let text: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        for t in ["WARM", "MEDIUM", "COLD"] {
            let n = text.iter().filter(|s| s.starts_with(t)).count();
            assert_eq!(n, 2, "{t} should appear once per depth step");
        }
        assert!(text.iter().any(|t| t.contains("SEA REGIONS ONLY")));
    }

    #[test]
    fn projection_can_map_a_pixel_back_to_the_world() {
        let mut p = Projection::new((-1024, -512, 1023, 511), 8);
        p.offset_x = 40;
        for x in [-1024, -1000, 0, 500, 1023] {
            let (px, _) = p.px(x, 0);
            // The round trip lands inside the block the pixel covers.
            let back = p.world_x(px);
            assert!(back <= x && x - back < 8, "{x} -> {px} -> {back}");
        }
    }

    #[test]
    fn the_depth_ramp_gets_darker_with_depth() {
        let mut last = 1.0f32;
        for blocks in [0u16, 5, 10, 20, 30, 45, 60, 80, 100] {
            let l = luminance(depth_ramp(blocks));
            assert!(
                l < last,
                "depth {blocks} is not darker than the step before it"
            );
            last = l;
        }
        // Past the last stop the ramp saturates instead of wrapping around.
        assert_eq!(depth_ramp(100), depth_ramp(500));
        // The three classification bands land in visibly different places.
        assert!(luminance(depth_ramp(5)) - luminance(depth_ramp(20)) > 0.1);
        assert!(luminance(depth_ramp(20)) - luminance(depth_ramp(45)) > 0.1);
    }

    #[test]
    fn temperature_still_separates_colours_at_equal_depth() {
        let mut seen = std::collections::HashSet::new();
        for t in [Temperature::Warm, Temperature::Medium, Temperature::Cold] {
            let c = style_color(WaterKind::Sea, t, Some(Depth::Normal), Vegetation::Normal);
            assert!(seen.insert(c), "temperatures collapsed to one colour");
        }
    }

    #[test]
    fn region_ids_get_distinct_colours() {
        let mut seen = std::collections::HashSet::new();
        for id in 0..256u32 {
            seen.insert(id_color(id));
        }
        assert!(seen.len() > 240, "only {} distinct colours", seen.len());
    }

    #[test]
    fn projection_maps_world_bounds_onto_the_canvas() {
        let mut p = Projection::new((-1024, -512, 1023, 511), 8);
        assert_eq!(p.w, 256);
        assert_eq!(p.h, 128);
        assert_eq!(p.px(-1024, -512), (0, 0));
        assert_eq!(p.px(1023, 511), (255, 127));
        // The legend panel shifts the map to the right, never up or down.
        p.offset_x = 40;
        assert_eq!(p.px(-1024, -512), (40, 0));
        assert_eq!(p.px(1023, 511), (295, 127));
    }

    #[test]
    fn modifier_overlay_only_fires_for_set_flags() {
        assert!((0..64).all(|x| modifier_overlay(Modifiers::empty(), x, 0).is_none()));
        assert!((0..64).any(|x| modifier_overlay(Modifiers::ICE, x, 0).is_some()));
        assert!((0..64)
            .flat_map(|x| (0..64).map(move |y| (x, y)))
            .any(|(x, y)| modifier_overlay(Modifiers::CORALS, x, y).is_some()));
    }

    #[test]
    fn the_legend_covers_every_value_of_every_enum() {
        let rows = classification_rows(&[], 63, 8);
        let text: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        for kind in ["SEA", "RIVER", "LAKE", "SWAMP"] {
            assert!(text.contains(&kind), "legend is missing kind {kind}");
        }
        for t in ["WARM", "MEDIUM", "COLD"] {
            assert!(text.contains(&t), "legend is missing temperature {t}");
        }
        for m in ["ICE", "CORALS", "DESERT", "MANGROVE", "CAVE"] {
            assert!(text.contains(&m), "legend is missing modifier {m}");
        }
        assert!(text.iter().any(|t| t.starts_with("SHALLOW")));
        assert!(text.iter().any(|t| t.starts_with("NORMAL")));
        assert!(text.iter().any(|t| t.starts_with("DEEP")));
    }

    #[test]
    fn legend_entries_show_the_colour_they_describe() {
        let rows = classification_rows(&[], 63, 8);
        let find = |name: &str| {
            rows.iter()
                .find(|r| r.text == name)
                .unwrap_or_else(|| panic!("no legend row {name}"))
        };
        // The kind swatches must be the colours the map paints.
        match find("RIVER").swatch {
            Swatch::Color(c) => assert_eq!(
                c,
                style_color(
                    WaterKind::River,
                    Temperature::Medium,
                    Some(Depth::Normal),
                    Vegetation::Normal
                )
            ),
            _ => panic!("RIVER should have a plain colour swatch"),
        }
        // Modifiers must be drawn as patterns, otherwise the legend would not
        // show what to look for on the map.
        assert!(matches!(find("ICE").swatch, Swatch::Pattern(_, Modifiers::ICE)));
        assert!(matches!(
            find("MANGROVE").swatch,
            Swatch::Pattern(_, Modifiers::MANGROVE)
        ));
    }

    #[test]
    fn the_four_temperature_swatches_are_visibly_different() {
        let rows = classification_rows(&[], 63, 8);
        let mut seen = std::collections::HashSet::new();
        for name in ["WARM", "MEDIUM", "COLD"] {
            let row = rows.iter().find(|r| r.text == name).unwrap();
            match row.swatch {
                Swatch::Color(c) => assert!(seen.insert(c), "{name} duplicates another swatch"),
                _ => panic!("{name} should have a colour swatch"),
            }
        }
    }

    #[test]
    fn legend_scale_shrinks_to_fit_a_small_map() {
        // A tall map can afford big text, a short one cannot.
        assert_eq!(legend_scale(4000, 30), 5);
        assert!(legend_scale(400, 30) < 5);
        assert!(legend_scale(10, 30) >= 1);
    }

    #[test]
    fn legend_box_is_measured_from_its_longest_row() {
        let legend = Legend {
            rows: vec![heading("AB"), entry("ABCDEFGHIJ", Swatch::Color(BG_LAND))],
            scale: 2,
        };
        let short = Legend {
            rows: vec![heading("AB"), entry("AB", Swatch::Color(BG_LAND))],
            scale: 2,
        };
        assert!(legend.width() > short.width());
        assert!(legend.height() > 0);
    }
}
