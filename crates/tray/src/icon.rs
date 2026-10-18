use tokenshare_core::status::Health;

pub const SIZE: u32 = 32;

/// Green = active, blue = in use, red = down.
pub fn color(health: Health) -> [u8; 3] {
    match health {
        Health::Active => [46, 160, 67],
        Health::InUse => [47, 129, 247],
        Health::Down => [218, 54, 51],
    }
}

/// A filled, anti-aliased disc on a transparent background, as RGBA bytes.
pub fn disc_rgba(size: u32, [r, g, b]: [u8; 3]) -> Vec<u8> {
    let c = size as f32 / 2.0;
    let radius = c - 2.0;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
            let alpha = (radius + 0.5 - d).clamp(0.0, 1.0);
            out.extend_from_slice(&[r, g, b, (alpha * 255.0) as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn disc_is_opaque_in_the_middle_and_clear_in_the_corners() {
        let buf = disc_rgba(SIZE, color(Health::InUse));
        assert_eq!(buf.len(), (SIZE * SIZE * 4) as usize);
        assert_eq!(px(&buf, SIZE, SIZE / 2, SIZE / 2), [47, 129, 247, 255]);
        assert_eq!(px(&buf, SIZE, 0, 0)[3], 0);
    }

    #[test]
    fn each_state_has_its_own_color() {
        let c = [Health::Active, Health::InUse, Health::Down].map(color);
        assert!(c[0] != c[1] && c[1] != c[2] && c[0] != c[2]);
    }
}
