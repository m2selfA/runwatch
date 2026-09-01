/// Light-theme mark used by the window and tray (RGBA8, not premultiplied).
pub fn rgba(size: u32) -> Vec<u8> {
    let mut px = vec![0u8; (size * size * 4) as usize];
    let cx = size as f32 / 2.0;
    let cy = size as f32 / 2.0;
    let r_outer = size as f32 * 0.38;
    let r_inner = size as f32 * 0.30;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            let t = y as f32 / size as f32;
            let mut r = 255.0 * (1.0 - t) + 215.0 * t;
            let mut g = 248.0 * (1.0 - t) + 238.0 * t;
            let mut b = 240.0 * (1.0 - t) + 248.0 * t;
            if (d - (r_outer + r_inner) / 2.0).abs() < (r_outer - r_inner) / 2.0 + 0.6 {
                let angle = dy.atan2(dx);
                if !(1.15..=1.55).contains(&angle) {
                    r = 42.0;
                    g = 168.0;
                    b = 160.0;
                }
            }
            if (x as i32 - (size as i32 * 3 / 4)).abs() < 2 && (y as i32) < (size as i32 / 3) {
                r = 232.0;
                g = 163.0;
                b = 23.0;
            }
            let i = ((y * size + x) * 4) as usize;
            px[i] = r as u8;
            px[i + 1] = g as u8;
            px[i + 2] = b as u8;
            px[i + 3] = 255;
        }
    }
    px
}
