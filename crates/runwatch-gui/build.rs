use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let ico_path = write_icon();
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon(ico_path.to_str().unwrap());
        res.set("FileDescription", "runwatch");
        res.set("ProductName", "runwatch");
        res.set("FileVersion", env!("CARGO_PKG_VERSION"));
        res.set("LegalCopyright", "MIT");
        if let Err(err) = res.compile() {
            println!("cargo:warning=winres failed: {err}");
        }
    }
}

fn write_icon() -> PathBuf {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("icon.ico");
    let bytes = build_ico(32);
    let mut f = fs::File::create(&path).expect("write icon.ico");
    f.write_all(&bytes).expect("ico bytes");
    path
}

fn build_ico(size: u32) -> Vec<u8> {
    let pixels = raster(size);
    let dib = encode_dib(size, &pixels);
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.push(size as u8);
    out.push(size as u8);
    out.push(0);
    out.push(0);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&(dib.len() as u32).to_le_bytes());
    out.extend_from_slice(&22u32.to_le_bytes());
    out.extend_from_slice(&dib);
    out
}

fn encode_dib(size: u32, bgra_top: &[u8]) -> Vec<u8> {
    let mut dib = Vec::new();
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(size as i32).to_le_bytes());
    dib.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..size).rev() {
        let start = (y * size * 4) as usize;
        let end = start + (size as usize) * 4;
        dib.extend_from_slice(&bgra_top[start..end]);
    }
    let mask_stride = ((size + 31) / 32) * 4;
    dib.extend(std::iter::repeat(0u8).take((mask_stride * size) as usize));
    dib
}

fn raster(size: u32) -> Vec<u8> {
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
                if angle < 1.15 || angle > 1.55 {
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
            px[i] = b as u8;
            px[i + 1] = g as u8;
            px[i + 2] = r as u8;
            px[i + 3] = 255;
        }
    }
    px
}
