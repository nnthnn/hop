/// The switcher popup window: layout, rendering, state machine.

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::protocol::render::{
    ConnectionExt as RenderConnectionExt,
    Color as RenderColor,
    PictOp, PictType, CreatePictureAux, Transform,
};
use x11rb::rust_connection::RustConnection;

use crate::config::Config;
use crate::x11 as xh;

const FRAME_W: u32 = 8;
const LABEL_H: u32 = 20; // pixels reserved at the bottom of each tile for the title label

pub struct WindowEntry {
    pub id: Window,
    pub name: String,
    /// Raw ARGB pixels (width * height u32 values) from _NET_WM_ICON, if any.
    pub icon: Option<(u32, u32, Vec<u32>)>,
}

pub struct Switcher<'a> {
    conn: &'a RustConnection,
    config: &'a Config,
    pub windows: Vec<WindowEntry>,
    pub selected: usize,
    popup: Option<Window>,
    colormap: Colormap,
    visual_id: Visualid,
    screen_w: u16,
    screen_h: u16,
    xft: Option<xh::XftState>,
}

impl<'a> Switcher<'a> {
    pub fn new(
        conn: &'a RustConnection,
        config: &'a Config,
        display: &crate::x11::Display,
    ) -> Result<Self, Box<dyn Error>> {
        let (visual_id, colormap) = match (display.argb_visual, display.argb_colormap) {
            (Some(v), Some(c)) => (v, c),
            _ => {
                let screen = display.screen();
                (screen.root_visual, screen.default_colormap)
            }
        };

        let xft = xh::XftState::open();
        if xft.is_none() {
            eprintln!("hop: Xft unavailable, falling back to bitmap fonts");
        }

        Ok(Switcher {
            conn,
            config,
            windows: vec![],
            selected: 0,
            popup: None,
            colormap,
            visual_id,
            screen_w: display.screen_width,
            screen_h: display.screen_height,
            xft,
        })
    }

    /// Populate the window list from EWMH. Skip the switcher popup itself.
    pub fn load_windows(&mut self, root: Window) -> Result<(), Box<dyn Error>> {
        let win_ids = xh::get_window_list(self.conn, root)?;
        self.windows.clear();

        for id in win_ids {
            if self.popup == Some(id) {
                continue;
            }
            let name = xh::get_window_name(self.conn, id).unwrap_or_default();
            let icon = xh::get_window_icon(self.conn, id, self.config.window.icon_size)
                .unwrap_or(None);
            self.windows.push(WindowEntry { id, name, icon });
        }
        Ok(())
    }

    /// Show the popup, starting at the second window (index 1, or 0 if only one).
    pub fn show(&mut self, root: Window, backward: bool) -> Result<(), Box<dyn Error>> {
        if self.popup.is_some() {
            return Ok(());
        }
        self.load_windows(root)?;
        if self.windows.is_empty() {
            return Ok(());
        }

        if backward {
            self.selected = self.windows.len() - 1;
        } else {
            self.selected = if self.windows.len() > 1 { 1 } else { 0 };
        }

        self.create_popup()?;
        self.redraw()?;
        Ok(())
    }

    fn tile_w(&self) -> u32 { self.config.window.tile_width }
    fn tile_h(&self) -> u32 { self.config.window.tile_height }

    fn popup_dims(&self) -> (i16, i16, u16, u16) {
        let n = self.windows.len() as u32;
        let w = (self.tile_w() + FRAME_W) * n + FRAME_W;
        let h = self.tile_h() + 2 * FRAME_W;
        let x = ((self.screen_w as u32).saturating_sub(w) / 2) as i16;
        let y = ((self.screen_h as u32).saturating_sub(h) / 2) as i16;
        (x, y, w as u16, h as u16)
    }

    fn create_popup(&mut self) -> Result<(), Box<dyn Error>> {
        let (x, y, w, h) = self.popup_dims();
        let win = self.conn.generate_id()?;
        let border_w = self.config.window.border_width as u32;
        let border_color = Config::color_argb(&self.config.colors.border);

        self.conn.create_window(
            32,                          // depth
            win,
            self.conn.setup().roots[0].root,
            x, y, w, h,
            border_w as u16,
            WindowClass::INPUT_OUTPUT,
            self.visual_id,
            &CreateWindowAux::new()
                .background_pixel(0u32)  // transparent
                .border_pixel(border_color)
                .colormap(self.colormap)
                .override_redirect(1u32)
                .event_mask(EventMask::EXPOSURE | EventMask::KEY_PRESS | EventMask::KEY_RELEASE
                            | EventMask::BUTTON_PRESS),
        )?.check()?;

        xh::set_window_type_dialog(self.conn, win)?;
        xh::set_skip_taskbar(self.conn, win)?;

        // Set WM_CLASS so picom rules can match it
        let class_str = b"hop\0hop\0";
        let wm_class = xh::intern_atom(self.conn, "WM_CLASS")?;
        self.conn.change_property8(PropMode::REPLACE, win, wm_class, AtomEnum::STRING, class_str)?
            .check()?;

        if self.config.blur.enabled {
            xh::set_blur_hint(self.conn, win, self.config.blur.radius)?;
        }

        self.conn.map_window(win)?.check()?;
        self.conn.flush()?;

        self.popup = Some(win);
        Ok(())
    }

