//! Tray icons: a glossy round badge with a white symbol that says what is going
//! on without relying on color alone: a check (active), a lightning bolt (in
//! use), an exclamation mark (no models), pause bars (unavailable). Drawn in code
//! so there are no assets to ship.

use tokenshare_core::status::Health;

/// Pixel size of the icon we hand to the OS. Windows scales tray icons down to
/// about 16-32 px, so a smaller source stays crisp there.
#[cfg(target_os = "windows")]
pub const SIZE: u32 = 32;
#[cfg(not(target_os = "windows"))]
pub const SIZE: u32 = 64;

/// The emoji that goes with each state, for menu text and tooltips.
pub fn emoji(health: Health) -> &'static str {
    match health {
        Health::Active => "✅",
        Health::InUse => "⚡",
        Health::NoModels => "⚠️",
        Health::Down => "⏸️",
    }
}

type P = (f32, f32);

/// (top-left highlight, bottom-right shade) of the badge.
fn palette(health: Health) -> ([f32; 3], [f32; 3]) {
    match health {
        Health::Active => ([76.0, 217.0, 100.0], [30.0, 140.0, 62.0]),
        Health::InUse => ([90.0, 170.0, 255.0], [20.0, 90.0, 220.0]),
        Health::NoModels => ([255.0, 216.0, 72.0], [232.0, 150.0, 0.0]),
        // A calm slate gray: unavailable is not an alarm.
        Health::Down => ([164.0, 172.0, 186.0], [104.0, 114.0, 130.0]),
    }
}

/// Distance from `p` to segment `a`-`b`.
fn seg_dist(p: P, a: P, b: P) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}

fn polyline_dist(p: P, pts: &[P]) -> f32 {
    pts.windows(2)
        .map(|w| seg_dist(p, w[0], w[1]))
        .fold(f32::MAX, f32::min)
}

/// Signed distance to a closed polygon: negative inside.
fn polygon_sdf(p: P, pts: &[P]) -> f32 {
    let mut d = f32::MAX;
    let mut inside = false;
    for i in 0..pts.len() {
        let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
        d = d.min(seg_dist(p, a, b));
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
    }
    if inside {
        -d
    } else {
        d
    }
}

const CHECK: [P; 3] = [(0.27, 0.52), (0.43, 0.68), (0.74, 0.33)];
/// Pause: two rounded bars.
const PAUSE_L: [P; 2] = [(0.375, 0.34), (0.375, 0.66)];
const PAUSE_R: [P; 2] = [(0.625, 0.34), (0.625, 0.66)];
/// Exclamation mark: a bar and a dot.
const BAR: [P; 2] = [(0.5, 0.25), (0.5, 0.53)];
const DOT: P = (0.5, 0.74);
const BOLT: [P; 6] = [
    (0.60, 0.11),
    (0.27, 0.56),
    (0.46, 0.56),
    (0.39, 0.89),
    (0.73, 0.42),
    (0.54, 0.42),
];

/// Signed distance to the symbol, in unit coordinates (negative inside).
fn glyph_sdf(health: Health, p: P) -> f32 {
    match health {
        Health::Active => polyline_dist(p, &CHECK) - 0.065,
        Health::Down => {
            seg_dist(p, PAUSE_L[0], PAUSE_L[1]).min(seg_dist(p, PAUSE_R[0], PAUSE_R[1])) - 0.062
        }
        Health::InUse => polygon_sdf(p, &BOLT) - 0.012,
        Health::NoModels => {
            let dot = ((p.0 - DOT.0).powi(2) + (p.1 - DOT.1).powi(2)).sqrt() - 0.068;
            (polyline_dist(p, &BAR) - 0.058).min(dot)
        }
    }
}

