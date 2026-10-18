//! Draws the Token Share app icon and writes the files each platform needs.
//!
//!   cargo run --release -p tokenshare-tray --example app_icon -- <out_dir>
//!
//! Writes `<out_dir>/AppIcon.iconset/*.png` (for `iconutil`), `icon.ico` and
//! `icon-1024.png`. See `packaging/icon/generate.sh`.
//!
//! The artwork: a blue squircle holding a small mesh of three linked nodes
//! with a gold token in the middle; three more tokens travel along the links.

use std::path::Path;

type P = (f32, f32);
type Rgb = [f32; 3];

const SS: u32 = 3;

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn seg_dist(p: P, a: P, b: P) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}

fn dist(p: P, c: P) -> f32 {
    ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt()
}

/// Approximate signed distance to a superellipse (the macOS icon shape).
fn squircle(p: P, half: f32) -> f32 {
    let (x, y) = ((p.0 - 0.5).abs(), (p.1 - 0.5).abs());
    ((x.powi(5) + y.powi(5)).powf(0.2) - half) * 0.93
}

/// Alpha-composite `rgb` at `a` over a premultiplied accumulator.
fn over(acc: &mut [f32; 4], rgb: Rgb, a: f32) {
    let a = a.clamp(0.0, 1.0);
    for i in 0..3 {
        acc[i] = rgb[i] * a + acc[i] * (1.0 - a);
    }
    acc[3] = a + acc[3] * (1.0 - a);
}

fn cover(sdf: f32, px: f32) -> f32 {
    (0.5 - sdf / px).clamp(0.0, 1.0)
}

// Glyph geometry, in "glyph space" (unit square, scaled about the center when drawn).
const TOP: P = (0.50, 0.24);
const LEFT: P = (0.27, 0.64);
const RIGHT: P = (0.73, 0.64);
const CENTER: P = (0.50, 0.507);
const SCALE: f32 = 1.15;
/// The mesh is taller above its center than below; shift it down to center the whole shape.
const DROP: f32 = 0.06;
const COIN_R: f32 = 0.108;
const NODE_R: f32 = 0.060;

fn to_glyph(p: P) -> P {
    (0.5 + (p.0 - 0.5) / SCALE, 0.5 + (p.1 - 0.5) / SCALE - DROP)
}

fn mid(a: P, b: P) -> P {
    ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
}

/// Distance (image units) to the white structure: links, spokes and nodes.
fn structure_sdf(g: P) -> f32 {
    let mut d = f32::MAX;
    for (a, b) in [(TOP, LEFT), (LEFT, RIGHT), (RIGHT, TOP)] {
        d = d.min(seg_dist(g, a, b) - 0.011);
    }
    for n in [TOP, LEFT, RIGHT] {
        d = d.min(seg_dist(g, CENTER, n) - 0.006);
        d = d.min(dist(g, n) - NODE_R);
    }
    d * SCALE
}

