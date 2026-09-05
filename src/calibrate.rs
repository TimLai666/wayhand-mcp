//! Desktop-target calibration: find the cursor in screenshots after moving it
//! to known uinput positions, then fit the pixel -> ABS affine transform.
//!
//! Everything here is pure so it can be tested with synthetic images. The
//! server drives the move/screenshot loop and feeds the results in.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::coords::Transform;

/// (screenshot pixel, ABS coordinate) observation pair.
pub type Observation = ((f64, f64), (f64, f64));
/// (observed desktop pixel, ABS coordinate, intended desktop pixel).
pub type Probe = ((f64, f64), (f64, f64), (f64, f64));

/// Fractions of the ABS range that the pointer is sent to, in order. The
/// first one is the baseline position; the others are compared against it.
/// Positions inside the sandbox window (fractions of its rectangle) that the
/// real pointer is sent to during calibration.
pub const PROBE_FRACTIONS: [(f64, f64); 4] = [(0.2, 0.2), (0.8, 0.25), (0.5, 0.8), (0.75, 0.7)];
/// Background colour the sandbox compositor paints so its window can be found
/// in a desktop screenshot.
pub const SANDBOX_BG: (u8, u8, u8) = (255, 0, 255);
const MAX_CURSOR_SIZE: u32 = 96;
const MIN_CURSOR_PIXELS: usize = 12;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

fn is_bg(pixel: (u8, u8, u8)) -> bool {
    let (r, g, b) = pixel;
    r.abs_diff(SANDBOX_BG.0) < 24 && g.abs_diff(SANDBOX_BG.1) < 24 && b.abs_diff(SANDBOX_BG.2) < 24
}

/// Bounding box of the sandbox background colour in a desktop screenshot.
/// Rows and columns must be mostly background so stray pixels do not count.
pub fn find_sandbox_rect(image: &Image) -> Result<Rect> {
    let mut rows = vec![0u32; image.height as usize];
    let mut cols = vec![0u32; image.width as usize];
    for y in 0..image.height {
        for x in 0..image.width {
            if is_bg(image.pixel(x, y)) {
                rows[y as usize] += 1;
                cols[x as usize] += 1;
            }
        }
    }
    let span = |counts: &[u32], min_count: u32| -> Option<(u32, u32)> {
        let first = counts.iter().position(|&c| c >= min_count)? as u32;
        let last = counts.iter().rposition(|&c| c >= min_count)? as u32;
        Some((first, last))
    };
    let (x0, x1) = span(&cols, 200)
        .ok_or_else(|| anyhow!("sandbox window not found in the desktop screenshot"))?;
    let (y0, y1) = span(&rows, 200)
        .ok_or_else(|| anyhow!("sandbox window not found in the desktop screenshot"))?;
    let rect = Rect {
        x: x0,
        y: y0,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
    };
    if rect.width < 320 || rect.height < 180 {
        return Err(anyhow!(
            "sandbox window candidate {rect:?} is too small; is the sandbox window visible and unobstructed?"
        ));
    }
    Ok(rect)
}

/// Top-left of the largest non-background blob in a sandbox screenshot: the
/// cursor drawn by the sandbox compositor.
pub fn find_cursor_on_bg(image: &Image) -> Option<Blob> {
    let mut mask = vec![false; (image.width * image.height) as usize];
    for y in 0..image.height {
        for x in 0..image.width {
            if !is_bg(image.pixel(x, y)) {
                mask[(y * image.width + x) as usize] = true;
            }
        }
    }
    blobs_from_mask(&mask, image.width, image.height)
        .into_iter()
        .max_by_key(|blob| blob.pixels)
}

fn blobs_from_mask(mask: &[bool], width: u32, height: u32) -> Vec<Blob> {
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
    blobs
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
    use super::{Image, SANDBOX_BG, find_cursor_on_bg, find_sandbox_rect, fit_transform};

    fn filled(width: u32, height: u32, color: (u8, u8, u8)) -> Image {
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..width * height {
            rgb.extend_from_slice(&[color.0, color.1, color.2]);
        }
        Image { width, height, rgb }
    }

    fn paint(image: &mut Image, x0: u32, y0: u32, w: u32, h: u32, color: (u8, u8, u8)) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let i = ((y * image.width + x) * 3) as usize;
                image.rgb[i] = color.0;
                image.rgb[i + 1] = color.1;
                image.rgb[i + 2] = color.2;
            }
        }
    }

    #[test]
    fn finds_sandbox_rectangle_in_desktop_screenshot() {
        let mut desktop = filled(1000, 700, (30, 30, 30));
        paint(&mut desktop, 120, 80, 640, 360, SANDBOX_BG);
        // a stray magenta pixel elsewhere must not stretch the rectangle
        paint(&mut desktop, 900, 650, 1, 1, SANDBOX_BG);
        let rect = find_sandbox_rect(&desktop).unwrap();
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (120, 80, 640, 360)
        );
    }

    #[test]
    fn rejects_when_no_sandbox_window() {
        let desktop = filled(1000, 700, (30, 30, 30));
        assert!(find_sandbox_rect(&desktop).is_err());
    }

    #[test]
    fn finds_cursor_hotspot_on_background() {
        let mut sandbox = filled(640, 360, SANDBOX_BG);
        paint(&mut sandbox, 200, 100, 12, 18, (0, 0, 0));
        let cursor = find_cursor_on_bg(&sandbox).unwrap();
        assert_eq!(cursor.hotspot(), (200.0, 100.0));
        assert!(find_cursor_on_bg(&filled(64, 64, SANDBOX_BG)).is_none());
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
}
