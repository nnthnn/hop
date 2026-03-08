/// The switcher popup window: layout, rendering, state machine.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::protocol::render::{
    ConnectionExt as RenderConnectionExt,
    Color as RenderColor,
    PictOp, PictType, CreatePictureAux, Transform, Pointfix, Repeat,
};
use x11rb::protocol::composite::ConnectionExt as CompositeConnectionExt;
use x11rb::rust_connection::RustConnection;

use crate::config::Config;
use crate::x11 as xh;

// FRAME_W is now read from config.window.border_width (see frame_w()).
const LABEL_H: u32 = 48;    // pixels reserved at the bottom of each tile for the title label
const TITLE_MAX_CHARS: usize = 100; // hard cap so absurdly long titles don't bust layout
const SCREEN_MARGIN: u32 = 40;     // minimum gap (px) between popup edge and screen edge

pub struct WindowEntry {
    pub id: Window,
    pub name: String,
    /// Raw ARGB pixels (width * height u32 values) from _NET_WM_ICON, if any.
    pub icon: Option<(u32, u32, Vec<u32>)>,
    /// The WM frame window: the direct child of root that encloses this client.
    /// Compositors redirect direct children of root, so NameWindowPixmap must be
    /// called on this frame rather than on the client window itself.
    pub frame: Window,
}

pub struct Switcher<'a> {
    conn: &'a RustConnection,
    config: Config,
    pub windows: Vec<WindowEntry>,
    pub selected: usize,
    popup: Option<Window>,
    colormap: Colormap,
    visual_id: Visualid,
    screen_w: u16,
    screen_h: u16,
    screen_num: usize,
    xft: Option<xh::XftState>,
    /// Persistent off-screen buffer kept alive for the whole popup session.
    /// Reusing the same pixmap avoids allocation overhead and prevents picom
    /// from seeing rapid pixmap churn (which causes visible flicker).
    pix_buf: Option<(u32, u32, u16, u16)>,  // (pixmap xid, pix_pic xid, pw, ph)
    /// Cached A8 mask picture for the window rounded-corner clip (built once per show()).
    /// Reused by every blit so we never rebuild it mid-navigation.
    win_mask_pic: Option<u32>,
    cached_argb_fmt: u32,
    cached_a8_fmt: u32,
    /// window_bg_argb at last full redraw; used to restore border areas cheaply.
    cached_win_bg: u32,
    /// Verbose event tracing; enabled by HOP_DEBUG env var.
    debug: bool,
    /// Thumbnail pixel cache keyed by WM frame Window ID.
    /// Stores ARGB u32 pixels (width × height) captured via NameWindowPixmap + GetImage.
    /// Used as fallback when a window is on another desktop and its compositor backing
    /// pixmap is unavailable (NameWindowPixmap returns BadMatch for unmapped windows).
    thumb_cache: HashMap<Window, (u32, u32, Vec<u32>)>,
    /// Frames still waiting for a thumbnail to be fetched (progressive loading).
    /// Populated by show(); drained one entry per event-loop iteration by pump_one_thumb().
    thumb_queue: VecDeque<Window>,
}