    /// Redraw all tiles using XRender for ARGB compositing.
    pub fn redraw(&self) -> Result<(), Box<dyn Error>> {
        let win = match self.popup {
            Some(w) => w,
            None => return Ok(()),
        };

        let bg_argb = self.config.bg_argb();
        let fg_argb = Config::color_argb(&self.config.colors.foreground);
        let frame_argb = Config::color_argb(&self.config.colors.frame);
        let inact_argb = Config::color_argb(&self.config.colors.inactive);

        let tw = self.tile_w();
        let th = self.tile_h();

        // Find ARGB32 render format
        let formats = self.conn.render_query_pict_formats()?.reply()?;
        let argb_fmt = formats.formats.iter()
            .find(|f| f.depth == 32 && f.type_ == PictType::DIRECT
                && f.direct.alpha_mask == 0xFF
                && f.direct.red_mask == 0xFF)
            .map(|f| f.id);

        let Some(fmt) = argb_fmt else {
            eprintln!("hop: no ARGB32 render format found");
            return Ok(());
        };

        // Create Picture for the popup window
        let win_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(win_pic, win, fmt, &CreatePictureAux::new())?.check()?;

        for (i, entry) in self.windows.iter().enumerate() {
            let tile_x = (FRAME_W + i as u32 * (tw + FRAME_W)) as i16;
            let tile_y = FRAME_W as i16;

            // Background fill with configured alpha
            let (ar, ag, ab, aa) = argb_to_render_color(bg_argb);
            self.conn.render_fill_rectangles(
                PictOp::SRC,
                win_pic,
                RenderColor { red: ar, green: ag, blue: ab, alpha: aa },
                &[Rectangle { x: tile_x, y: tile_y, width: tw as u16, height: th as u16 }],
            )?.check()?;

            // Frame (selected = frame color, others = inactive)
            let frame_color = if i == self.selected { frame_argb } else { inact_argb };
            let (fr, fg_c, fb, fa) = argb_to_render_color(frame_color);
            let fw = FRAME_W as u16;
            // Top
            self.conn.render_fill_rectangles(PictOp::OVER, win_pic,
                RenderColor { red: fr, green: fg_c, blue: fb, alpha: fa },
                &[
                    Rectangle { x: tile_x, y: tile_y - fw as i16, width: tw as u16, height: fw },
                    Rectangle { x: tile_x, y: tile_y + th as i16, width: tw as u16, height: fw },
                    Rectangle { x: tile_x - fw as i16, y: tile_y - fw as i16, width: fw, height: th as u16 + 2 * fw },
                    Rectangle { x: tile_x + tw as i16, y: tile_y - fw as i16, width: fw, height: th as u16 + 2 * fw },
                ],
            )?.check()?;

            // Icon
            self.draw_icon(win_pic, fmt, win, entry, tile_x, tile_y, tw, th, fg_argb)?;

            // Title label
            self.draw_label(win, entry, tile_x, tile_y, tw, th, fg_argb)?;
        }

        self.conn.render_free_picture(win_pic)?.check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn draw_icon(
        &self,
        win_pic: u32,
        fmt: u32,
        drawable: Window,
        entry: &WindowEntry,
        tile_x: i16,
        tile_y: i16,
        tile_w: u32,
        tile_h: u32,
        fg_argb: u32,
    ) -> Result<(), Box<dyn Error>> {
        let icon_size = self.config.window.icon_size;
        let icon_x = tile_x + ((tile_w - icon_size) / 2) as i16;
        // Shift icon up by half of LABEL_H so it sits centered in the non-label area
        let icon_y = tile_y + (tile_h.saturating_sub(icon_size + LABEL_H) / 2) as i16;

        let (src_w, src_h, pixels) = match &entry.icon {
            Some(icon) => icon,
            None => {
                // No icon data — draw a dim placeholder rectangle
                let (fr, fg_c, fb, fa) = argb_to_render_color(fg_argb);
                self.conn.render_fill_rectangles(
                    PictOp::OVER,
                    win_pic,
                    RenderColor { red: fr, green: fg_c, blue: fb, alpha: fa },
                    &[Rectangle {
                        x: icon_x, y: icon_y,
                        width: icon_size as u16, height: icon_size as u16,
                    }],
                )?.check()?;
                return Ok(());
            }
        };
        let (src_w, src_h) = (*src_w, *src_h);

        // Upload pixels into a 32-bit pixmap
        let pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(32, pixmap, drawable, src_w as u16, src_h as u16)?.check()?;

        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0).background(0))?.check()?;

