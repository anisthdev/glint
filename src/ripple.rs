//! CPU port of the AOSP SystemUI wired-charging ripple (`RippleShader.kt`, circle shape).
//! The upstream source is kept in `reference/aosp-charging-ripple/`.
//!
//! Android evaluates the shader per pixel on the GPU. To keep this cheap on the CPU:
//! - everything radial (fill, ring, sparkle mask) becomes a per-frame lookup table
//!   indexed by distance from the center,
//! - the edge distortion is sampled on a coarse grid and interpolated,
//! - sparkle noise is a precomputed tile, its twinkle a 256-entry table per frame,
//! - fully transparent blocks are skipped and only previously drawn ones are cleared,
//! - rows are split across threads.

use crate::Color;
use std::f32::consts::{PI, SQRT_2};
use std::thread;

const BLOCK: usize = 16;
const NOISE_TILE: usize = 512;
// Sparkles are negligible below this noise value and zero above the max, so the
// quantized range only has to cover this window.
const NOISE_MIN: f32 = -0.5;
const NOISE_MAX: f32 = 0.2;
const SPARKLE_CELL: f32 = 0.8;
const SPARKLE_STRENGTH: f32 = 0.3;
const RIPPLE_ALPHA: f32 = 115.0 / 255.0;
const DISTORT_STRENGTH: f32 = 75.0;
const VISIBLE_EPS: f32 = 0.5 / 255.0;
const MAX_THREADS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl Rect {
    pub fn full(width: u32, height: u32) -> Self {
        Self {
            x0: 0,
            y0: 0,
            x1: width,
            y1: height,
        }
    }

    pub fn merge(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
        match (a, b) {
            (Some(a), Some(b)) => Some(Rect {
                x0: a.x0.min(b.x0),
                y0: a.y0.min(b.y0),
                x1: a.x1.max(b.x1),
                y1: a.y1.max(b.y1),
            }),
            (a, None) => a,
            (None, b) => b,
        }
    }

    fn intersects(self, other: Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }
}

#[derive(Debug, Clone)]
pub struct ChargeRipple {
    noise: Vec<u8>,
    color: Color,
    /// Center as a fraction of the screen size, (0, 0) being top left.
    origin: (f32, f32),
}

impl ChargeRipple {
    pub fn new(color: Color, origin: (f32, f32)) -> Self {
        Self {
            noise: build_noise_tile(),
            color,
            origin,
        }
    }

    /// Renders one frame into an ARGB8888 buffer. `prev` is what this buffer held from
    /// its last use; those pixels are cleared where the new frame is transparent.
    /// Returns the region that now holds non-transparent pixels.
    pub fn render(
        &self,
        pixels: &mut [u8],
        width: u32,
        height: u32,
        raw_progress: f32,
        time_ms: f32,
        prev: Option<Rect>,
    ) -> Option<Rect> {
        let frame = Frame::new(self, width, height, raw_progress, time_ms);
        let width = width as usize;
        let height = height as usize;
        let stride = width * 4;
        let blocks_y = height.div_ceil(BLOCK);
        let threads = thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, MAX_THREADS)
            .min(blocks_y.max(1));
        let band_blocks = blocks_y.div_ceil(threads).max(1);

        thread::scope(|scope| {
            let workers: Vec<_> = pixels[..stride * height]
                .chunks_mut(band_blocks * BLOCK * stride)
                .enumerate()
                .map(|(band_idx, band)| {
                    let frame = &frame;
                    let origin_y = band_idx * band_blocks * BLOCK;
                    scope.spawn(move || frame.render_band(band, width, origin_y, prev))
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("ripple worker panicked"))
                .fold(None, Rect::merge)
        })
    }
}

struct Frame<'a> {
    noise: &'a [u8],
    cx: f32,
    cy: f32,
    time_ms: f32,
    distort_radial: f32,
    distort_xy: f32,
    color: [f32; 3],
    /// Premultiplied ripple alpha by distance.
    alpha: Vec<f32>,
    /// Sparkle visibility by distance.
    mask: Vec<f32>,
    /// Packed pixel by distance for when no sparkle is visible.
    solid: Vec<u32>,
    /// Sparkle intensity by quantized noise value.
    sparkle: [f32; 256],
    /// Distance range with anything visible.
    visible: Option<(f32, f32)>,
}