impl<'a> Switcher<'a> {
    pub fn new(
        conn: &'a RustConnection,
        config: Config,
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
            screen_num: display.screen_num,
            xft,
            pix_buf: None,
            win_mask_pic: None,
            cached_argb_fmt: 0,
            cached_a8_fmt: 0,
            cached_win_bg: 0,
            debug: std::env::var("HOP_DEBUG").is_ok(),
            thumb_cache: HashMap::new(),
            thumb_queue: VecDeque::new(),
        })
    }

    /// Populate the window list from EWMH. Skip the switcher popup itself.
    pub fn load_windows(&mut self, root: Window) -> Result<(), Box<dyn Error>> {
        let win_ids = xh::get_window_list(self.conn, root)?;
        self.windows.clear();

        // Extract fields so the loop body can push to self.windows without a borrow conflict.
        let conn = self.conn;
        let icon_size = self.config.tile.icon_size;

        for id in win_ids {
            if self.popup == Some(id) {
                continue;
            }
            // Skip panels, docks, desktop windows, etc.
            if xh::should_skip_window(conn, id) {
                continue;
            }
            let name = xh::get_window_name(conn, id).unwrap_or_default();
            let icon = xh::get_window_icon(conn, id, icon_size).unwrap_or(None);
            // If _NET_WM_ICON is absent, fall back to the XDG icon theme via WM_CLASS.
            let icon = if icon.is_none() {
                xh::get_wm_class(conn, id)
                    .and_then(|cls| load_icon_file(&cls, icon_size))
            } else {
                icon
            };
            let frame = find_frame_win(conn, id, root);
            self.windows.push(WindowEntry { id, name, icon, frame });
        }
        Ok(())
    }

    /// Show the popup, starting at the second window (index 1, or 0 if only one).
    pub fn show(&mut self, root: Window, backward: bool) -> Result<(), Box<dyn Error>> {
        if self.popup.is_some() {
            return Ok(());
        }
        // Reload config on every popup open so edits take effect without restarting.
        if let Ok(fresh) = Config::load() {
            self.config = fresh;
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

        // Register an AUTOMATIC composite redirect so NameWindowPixmap succeeds.
        // picom uses MANUAL; per the X Composite spec both modes can coexist and
        // share the same off-screen buffer — this won't disturb picom's rendering.
        if self.config.tile.content == "thumbnail" {
            let _ = self.conn.composite_redirect_subwindows(
                root,
                x11rb::protocol::composite::Redirect::AUTOMATIC,
            )?.check();
            self.conn.flush()?;
        }

        self.create_popup()?;
        if self.debug { eprintln!("[hop] show(): {} windows, selected={}", self.windows.len(), self.selected); }
        // Draw immediately with whatever is already cached (icons for uncached windows).
        // Thumbnails are loaded progressively by pump_one_thumb() in the event loop.
        self.redraw()?;

        if self.config.tile.content == "thumbnail" {
            // Queue frames for progressive thumbnail loading. Already-cached entries
            // (from MapNotify) will be refreshed; uncached ones loaded for the first time.
            self.thumb_queue = self.windows.iter().map(|e| e.frame).collect();
        }

        Ok(())
    }

    fn tile_w(&self) -> u32 { self.config.tile.width }
    fn tile_h(&self) -> u32 { self.config.tile.height }
    fn frame_w(&self) -> u32 { self.config.tile.border_width.max(1) }
    fn gap_w(&self) -> u32 { self.config.window.gap }
    fn tile_pad(&self) -> u32 { self.config.tile.padding }
    fn win_pad(&self) -> u32 { self.config.window.padding }
    fn border_radius(&self) -> u32 { self.config.tile.border_radius }

    /// Compute how many columns (and resulting rows) fit without the popup
    /// getting within SCREEN_MARGIN pixels of the screen edges.
    fn grid_layout(&self) -> (usize, usize) {
        let n = self.windows.len();
        if n == 0 { return (1, 0); }
        let tw  = self.tile_w();
        let fw  = self.frame_w();
        let gap = self.gap_w();
        let wp  = self.win_pad();
        // Each tile slot is (tw + fw + gap) wide; the window also needs fw + 2*wp overhead.
        // Solving: n_cols*(tw+fw+gap) ≤ available - fw - 2*wp + gap
        let available = (self.screen_w as u32).saturating_sub(2 * SCREEN_MARGIN);
        let slot   = (tw + fw + gap).max(1);
        let budget = available.saturating_sub(fw + 2 * wp).saturating_add(gap);
        let n_cols = ((budget / slot) as usize).max(1).min(n);
        let n_rows = n.div_ceil(n_cols);
        (n_cols, n_rows)
    }

    /// Top-left content corner of tile `i` in popup-relative coordinates.
    fn tile_pos(&self, i: usize, n_cols: usize) -> (i16, i16) {
        let nc  = n_cols.max(1);
        let col = (i % nc) as u32;
        let row = (i / nc) as u32;
        let fw  = self.frame_w();
        let gap = self.gap_w();
        let wp  = self.win_pad();
        let tw  = self.tile_w();
        let th  = self.tile_h();
        let slot = tw + fw + gap; // horizontal stride per tile column

        // If the last row is not full, offset tiles according to last_row_position.
        let x_extra: u32 = {
            let n = self.windows.len();
            let n_rows = n.div_ceil(nc);
            let last_row = n_rows.saturating_sub(1);
            if row as usize == last_row {
                let last_count = n - last_row * nc;
                let missing = (nc - last_count) as u32;
                match self.config.window.last_row_position.as_str() {
                    "center" if missing > 0 => missing * slot / 2,
                    "right"  if missing > 0 => missing * slot,
                    _ => 0, // "left" or full row
                }
            } else {
                0
            }
        };

        let x = (wp + fw + col * slot + x_extra) as i16;
        let y = (wp + fw + row * (th + fw + gap)) as i16;
        (x, y)
    }

    fn popup_dims(&self) -> (i16, i16, u16, u16) {
        let (n_cols, n_rows) = self.grid_layout();
        let nc  = n_cols as u32;
        let nr  = n_rows as u32;
        let fw  = self.frame_w();
        let gap = self.gap_w();
        let wp  = self.win_pad();
        // Layout per axis: [wp][fw][tile][fw][gap]...[fw][tile][fw][wp]
        let w = (self.tile_w() + fw) * nc + fw + nc.saturating_sub(1) * gap + 2 * wp;
        let h = (self.tile_h() + fw) * nr + fw + nr.saturating_sub(1) * gap + 2 * wp;
        let x = ((self.screen_w as u32).saturating_sub(w) / 2) as i16;
        let y = ((self.screen_h as u32).saturating_sub(h) / 2) as i16;
        (x, y, w as u16, h as u16)
    }

    fn create_popup(&mut self) -> Result<(), Box<dyn Error>> {
        let (x, y, w, h) = self.popup_dims();
        let win = self.conn.generate_id()?;
        let outer_bw = self.config.window.outer_border_width;

        // border_pixel must always be set for 32-bit depth windows — X11 forbids
        // inheriting a border pixmap across depth mismatches (Match error otherwise).
        // When outer_bw == 0 the value is irrelevant; use the configured color anyway.
        let border_color = Config::color_argb(&self.config.window.border);
        let aux = CreateWindowAux::new()
            .background_pixel(0u32)  // transparent
            .border_pixel(border_color)
            .colormap(self.colormap)
            .override_redirect(1u32)
            .event_mask(EventMask::EXPOSURE | EventMask::KEY_PRESS | EventMask::KEY_RELEASE
                        | EventMask::BUTTON_PRESS | EventMask::POINTER_MOTION);

        self.conn.create_window(
            32,                          // depth
            win,
            self.conn.setup().roots[0].root,
            x, y, w, h,
            outer_bw as u16,
            WindowClass::INPUT_OUTPUT,
            self.visual_id,
            &aux,
        )?.check()?;

        xh::set_window_type_dialog(self.conn, win)?;
        xh::set_skip_taskbar(self.conn, win)?;

        // Set WM_CLASS so picom rules can match it
        let class_str = b"hop\0hop\0";
        let wm_class = xh::intern_atom(self.conn, "WM_CLASS")?;
        self.conn.change_property8(PropMode::REPLACE, win, wm_class, AtomEnum::STRING, class_str)?
            .check()?;

        if self.config.window.blur || self.config.tile.blur {
            // When only tile blur is requested, pass individual tile rectangles so the
            // compositor blurs only behind each tile — not the gaps between them.
            // When window blur is set (or both), pass an empty slice (= whole window).
            let rects: Vec<(i16, i16, u16, u16)> = if !self.config.window.blur && self.config.tile.blur {
                let (n_cols, _) = self.grid_layout();
                let tw = self.tile_w();
                let th = self.tile_h();
                self.windows.iter().enumerate()
                    .map(|(i, _)| {
                        let (tx, ty) = self.tile_pos(i, n_cols);
                        (tx, ty, tw as u16, th as u16)
                    })
                    .collect()
            } else {
                vec![]  // empty = whole window
            };
            xh::set_blur_hint(self.conn, win, &rects)?;
        }

        self.conn.map_window(win)?.check()?;
        self.conn.flush()?;

        // Grab the entire keyboard so we receive ALL key events while visible —
        // especially modifier releases (Alt up) which are not covered by grab_key.
        self.conn.grab_keyboard(false, win, 0u32, GrabMode::ASYNC, GrabMode::ASYNC)?
            .reply()?;

        self.popup = Some(win);
        Ok(())
    }

    /// Full redraw: renders all tiles, icons, and labels into the persistent
    /// off-screen pixmap, then blits it to the window.  The pixmap is kept
    /// alive across calls; it is only freed in `hide()`.
    pub fn redraw(&mut self) -> Result<(), Box<dyn Error>> {
        let win = match self.popup {
            Some(w) => w,
            None => return Ok(()),
        };
        let t0 = if self.debug { Some(std::time::Instant::now()) } else { None };
        if self.debug { eprintln!("[hop] redraw() start (pix_buf={})", self.pix_buf.is_some()); }

        let window_bg_argb = self.config.window_bg_argb();
        let bg_argb = self.config.bg_argb();
        let fg_argb = Config::color_argb(&self.config.tile.foreground);
        let frame_argb = Config::color_argb(&self.config.tile.frame);
        let inact_argb = Config::color_argb(&self.config.tile.inactive);

        let tw = self.tile_w();
        let th = self.tile_h();

        // Query render formats (needed on first draw; cheap thereafter).
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

        self.cached_argb_fmt = fmt;
        self.cached_win_bg   = window_bg_argb;

        let (_, _, pw, ph) = self.popup_dims();
        let win_br = self.config.window.border_radius;
        let br = self.border_radius();

        let a8_fmt = if br > 0 || win_br > 0 {
            match find_a8_format(&formats) {
                Some(f) => f,
                None => {
                    eprintln!("hop: no A8 render format found; border_radius ignored");
                    0
                }
            }
        } else {
            0
        };
        self.cached_a8_fmt = a8_fmt;

        // Reuse the persistent pixmap when dimensions match; recreate otherwise.
        let pix_buf_fresh = match self.pix_buf {
            Some((_, _, cpw, cph)) if cpw == pw && cph == ph => false,
            _ => true,
        };
        let (pixmap, pix_pic) = match self.pix_buf {
            Some((pix, pic, cpw, cph)) if cpw == pw && cph == ph => (pix, pic),
            _ => {
                if let Some((old_pix, old_pic, _, _)) = self.pix_buf.take() {
                    self.conn.render_free_picture(old_pic)?.check()?;
                    self.conn.free_pixmap(old_pix)?.check()?;
                }
                let pix = self.conn.generate_id()?;
                self.conn.create_pixmap(32, pix, win, pw, ph)?.check()?;
                let pic = self.conn.generate_id()?;
                self.conn.render_create_picture(pic, pix, fmt, &CreatePictureAux::new())?.check()?;
                self.pix_buf = Some((pix, pic, pw, ph));
                (pix, pic)
            }
        };

        // Build (or rebuild when popup size changes) the A8 window-corner mask.
        // Kept alive for the whole session so blit_to_window() never rebuilds it
        // mid-navigation — eliminating the pre-clear + OVER two-step flicker source.
        if pix_buf_fresh || self.win_mask_pic.is_none() {
            if let Some(old_mask) = self.win_mask_pic.take() {
                self.conn.render_free_picture(old_mask)?.check()?;
            }
            if win_br > 0 && a8_fmt != 0 {
                let mask_pix = self.conn.generate_id()?;
                self.conn.create_pixmap(8, mask_pix, win, pw, ph)?.check()?;
                let gc = self.conn.generate_id()?;
                self.conn.create_gc(gc, mask_pix, &CreateGCAux::new().foreground(0u32))?.check()?;
                self.conn.poly_fill_rectangle(mask_pix, gc,
                    &[Rectangle { x: 0, y: 0, width: pw, height: ph }])?.check()?;
                self.conn.change_gc(gc, &ChangeGCAux::new().foreground(255u32))?.check()?;
                let clamped_r = win_br.min(pw as u32 / 2).min(ph as u32 / 2);
                fill_rounded_rect_to_gc(self.conn, mask_pix, gc, 0, 0, pw, ph, clamped_r)?;
                self.conn.free_gc(gc)?.check()?;
                let mask_pic = self.conn.generate_id()?;
                self.conn.render_create_picture(mask_pic, mask_pix, a8_fmt,
                    &CreatePictureAux::new())?.check()?;
                self.conn.free_pixmap(mask_pix)?.check()?;
                self.win_mask_pic = Some(mask_pic);
            }
        }

        // Fill pixmap with the window background. Either a flat fill or a gradient.
        let gradient_mode = self.config.window.background_gradient.as_str();
        if gradient_mode == "none" {
            let (wbr, wbg, wbb, wba) = argb_to_render_color(window_bg_argb);
            self.conn.render_fill_rectangles(
                PictOp::SRC, pix_pic,
                RenderColor { red: wbr, green: wbg, blue: wbb, alpha: wba },
                &[Rectangle { x: 0, y: 0, width: pw, height: ph }],
            )?.check()?;
        } else {
            // Clear to transparent, then composite gradient on top.
            self.conn.render_fill_rectangles(
                PictOp::SRC, pix_pic,
                RenderColor { red: 0, green: 0, blue: 0, alpha: 0 },
                &[Rectangle { x: 0, y: 0, width: pw, height: ph }],
            )?.check()?;
            self.draw_bg_gradient(pix_pic, pw, ph, window_bg_argb, gradient_mode)?;
        }

        let fw = self.frame_w() as u16;
        let fw32 = self.frame_w();
        let (n_cols, _) = self.grid_layout();
        let use_rounded = br > 0 && a8_fmt != 0;

        for (i, entry) in self.windows.iter().enumerate() {
            let (tile_x, tile_y) = self.tile_pos(i, n_cols);
            let border_argb = if i == self.selected { frame_argb } else { inact_argb };

            if use_rounded {
                // `border_radius` is the outer corner radius (CSS semantics).
                // Draw bg and border into non-overlapping areas so neither bleeds
                // into the other's region and corner pixels are handled correctly.
                let outer_r = br.min((tw + 2*fw32) / 2).min((th + 2*fw32) / 2);
                let inner_r = br.saturating_sub(fw32).min(tw / 2).min(th / 2);
                // Tile background: inner rounded rect, composited OVER window bg.
                draw_filled_rounded_rect(
                    self.conn, pix_pic, fmt, a8_fmt, pixmap,
                    tile_x, tile_y, tw, th, inner_r, bg_argb,
                )?;
                // Border ring: outer shape minus inner shape, so it only covers
                // the fw-wide ring and never overwrites the bg or icon area.
                draw_border_ring(
                    self.conn, pix_pic, fmt, a8_fmt, pixmap,
                    tile_x - fw as i16, tile_y - fw as i16,
                    tw + 2*fw32, th + 2*fw32,
                    outer_r, fw32, inner_r, border_argb,
                )?;
            } else {
                // Tile background — OVER composites on top of the window background.
                let (ar, ag, ab, aa) = argb_to_render_color(bg_argb);
                self.conn.render_fill_rectangles(
                    PictOp::OVER, pix_pic,
                    RenderColor { red: ar, green: ag, blue: ab, alpha: aa },
                    &[Rectangle { x: tile_x, y: tile_y, width: tw as u16, height: th as u16 }],
                )?.check()?;

                // Frame (selected = frame color, others = inactive)
                let (fr, fg_c, fb, fa) = argb_to_render_color(border_argb);
                self.conn.render_fill_rectangles(PictOp::OVER, pix_pic,
                    RenderColor { red: fr, green: fg_c, blue: fb, alpha: fa },
                    &[
                        Rectangle { x: tile_x, y: tile_y - fw as i16, width: tw as u16, height: fw },
                        Rectangle { x: tile_x, y: tile_y + th as i16, width: tw as u16, height: fw },
                        Rectangle { x: tile_x - fw as i16, y: tile_y - fw as i16, width: fw, height: th as u16 + 2 * fw },
                        Rectangle { x: tile_x + tw as i16, y: tile_y - fw as i16, width: fw, height: th as u16 + 2 * fw },
                    ],
                )?.check()?;
            }

            // Icon or thumbnail
            if self.config.tile.content == "thumbnail" {
                self.draw_thumb(pix_pic, fmt, pixmap, entry, tile_x, tile_y, tw, th, fg_argb, &formats)?;
                if self.config.tile.icon_overlay && a8_fmt != 0 {
                    self.draw_icon_overlay(pix_pic, fmt, a8_fmt, pixmap, entry, tile_x, tile_y, tw, th)?;
                }
            } else {
                self.draw_icon(pix_pic, fmt, pixmap, entry, tile_x, tile_y, tw, th, fg_argb)?;
            }
        }

        // Redraw the selected tile's frame on top so it's never occluded by an
        // adjacent tile's border (left/right borders share the same X coordinate).
        let (sel_x, sel_y) = self.tile_pos(self.selected, n_cols);
        if use_rounded {
            // Only the border ring needs to be redrawn — it never touches the
            // inner bg or icon area, so those don't need to be repainted.
            let outer_r = br.min((tw + 2*fw32) / 2).min((th + 2*fw32) / 2);
            let inner_r = br.saturating_sub(fw32).min(tw / 2).min(th / 2);
            draw_border_ring(
                self.conn, pix_pic, fmt, a8_fmt, pixmap,
                sel_x - fw as i16, sel_y - fw as i16,
                tw + 2*fw32, th + 2*fw32,
                outer_r, fw32, inner_r, frame_argb,
            )?;
        } else {
            let (fr, fg_c, fb, fa) = argb_to_render_color(frame_argb);
            self.conn.render_fill_rectangles(PictOp::OVER, pix_pic,
                RenderColor { red: fr, green: fg_c, blue: fb, alpha: fa },
                &[
                    Rectangle { x: sel_x,              y: sel_y - fw as i16,  width: tw as u16,      height: fw },
                    Rectangle { x: sel_x,              y: sel_y + th as i16,  width: tw as u16,      height: fw },
                    Rectangle { x: sel_x - fw as i16,  y: sel_y - fw as i16,  width: fw, height: th as u16 + 2 * fw },
                    Rectangle { x: sel_x + tw as i16,  y: sel_y - fw as i16,  width: fw, height: th as u16 + 2 * fw },
                ],
            )?.check()?;
        }

        // Flush x11rb so all XRender work is committed to the pixmap before
        // Xft draws on the same drawable via the separate Xlib connection.
        self.conn.flush()?;

        // Draw Xft text labels onto the off-screen pixmap.
        for (i, entry) in self.windows.iter().enumerate() {
            let (tile_x, tile_y) = self.tile_pos(i, n_cols);
            self.draw_label(pixmap, entry, tile_x, tile_y, tw, th, fg_argb)?;
        }

        // XSync the Xlib connection: wait for the server to finish all Xft draws
        // so the pixmap is complete before we blit it to the window.
        if let Some(ref xft) = self.xft {
            unsafe { x11::xlib::XSync(xft.display, 0); }
        }

        if let Some(t) = t0 {
            eprintln!("[hop] redraw() done in {:?} → blitting", t.elapsed());
        }
        self.blit_to_window()
    }

    /// Blit the persistent off-screen pixmap to the window in one atomic step.
    /// Applies the window border-radius mask when configured.
    fn blit_to_window(&self) -> Result<(), Box<dyn Error>> {
        let win = match self.popup { Some(w) => w, None => return Ok(()) };
        let (_, pix_pic, pw, ph) = match self.pix_buf { Some(b) => b, None => return Ok(()) };
        let fmt = self.cached_argb_fmt;
        let a8_fmt = self.cached_a8_fmt;
        let win_br = self.config.window.border_radius;
        if fmt == 0 { return Ok(()); }
        if self.debug { eprintln!("[hop] blit_to_window() win_br={win_br}"); }

        let win_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(win_pic, win, fmt, &CreatePictureAux::new())?.check()?;

        // Use the cached mask (built once in redraw()) for a single atomic SRC blit.
        // PictOp::SRC with mask: result = src * mask_alpha — corners (mask=0) become
        // transparent in one operation with no intermediate transparent state that
        // the compositor could sample, eliminating the pre-clear flicker.
        let mask_pic = if win_br > 0 && a8_fmt != 0 { self.win_mask_pic.unwrap_or(0) } else { 0 };
        self.conn.render_composite(
            PictOp::SRC,
            pix_pic, mask_pic, win_pic,
            0, 0,   // src x, y
            0, 0,   // mask x, y
            0, 0,   // dst x, y
            pw, ph,
        )?.check()?;

        self.conn.render_free_picture(win_pic)?.check()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Partial redraw for selection changes: only repaints the two affected tile
    /// borders (old → inactive, new → frame) on the persistent pixmap, then blits.
    ///
    /// This skips all icon and text rendering, making next()/prev() nearly instant.
    /// For rounded borders with a flat background: SRC-erases the border rect area
    /// with the cached window bg, then composites the rounded ring with draw_border_ring.
    /// Only falls back to a full redraw when a gradient background is active (the
    /// gradient pixels in the corner areas can't be cheaply restored).
    fn border_redraw(&mut self, old_sel: usize) -> Result<(), Box<dyn Error>> {
        if old_sel == self.selected {
            return Ok(());
        }
        if self.debug {
            eprintln!("[hop] border_redraw(old={old_sel} → new={})", self.selected);
        }

        let (pixmap, pix_pic, _, _) = match self.pix_buf {
            Some(b) => b,
            None => return Ok(()),
        };

        let br        = self.border_radius();
        let a8_fmt    = self.cached_a8_fmt;
        let argb_fmt  = self.cached_argb_fmt;
        let use_rings = br > 0 && a8_fmt != 0;

        // Gradient corners can't be cheaply restored — fall back to full redraw.
        if use_rings && self.config.window.background_gradient != "none" {
            if self.debug { eprintln!("[hop] border_redraw: rounded+gradient → fallback full redraw"); }
            return self.redraw();
        }

        let tw         = self.tile_w();
        let th         = self.tile_h();
        let fw32       = self.frame_w();
        let fw         = fw32 as u16;
        let frame_argb = Config::color_argb(&self.config.tile.frame);
        let inact_argb = Config::color_argb(&self.config.tile.inactive);
        let (n_cols, _) = self.grid_layout();

        let (wbr, wbg, wbb, wba) = argb_to_render_color(self.cached_win_bg);
        let win_bg = RenderColor { red: wbr, green: wbg, blue: wbb, alpha: wba };

        for &(sel, border_argb) in &[(old_sel, inact_argb), (self.selected, frame_argb)] {
            if sel >= self.windows.len() { continue; }
            let (tile_x, tile_y) = self.tile_pos(sel, n_cols);
            // The four non-overlapping rectangular strips that cover the border ring area.
            let border_rects = [
                Rectangle { x: tile_x,              y: tile_y - fw as i16, width: tw as u16, height: fw },
                Rectangle { x: tile_x,              y: tile_y + th as i16, width: tw as u16, height: fw },
                Rectangle { x: tile_x - fw as i16,  y: tile_y - fw as i16, width: fw, height: th as u16 + 2 * fw },
                Rectangle { x: tile_x + tw as i16,  y: tile_y - fw as i16, width: fw, height: th as u16 + 2 * fw },
            ];
            // Erase the border strip area back to the flat window background.
            self.conn.render_fill_rectangles(PictOp::SRC, pix_pic, win_bg, &border_rects)?.check()?;

            if use_rings {
                // Composite the rounded ring shape on top (same formula as redraw()).
                let outer_r = br.min((tw + 2*fw32) / 2).min((th + 2*fw32) / 2);
                let inner_r = br.saturating_sub(fw32).min(tw / 2).min(th / 2);

                // draw_border_ring's inner punch-out uses rounded arcs. The square
                // corner areas of each inner_r×inner_r corner box fall outside the
                // arc and stay as mask=255 (ring pixels). Those pixels land inside
                // the tile interior bounds, so the 4 border rects above don't erase
                // them. Clear those ~inner_r×inner_r corner squares now so the old
                // ring color doesn't bleed through as tiny artifacts.
                if inner_r > 0 {
                    let cr  = inner_r as u16;
                    let cr16 = inner_r as i16;
                    let tw16 = tw as i16;
                    let th16 = th as i16;
                    self.conn.render_fill_rectangles(PictOp::SRC, pix_pic, win_bg, &[
                        Rectangle { x: tile_x,           y: tile_y,           width: cr, height: cr },
                        Rectangle { x: tile_x+tw16-cr16, y: tile_y,           width: cr, height: cr },
                        Rectangle { x: tile_x,           y: tile_y+th16-cr16, width: cr, height: cr },
                        Rectangle { x: tile_x+tw16-cr16, y: tile_y+th16-cr16, width: cr, height: cr },
                    ])?.check()?;
                }

                draw_border_ring(
                    self.conn, pix_pic, argb_fmt, a8_fmt, pixmap,
                    tile_x - fw as i16, tile_y - fw as i16,
                    tw + 2*fw32, th + 2*fw32,
                    outer_r, fw32, inner_r, border_argb,
                )?;
            } else {
                // Flat border: fill the strips directly with the border color.
                let (cr, cg, cb, ca) = argb_to_render_color(border_argb);
                self.conn.render_fill_rectangles(PictOp::OVER, pix_pic,
                    RenderColor { red: cr, green: cg, blue: cb, alpha: ca },
                    &border_rects)?.check()?;
            }
        }

        self.blit_to_window()
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
        let icon_size = self.config.tile.icon_size;
        let pad = self.tile_pad();
        // Center icon horizontally within the padded inner region
        let avail_w = tile_w.saturating_sub(2 * pad);
        let icon_x = tile_x + (pad as i16) + (avail_w.saturating_sub(icon_size) / 2) as i16;
        // Center icon vertically in the non-label area, respecting top/bottom padding
        let avail_icon_h = tile_h.saturating_sub(LABEL_H + 2 * pad);
        let icon_y = tile_y + (pad as i16) + (avail_icon_h.saturating_sub(icon_size) / 2) as i16;

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

    /// Draw a small app icon in the bottom-right corner of the tile content area,
    /// semi-transparent, as an overlay on top of a thumbnail. Silently does nothing
    /// if the entry has no icon data or the overlay is disabled.
    fn draw_icon_overlay(
        &self,
        pix_pic: u32,
        argb_fmt: u32,
        a8_fmt: u32,
        drawable: Window,
        entry: &WindowEntry,
        tile_x: i16,
        tile_y: i16,
        tile_w: u32,
        tile_h: u32,
    ) -> Result<(), Box<dyn Error>> {
        let (src_w, src_h, pixels) = match &entry.icon {
            Some(icon) => icon,
            None => return Ok(()),
        };
        let (src_w, src_h) = (*src_w, *src_h);
        if src_w == 0 || src_h == 0 { return Ok(()); }

        let ov_size = self.config.tile.icon_overlay_size.max(8);
        let pad = self.tile_pad();
        let avail_w = tile_w.saturating_sub(2 * pad);
        let avail_h = tile_h.saturating_sub(LABEL_H + 2 * pad);
        if avail_w < ov_size || avail_h < ov_size { return Ok(()); }

        // Bottom-right corner of the content area, with a small inset margin.
        let margin = 6i16;
        let ov_x = tile_x + pad as i16 + avail_w as i16 - ov_size as i16 - margin;
        let ov_y = tile_y + pad as i16 + avail_h as i16 - ov_size as i16 - margin;

        // Upload icon pixels into a temporary pixmap.
        let pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(32, pixmap, drawable, src_w as u16, src_h as u16)?.check()?;
        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0).background(0))?.check()?;
        let bytes: Vec<u8> = pixels.iter().flat_map(|&p| p.to_ne_bytes()).collect();
        self.conn.put_image(ImageFormat::Z_PIXMAP, pixmap, gc,
            src_w as u16, src_h as u16, 0, 0, 0, 32, &bytes)?.check()?;
        self.conn.free_gc(gc)?.check()?;

        let icon_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(icon_pic, pixmap, argb_fmt, &CreatePictureAux::new())?.check()?;
        self.conn.free_pixmap(pixmap)?.check()?;

        // Scale to ov_size if needed.
        if src_w != ov_size || src_h != ov_size {
            let sx = (src_w as i64 * 65536 / ov_size as i64) as i32;
            let sy = (src_h as i64 * 65536 / ov_size as i64) as i32;
            self.conn.render_set_picture_transform(icon_pic, Transform {
                matrix11: sx, matrix12: 0, matrix13: 0,
                matrix21: 0, matrix22: sy, matrix23: 0,
                matrix31: 0, matrix32: 0, matrix33: 65536,
            })?.check()?;
            self.conn.render_set_picture_filter(icon_pic, b"bilinear", &[])?.check()?;
        }

        // Build a 1×1 A8 alpha-mask picture with repeat, giving 80% opacity.
        let alpha_pix = self.conn.generate_id()?;
        self.conn.create_pixmap(8, alpha_pix, drawable, 1, 1)?.check()?;
        let alpha_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(
            alpha_pic, alpha_pix, a8_fmt,
            &CreatePictureAux::new().repeat(Repeat::NORMAL),
        )?.check()?;
        self.conn.free_pixmap(alpha_pix)?.check()?;
        // 80% opacity: 204/255 * 65535 ≈ 52428 in 16-bit XRender alpha space.
        self.conn.render_fill_rectangles(PictOp::SRC, alpha_pic,
            RenderColor { red: 0, green: 0, blue: 0, alpha: 52428 },
            &[Rectangle { x: 0, y: 0, width: 1, height: 1 }],
        )?.check()?;

        // Composite icon over the thumbnail with the alpha mask.
        self.conn.render_composite(
            PictOp::OVER,
            icon_pic, alpha_pic, pix_pic,
            0, 0, 0, 0,
            ov_x, ov_y,
            ov_size as u16, ov_size as u16,
        )?.check()?;

        self.conn.render_free_picture(alpha_pic)?.check()?;
        self.conn.render_free_picture(icon_pic)?.check()?;
        Ok(())
    }

    /// Render a scaled screenshot of the window (from the compositor's backing pixmap)
    /// into the tile content area. Falls back to `draw_icon` when the compositor is not
    /// running, the window is not redirected, or any X11 call fails.
    fn draw_thumb(
        &self,
        pix_pic: u32,
        argb_fmt: u32,
        drawable: Window,
        entry: &WindowEntry,
        tile_x: i16,
        tile_y: i16,
        tile_w: u32,
        tile_h: u32,
        fg_argb: u32,
        formats: &x11rb::protocol::render::QueryPictFormatsReply,
    ) -> Result<(), Box<dyn Error>> {
        let pad = self.tile_pad();
        let avail_w = tile_w.saturating_sub(2 * pad);
        let avail_h = tile_h.saturating_sub(LABEL_H + 2 * pad);

        if avail_w == 0 || avail_h == 0 {
            return self.draw_icon(pix_pic, argb_fmt, drawable, entry, tile_x, tile_y, tile_w, tile_h, fg_argb);
        }

        // Use the pre-computed WM frame window (direct child of root).
        // Compositors redirect only direct children of root, so NameWindowPixmap
        // must be called on the frame rather than the client window.
        let frame_win = entry.frame;

        // Get the frame window's visual for XRender format lookup.
        let visual_id = match self.conn.get_window_attributes(frame_win)
            .ok().and_then(|c| c.reply().ok()).map(|a| a.visual)
        {
            Some(v) => v,
            None => {
                if let Some((cw, ch, cpixels)) = self.thumb_cache.get(&frame_win) {
                    return self.draw_pixels_scaled(pix_pic, argb_fmt, drawable,
                        cpixels, *cw, *ch, tile_x, tile_y, avail_w, avail_h, pad);
                }
                return self.draw_icon(pix_pic, argb_fmt, drawable, entry, tile_x, tile_y, tile_w, tile_h, fg_argb);
            }
        };

        // Grab the compositor's off-screen backing pixmap for the frame window.
        let thumb_pix = self.conn.generate_id()?;
        let name_ok = self.conn
            .composite_name_window_pixmap(frame_win, thumb_pix)
            .ok().and_then(|c| c.check().ok()).is_some();
        if !name_ok {
            // Window is likely on another desktop (unmapped). Try the pixel cache.
            if let Some((cw, ch, cpixels)) = self.thumb_cache.get(&frame_win) {
                return self.draw_pixels_scaled(pix_pic, argb_fmt, drawable,
                    cpixels, *cw, *ch, tile_x, tile_y, avail_w, avail_h, pad);
            }
            return self.draw_icon(pix_pic, argb_fmt, drawable, entry, tile_x, tile_y, tile_w, tile_h, fg_argb);
        }

        // Query actual window dimensions via the pixmap geometry.
        let geom = match self.conn.get_geometry(thumb_pix).ok().and_then(|c| c.reply().ok()) {
            Some(g) => g,
            None => {
                let _ = self.conn.free_pixmap(thumb_pix).ok();
                if let Some((cw, ch, cpixels)) = self.thumb_cache.get(&frame_win) {
                    return self.draw_pixels_scaled(pix_pic, argb_fmt, drawable,
                        cpixels, *cw, *ch, tile_x, tile_y, avail_w, avail_h, pad);
                }
                return self.draw_icon(pix_pic, argb_fmt, drawable, entry, tile_x, tile_y, tile_w, tile_h, fg_argb);
            }
        };
        let src_w = geom.width as u32;
        let src_h = geom.height as u32;
        if src_w == 0 || src_h == 0 {
            let _ = self.conn.free_pixmap(thumb_pix).ok();
            return self.draw_icon(pix_pic, argb_fmt, drawable, entry, tile_x, tile_y, tile_w, tile_h, fg_argb);
        }

        // Find the XRender PictFormat that matches this window's visual.
        let win_fmt_id = match find_format_for_visual(formats, visual_id, self.screen_num) {
            Some(id) => id,
            None => {
                let _ = self.conn.free_pixmap(thumb_pix).ok();
                if let Some((cw, ch, cpixels)) = self.thumb_cache.get(&frame_win) {
                    return self.draw_pixels_scaled(pix_pic, argb_fmt, drawable,
                        cpixels, *cw, *ch, tile_x, tile_y, avail_w, avail_h, pad);
                }
                return self.draw_icon(pix_pic, argb_fmt, drawable, entry, tile_x, tile_y, tile_w, tile_h, fg_argb);
            }
        };

        // Create an XRender Picture for the pixmap; the picture holds a server-side
        // reference so we can free the pixmap name immediately.
        let thumb_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(thumb_pic, thumb_pix, win_fmt_id, &CreatePictureAux::new())?.check()?;
        self.conn.free_pixmap(thumb_pix)?.check()?;

        // Scale the window to fit inside (avail_w × avail_h), preserving aspect ratio.
        let scale = (src_w as f64 / avail_w as f64).max(src_h as f64 / avail_h as f64);
        let dst_w = ((src_w as f64 / scale).round() as u32).max(1).min(avail_w);
        let dst_h = ((src_h as f64 / scale).round() as u32).max(1).min(avail_h);

        // Center the thumbnail within the available content area.
        let dst_x = tile_x + pad as i16 + (avail_w.saturating_sub(dst_w) / 2) as i16;
        let dst_y = tile_y + pad as i16 + (avail_h.saturating_sub(dst_h) / 2) as i16;

        // Set a bilinear scaling transform (16.16 fixed-point).
        let sx = ((src_w as f64 / dst_w as f64) * 65536.0).round() as i32;
        let sy = ((src_h as f64 / dst_h as f64) * 65536.0).round() as i32;
        self.conn.render_set_picture_transform(thumb_pic, Transform {
            matrix11: sx, matrix12: 0, matrix13: 0,
            matrix21: 0, matrix22: sy, matrix23: 0,
            matrix31: 0, matrix32: 0, matrix33: 65536,
        })?.check()?;
        self.conn.render_set_picture_filter(thumb_pic, b"bilinear", &[])?.check()?;

        // Composite the thumbnail onto the off-screen buffer.
        self.conn.render_composite(
            PictOp::OVER,
            thumb_pic, 0u32, pix_pic,
            0, 0,   // src_x, src_y
            0, 0,   // mask_x, mask_y
            dst_x, dst_y,
            dst_w as u16, dst_h as u16,
        )?.check()?;

        self.conn.render_free_picture(thumb_pic)?.check()?;
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
        let title = truncate_title(&entry.name, TITLE_MAX_CHARS);
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
        let font = if font.is_null() {
            let fallback = CString::new(format!("sans:size={}", self.config.font.size))?;
            unsafe { xft::XftFontOpenName(xst.display, xst.screen_num, fallback.as_ptr()) }
        } else {
            font
        };
        if font.is_null() {
            return Ok(());
        }

        // Word-wrap the title to fit within the tile.
        // h_pad = 10px base + tile_padding for horizontal inset; v_pad for vertical.
        let h_pad = 10u32.saturating_add(self.tile_pad());
        let v_pad = self.tile_pad();
        let inner_w = tile_w.saturating_sub(2 * h_pad);
        let lines = wrap_text_xft(xst.display, font, title, inner_w);

        let line_h = unsafe { (*font).height.max(1) } as u32;
        let ascent = unsafe { (*font).ascent.max(0) } as i16;

        // Label area starts LABEL_H + v_pad px from the tile bottom
        let label_top = tile_y + (tile_h.saturating_sub(LABEL_H + v_pad)) as i16;

        // How many lines actually fit in LABEL_H
        let max_lines = (LABEL_H / line_h).max(1) as usize;
        let lines: Vec<String> = lines.into_iter().take(max_lines).collect();

        // Allocate Xft color from fg_argb
        let argb_to_xrender = |argb: u32| -> XRenderColor {
            let a = ((argb >> 24) & 0xFF) as u16;
            let r = ((argb >> 16) & 0xFF) as u16;
            let g = ((argb >>  8) & 0xFF) as u16;
            let b = ( argb        & 0xFF) as u16;
            XRenderColor { red: r * 0x101, green: g * 0x101, blue: b * 0x101, alpha: a * 0x101 }
        };

        let fg_rc = argb_to_xrender(fg_argb);
        let mut xft_color: xft::XftColor = unsafe { std::mem::zeroed() };
        unsafe { xft::XftColorAllocValue(xst.display, xst.visual, xst.colormap, &fg_rc, &mut xft_color); }

        // Optionally allocate a shadow color.
        let has_shadow = self.config.font.shadow;
        let mut shadow_color: xft::XftColor = unsafe { std::mem::zeroed() };
        if has_shadow {
            let shadow_argb = resolve_shadow_color(&self.config.font.shadow_color, fg_argb);
            let shadow_rc = argb_to_xrender(shadow_argb);
            unsafe { xft::XftColorAllocValue(xst.display, xst.visual, xst.colormap, &shadow_rc, &mut shadow_color); }
        }

        let draw = unsafe {
            xft::XftDrawCreate(xst.display, win as u64, xst.visual, xst.colormap)
        };

        for (i, line) in lines.iter().enumerate() {
            let text = line.as_bytes();

            // Measure for horizontal centering
            let mut extents: XGlyphInfo = unsafe { std::mem::zeroed() };
            unsafe {
                xft::XftTextExtentsUtf8(
                    xst.display, font,
                    text.as_ptr(), text.len() as i32,
                    &mut extents,
                );
            }
            let text_w = extents.xOff.max(0) as u32;
            let x = tile_x + (h_pad as i16) + (inner_w.saturating_sub(text_w) / 2) as i16;
            let y = label_top + ascent + (i as u32 * line_h) as i16;

            // Shadow pass (drawn first so it appears behind the main text).
            if has_shadow {
                let off = self.config.font.shadow_offset as i16;
                unsafe {
                    xft::XftDrawStringUtf8(
                        draw, &shadow_color, font,
                        (x + off) as i32, (y + off) as i32,
                        text.as_ptr(), text.len() as i32,
                    );
                }
            }

            unsafe {
                xft::XftDrawStringUtf8(
                    draw, &xft_color, font,
                    x as i32, y as i32,
                    text.as_ptr(), text.len() as i32,
                );
            }
        }

        unsafe {
            xft::XftDrawDestroy(draw);
            if has_shadow {
                xft::XftColorFree(xst.display, xst.visual, xst.colormap, &mut shadow_color);
            }
            xft::XftColorFree(xst.display, xst.visual, xst.colormap, &mut xft_color);
            xft::XftFontClose(xst.display, font);
            x11::xlib::XFlush(xst.display);
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

        let h_pad = 10u32.saturating_add(self.tile_pad());
        let v_pad = self.tile_pad();
        let inner_w = tile_w.saturating_sub(2 * h_pad);
        let label_x = tile_x + (h_pad as i16) + (inner_w.saturating_sub(text_w) / 2) as i16;
        let label_y = tile_y + tile_h as i16 - 4 - v_pad as i16;

        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, win, &CreateGCAux::new()
            .foreground(fg_argb)
            .background(0)
            .font(font)
        )?.check()?;

        let mut items = vec![title_bytes.len() as u8, 0u8];
        items.extend_from_slice(title_bytes);

        // Shadow pass.
        if self.config.font.shadow {
            let shadow_argb = resolve_shadow_color(&self.config.font.shadow_color, fg_argb);
            let off = self.config.font.shadow_offset as i16;
            self.conn.change_gc(gc, &ChangeGCAux::new().foreground(shadow_argb))?.check()?;
            self.conn.poly_text8(win, gc, label_x + off, label_y + off, &items)?.check()?;
            self.conn.change_gc(gc, &ChangeGCAux::new().foreground(fg_argb))?.check()?;
        }

        self.conn.poly_text8(win, gc, label_x, label_y, &items)?.check()?;

        self.conn.free_gc(gc)?.check()?;
        self.conn.close_font(font)?.check()?;
        Ok(())
    }

    pub fn next(&mut self) -> Result<(), Box<dyn Error>> {
        if self.windows.is_empty() { return Ok(()); }
        let old = self.selected;
        self.selected = (self.selected + 1) % self.windows.len();
        if self.debug { eprintln!("[hop] next(): {old} → {}", self.selected); }
        self.border_redraw(old)
    }

    pub fn prev(&mut self) -> Result<(), Box<dyn Error>> {
        if self.windows.is_empty() { return Ok(()); }
        let old = self.selected;
        self.selected = self.selected.checked_sub(1).unwrap_or(self.windows.len() - 1);
        if self.debug { eprintln!("[hop] prev(): {old} → {}", self.selected); }
        self.border_redraw(old)
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
        if self.debug { eprintln!("[hop] hide()"); }
        if let Some(win) = self.popup.take() {
            // Release the AUTOMATIC composite redirect we registered in show().
            if self.config.tile.content == "thumbnail" {
                let root = self.conn.setup().roots[self.screen_num].root;
                let _ = self.conn.composite_unredirect_subwindows(
                    root,
                    x11rb::protocol::composite::Redirect::AUTOMATIC,
                )?.check();
            }
            self.conn.ungrab_keyboard(0u32)?.check()?;
            self.conn.destroy_window(win)?.check()?;
            // Free the persistent off-screen buffer and window mask.
            if let Some((pix, pic, _, _)) = self.pix_buf.take() {
                self.conn.render_free_picture(pic)?.check()?;
                self.conn.free_pixmap(pix)?.check()?;
            }
            if let Some(mask) = self.win_mask_pic.take() {
                self.conn.render_free_picture(mask)?.check()?;
            }
            self.cached_argb_fmt = 0;
            self.cached_a8_fmt   = 0;
            self.conn.flush()?;
        }
        self.windows.clear();
        self.thumb_queue.clear();
        Ok(())
    }

    /// Return the tile index whose content rect contains popup-relative point (px, py).
    fn tile_at(&self, px: i16, py: i16) -> Option<usize> {
        let (n_cols, _) = self.grid_layout();
        let tw = self.tile_w() as i16;
        let th = self.tile_h() as i16;
        for (i, _) in self.windows.iter().enumerate() {
            let (tx, ty) = self.tile_pos(i, n_cols);
            if px >= tx && px < tx + tw && py >= ty && py < ty + th {
                return Some(i);
            }
        }
        None
    }

    /// Update selection when the pointer moves over a tile. Only redraws if the
    /// hovered tile differs from the current selection; ignores motion outside tiles.
    pub fn hover_at(&mut self, px: i16, py: i16) -> Result<(), Box<dyn Error>> {
        if let Some(idx) = self.tile_at(px, py) {
            if idx != self.selected {
                let old = self.selected;
                self.selected = idx;
                self.border_redraw(old)?;
            }
        }
        Ok(())
    }

    /// Handle a mouse click at popup-relative (px, py).
    /// Activates the clicked tile, or cancels if the click lands outside all tiles.
    pub fn click_at(&mut self, root: Window, px: i16, py: i16) -> Result<(), Box<dyn Error>> {
        if let Some(idx) = self.tile_at(px, py) {
            self.selected = idx;
            self.commit(root)?;
        } else {
            self.cancel()?;
        }
        Ok(())
    }

    /// Composite a gradient over `pix_pic` as the window background.
    ///
    /// The gradient always runs from fully transparent to `bg_argb` (at its configured alpha).
    /// `mode` controls the shape: "radial" (opaque center, transparent edges),
    /// "vertical" (transparent top → opaque bottom), "horizontal" (transparent left → opaque right).
    fn draw_bg_gradient(
        &self,
        pix_pic: u32,
        pw: u16,
        ph: u16,
        bg_argb: u32,
        mode: &str,
    ) -> Result<(), Box<dyn Error>> {
        let (opr, opg, opb, opa) = argb_to_render_color(bg_argb);
        let color_bg    = RenderColor { red: opr, green: opg, blue: opb, alpha: opa };
        let color_trans = RenderColor { red: 0, green: 0, blue: 0, alpha: 0 };

        // Two stops: 0.0 and 1.0 in 16.16 fixed-point
        let stops: &[i32] = &[0, 65536];

        let grad_pic = self.conn.generate_id()?;

        match mode {
            "radial" => {
                // Center is opaque background, edges fade to transparent.
                let cx = pw as i32 * 32768; // pw/2 in 16.16
                let cy = ph as i32 * 32768; // ph/2 in 16.16
                // Outer radius = distance from center to corner (in pixels × 65536)
                let outer_r = (((pw as f64 * 0.5).hypot(ph as f64 * 0.5)) * 65536.0) as i32;
                let center = Pointfix { x: cx, y: cy };
                self.conn.render_create_radial_gradient(
                    grad_pic,
                    center, center,
                    0,       // inner radius
                    outer_r, // outer radius
                    stops,
                    &[color_bg, color_trans], // center = bg, edge = transparent
                )?.check()?;
            }
            "horizontal" => {
                // Left = transparent, right = background color.
                let p1 = Pointfix { x: 0, y: 0 };
                let p2 = Pointfix { x: pw as i32 * 65536, y: 0 };
                self.conn.render_create_linear_gradient(
                    grad_pic, p1, p2, stops, &[color_trans, color_bg],
                )?.check()?;
            }
            _ => {
                // "vertical" (default): transparent top → opaque bottom.
                let p1 = Pointfix { x: 0, y: 0 };
                let p2 = Pointfix { x: 0, y: ph as i32 * 65536 };
                self.conn.render_create_linear_gradient(
                    grad_pic, p1, p2, stops, &[color_trans, color_bg],
                )?.check()?;
            }
        }

        self.conn.render_composite(
            PictOp::OVER,
            grad_pic, 0u32, pix_pic,
            0, 0,  // src x, y
            0, 0,  // mask x, y
            0, 0,  // dst x, y
            pw, ph,
        )?.check()?;

        self.conn.render_free_picture(grad_pic)?.check()?;
        Ok(())
    }

    /// Capture and cache a thumbnail for `frame_win` (a direct child of root).
    /// Downloads the compositor's backing pixmap pixels via GetImage and stores them
    /// as ARGB u32 values. Called on MapNotify and at popup-open time so thumbnails
    /// True while there are frames still waiting for their thumbnail to be fetched.
    /// Used by the event loop to decide between blocking and non-blocking event polling.
    pub fn has_pending_thumbs(&self) -> bool {
        !self.thumb_queue.is_empty()
    }

    /// Fetch one thumbnail from the front of the queue and update the display.
    /// Called from the event loop (after polling for events) so that the popup
    /// appears immediately and thumbnails fill in one per iteration.
    pub fn pump_one_thumb(&mut self) -> Result<(), Box<dyn Error>> {
        let frame = match self.thumb_queue.pop_front() {
            Some(f) => f,
            None => return Ok(()),
        };
        self.cache_thumb(frame);
        // Redraw now so each thumbnail appears as soon as it's ready.
        self.redraw()
    }

    /// remain available after windows move to other desktops and become unmapped.
    /// Silently does nothing if the window is not redirected or any call fails.
    pub fn cache_thumb(&mut self, frame_win: Window) {
        if self.config.tile.content != "thumbnail" {
            return;
        }
        let pix_id = match self.conn.generate_id() {
            Ok(id) => id,
            Err(_) => return,
        };
        let ok = self.conn
            .composite_name_window_pixmap(frame_win, pix_id)
            .ok()
            .and_then(|c| c.check().ok())
            .is_some();
        if !ok {
            return;
        }
        let geom = match self.conn.get_geometry(pix_id).ok().and_then(|c| c.reply().ok()) {
            Some(g) => g,
            None => { let _ = self.conn.free_pixmap(pix_id).ok(); return; }
        };
        let w = geom.width as u32;
        let h = geom.height as u32;
        // Skip unreasonably large or empty pixmaps.
        if w == 0 || h == 0 || w > 8192 || h > 8192 {
            let _ = self.conn.free_pixmap(pix_id).ok();
            return;
        }
        // Download all pixels as Z_PIXMAP (native-endian 32-bit values).
        let image = match self.conn.get_image(
            ImageFormat::Z_PIXMAP,
            pix_id, 0, 0, geom.width, geom.height, !0u32,
        ).ok().and_then(|c| c.reply().ok()) {
            Some(img) => img,
            None => { let _ = self.conn.free_pixmap(pix_id).ok(); return; }
        };
        let _ = self.conn.free_pixmap(pix_id).ok();

        let depth = image.depth;
        let data = &image.data;
        let n = (w * h) as usize;
        if data.len() < n * 4 { return; }

        // Convert from server native-endian BGR(A) to ARGB u32.
        // On little-endian x86: bytes are [B, G, R, A/pad] per pixel.
        let pixels: Vec<u32> = data[..n * 4]
            .chunks_exact(4)
            .map(|c| {
                let a = if depth == 32 { c[3] as u32 } else { 0xFF };
                (a << 24) | ((c[2] as u32) << 16) | ((c[1] as u32) << 8) | c[0] as u32
            })
            .collect();

        // Downscale to tile content size before storing so that draw_pixels_scaled
        // uploads only ~tile-sized pixel data (~170KB) instead of full-resolution
        // window pixels (~8MB). This makes each redraw 40-50× cheaper.
        let pad = self.config.tile.padding;
        let avail_w = self.config.tile.width.saturating_sub(2 * pad).max(1);
        let avail_h = self.config.tile.height.saturating_sub(LABEL_H + 2 * pad).max(1);
        let scale = (w as f64 / avail_w as f64).max(h as f64 / avail_h as f64);
        let (store_w, store_h, pixels) = if scale > 1.0 {
            let dst_w = ((w as f64 / scale).round() as u32).max(1);
            let dst_h = ((h as f64 / scale).round() as u32).max(1);
            let scaled = downscale_argb(&pixels, w, h, dst_w, dst_h);
            (dst_w, dst_h, scaled)
        } else {
            (w, h, pixels)
        };

        if self.debug {
            eprintln!("[hop] cache_thumb({frame_win:#x}): {w}x{h} depth={depth} → stored {store_w}x{store_h}");
        }
        self.thumb_cache.insert(frame_win, (store_w, store_h, pixels));
    }

    /// Remove stale cache entries when a window is destroyed.
    pub fn on_window_destroyed(&mut self, win: Window) {
        self.thumb_cache.remove(&win);
    }

    /// Upload ARGB pixel data into a temporary pixmap, scale it to fit within
    /// `(avail_w × avail_h)` preserving aspect ratio, and composite OVER `pix_pic`.
    /// Used to render cached thumbnails for off-desktop windows.
    fn draw_pixels_scaled(
        &self,
        pix_pic: u32,
        argb_fmt: u32,
        drawable: Window,
        pixels: &[u32],
        src_w: u32,
        src_h: u32,
        tile_x: i16,
        tile_y: i16,
        avail_w: u32,
        avail_h: u32,
        pad: u32,
    ) -> Result<(), Box<dyn Error>> {
        if src_w == 0 || src_h == 0 { return Ok(()); }

        let pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(32, pixmap, drawable, src_w as u16, src_h as u16)?.check()?;
        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0).background(0))?.check()?;
        let bytes: Vec<u8> = pixels.iter().flat_map(|&p| p.to_ne_bytes()).collect();
        self.conn.put_image(
            ImageFormat::Z_PIXMAP, pixmap, gc,
            src_w as u16, src_h as u16, 0, 0, 0, 32, &bytes,
        )?.check()?;
        self.conn.free_gc(gc)?.check()?;

        let pic = self.conn.generate_id()?;
        self.conn.render_create_picture(pic, pixmap, argb_fmt, &CreatePictureAux::new())?.check()?;
        self.conn.free_pixmap(pixmap)?.check()?;

        let scale = (src_w as f64 / avail_w as f64).max(src_h as f64 / avail_h as f64);
        let dst_w = ((src_w as f64 / scale).round() as u32).max(1).min(avail_w);
        let dst_h = ((src_h as f64 / scale).round() as u32).max(1).min(avail_h);
        let dst_x = tile_x + pad as i16 + (avail_w.saturating_sub(dst_w) / 2) as i16;
        let dst_y = tile_y + pad as i16 + (avail_h.saturating_sub(dst_h) / 2) as i16;

        let sx = ((src_w as f64 / dst_w as f64) * 65536.0).round() as i32;
        let sy = ((src_h as f64 / dst_h as f64) * 65536.0).round() as i32;
        self.conn.render_set_picture_transform(pic, Transform {
            matrix11: sx, matrix12: 0, matrix13: 0,
            matrix21: 0, matrix22: sy, matrix23: 0,
            matrix31: 0, matrix32: 0, matrix33: 65536,
        })?.check()?;
        self.conn.render_set_picture_filter(pic, b"bilinear", &[])?.check()?;

        self.conn.render_composite(
            PictOp::OVER, pic, 0u32, pix_pic,
            0, 0, 0, 0, dst_x, dst_y, dst_w as u16, dst_h as u16,
        )?.check()?;
        self.conn.render_free_picture(pic)?.check()?;
        Ok(())
    }

    pub fn popup_window(&self) -> Option<Window> {
        self.popup
    }

    pub fn is_visible(&self) -> bool {
        self.popup.is_some()
    }

    /// Repaint the window in response to an Expose event.
    /// If the persistent pixmap is already rendered, just blits it (no Xft work).
    /// Falls back to a full redraw only if the pixmap has been freed (shouldn't
    /// happen during normal use, but guards against edge cases like window resize).
    pub fn repaint(&mut self) -> Result<(), Box<dyn Error>> {
        if self.pix_buf.is_some() && self.cached_argb_fmt != 0 {
            if self.debug { eprintln!("[hop] repaint() → blit (pix_buf valid)"); }
            self.blit_to_window()
        } else {
            if self.debug { eprintln!("[hop] repaint() → full redraw (pix_buf gone)"); }
            self.redraw()
        }
    }
}