        // Vec<u32> ARGB pixels → native-endian bytes (matches XRender ARGB32 layout)
        let bytes: Vec<u8> = pixels.iter().flat_map(|&p| p.to_ne_bytes()).collect();
        self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            pixmap, gc,
            src_w as u16, src_h as u16,
            0i16, 0i16,  // dst_x, dst_y
            0u8,         // left_pad
            32u8,        // depth
            &bytes,
        )?.check()?;
        self.conn.free_gc(gc)?.check()?;

        // Create an XRender Picture for the pixmap
        let icon_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(icon_pic, pixmap, fmt, &CreatePictureAux::new())?.check()?;
        self.conn.free_pixmap(pixmap)?.check()?;

        // Scale to icon_size if the source dimensions differ
        if src_w != icon_size || src_h != icon_size {
            let sx = (src_w as i64 * 65536 / icon_size as i64) as i32;
            let sy = (src_h as i64 * 65536 / icon_size as i64) as i32;
            self.conn.render_set_picture_transform(icon_pic, Transform {
                matrix11: sx, matrix12: 0, matrix13: 0,
                matrix21: 0, matrix22: sy, matrix23: 0,
                matrix31: 0, matrix32: 0, matrix33: 65536,
            })?.check()?;
            self.conn.render_set_picture_filter(icon_pic, b"bilinear", &[])?.check()?;
        }

        // Composite icon OVER the tile
        self.conn.render_composite(
            PictOp::OVER,
            icon_pic,
            0u32,        // mask = None
            win_pic,
            0, 0,        // src_x, src_y
            0, 0,        // mask_x, mask_y
            icon_x, icon_y,
            icon_size as u16, icon_size as u16,
        )?.check()?;

        self.conn.render_free_picture(icon_pic)?.check()?;
        Ok(())
    }

    /// Render the window title at the bottom of a tile.
    /// Uses Xft (antialiased, Fontconfig names) when available, bitmap fonts as fallback.
    fn draw_label(
        &self,
        win: Window,
        entry: &WindowEntry,
        tile_x: i16,
        tile_y: i16,
        tile_w: u32,
        tile_h: u32,
        fg_argb: u32,
    ) -> Result<(), Box<dyn Error>> {
        let title = truncate_title(&entry.name, 24);
        if title.is_empty() {
            return Ok(());
        }

        if let Some(ref xft) = self.xft {
            self.draw_label_xft(xft, win, &title, tile_x, tile_y, tile_w, tile_h, fg_argb)
        } else {
            self.draw_label_bitmap(win, &title, tile_x, tile_y, tile_w, tile_h, fg_argb)
        }
    }

    fn draw_label_xft(
        &self,
        xst: &xh::XftState,
        win: Window,
        title: &str,
        tile_x: i16,
        tile_y: i16,
        tile_w: u32,
        tile_h: u32,
        fg_argb: u32,
    ) -> Result<(), Box<dyn Error>> {
        use std::ffi::CString;
        use x11::xft;
        use x11::xrender::XRenderColor;
        use x11::xrender::_XGlyphInfo as XGlyphInfo;

        let font_pattern = CString::new(
            format!("{}:size={}", self.config.font.name, self.config.font.size)
        )?;

        let font = unsafe {
            xft::XftFontOpenName(xst.display, xst.screen_num, font_pattern.as_ptr())
        };
        // If named font not found, fall back to "sans"
        let font = if font.is_null() {
            let fallback = CString::new(format!("sans:size={}", self.config.font.size))?;
            unsafe { xft::XftFontOpenName(xst.display, xst.screen_num, fallback.as_ptr()) }
        } else {
            font
        };
        if font.is_null() {
            return Ok(());
        }

        let text = title.as_bytes();

        // Measure text for centering
        let mut extents: XGlyphInfo = unsafe { std::mem::zeroed() };
        unsafe {
            xft::XftTextExtentsUtf8(
                xst.display, font,
                text.as_ptr(), text.len() as i32,
                &mut extents,
            );
        }
        let text_w = extents.xOff.max(0) as u32;
        let label_x = tile_x + tile_w.saturating_sub(text_w).wrapping_div(2) as i16;
        let label_y = tile_y + tile_h as i16 - 4;

        // Allocate Xft color from fg_argb
        let a = ((fg_argb >> 24) & 0xFF) as u16;
        let r = ((fg_argb >> 16) & 0xFF) as u16;
        let g = ((fg_argb >>  8) & 0xFF) as u16;
        let b = ( fg_argb        & 0xFF) as u16;
        let render_color = XRenderColor {
            red:   r * 0x101,
            green: g * 0x101,
            blue:  b * 0x101,
            alpha: a * 0x101,
        };
        let mut xft_color: xft::XftColor = unsafe { std::mem::zeroed() };
        unsafe {
            xft::XftColorAllocValue(
                xst.display, xst.visual, xst.colormap,
                &render_color, &mut xft_color,
            );
        }

        let draw = unsafe {
            xft::XftDrawCreate(xst.display, win as u64, xst.visual, xst.colormap)
        };

        unsafe {
            xft::XftDrawStringUtf8(
                draw, &xft_color, font,
                label_x as i32, label_y as i32,
                text.as_ptr(), text.len() as i32,
            );
            xft::XftDrawDestroy(draw);
            xft::XftColorFree(xst.display, xst.visual, xst.colormap, &mut xft_color);
            xft::XftFontClose(xst.display, font);
        }

        Ok(())
    }

    /// Bitmap font fallback for draw_label when Xft is not available.
    fn draw_label_bitmap(
        &self,
        win: Window,
        title: &str,
        tile_x: i16,
        tile_y: i16,
        tile_w: u32,
        tile_h: u32,
        fg_argb: u32,
    ) -> Result<(), Box<dyn Error>> {
        let title_bytes = title.as_bytes();

        let font = open_core_font(self.conn, self.config.font.size)?;
        let Some(font) = font else { return Ok(()); };

        let chars: Vec<Char2b> = title_bytes.iter()
            .map(|&b| Char2b { byte1: 0, byte2: b })
            .collect();
        let ext = self.conn.query_text_extents(font, &chars)?.reply()?;
        let text_w = ext.overall_width.max(0) as u32;

        let label_x = tile_x + tile_w.saturating_sub(text_w).wrapping_div(2) as i16;
        let label_y = tile_y + tile_h as i16 - 4;

        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, win, &CreateGCAux::new()
            .foreground(fg_argb)
            .background(0)
            .font(font)
        )?.check()?;

        let mut items = vec![title_bytes.len() as u8, 0u8];
        items.extend_from_slice(title_bytes);
        self.conn.poly_text8(win, gc, label_x, label_y, &items)?.check()?;

        self.conn.free_gc(gc)?.check()?;
        self.conn.close_font(font)?.check()?;
        Ok(())
    }

    pub fn next(&mut self) -> Result<(), Box<dyn Error>> {
        if self.windows.is_empty() { return Ok(()); }
        self.selected = (self.selected + 1) % self.windows.len();
        self.redraw()
    }

    pub fn prev(&mut self) -> Result<(), Box<dyn Error>> {
        if self.windows.is_empty() { return Ok(()); }
        self.selected = self.selected.checked_sub(1).unwrap_or(self.windows.len() - 1);
        self.redraw()
    }

    /// Hide popup and switch to the selected window.
    pub fn commit(&mut self, root: Window) -> Result<(), Box<dyn Error>> {
        let target = self.windows.get(self.selected).map(|e| e.id);
        self.hide()?;
        if let Some(win) = target {
            xh::activate_window(self.conn, root, win)?;
        }
        Ok(())
    }

    /// Hide popup and return to the original window (cancel).
    pub fn cancel(&mut self) -> Result<(), Box<dyn Error>> {
        self.hide()
    }

    fn hide(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(win) = self.popup.take() {
            self.conn.destroy_window(win)?.check()?;
            self.conn.flush()?;
        }
        self.windows.clear();
        Ok(())
    }

    pub fn popup_window(&self) -> Option<Window> {
        self.popup
    }

    pub fn is_visible(&self) -> bool {
        self.popup.is_some()
    }
}

/// Try to open a core X11 font sized for `size` (config pt size).
/// Falls back to "fixed" which is always available. Returns None only if both fail.
fn open_core_font(
    conn: &x11rb::rust_connection::RustConnection,
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
fn truncate_title(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}

/// Convert a packed 0xAARRGGBB u32 into XRender color components (0–0xFFFF each).
fn argb_to_render_color(argb: u32) -> (u16, u16, u16, u16) {
    let a = ((argb >> 24) & 0xFF) as u16;
    let r = ((argb >> 16) & 0xFF) as u16;
    let g = ((argb >>  8) & 0xFF) as u16;
    let b = ( argb        & 0xFF) as u16;
    // Scale 0–255 → 0–65535 with premultiplied alpha for XRender
    let scale = |c: u16| c * 0x101;
    let alpha = scale(a);
    // XRender expects premultiplied color components
    let premul = |c: u16| (c as u32 * a as u32 / 255) as u16 * 0x101;
    (premul(r), premul(g), premul(b), alpha)
}