impl<'a> Frame<'a> {
    fn new(ripple: &'a ChargeRipple, width: u32, height: u32, raw: f32, time_ms: f32) -> Self {
        let w = width as f32;
        let h = height as f32;
        let progress = standard_ease(raw);
        let cx = w * ripple.origin.0;
        let cy = h * ripple.origin.1;
        // Upstream grows to the longest screen side, which suits a port at the bottom
        // middle of a phone. Reaching the farthest corner generalizes that to any origin.
        let reach = (cx.max(w - cx)).hypot(cy.max(h - cy));
        let radius = reach * progress;
        let blur = lerp(1.25, 0.5, progress);
        let fade_sparkle = fade(raw, 0.0, 0.1, 0.4, 1.0);
        let fade_ring = fade(raw, 0.0, 0.1, 0.3, 1.0);
        let fade_fill = fade(raw, 0.0, 0.0, 0.0, 0.6);
        let color_alpha = RIPPLE_ALPHA * ripple.color.a as f32 / 255.0;
        let color = [
            ripple.color.r as f32 / 255.0,
            ripple.color.g as f32 / 255.0,
            ripple.color.b as f32 / 255.0,
        ];

        let mut frame = Self {
            noise: &ripple.noise,
            cx,
            cy,
            time_ms,
            distort_radial: DISTORT_STRENGTH * raw * (1.0 - raw),
            distort_xy: DISTORT_STRENGTH * (1.0 - raw),
            color,
            alpha: Vec::new(),
            mask: Vec::new(),
            solid: Vec::new(),
            sparkle: [0.0; 256],
            visible: None,
        };
        if radius < 0.5 {
            return frame;
        }

        let len = (w.hypot(h) + 2.0 * DISTORT_STRENGTH * SQRT_2).ceil() as usize + 2;
        frame.alpha.reserve_exact(len);
        frame.mask.reserve_exact(len);
        frame.solid.reserve_exact(len);
        let outer = radius * 1.25;
        let (mut lo, mut hi) = (usize::MAX, 0);
        for idx in 0..len {
            let d = idx as f32;
            let sd_outer = (d - outer) / outer;
            let sd_inner = (d - radius) / radius;
            let sparkle_ring = soften(sd_outer.max(-sd_inner), blur);
            let inside = soften(sd_outer, blur);
            let alpha =
                ((1.0 - inside) * fade_fill).max((1.0 - sparkle_ring) * fade_ring) * color_alpha;
            let mask = (1.0 - sparkle_ring) * fade_sparkle;
            if alpha > VISIBLE_EPS || mask > VISIBLE_EPS {
                lo = lo.min(idx);
                hi = idx;
            }
            frame.alpha.push(alpha);
            frame.mask.push(mask);
            frame.solid.push(pack(color.map(|c| c * alpha), alpha));
        }
        if lo <= hi {
            frame.visible = Some((lo as f32, hi as f32 + 1.0));
        }

        let t = time_ms * 0.00175;
        for (q, value) in frame.sparkle.iter_mut().enumerate() {
            *value = sparkles(dequantize_noise(q as u8), t);
        }
        frame
    }

    /// Shades one horizontal band of whole block rows starting at pixel row `origin_y`.
    fn render_band(
        &self,
        band: &mut [u8],
        width: usize,
        origin_y: usize,
        prev: Option<Rect>,
    ) -> Option<Rect> {
        let stride = width * 4;
        let rows = band.len() / stride;
        let blocks_x = width.div_ceil(BLOCK);
        let blocks_y = rows.div_ceil(BLOCK);
        let cols = blocks_x + 1;

        let mut corners = Vec::new();
        if self.visible.is_some() {
            corners.reserve_exact(cols * (blocks_y + 1));
            for by in 0..=blocks_y {
                let y = (origin_y + by * BLOCK) as f32;
                for bx in 0..cols {
                    corners.push(self.distance((bx * BLOCK) as f32, y));
                }
            }
        }

        let mut drawn = None;
        for by in 0..blocks_y {
            let y0 = by * BLOCK;
            let y1 = (y0 + BLOCK).min(rows);
            for bx in 0..blocks_x {
                let x0 = bx * BLOCK;
                let x1 = (x0 + BLOCK).min(width);
                let area = Rect {
                    x0: x0 as u32,
                    y0: (origin_y + y0) as u32,
                    x1: x1 as u32,
                    y1: (origin_y + y1) as u32,
                };
                let corner = |cx: usize, cy: usize| {
                    corners
                        .get((by + cy) * cols + bx + cx)
                        .copied()
                        .unwrap_or(0.0)
                };
                let c = [corner(0, 0), corner(1, 0), corner(0, 1), corner(1, 1)];

                if self.block_visible(c) {
                    self.shade_block(band, stride, x0..x1, y0..y1, origin_y, c);
                    drawn = Rect::merge(drawn, Some(area));
                } else if prev.is_some_and(|prev| prev.intersects(area)) {
                    for y in y0..y1 {
                        band[y * stride + x0 * 4..y * stride + x1 * 4].fill(0);
                    }
                }
            }
        }
        drawn
    }