fn glyph_color(health: Health) -> [f32; 3] {
    match health {
        Health::NoModels => [78.0, 48.0, 0.0],
        _ => [255.0; 3],
    }
}

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Premultiplied RGBA of one sample at unit coordinates `p`.
fn sample(health: Health, p: P, px: f32) -> [f32; 4] {
    let (hi, lo) = palette(health);
    let (cx, cy) = (p.0 - 0.5, p.1 - 0.5);
    let r = (cx * cx + cy * cy).sqrt();
    // Badge: a disc with a gentle diagonal gradient, a darker rim and a soft top gloss.
    let edge = 0.47 - r;
    let cover = (edge / px + 0.5).clamp(0.0, 1.0);
    if cover <= 0.0 {
        return [0.0; 4];
    }
    let t = ((cx + cy) * 0.9 + 0.5).clamp(0.0, 1.0);
    let mut c = lerp(hi, lo, t);
    let rim = (1.0 - (edge / 0.045).clamp(0.0, 1.0)) * 0.28;
    c = lerp(c, [c[0] * 0.55, c[1] * 0.55, c[2] * 0.55], rim);
    let gloss = ((0.5 - p.1) * 2.0).clamp(0.0, 1.0) * (1.0 - (r / 0.45).clamp(0.0, 1.0)) * 0.22;
    c = lerp(c, [255.0; 3], gloss);

    // Symbol: white on green/blue/gray, dark on yellow (as on a warning sign).
    // White glyphs get a soft shadow just below them.
    let ink = glyph_color(health);
    if ink[0] > 200.0 {
        let shadow = ((-glyph_sdf(health, (p.0, p.1 - 0.025)) / 0.03) + 0.5).clamp(0.0, 1.0) * 0.30;
        c = lerp(c, [0.0; 3], shadow);
    }
    let g = (-glyph_sdf(health, p) / px + 0.5).clamp(0.0, 1.0);
    c = lerp(c, ink, g);
    [c[0] * cover, c[1] * cover, c[2] * cover, 255.0 * cover]
}

