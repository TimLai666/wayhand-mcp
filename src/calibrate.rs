//! Desktop-target calibration: find the cursor in screenshots after moving it
//! to known uinput positions, then fit the pixel -> ABS affine transform.
//!
//! Everything here is pure so it can be tested with synthetic images. The
//! server drives the move/screenshot loop and feeds the results in.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::coords::{ABSOLUTE_MAX, Transform};

/// (screenshot pixel, ABS coordinate) observation pair.
pub type Observation = ((f64, f64), (f64, f64));

/// Fractions of the ABS range that the pointer is sent to, in order. The
/// first one is the baseline position; the others are compared against it.
pub const PROBE_FRACTIONS: [(f64, f64); 4] = [(0.2, 0.2), (0.8, 0.2), (0.5, 0.8), (0.8, 0.7)];
const DIFF_THRESHOLD: u32 = 40;
const MAX_CURSOR_SIZE: u32 = 96;
const MIN_CURSOR_PIXELS: usize = 12;

pub fn probe_abs(fraction: (f64, f64)) -> (u32, u32) {
    let scale = |f: f64| (f * f64::from(ABSOLUTE_MAX)).round() as u32;
    (scale(fraction.0), scale(fraction.1))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Blob {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
    pub pixels: usize,
}

impl Blob {
    /// The cursor hotspot for an arrow cursor is its top-left corner.
    pub fn hotspot(&self) -> (f64, f64) {
        (f64::from(self.min_x), f64::from(self.min_y))
    }

    fn width(&self) -> u32 {
        self.max_x - self.min_x + 1
    }

    fn height(&self) -> u32 {
        self.max_y - self.min_y + 1
    }
}

/// Decoded RGB image.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Image {
    pub fn from_png(bytes: &[u8]) -> Result<Self> {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().context("decode PNG")?;
        let mut buf = vec![
            0;
            reader
                .output_buffer_size()
                .ok_or_else(|| anyhow!("PNG too large"))?
        ];
        let info = reader.next_frame(&mut buf).context("read PNG frame")?;
        let bytes_per_pixel = info.color_type.samples() * usize::from(info.bit_depth as u8 / 8);
        if bytes_per_pixel < 3 {
            return Err(anyhow!("PNG is not an RGB image"));
        }
        let mut rgb = Vec::with_capacity((info.width * info.height * 3) as usize);
        for pixel in buf[..info.buffer_size()].chunks_exact(bytes_per_pixel) {
            rgb.extend_from_slice(&pixel[..3]);
        }
        Ok(Self {
            width: info.width,
            height: info.height,
            rgb,
        })
    }

    fn pixel(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let i = ((y * self.width + x) * 3) as usize;
        (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2])
    }
}

/// Connected regions where `a` and `b` differ. Regions larger than a cursor
/// (window content changing) are dropped.
pub fn changed_blobs(a: &Image, b: &Image) -> Result<Vec<Blob>> {
    if a.width != b.width || a.height != b.height {
        return Err(anyhow!(
            "screenshots differ in size ({}x{} vs {}x{}); the screen changed during calibration",
            a.width,
            a.height,
            b.width,
            b.height
        ));
    }
    let (width, height) = (a.width, a.height);
    let mut mask = vec![false; (width * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let (r1, g1, b1) = a.pixel(x, y);
            let (r2, g2, b2) = b.pixel(x, y);
            let diff = u32::from(r1.abs_diff(r2))
                + u32::from(g1.abs_diff(g2))
                + u32::from(b1.abs_diff(b2));
            if diff > DIFF_THRESHOLD {
                mask[(y * width + x) as usize] = true;
            }
        }
    }

    let mut blobs = Vec::new();
    let mut visited = vec![false; mask.len()];
    let mut stack = Vec::new();
    for start in 0..mask.len() {
        if !mask[start] || visited[start] {
            continue;
        }
        let mut blob = Blob {
            min_x: u32::MAX,
            min_y: u32::MAX,
            max_x: 0,
            max_y: 0,
            pixels: 0,
        };
        visited[start] = true;
        stack.push(start);
        while let Some(index) = stack.pop() {
            let x = index as u32 % width;
            let y = index as u32 / width;
            blob.min_x = blob.min_x.min(x);
            blob.min_y = blob.min_y.min(y);
            blob.max_x = blob.max_x.max(x);
            blob.max_y = blob.max_y.max(y);
            blob.pixels += 1;
            // 8-neighbourhood with a 2px reach so anti-aliased cursor edges stay connected.
            for dy in -2i64..=2 {
                for dx in -2i64..=2 {
                    let nx = i64::from(x) + dx;
                    let ny = i64::from(y) + dy;
                    if nx < 0 || ny < 0 || nx >= i64::from(width) || ny >= i64::from(height) {
                        continue;
                    }
                    let neighbour = (ny as u32 * width + nx as u32) as usize;
                    if mask[neighbour] && !visited[neighbour] {
                        visited[neighbour] = true;
                        stack.push(neighbour);
                    }
                }
            }
        }
        if blob.pixels >= MIN_CURSOR_PIXELS
            && blob.width() <= MAX_CURSOR_SIZE
            && blob.height() <= MAX_CURSOR_SIZE
        {
            blobs.push(blob);
        }
    }
    Ok(blobs)
}