/// Box-filter downscale of ARGB u32 pixels from (src_w×src_h) to (dst_w×dst_h).
/// Each destination pixel is the average of the corresponding source block.
/// Only called when dst is smaller than src; caller must ensure dst > 0.
fn downscale_argb(pixels: &[u32], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u32> {
    let mut out = vec![0u32; (dst_w * dst_h) as usize];
    let x_ratio = src_w as f64 / dst_w as f64;
    let y_ratio = src_h as f64 / dst_h as f64;
    for dy in 0..dst_h {
        let y0 = (dy as f64 * y_ratio) as u32;
        let y1 = ((dy + 1) as f64 * y_ratio) as u32;
        let y1 = y1.min(src_h);
        for dx in 0..dst_w {
            let x0 = (dx as f64 * x_ratio) as u32;
            let x1 = ((dx + 1) as f64 * x_ratio) as u32;
            let x1 = x1.min(src_w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = pixels[(sy * src_w + sx) as usize];
                    a += ((p >> 24) & 0xFF) as u64;
                    r += ((p >> 16) & 0xFF) as u64;
                    g += ((p >>  8) & 0xFF) as u64;
                    b += ( p        & 0xFF) as u64;
                    n += 1;
                }
            }
            if n > 0 {
                out[(dy * dst_w + dx) as usize] =
                    (((a/n) as u32) << 24) | (((r/n) as u32) << 16) | (((g/n) as u32) << 8) | (b/n) as u32;
            }
        }
    }
    out
}