/// RGBA bytes (straight alpha) for the icon, `size` x `size`, 4x4 supersampled.
pub fn render(health: Health, size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let px = 1.0 / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let p = (
                        (x as f32 + (sx as f32 + 0.5) / SS as f32) * px,
                        (y as f32 + (sy as f32 + 0.5) / SS as f32) * px,
                    );
                    let s = sample(health, p, px);
                    for i in 0..4 {
                        acc[i] += s[i];
                    }
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            if a <= 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                // Un-premultiply: acc holds color scaled by coverage, alpha is 255 * coverage.
                let k = 255.0 / acc[3];
                out.extend_from_slice(&[
                    (acc[0] * k).round().min(255.0) as u8,
                    (acc[1] * k).round().min(255.0) as u8,
                    (acc[2] * k).round().min(255.0) as u8,
                    a.round() as u8,
                ]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Health; 4] = [
        Health::Active,
        Health::InUse,
        Health::NoModels,
        Health::Down,
    ];

    fn px(buf: &[u8], size: u32, u: f32, v: f32) -> [u8; 4] {
        let (x, y) = ((u * size as f32) as u32, (v * size as f32) as u32);
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    fn is_white(p: [u8; 4]) -> bool {
        p[3] == 255 && p[0] > 235 && p[1] > 235 && p[2] > 235
    }

    #[test]
    fn badge_fills_the_middle_and_leaves_the_corners_clear() {
        for h in ALL {
            let buf = render(h, 64);
            assert_eq!(buf.len(), 64 * 64 * 4);
            assert_eq!(px(&buf, 64, 0.01, 0.01)[3], 0, "{h:?} corner");
            assert_eq!(px(&buf, 64, 0.99, 0.5)[3], 0, "{h:?} edge");
            assert_eq!(px(&buf, 64, 0.5, 0.95)[3], 255, "{h:?} inside the rim");
        }
    }

    #[test]
    fn each_state_draws_its_own_symbol_in_white() {
        let check = render(Health::Active, 64);
        assert!(is_white(px(&check, 64, 0.43, 0.68)), "check vertex");
        assert!(
            !is_white(px(&check, 64, 0.70, 0.70)),
            "no cross in the check icon"
        );

        let bolt = render(Health::InUse, 64);
        assert!(is_white(px(&bolt, 64, 0.50, 0.50)), "bolt body");
        assert!(!is_white(px(&bolt, 64, 0.20, 0.20)));

        let warn = render(Health::NoModels, 64);
        let dark = |p: [u8; 4]| p[3] == 255 && p[0] < 110 && p[1] < 80;
        assert!(dark(px(&warn, 64, 0.50, 0.40)), "exclamation bar");
        assert!(dark(px(&warn, 64, 0.50, 0.74)), "exclamation dot");
        assert!(!dark(px(&warn, 64, 0.50, 0.63)), "gap between bar and dot");
        assert!(!is_white(px(&warn, 64, 0.50, 0.40)));

        let pause = render(Health::Down, 64);
        assert!(is_white(px(&pause, 64, 0.375, 0.50)), "left bar");
        assert!(is_white(px(&pause, 64, 0.625, 0.50)), "right bar");
        assert!(
            !is_white(px(&pause, 64, 0.50, 0.50)),
            "gap between the bars"
        );
        assert!(!is_white(px(&pause, 64, 0.375, 0.20)), "bars are short");
    }

    #[test]
    fn badge_colors_are_green_blue_yellow_gray() {
        let [g, b, y, r] = ALL.map(|h| {
            let buf = render(h, 64);
            px(&buf, 64, 0.5, 0.88)
        });
        assert!(y[0] > 200 && y[1] > 120 && y[2] < 60, "yellow {y:?}");
        assert!(g[1] > g[0] && g[1] > g[2], "green {g:?}");
        assert!(b[2] > b[0] && b[2] > b[1], "blue {b:?}");
        // Gray: nearly equal channels, mid brightness (not red, so not alarming).
        let spread = r[0].abs_diff(r[1]).max(r[1].abs_diff(r[2]));
        assert!(spread < 40 && r[0] > 90 && r[0] < 190, "gray {r:?}");
    }

    #[test]
    fn emoji_differ_per_state() {
        assert_eq!(ALL.map(emoji), ["✅", "⚡", "⚠️", "⏸️"]);
    }

    /// `ICON_PREVIEW=/path/sheet.png cargo test -p tokenshare-tray preview` writes
    /// the three icons at 64 and 16 px on a dark and a light background.
    #[test]
    fn preview_sheet() {
        let Ok(path) = std::env::var("ICON_PREVIEW") else {
            return;
        };
        let (cell, w, h) = (96u32, 96 * 4, 96 * 2);
        let mut img = vec![255u8; (w * h * 4) as usize];
        for (row, bg) in [[30u8, 30, 32], [236, 236, 238]].into_iter().enumerate() {
            for (col, health) in ALL.into_iter().enumerate() {
                let (x0, y0) = (col as u32 * cell, row as u32 * cell);
                for y in 0..cell {
                    for x in 0..cell {
                        let i = (((y0 + y) * w + x0 + x) * 4) as usize;
                        img[i..i + 3].copy_from_slice(&bg);
                    }
                }
                for (size, dx, dy) in [(64u32, 16u32, 8u32), (16, 40, 76)] {
                    let icon = render(health, size);
                    for y in 0..size {
                        for x in 0..size {
                            let s = ((y * size + x) * 4) as usize;
                            let a = icon[s + 3] as f32 / 255.0;
                            let d = (((y0 + dy + y) * w + x0 + dx + x) * 4) as usize;
                            for k in 0..3 {
                                img[d + k] =
                                    (icon[s + k] as f32 * a + img[d + k] as f32 * (1.0 - a)) as u8;
                            }
                        }
                    }
                }
            }
        }
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&img).unwrap();
    }
}