/// Pick the blob whose hotspot is closest to `expected`.
pub fn closest_blob(blobs: &[Blob], expected: (f64, f64)) -> Option<Blob> {
    blobs.iter().copied().min_by(|a, b| {
        let da = distance(a.hotspot(), expected);
        let db = distance(b.hotspot(), expected);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Solve a 3x3 linear system by Gaussian elimination.
fn solve3(mut m: [[f64; 4]; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let pivot = (col..3).max_by(|&a, &b| {
            m[a][col]
                .abs()
                .partial_cmp(&m[b][col].abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
        if m[pivot][col].abs() < 1e-9 {
            return None;
        }
        m.swap(col, pivot);
        for row in 0..3 {
            if row == col {
                continue;
            }
            let factor = m[row][col] / m[col][col];
            let pivot_row = m[col];
            for (cell, pivot_cell) in m[row].iter_mut().zip(pivot_row.iter()).skip(col) {
                *cell -= factor * pivot_cell;
            }
        }
    }
    Some([m[0][3] / m[0][0], m[1][3] / m[1][1], m[2][3] / m[2][2]])
}

/// Least-squares affine fit `abs = M * pixel + t` from (pixel, abs) pairs.
/// Returns the transform and the worst residual in pixels.
pub fn fit_transform(pairs: &[Observation]) -> Result<(Transform, f64)> {
    if pairs.len() < 3 {
        return Err(anyhow!(
            "need at least 3 cursor observations, found {}",
            pairs.len()
        ));
    }
    // Normal equations: sum over pairs of [x y 1]^T [x y 1] * coeffs = [x y 1]^T target
    let mut normal = [[0.0f64; 3]; 3];
    let mut rhs_x = [0.0f64; 3];
    let mut rhs_y = [0.0f64; 3];
    for &((px, py), (ax, ay)) in pairs {
        let row = [px, py, 1.0];
        for i in 0..3 {
            for j in 0..3 {
                normal[i][j] += row[i] * row[j];
            }
            rhs_x[i] += row[i] * ax;
            rhs_y[i] += row[i] * ay;
        }
    }
    let augment = |rhs: [f64; 3]| {
        let mut m = [[0.0f64; 4]; 3];
        for i in 0..3 {
            m[i][..3].copy_from_slice(&normal[i]);
            m[i][3] = rhs[i];
        }
        m
    };
    let cx = solve3(augment(rhs_x)).ok_or_else(|| anyhow!("cursor observations are collinear"))?;
    let cy = solve3(augment(rhs_y)).ok_or_else(|| anyhow!("cursor observations are collinear"))?;
    let transform = Transform {
        m11: cx[0],
        m12: cx[1],
        m13: cx[2],
        m21: cy[0],
        m22: cy[1],
        m23: cy[2],
    };

    // Residual expressed in pixels: invert the fitted map for each observed ABS.
    let det = transform.m11 * transform.m22 - transform.m12 * transform.m21;
    if det.abs() < 1e-9 {
        return Err(anyhow!("fitted transform is degenerate"));
    }
    let mut worst = 0.0f64;
    for &((px, py), (ax, ay)) in pairs {
        let ux = ax - transform.m13;
        let uy = ay - transform.m23;
        let ix = (transform.m22 * ux - transform.m12 * uy) / det;
        let iy = (-transform.m21 * ux + transform.m11 * uy) / det;
        worst = worst.max(distance((ix, iy), (px, py)));
    }
    Ok((transform, worst))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Calibration {
    pub width: u32,
    pub height: u32,
    pub transform: Transform,
    pub max_residual_px: f64,
    pub calibrated_at: String,
}

pub fn calibration_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(base.join("wayhand-mcp").join("calibration.json"))
}

pub fn load_calibration() -> Option<Calibration> {
    let path = calibration_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_calibration(calibration: &Calibration) -> Result<PathBuf> {
    let path = calibration_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(calibration)?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{Image, changed_blobs, closest_blob, fit_transform, probe_abs};
    use crate::coords::ABSOLUTE_MAX;

    fn blank(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            rgb: vec![200; (width * height * 3) as usize],
        }
    }

    fn draw_cursor(image: &mut Image, x: u32, y: u32) {
        // A 12x18 arrow-ish block whose top-left is the hotspot.
        for dy in 0..18 {
            for dx in 0..12 {
                if dx <= dy {
                    let i = (((y + dy) * image.width + x + dx) * 3) as usize;
                    image.rgb[i] = 0;
                    image.rgb[i + 1] = 0;
                    image.rgb[i + 2] = 0;
                }
            }
        }
    }

    #[test]
    fn finds_cursor_blobs_and_hotspots() {
        let baseline = {
            let mut image = blank(200, 150);
            draw_cursor(&mut image, 20, 20);
            image
        };
        let moved = {
            let mut image = blank(200, 150);
            draw_cursor(&mut image, 150, 100);
            image
        };
        let blobs = changed_blobs(&baseline, &moved).unwrap();
        assert_eq!(blobs.len(), 2);
        let near_new = closest_blob(&blobs, (148.0, 98.0)).unwrap();
        assert_eq!(near_new.hotspot(), (150.0, 100.0));
        let near_old = closest_blob(&blobs, (0.0, 0.0)).unwrap();
        assert_eq!(near_old.hotspot(), (20.0, 20.0));
    }

    #[test]
    fn large_changes_are_not_cursor_blobs() {
        let baseline = blank(200, 150);
        let mut changed = blank(200, 150);
        for i in 0..(200 * 150 * 3) {
            changed.rgb[i] = 0;
        }
        assert!(changed_blobs(&baseline, &changed).unwrap().is_empty());
    }

    #[test]
    fn fits_a_known_affine_transform() {
        let truth = |px: f64, py: f64| (px * 22.75 + 100.0, py * 36.4 - 50.0);
        let pairs: Vec<_> = [(10.0, 10.0), (500.0, 20.0), (300.0, 400.0), (900.0, 700.0)]
            .into_iter()
            .map(|(px, py)| ((px, py), truth(px, py)))
            .collect();
        let (transform, residual) = fit_transform(&pairs).unwrap();
        assert!((transform.m11 - 22.75).abs() < 1e-6);
        assert!((transform.m22 - 36.4).abs() < 1e-6);
        assert!((transform.m13 - 100.0).abs() < 1e-3);
        assert!((transform.m23 + 50.0).abs() < 1e-3);
        assert!(residual < 1e-6);
    }

    #[test]
    fn rejects_collinear_observations() {
        let pairs = vec![
            ((0.0, 0.0), (0.0, 0.0)),
            ((10.0, 10.0), (100.0, 100.0)),
            ((20.0, 20.0), (200.0, 200.0)),
        ];
        assert!(fit_transform(&pairs).is_err());
    }

    #[test]
    fn probe_positions_stay_inside_abs_range() {
        for fraction in super::PROBE_FRACTIONS {
            let (x, y) = probe_abs(fraction);
            assert!(x <= u32::from(ABSOLUTE_MAX) && y <= u32::from(ABSOLUTE_MAX));
        }
    }
}