fn shade(p: P, px: f32) -> [f32; 4] {
    let mut acc = [0.0f32; 4];
    let shape = squircle(p, 0.402);

    // Drop shadow under the whole icon.
    let sh = squircle((p.0, p.1 - 0.016), 0.402);
    over(
        &mut acc,
        [0.0; 3],
        ((-sh / 0.03) + 0.5).clamp(0.0, 1.0) * 0.38,
    );

    let body = cover(shape, px);
    if body > 0.0 {
        // Background: indigo to electric blue, with a glow behind the glyph.
        let t = ((p.0 * 0.35 + p.1 * 0.9) - 0.15) / 0.95;
        let mut bg = lerp([58.0, 66.0, 214.0], [22.0, 156.0, 255.0], t);
        let glow = (1.0 - dist(p, (0.5, 0.47)) / 0.42).clamp(0.0, 1.0);
        bg = lerp(bg, [190.0, 225.0, 255.0], glow * glow * 0.30);
        let sheen = ((0.42 - p.1) / 0.42).clamp(0.0, 1.0);
        bg = lerp(bg, [255.0; 3], sheen * 0.14);
        let rim = (1.0 - (-shape / 0.007).clamp(0.0, 1.0)) * (1.0 - p.1).clamp(0.0, 1.0);
        bg = lerp(bg, [255.0; 3], rim * 0.30);
        over(&mut acc, bg, body);

        let g = to_glyph(p);
        // Soft shadow under the glyph.
        let gs = structure_sdf(to_glyph((p.0, p.1 - 0.014)))
            .min((dist(to_glyph((p.0, p.1 - 0.014)), CENTER) - COIN_R) * SCALE);
        over(
            &mut acc,
            [10.0, 20.0, 110.0],
            ((-gs / 0.025) + 0.5).clamp(0.0, 1.0) * 0.30 * body,
        );

        // Links, spokes, nodes in white; node centers are blue "hubs".
        over(
            &mut acc,
            [255.0; 3],
            cover(structure_sdf(g), px) * 0.96 * body,
        );
        for n in [TOP, LEFT, RIGHT] {
            let hub = cover((dist(g, n) - 0.030) * SCALE, px);
            over(
                &mut acc,
                lerp([40.0, 110.0, 255.0], [30.0, 80.0, 220.0], g.1),
                hub * body,
            );
        }

        // Tokens traveling along the links.
        for m in [mid(TOP, LEFT), mid(TOP, RIGHT), mid(LEFT, RIGHT)] {
            let d = (dist(g, m) - 0.021) * SCALE;
            let gold = lerp(
                [255.0, 224.0, 110.0],
                [240.0, 160.0, 30.0],
                (g.1 - m.1) * 20.0 + 0.5,
            );
            over(&mut acc, gold, cover(d, px) * body);
            over(
                &mut acc,
                [255.0, 250.0, 220.0],
                cover(d + 0.006 * SCALE, px) * 0.0,
            );
        }

        // The central token: a gold coin with a rim and an engraved T.
        let dc = (dist(g, CENTER) - COIN_R) * SCALE;
        let t = ((g.0 - CENTER.0) * 0.6 + (g.1 - CENTER.1)) / (COIN_R * 2.0) + 0.5;
        let mut gold = lerp([255.0, 226.0, 120.0], [232.0, 146.0, 18.0], t);
        let ring = ((dist(g, CENTER) - (COIN_R - 0.017)).abs() * SCALE - 0.0035).min(1.0);
        gold = lerp(gold, [196.0, 112.0, 6.0], cover(ring, px) * 0.85);
        over(&mut acc, gold, cover(dc, px) * body);
        // Engraved T: dark with a light edge underneath for an embossed look.
        let tee = |q: P| {
            let (x, y) = (q.0 - CENTER.0, q.1 - CENTER.1);
            let bar = ((x.abs() - 0.050).max((y + 0.030).abs() - 0.013)).max(-1.0);
            let stem = ((x.abs() - 0.0135).max((y - 0.012).abs() - 0.048)).max(-1.0);
            bar.min(stem) * SCALE
        };
        over(
            &mut acc,
            [255.0, 240.0, 190.0],
            cover(tee((g.0, g.1 - 0.004)), px) * 0.5 * body,
        );
        over(&mut acc, [168.0, 96.0, 4.0], cover(tee(g), px) * body);
    }
    acc
}

fn render(size: u32) -> Vec<u8> {
    let px = 0.8 / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let p = (
                        (x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32,
                        (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32,
                    );
                    let s = shade(p, px);
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
                // `over` stored color premultiplied by alpha (0..1) in 0..255 units.
                let k = 1.0 / (acc[3] / n);
                for c in &acc[..3] {
                    out.push(((c / n) * k).round().clamp(0.0, 255.0) as u8);
                }
                out.push((a * 255.0).round() as u8);
            }
        }
    }
    out
}

fn png_bytes(rgba: &[u8], size: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut enc = png::Encoder::new(&mut buf, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
    buf
}

/// A .ico holding PNG-compressed images (supported since Windows Vista).
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend((images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in images {
        out.push(if *size >= 256 { 0 } else { *size as u8 });
        out.push(if *size >= 256 { 0 } else { *size as u8 });
        out.extend([0, 0, 1, 0, 32, 0]);
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend(data);
    }
    out
}

fn main() {
    let out = std::env::args().nth(1).expect("usage: app_icon <out_dir>");
    let out = Path::new(&out);
    let iconset = out.join("AppIcon.iconset");
    std::fs::create_dir_all(&iconset).unwrap();

    let mut cache: std::collections::BTreeMap<u32, Vec<u8>> = Default::default();
    let mut png_for = |size: u32| -> Vec<u8> {
        cache
            .entry(size)
            .or_insert_with(|| png_bytes(&render(size), size))
            .clone()
    };

    for (name, size) in [
        ("icon_16x16", 16),
        ("icon_16x16@2x", 32),
        ("icon_32x32", 32),
        ("icon_32x32@2x", 64),
        ("icon_128x128", 128),
        ("icon_128x128@2x", 256),
        ("icon_256x256", 256),
        ("icon_256x256@2x", 512),
        ("icon_512x512", 512),
        ("icon_512x512@2x", 1024),
    ] {
        std::fs::write(iconset.join(format!("{name}.png")), png_for(size)).unwrap();
    }
    std::fs::write(out.join("icon-1024.png"), png_for(1024)).unwrap();
    let images: Vec<(u32, Vec<u8>)> = [16, 24, 32, 48, 64, 128, 256]
        .map(|s| (s, png_for(s)))
        .to_vec();
    std::fs::write(out.join("icon.ico"), ico(&images)).unwrap();
    println!("wrote {}", out.display());
}