    fn block_visible(&self, corners: [f32; 4]) -> bool {
        // Bilinear interpolation stays within the corner values, so this is exact.
        let Some((lo, hi)) = self.visible else {
            return false;
        };
        let min = corners.iter().copied().fold(f32::INFINITY, f32::min);
        let max = corners.iter().copied().fold(0.0, f32::max);
        max >= lo && min <= hi
    }

    fn shade_block(
        &self,
        band: &mut [u8],
        stride: usize,
        xs: std::ops::Range<usize>,
        ys: std::ops::Range<usize>,
        origin_y: usize,
        c: [f32; 4],
    ) {
        let last = self.alpha.len() - 1;
        let block = BLOCK as f32;
        for y in ys.clone() {
            let fy = ((y - ys.start) as f32 + 0.5) / block;
            let left = c[0] + (c[2] - c[0]) * fy;
            let right = c[1] + (c[3] - c[1]) * fy;
            let step = (right - left) / block;
            let mut d = left + step * 0.5;

            let noise_row = (origin_y + y) % NOISE_TILE * NOISE_TILE;
            let noise_row = &self.noise[noise_row..noise_row + NOISE_TILE];
            let row = &mut band[y * stride + xs.start * 4..y * stride + xs.end * 4];
            for (x, px) in xs.clone().zip(row.chunks_exact_mut(4)) {
                let idx = (d as usize).min(last);
                d += step;
                // Most noise values give a sparkle too faint to change the pixel.
                let sparkle = self.sparkle[noise_row[x % NOISE_TILE] as usize] * self.mask[idx];
                let value = if sparkle * SPARKLE_STRENGTH <= VISIBLE_EPS {
                    self.solid[idx]
                } else {
                    self.blend(self.alpha[idx], sparkle)
                };
                px.copy_from_slice(&value.to_le_bytes());
            }
        }
    }

    /// `mix(ripple, vec4(sparkle), sparkle * strength)` from the shader, premultiplied.
    fn blend(&self, alpha: f32, sparkle: f32) -> u32 {
        let k = (sparkle * SPARKLE_STRENGTH).min(1.0);
        let keep = alpha * (1.0 - k);
        let white = sparkle * k;
        let a = (keep + white).min(1.0);
        pack(self.color.map(|c| (c * keep + white).min(a)), a)
    }

    /// Distance from the ripple center after the shader's `distort()`, which works in
    /// absolute pixel coordinates.
    fn distance(&self, x: f32, y: f32) -> f32 {
        let angle = y.atan2(x);
        let t = self.time_ms;
        let px = x
            + (angle * 8.0 + t * 0.003 + 1.641).sin() * self.distort_radial
            + (x * 0.01 + t * 0.00215 + 0.8123).sin() * self.distort_xy;
        let py = y
            + (angle * 5.0 + 2.14 + t * 0.00412).cos() * self.distort_radial
            + (y * 0.01 + t * 0.005931).cos() * self.distort_xy;
        (px - self.cx).hypot(py - self.cy)
    }
}

fn build_noise_tile() -> Vec<u8> {
    let snap = |v: usize| {
        let p = v as f32 + 0.5;
        p - p.rem_euclid(SPARKLE_CELL)
    };
    let mut tile = Vec::with_capacity(NOISE_TILE * NOISE_TILE);
    for y in 0..NOISE_TILE {
        for x in 0..NOISE_TILE {
            tile.push(quantize_noise(triangle_noise(snap(x), snap(y))));
        }
    }
    tile
}

