//! The app icon, drawn in code: a rounded square with three overlapping rings (mail, calendar,
//! chat meeting in one place). No external artwork.

use anyhow::Result;

const SIZE: u32 = 1024;

fn coverage_round_rect(x: f64, y: f64, s: f64, r: f64) -> f64 {
    let qx = (x - s / 2.0).abs() - (s / 2.0 - r);
    let qy = (y - s / 2.0).abs() - (s / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
    (0.5 - outside).clamp(0.0, 1.0)
}

fn ring(x: f64, y: f64, cx: f64, cy: f64, radius: f64, width: f64) -> f64 {
    let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
    (width / 2.0 - (d - radius).abs() + 0.5).clamp(0.0, 1.0)
}

/// Writes `apps/quorum/icons/icon.png`.
pub fn icon() -> Result<()> {
    let s = f64::from(SIZE);
    let mut px = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let rings = [(0.40, 0.42, [255u8, 255, 255]), (0.60, 0.42, [210, 232, 255]), (0.50, 0.60, [180, 215, 255])];
    for yi in 0..SIZE {
        for xi in 0..SIZE {
            let (x, y) = (f64::from(xi) + 0.5, f64::from(yi) + 0.5);
            let inset = s * 0.1;
            let bg = coverage_round_rect(x - inset, y - inset, s - 2.0 * inset, s * 0.18);
            let t = y / s;
            let mut c = [20.0 + 20.0 * t, 90.0 + 30.0 * t, 220.0 - 30.0 * t];
            for (cx, cy, col) in rings {
                let a = ring(x, y, cx * s, cy * s, s * 0.17, s * 0.045);
                for (ch, v) in c.iter_mut().zip(col) {
                    *ch = *ch * (1.0 - a) + f64::from(v) * a;
                }
            }
            px.extend(c.iter().map(|v| v.round().clamp(0.0, 255.0) as u8));
            px.push((bg * 255.0).round() as u8);
        }
    }
    let path = super::root().join("apps/quorum/icons/icon.png");
    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut enc = png::Encoder::new(file, SIZE, SIZE);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&px)?;
    println!("icon: wrote {}", path.display());
    Ok(())
}
