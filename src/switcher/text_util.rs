/// Text utilities: font loading, word-wrap, title truncation, shadow color.

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, Font};
use x11rb::rust_connection::RustConnection;

use crate::config::Config;

/// Try to open a core X11 font sized for `size` (config pt size).
/// Falls back to "fixed" which is always available. Returns None only if both fail.
pub(super) fn open_core_font(
    conn: &RustConnection,
    size: u32,
) -> Result<Option<Font>, Box<dyn Error>> {
    // Map config point size → closest standard bitmap font name
    let preferred: &[u8] = match size {
        0..=9   => b"6x13",
        10..=11 => b"7x14",
        12..=13 => b"9x15",
        14..=15 => b"10x20",
        _       => b"10x20",
    };

    let font = conn.generate_id()?;
    if conn.open_font(font, preferred)?.check().is_ok() {
        return Ok(Some(font));
    }

    // Preferred size not available — try "fixed" (always present)
    let font2 = conn.generate_id()?;
    if conn.open_font(font2, b"fixed")?.check().is_ok() {
        return Ok(Some(font2));
    }

    Ok(None)
}

/// Truncate a window title to at most `max_chars` characters, appending "..." if needed.
pub(super) fn truncate_title(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    // Use char_indices to count without iterating the whole string when within limit.
    let mut char_count = 0;
    for (_, _) in s.char_indices() {
        char_count += 1;
        if char_count > max_chars {
            let truncated: String = s.chars().take(max_chars.saturating_sub(3)).collect();
            return format!("{}...", truncated);
        }
    }
    s.to_string()
}

/// Word-wrap `text` so each line is at most `max_px` pixels wide when rendered
/// with `font`.  Falls back to character-level breaking for words that are
/// wider than `max_px` on their own.
pub(super) fn wrap_text_xft(
    display: *mut x11::xlib::Display,
    font: *mut x11::xft::XftFont,
    text: &str,
    max_px: u32,
) -> Vec<String> {
    use x11::xft;
    use x11::xrender::_XGlyphInfo as XGlyphInfo;

    let measure = |s: &str| -> u32 {
        let bytes = s.as_bytes();
        if bytes.is_empty() { return 0; }
        let mut ext: XGlyphInfo = unsafe { std::mem::zeroed() };
        unsafe {
            xft::XftTextExtentsUtf8(display, font, bytes.as_ptr(), bytes.len() as i32, &mut ext);
        }
        ext.xOff.max(0) as u32
    };

    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return vec![];
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for word in &words {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{} {}", current, word)
        };

        if measure(&candidate) <= max_px {
            current = candidate;
        } else {
            // Flush current line before handling the word
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }

            if measure(word) <= max_px {
                // Word fits on a fresh line
                current = word.to_string();
            } else {
                // Word alone is too wide — break at character boundaries
                let mut chunk = String::new();
                for ch in word.chars() {
                    let mut trial = chunk.clone();
                    trial.push(ch);
                    if measure(&trial) <= max_px || chunk.is_empty() {
                        chunk = trial;
                    } else {
                        lines.push(std::mem::take(&mut chunk));
                        chunk.push(ch);
                    }
                }
                if !chunk.is_empty() {
                    current = chunk;
                }
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    lines
}

/// Compute an appropriate shadow color for `fg_argb` using WCAG 2.1 relative luminance.
///
/// Returns a dark shadow (0xCC000000, ~80% opaque black) when the foreground is light,
/// or a light shadow (0xCCFFFFFF, ~80% opaque white) when the foreground is dark.
/// The crossover threshold (~0.179) is the luminance at which contrast against black
/// equals contrast against white.
pub(super) fn wcag_shadow_argb(fg_argb: u32) -> u32 {
    let linearize = |c_u8: u8| -> f64 {
        let c = c_u8 as f64 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let r = linearize(((fg_argb >> 16) & 0xFF) as u8);
    let g = linearize(((fg_argb >>  8) & 0xFF) as u8);
    let b = linearize(( fg_argb        & 0xFF) as u8);
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    if lum > 0.179 {
        0xCC000000 // light fg → dark shadow
    } else {
        0xCCFFFFFF // dark fg → light shadow
    }
}

/// Resolve the shadow color string from config.
/// If `shadow_color_str` is `"auto"`, picks a shadow color based on WCAG luminance of `fg_argb`.
/// Otherwise parses the hex value directly via `Config::color_argb`.
pub(super) fn resolve_shadow_color(shadow_color_str: &str, fg_argb: u32) -> u32 {
    if shadow_color_str == "auto" {
        wcag_shadow_argb(fg_argb)
    } else {
        Config::color_argb(shadow_color_str)
    }
}