fn triangle_noise(x: f32, y: f32) -> f32 {
    let mut nx = fract(x * 5.3987);
    let mut ny = fract(y * 5.4421);
    let dot = ny * (nx + 21.5351) + nx * (ny + 14.3137);
    nx += dot;
    ny += dot;
    let xy = nx * ny;
    fract(xy * 95.4307) + fract(xy * 75.04961) - 1.0
}

fn sparkles(n: f32, t: f32) -> f32 {
    (0..4)
        .map(|i| {
            let i = i as f32;
            let l = i * 0.01;
            let o = smoothstep(n - l, l + 0.1, n);
            o * (PI * o * (t + 0.55 * i)).sin().abs()
        })
        .sum()
}

fn quantize_noise(n: f32) -> u8 {
    ((n - NOISE_MIN) / (NOISE_MAX - NOISE_MIN) * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn dequantize_noise(q: u8) -> f32 {
    NOISE_MIN + q as f32 / 255.0 * (NOISE_MAX - NOISE_MIN)
}

/// Android's `Interpolators.STANDARD`, cubic-bezier(0.2, 0, 0, 1).
fn standard_ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    let bezier_x = |s: f32| 0.6 * (1.0 - s) * (1.0 - s) * s + s * s * s;
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..24 {
        let mid = (lo + hi) * 0.5;
        if bezier_x(mid) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let s = (lo + hi) * 0.5;
    3.0 * (1.0 - s) * s * s + s * s * s
}

fn fade(progress: f32, in_start: f32, in_end: f32, out_start: f32, out_end: f32) -> f32 {
    let fade_in = sub_progress(in_start, in_end, progress);
    let fade_out = 1.0 - sub_progress(out_start, out_end, progress);
    fade_in.min(fade_out)
}

fn sub_progress(start: f32, end: f32, progress: f32) -> f32 {
    if start == end {
        return if progress > start { 1.0 } else { 0.0 };
    }
    (progress.clamp(start.min(end), start.max(end)) - start) / (end - start)
}

fn soften(d: f32, blur: f32) -> f32 {
    smoothstep(-blur * 0.5, blur * 0.5, d)
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let span = edge1 - edge0;
    if span.abs() < 1e-6 {
        return 0.0;
    }
    let t = ((x - edge0) / span).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn fract(v: f32) -> f32 {
    v - v.floor()
}

fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t
}

fn pack([r, g, b]: [f32; 3], a: f32) -> u32 {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    channel(a) << 24 | channel(r) << 16 | channel(g) << 8 | channel(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_ease_hits_endpoints() {
        assert!(standard_ease(0.0).abs() < 1e-4);
        assert!((standard_ease(1.0) - 1.0).abs() < 1e-4);
        assert!(standard_ease(0.5) > 0.5);
    }

    #[test]
    fn fades_match_upstream_defaults() {
        assert_eq!(fade(0.05, 0.0, 0.1, 0.3, 1.0), 0.5);
        assert_eq!(fade(0.2, 0.0, 0.1, 0.3, 1.0), 1.0);
        assert_eq!(fade(1.0, 0.0, 0.1, 0.3, 1.0), 0.0);
        assert_eq!(fade(0.3, 0.0, 0.0, 0.0, 0.6), 0.5);
    }

    #[test]
    fn noise_quantization_round_trips() {
        for n in [-0.5, -0.1, 0.0, 0.12, 0.2] {
            assert!((dequantize_noise(quantize_noise(n)) - n).abs() < 0.002);
        }
    }

    #[test]
    fn renders_and_clears() {
        let color = Color {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };
        let ripple = ChargeRipple::new(color, (1.0, 1.0));
        let (w, h) = (320u32, 200u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let drawn = ripple.render(&mut pixels, w, h, 0.3, 500.0, None);
        assert!(drawn.is_some());
        assert!(pixels.iter().any(|&b| b != 0));

        let drawn = ripple.render(&mut pixels, w, h, 1.0, 1750.0, drawn);
        assert!(drawn.is_none());
        assert!(pixels.iter().all(|&b| b == 0));
    }
}
