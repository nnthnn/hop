/// The switcher popup window: layout, rendering, state machine.

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::protocol::render::{
    ConnectionExt as RenderConnectionExt,
    Color as RenderColor,
    PictOp, PictType, CreatePictureAux,
};
use x11rb::rust_connection::RustConnection;

use crate::config::Config;
use crate::x11 as xh;

const FRAME_W: u32 = 8;

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
        let class_str = b"xwitch\0xwitch\0";
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
            eprintln!("xwitch: no ARGB32 render format found");
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

            // Icon (placeholder — full icon rendering via XRender to be added)
            self.draw_icon_placeholder(win_pic, fmt, entry, tile_x, tile_y, tw, th, fg_argb)?;
        }

        self.conn.render_free_picture(win_pic)?.check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn draw_icon_placeholder(
        &self,
        win_pic: u32,
        fmt: u32,
        entry: &WindowEntry,
        tile_x: i16,
        tile_y: i16,
        tile_w: u32,
        tile_h: u32,
        fg_argb: u32,
    ) -> Result<(), Box<dyn Error>> {
        let icon_size = self.config.window.icon_size;
        let icon_x = tile_x + ((tile_w - icon_size) / 2) as i16;
        let icon_y = tile_y + ((tile_h - icon_size) / 2) as i16;

        // TODO: render actual icon pixmap from entry.icon
        // For now, draw a simple rectangle placeholder
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
