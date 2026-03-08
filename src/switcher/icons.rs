/// Icon loading: XDG hicolor theme search and PNG decoding.

/// Search standard XDG icon theme paths for a PNG icon matching `class`.
/// Tries both the lowercase and original-case class name, and prefers the size
/// closest to `target_size`. Returns `(width, height, ARGB pixels)` on success.
pub(super) fn load_icon_file(class: &str, target_size: u32) -> Option<(u32, u32, Vec<u32>)> {
    if class.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let name_lower = class.to_lowercase();

    let icon_dirs = [
        format!("{}/.local/share/icons/hicolor", home),
        "/usr/share/icons/hicolor".to_string(),
    ];
    let sizes = [512u32, 256, 128, 96, 64, 48, 32];

    let mut candidates: Vec<(u64, String)> = Vec::new();
    for dir in &icon_dirs {
        for &sz in &sizes {
            let diff = (sz as i64 - target_size as i64).unsigned_abs();
            candidates.push((diff, format!("{}/{}x{}/apps/{}.png", dir, sz, sz, name_lower)));
            if name_lower != class {
                candidates.push((diff, format!("{}/{}x{}/apps/{}.png", dir, sz, sz, class)));
            }
        }
    }
    // /usr/share/icons/{name}.png (some apps install here directly, e.g. zed)
    candidates.push((u64::MAX, format!("/usr/share/icons/{}.png", name_lower)));
    if name_lower != class {
        candidates.push((u64::MAX, format!("/usr/share/icons/{}.png", class)));
    }
    // pixmaps as last resort
    candidates.push((u64::MAX, format!("/usr/share/pixmaps/{}.png", name_lower)));
    if name_lower != class {
        candidates.push((u64::MAX, format!("/usr/share/pixmaps/{}.png", class)));
    }

    candidates.sort_by_key(|(d, _)| *d);
    for (_, path) in &candidates {
        if let Some(icon) = load_png_file(path) {
            return Some(icon);
        }
    }
    None
}

/// Decode a PNG file into `(width, height, ARGB u32 pixels)`.
/// Only 8-bit-per-channel RGB and RGBA PNGs are supported; returns `None` otherwise.
pub(super) fn load_png_file(path: &str) -> Option<(u32, u32, Vec<u32>)> {
    use png::{BitDepth, ColorType};

    let file = std::fs::File::open(path).ok()?;
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;

    if info.bit_depth != BitDepth::Eight {
        return None;
    }

    let w = info.width;
    let h = info.height;
    let n = (w as usize) * (h as usize);

    let pixels: Vec<u32> = match info.color_type {
        ColorType::Rgba => {
            if buf.len() < n * 4 {
                return None;
            }
            buf[..n * 4]
                .chunks_exact(4)
                .map(|c| {
                    ((c[3] as u32) << 24)
                        | ((c[0] as u32) << 16)
                        | ((c[1] as u32) << 8)
                        | (c[2] as u32)
                })
                .collect()
        }
        ColorType::Rgb => {
            if buf.len() < n * 3 {
                return None;
            }
            buf[..n * 3]
                .chunks_exact(3)
                .map(|c| {
                    0xFF00_0000
                        | ((c[0] as u32) << 16)
                        | ((c[1] as u32) << 8)
                        | (c[2] as u32)
                })
                .collect()
        }
        _ => return None,
    };

    Some((w, h, pixels))
}