/// Walk up the window hierarchy to find the WM frame: the direct child of root
/// that contains `client`. Compositors redirect only direct children of root,
/// so NameWindowPixmap must be called on the frame, not the client window.
fn find_frame_win(conn: &RustConnection, client: Window, root: Window) -> Window {
    let mut w = client;
    loop {
        match conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
            Some(t) if t.parent == root || t.parent == 0 => return w,
            Some(t) if t.parent != w => w = t.parent,
            _ => return client,
        }
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

/// Search standard XDG icon theme paths for a PNG icon matching `class`.
/// Tries both the lowercase and original-case class name, and prefers the size
/// closest to `target_size`. Returns `(width, height, ARGB pixels)` on success.
fn load_icon_file(class: &str, target_size: u32) -> Option<(u32, u32, Vec<u32>)> {
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
fn load_png_file(path: &str) -> Option<(u32, u32, Vec<u32>)> {
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

/// Word-wrap `text` so each line is at most `max_px` pixels wide when rendered
/// with `font`.  Falls back to character-level breaking for words that are
/// wider than `max_px` on their own.
fn wrap_text_xft(
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
fn wcag_shadow_argb(fg_argb: u32) -> u32 {
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
fn resolve_shadow_color(shadow_color_str: &str, fg_argb: u32) -> u32 {
    if shadow_color_str == "auto" {
        wcag_shadow_argb(fg_argb)
    } else {
        Config::color_argb(shadow_color_str)
    }
}

/// Find the XRender PictFormat ID for the given visual on the given screen.
/// Used to create a Picture from a composite backing pixmap whose depth may differ
/// from our ARGB32 popup (e.g., a 24-bit window has a 24-bit format).
fn find_format_for_visual(
    formats: &x11rb::protocol::render::QueryPictFormatsReply,
    visual_id: Visualid,
    screen_num: usize,
) -> Option<u32> {
    let screen = formats.screens.get(screen_num)?;
    for depth in &screen.depths {
        for visual in &depth.visuals {
            if visual.visual == visual_id {
                return Some(visual.format);
            }
        }
    }
    None
}

/// Find the XRender A8 (8-bit alpha-only) picture format.
fn find_a8_format(formats: &x11rb::protocol::render::QueryPictFormatsReply) -> Option<u32> {
    formats.formats.iter()
        .find(|f| {
            f.depth == 8
                && f.type_ == PictType::DIRECT
                && f.direct.alpha_mask == 0xFF
                && f.direct.red_mask == 0
                && f.direct.green_mask == 0
                && f.direct.blue_mask == 0
        })
        .map(|f| f.id)
}

/// Draw a filled rounded rectangle shape into a pixmap using a GC.
/// The GC's current foreground pixel value is used for the fill.
/// Painting order: three rectangles for the interior cross, four filled arcs for corners.
fn fill_rounded_rect_to_gc(
    conn: &RustConnection,
    pix: u32,
    gc: u32,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    radius: u32,
) -> Result<(), Box<dyn Error>> {
    let r = (radius as u16).min(w / 2).min(h / 2);
    if r == 0 {
        conn.poly_fill_rectangle(pix, gc, &[Rectangle { x, y, width: w, height: h }])?.check()?;
        return Ok(());
    }
    let r2 = r * 2;
    conn.poly_fill_rectangle(pix, gc, &[
        Rectangle { x: x + r as i16,           y,               width: w - r2, height: h      },
        Rectangle { x,                          y: y + r as i16, width: r,      height: h - r2 },
        Rectangle { x: x + (w - r) as i16,     y: y + r as i16, width: r,      height: h - r2 },
    ])?.check()?;
    conn.poly_fill_arc(pix, gc, &[
        Arc { x,                     y,                     width: r2, height: r2, angle1: 90*64,  angle2: 90*64 }, // top-left
        Arc { x: x+(w-r2) as i16,   y,                     width: r2, height: r2, angle1: 0,      angle2: 90*64 }, // top-right
        Arc { x: x+(w-r2) as i16,   y: y+(h-r2) as i16,   width: r2, height: r2, angle1: 17280,  angle2: 90*64 }, // bottom-right (270*64)
        Arc { x,                     y: y+(h-r2) as i16,   width: r2, height: r2, angle1: 180*64, angle2: 90*64 }, // bottom-left
    ])?.check()?;
    Ok(())
}

/// Composite a solid color through an A8 rounded-rect mask onto `dst_pic` (OVER).
///
/// When `radius == 0` falls back to a plain `render_fill_rectangles` call.
fn draw_filled_rounded_rect(
    conn: &RustConnection,
    dst_pic: u32,
    argb_fmt: u32,
    a8_fmt: u32,
    drawable: u32,
    x: i16,
    y: i16,
    w: u32,
    h: u32,
    radius: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    if w == 0 || h == 0 { return Ok(()); }
    let wu = w as u16;
    let hu = h as u16;

    if radius == 0 {
        let (cr, cg, cb, ca) = argb_to_render_color(color_argb);
        conn.render_fill_rectangles(
            PictOp::OVER, dst_pic,
            RenderColor { red: cr, green: cg, blue: cb, alpha: ca },
            &[Rectangle { x, y, width: wu, height: hu }],
        )?.check()?;
        return Ok(());
    }

    let mask_pix = conn.generate_id()?;
    conn.create_pixmap(8, mask_pix, drawable, wu, hu)?.check()?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, mask_pix, &CreateGCAux::new().foreground(0u32))?.check()?;
    conn.poly_fill_rectangle(mask_pix, gc, &[Rectangle { x: 0, y: 0, width: wu, height: hu }])?.check()?;
    conn.change_gc(gc, &ChangeGCAux::new().foreground(255u32))?.check()?;
    fill_rounded_rect_to_gc(conn, mask_pix, gc, 0, 0, wu, hu, radius)?;
    conn.free_gc(gc)?.check()?;

    composite_color_through_mask(conn, dst_pic, argb_fmt, a8_fmt, drawable, x, y, wu, hu, mask_pix, color_argb)?;
    conn.free_pixmap(mask_pix)?.check()?;
    Ok(())
}

/// Composite a solid color through a ring-shaped A8 mask onto `dst_pic` (OVER).
///
/// The ring = outer rounded rect minus the inner rounded rect punched out.
/// `(ox, oy, ow, oh)` is the outer rect; the inner rect is inset by `fw` on all sides.
/// `outer_r` is the corner radius of the outer edge; `inner_r` for the inner edge
/// (typically `outer_r - fw`, or 0 when `outer_r <= fw`).
fn draw_border_ring(
    conn: &RustConnection,
    dst_pic: u32,
    argb_fmt: u32,
    a8_fmt: u32,
    drawable: u32,
    ox: i16,
    oy: i16,
    ow: u32,
    oh: u32,
    outer_r: u32,
    fw: u32,
    inner_r: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    if ow == 0 || oh == 0 { return Ok(()); }
    let owu = ow as u16;
    let ohu = oh as u16;

    let mask_pix = conn.generate_id()?;
    conn.create_pixmap(8, mask_pix, drawable, owu, ohu)?.check()?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, mask_pix, &CreateGCAux::new().foreground(0u32))?.check()?;
    // Clear mask to 0.
    conn.poly_fill_rectangle(mask_pix, gc, &[Rectangle { x: 0, y: 0, width: owu, height: ohu }])?.check()?;
    // Paint outer rounded rect with 255.
    conn.change_gc(gc, &ChangeGCAux::new().foreground(255u32))?.check()?;
    fill_rounded_rect_to_gc(conn, mask_pix, gc, 0, 0, owu, ohu, outer_r)?;
    // Punch out inner rounded rect with 0.
    let iw = ow.saturating_sub(2 * fw) as u16;
    let ih = oh.saturating_sub(2 * fw) as u16;
    if iw > 0 && ih > 0 {
        conn.change_gc(gc, &ChangeGCAux::new().foreground(0u32))?.check()?;
        fill_rounded_rect_to_gc(conn, mask_pix, gc, fw as i16, fw as i16, iw, ih, inner_r)?;
    }
    conn.free_gc(gc)?.check()?;

    composite_color_through_mask(conn, dst_pic, argb_fmt, a8_fmt, drawable, ox, oy, owu, ohu, mask_pix, color_argb)?;
    conn.free_pixmap(mask_pix)?.check()?;
    Ok(())
}

/// Composite `color_argb` through an existing A8 `mask_pix` onto `dst_pic` using OVER.
/// Creates a temporary 1×1 repeated source and frees it afterwards.
/// Does NOT free `mask_pix` — the caller is responsible for that.
fn composite_color_through_mask(
    conn: &RustConnection,
    dst_pic: u32,
    argb_fmt: u32,
    a8_fmt: u32,
    drawable: u32,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    mask_pix: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    let mask_pic = conn.generate_id()?;
    conn.render_create_picture(mask_pic, mask_pix, a8_fmt, &CreatePictureAux::new())?.check()?;

    let src_pix = conn.generate_id()?;
    conn.create_pixmap(32, src_pix, drawable, 1, 1)?.check()?;
    let src_pic = conn.generate_id()?;
    conn.render_create_picture(src_pic, src_pix, argb_fmt,
        &CreatePictureAux::new().repeat(Repeat::NORMAL))?.check()?;
    conn.free_pixmap(src_pix)?.check()?;

    let (cr, cg, cb, ca) = argb_to_render_color(color_argb);
    conn.render_fill_rectangles(
        PictOp::SRC, src_pic,
        RenderColor { red: cr, green: cg, blue: cb, alpha: ca },
        &[Rectangle { x: 0, y: 0, width: 1, height: 1 }],
    )?.check()?;

    conn.render_composite(
        PictOp::OVER,
        src_pic, mask_pic, dst_pic,
        0, 0,  // src x, y  (1×1 with repeat)
        0, 0,  // mask x, y
        x, y,  // dst x, y
        w, h,
    )?.check()?;

    conn.render_free_picture(src_pic)?.check()?;
    conn.render_free_picture(mask_pic)?.check()?;
    Ok(())
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
