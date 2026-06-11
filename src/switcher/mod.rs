//! The switcher popup window: layout, rendering, state machine.

mod icons;
mod text_util;
mod render_util;

use icons::load_icon_file;
use text_util::{open_core_font, truncate_title, wrap_text_xft, resolve_shadow_color};
use render_util::{
    downscale_argb, find_frames_batched, find_a8_format,
    fill_rounded_rect_to_gc, draw_filled_rounded_rect, draw_border_ring,
    argb_to_render_color,
};

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
/// Maximum number of entries kept in the thumbnail pixel cache.
/// Entries for windows not in the current list are evicted first; after that
/// an arbitrary entry is dropped so the cap is always respected.
const MAX_THUMB_CACHE: usize = 64;

/// Tile position and size in popup-relative coordinates.
#[derive(Clone, Copy)]
struct TileGeom { x: i16, y: i16, w: u32, h: u32 }

/// XRender destination picture, format IDs, and backing drawable.
/// Bundled to avoid threading 4 related values through every draw function.
#[derive(Clone, Copy)]
struct PictCtx {
    /// The XRender picture to composite into.
    pic: u32,
    /// ARGB32 picture format ID.
    argb_fmt: u32,
    /// A8 (alpha-only) picture format ID. May be 0 if rounded corners are disabled.
    a8_fmt: u32,
    /// X11 drawable backing `pic`. Used when creating temporary pixmaps.
    drawable: Window,
}

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
    root: Window,
    screen_w: u16,
    screen_h: u16,
    screen_num: usize,
    /// EWMH atoms interned once at startup (see x11::Atoms).
    atoms: xh::Atoms,
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
    /// Window indices still waiting to be enriched (icon fetch + thumbnail capture).
    /// Populated by show(); drained one entry per event-loop iteration by
    /// pump_one_enrich(). This is the "enrich after a tick" phase: the popup paints
    /// immediately with backgrounds/borders/labels, then icons and thumbnails
    /// stream in one window at a time without blocking input.
    enrich_queue: VecDeque<usize>,
    /// XRender pict-formats reply, fetched once and reused. The reply is large and
    /// never changes for the connection, so caching it avoids a big round-trip on
    /// every redraw — notably once per thumbnail during progressive loading.
    cached_formats: Option<std::rc::Rc<x11rb::protocol::render::QueryPictFormatsReply>>,
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
            root: display.root,
            screen_w: display.screen_width,
            screen_h: display.screen_height,
            screen_num: display.screen_num,
            atoms: display.atoms,
            xft,
            pix_buf: None,
            win_mask_pic: None,
            cached_argb_fmt: 0,
            cached_a8_fmt: 0,
            cached_win_bg: 0,
            debug: std::env::var("HOP_DEBUG").is_ok(),
            thumb_cache: HashMap::new(),
            enrich_queue: VecDeque::new(),
            cached_formats: None,
        })
    }

    /// Populate the window list from EWMH. Skip the switcher popup itself.
    ///
    /// Only the cheap metadata needed for the first paint is fetched here —
    /// window type (skip filter) and title — and both are pipelined (all requests
    /// issued up front, replies collected afterward, ~1 round-trip total). Icons
    /// are deliberately NOT fetched: `_NET_WM_ICON` ships every icon size and
    /// transferring all of them for every window dominated popup-open latency.
    /// Icons load progressively afterward via `pump_one_enrich` (off the critical
    /// path), exactly like thumbnails.
    pub fn load_windows(&mut self, root: Window) -> Result<(), Box<dyn Error>> {
        let win_ids = xh::get_window_list(self.conn, root, &self.atoms)?;
        self.windows.clear();

        // Copy out the borrowed/Copy state so the loop can push to self.windows
        // without holding an immutable borrow of self.
        let conn = self.conn;
        let atoms = self.atoms;
        let want_thumbs = self.config.tile.content == "thumbnail";

        // Phase 1: fire off type + name requests without waiting for any reply.
        let mut ids: Vec<Window> = Vec::with_capacity(win_ids.len());
        let mut type_cookies     = Vec::with_capacity(win_ids.len());
        let mut net_name_cookies = Vec::with_capacity(win_ids.len());
        let mut wm_name_cookies  = Vec::with_capacity(win_ids.len());

        for id in win_ids {
            if self.popup == Some(id) {
                continue;
            }
            ids.push(id);
            type_cookies.push(conn.get_property(false, id, atoms.net_wm_window_type, AtomEnum::ATOM, 0, 32)?);
            net_name_cookies.push(conn.get_property(false, id, atoms.net_wm_name, atoms.utf8_string, 0, 256)?);
            wm_name_cookies.push(conn.get_property(false, id, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256)?);
        }

        // Phase 2: collect replies in lockstep and build the kept entries.
        let mut type_it = type_cookies.into_iter();
        let mut net_it  = net_name_cookies.into_iter();
        let mut wmn_it  = wm_name_cookies.into_iter();

        // (id, name) for windows that pass the skip filter; icons fill in later.
        let mut kept: Vec<(Window, String)> = Vec::new();

        for &id in &ids {
            let type_c = type_it.next().unwrap();
            let net_c  = net_it.next().unwrap();
            let wmn_c  = wmn_it.next().unwrap();

            // Skip panels, docks, desktop windows, etc.
            let types: Vec<u32> = type_c.reply().ok()
                .filter(|r| r.format == 32)
                .and_then(|r| r.value32().map(|it| it.collect()))
                .unwrap_or_default();
            if xh::is_skip_window_type(&types, &atoms) {
                continue;
            }

            // Name: prefer _NET_WM_NAME, fall back to WM_NAME. (Unread cookies are
            // dropped, which discards their replies — no extra round-trips.)
            let name = net_c.reply().ok()
                .filter(|r| !r.value.is_empty())
                .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                .or_else(|| wmn_c.reply().ok()
                    .map(|r| String::from_utf8_lossy(&r.value).into_owned()))
                .unwrap_or_default();

            kept.push((id, name));
        }

        // Resolve WM frames only for kept windows, and only in thumbnail mode
        // (the frame is unused for icon tiles). Batched to keep it cheap.
        let kept_ids: Vec<Window> = kept.iter().map(|(id, _)| *id).collect();
        let frames = if want_thumbs {
            find_frames_batched(conn, &kept_ids, root)
        } else {
            kept_ids
        };

        for ((id, name), frame) in kept.into_iter().zip(frames) {
            self.windows.push(WindowEntry { id, name, icon: None, frame });
        }
        Ok(())
    }

    /// Fetch one window's icon synchronously: prefer `_NET_WM_ICON`, fall back to
    /// the XDG icon theme via `WM_CLASS`. Returns None if neither yields an icon.
    /// Called off the critical path by the progressive enrich pump.
    fn fetch_icon(&self, id: Window) -> Option<(u32, u32, Vec<u32>)> {
        let icon_size = self.config.tile.icon_size;
        // _NET_WM_ICON (raw ARGB, all sizes — we pick the best match).
        let from_prop = self.conn
            .get_property(false, id, self.atoms.net_wm_icon, AtomEnum::CARDINAL, 0, u32::MAX / 4)
            .ok()
            .and_then(|c| c.reply().ok())
            .filter(|r| r.format == 32 && !r.value.is_empty())
            .and_then(|r| r.value32().map(|it| it.collect::<Vec<u32>>()))
            .and_then(|data| xh::parse_net_wm_icon(&data, icon_size));
        if from_prop.is_some() {
            return from_prop;
        }
        // Fall back to the icon theme keyed on WM_CLASS.
        self.conn
            .get_property(false, id, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| xh::parse_wm_class(&r.value))
            .and_then(|cls| load_icon_file(&cls, icon_size))
    }

    /// Show the popup, starting at the second window (index 1, or 0 if only one).
    pub fn show(&mut self, root: Window, backward: bool) -> Result<(), Box<dyn Error>> {
        if self.popup.is_some() {
            return Ok(());
        }
        let t0 = if self.debug { Some(std::time::Instant::now()) } else { None };
        // Reload config on every popup open so edits take effect without restarting.
        if let Ok(fresh) = Config::load() {
            self.config = fresh;
        }
        self.load_windows(root)?;
        if let Some(t) = t0 {
            eprintln!("[hop] load_windows(): {} windows in {:?}", self.windows.len(), t.elapsed());
        }
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
        // Paint immediately with just backgrounds, borders, and labels. Icons and
        // thumbnails are not loaded yet — they stream in via pump_one_enrich().
        self.redraw()?;

        // Queue every window for enrichment. The selected window goes first so its
        // icon/thumbnail appears soonest, then the rest in order.
        self.enrich_queue = (0..self.windows.len()).collect();
        if self.selected != 0 {
            if let Some(pos) = self.enrich_queue.iter().position(|&i| i == self.selected) {
                let sel = self.enrich_queue.remove(pos).unwrap();
                self.enrich_queue.push_front(sel);
            }
        }

        Ok(())
    }

    /// Get the monitor that currently contains the mouse pointer.
    fn current_monitor(&self) -> xh::MonitorGeom {
        let monitors = xh::query_monitors(self.conn, self.screen_w, self.screen_h);
        let (px, py) = xh::pointer_position(self.conn, self.root);
        xh::monitor_at(&monitors, px, py)
    }

    fn tile_w(&self) -> u32 { self.config.tile.width }
    fn tile_h(&self) -> u32 { self.config.tile.height }
    fn frame_w(&self) -> u32 { self.config.tile.border_width.max(1) }
    fn gap_w(&self) -> u32 { self.config.window.gap }
    fn tile_pad(&self) -> u32 { self.config.tile.padding }
    fn win_pad(&self) -> u32 { self.config.window.padding }
    fn border_radius(&self) -> u32 { self.config.tile.border_radius }

    /// Compute how many columns (and resulting rows) fit without the popup
    /// getting within SCREEN_MARGIN pixels of the monitor edges.
    fn grid_layout(&self) -> (usize, usize) {
        let n = self.windows.len();
        if n == 0 { return (1, 0); }
        let tw  = self.tile_w();
        let fw  = self.frame_w();
        let gap = self.gap_w();
        let wp  = self.win_pad();
        let mon = self.current_monitor();
        // Each tile slot is (tw + fw + gap) wide; the window also needs fw + 2*wp overhead.
        // Solving: n_cols*(tw+fw+gap) ≤ available - fw - 2*wp + gap
        let available = (mon.w as u32).saturating_sub(2 * SCREEN_MARGIN);
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
        let (x, y) = match self.config.window.position.split_once(',') {
            Some((xs, ys)) => (
                xs.trim().parse::<i16>().unwrap_or(0),
                ys.trim().parse::<i16>().unwrap_or(0),
            ),
            None => {
                let mon = self.current_monitor();
                (
                    (mon.x as u32 + (mon.w as u32).saturating_sub(w) / 2) as i16,
                    (mon.y as u32 + (mon.h as u32).saturating_sub(h) / 2) as i16,
                )
            },
        };
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

        // Query render formats once and cache the (large) reply; it never changes
        // for the connection, so subsequent redraws skip the round-trip.
        if self.cached_formats.is_none() {
            self.cached_formats = Some(std::rc::Rc::new(self.conn.render_query_pict_formats()?.reply()?));
        }
        let formats = self.cached_formats.clone().unwrap();
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
        let pix_buf_fresh = !matches!(self.pix_buf,
            Some((_, _, cpw, cph)) if cpw == pw && cph == ph);
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
            )?;
        } else {
            // Clear to transparent, then composite gradient on top.
            self.conn.render_fill_rectangles(
                PictOp::SRC, pix_pic,
                RenderColor { red: 0, green: 0, blue: 0, alpha: 0 },
                &[Rectangle { x: 0, y: 0, width: pw, height: ph }],
            )?;
            self.draw_bg_gradient(pix_pic, pw, ph, window_bg_argb, gradient_mode)?;
        }

        let fw = self.frame_w() as u16;
        let fw32 = self.frame_w();
        let (n_cols, _) = self.grid_layout();
        let use_rounded = br > 0 && a8_fmt != 0;
        let ctx = PictCtx { pic: pix_pic, argb_fmt: fmt, a8_fmt, drawable: pixmap };

        for (i, entry) in self.windows.iter().enumerate() {
            let (tile_x, tile_y) = self.tile_pos(i, n_cols);
            let tile = TileGeom { x: tile_x, y: tile_y, w: tw, h: th };
            let border_argb = if i == self.selected { frame_argb } else { inact_argb };

            if use_rounded {
                // `border_radius` is the outer corner radius (CSS semantics).
                // Draw bg and border into non-overlapping areas so neither bleeds
                // into the other's region and corner pixels are handled correctly.
                let outer_r = br.min((tw + 2*fw32) / 2).min((th + 2*fw32) / 2);
                let inner_r = br.saturating_sub(fw32).min(tw / 2).min(th / 2);
                // Tile background: inner rounded rect, composited OVER window bg.
                draw_filled_rounded_rect(
                    self.conn, ctx,
                    tile_x, tile_y, tw, th, inner_r, bg_argb,
                )?;
                // Border ring: outer shape minus inner shape, so it only covers
                // the fw-wide ring and never overwrites the bg or icon area.
                draw_border_ring(
                    self.conn, ctx,
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
                )?;

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
                )?;
            }

            // Icon or thumbnail
            if self.config.tile.content == "thumbnail" {
                let drew_thumb = self.draw_thumb(ctx, tile, entry, fg_argb)?;
                // Only overlay the small corner icon on real thumbnails — on icon
                // fallbacks the big icon already identifies the window, so the
                // overlay would just be the same icon drawn twice.
                if drew_thumb && self.config.tile.icon_overlay && a8_fmt != 0 {
                    self.draw_icon_overlay(ctx, tile, entry)?;
                }
            } else {
                self.draw_icon(ctx, tile, entry, fg_argb)?;
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
                self.conn, ctx,
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
            )?;
        }

        // Flush x11rb so all XRender work is committed to the pixmap before
        // Xft draws on the same drawable via the separate Xlib connection.
        self.conn.flush()?;

        // Draw Xft text labels onto the off-screen pixmap.
        for (i, entry) in self.windows.iter().enumerate() {
            let (tile_x, tile_y) = self.tile_pos(i, n_cols);
            let tile = TileGeom { x: tile_x, y: tile_y, w: tw, h: th };
            self.draw_label(pixmap, entry, tile, fg_argb)?;
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
        self.conn.render_create_picture(win_pic, win, fmt, &CreatePictureAux::new())?;

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
        )?;

        self.conn.render_free_picture(win_pic)?;
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
            self.conn.render_fill_rectangles(PictOp::SRC, pix_pic, win_bg, &border_rects)?;

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
                    ])?;
                }

                draw_border_ring(
                    self.conn,
                    PictCtx { pic: pix_pic, argb_fmt, a8_fmt, drawable: pixmap },
                    tile_x - fw as i16, tile_y - fw as i16,
                    tw + 2*fw32, th + 2*fw32,
                    outer_r, fw32, inner_r, border_argb,
                )?;
            } else {
                // Flat border: fill the strips directly with the border color.
                let (cr, cg, cb, ca) = argb_to_render_color(border_argb);
                self.conn.render_fill_rectangles(PictOp::OVER, pix_pic,
                    RenderColor { red: cr, green: cg, blue: cb, alpha: ca },
                    &border_rects)?;
            }
        }

        self.blit_to_window()
    }

    fn draw_icon(
        &self,
        ctx: PictCtx,
        tile: TileGeom,
        entry: &WindowEntry,
        fg_argb: u32,
    ) -> Result<(), Box<dyn Error>> {
        let icon_size = self.config.tile.icon_size;
        let pad = self.tile_pad();
        // Center icon horizontally within the padded inner region
        let avail_w = tile.w.saturating_sub(2 * pad);
        let icon_x = tile.x + (pad as i16) + (avail_w.saturating_sub(icon_size) / 2) as i16;
        // Center icon vertically in the non-label area, respecting top/bottom padding
        let avail_icon_h = tile.h.saturating_sub(LABEL_H + 2 * pad);
        let icon_y = tile.y + (pad as i16) + (avail_icon_h.saturating_sub(icon_size) / 2) as i16;

        let (src_w, src_h, pixels) = match &entry.icon {
            Some(icon) => icon,
            None => {
                // No icon data — draw a dim placeholder rectangle
                let (fr, fg_c, fb, fa) = argb_to_render_color(fg_argb);
                self.conn.render_fill_rectangles(
                    PictOp::OVER,
                    ctx.pic,
                    RenderColor { red: fr, green: fg_c, blue: fb, alpha: fa },
                    &[Rectangle {
                        x: icon_x, y: icon_y,
                        width: icon_size as u16, height: icon_size as u16,
                    }],
                )?;
                return Ok(());
            }
        };
        let (src_w, src_h) = (*src_w, *src_h);

        // Upload pixels into a 32-bit pixmap
        let pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(32, pixmap, ctx.drawable, src_w as u16, src_h as u16)?;

        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0).background(0))?;

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
        )?;
        self.conn.free_gc(gc)?;

        // Create an XRender Picture for the pixmap
        let icon_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(icon_pic, pixmap, ctx.argb_fmt, &CreatePictureAux::new())?;
        self.conn.free_pixmap(pixmap)?;

        // Scale to icon_size if the source dimensions differ
        if src_w != icon_size || src_h != icon_size {
            let sx = (src_w as i64 * 65536 / icon_size as i64) as i32;
            let sy = (src_h as i64 * 65536 / icon_size as i64) as i32;
            self.conn.render_set_picture_transform(icon_pic, Transform {
                matrix11: sx, matrix12: 0, matrix13: 0,
                matrix21: 0, matrix22: sy, matrix23: 0,
                matrix31: 0, matrix32: 0, matrix33: 65536,
            })?;
            self.conn.render_set_picture_filter(icon_pic, b"bilinear", &[])?;
        }

        // Composite icon OVER the tile
        self.conn.render_composite(
            PictOp::OVER,
            icon_pic,
            0u32,        // mask = None
            ctx.pic,
            0, 0,        // src_x, src_y
            0, 0,        // mask_x, mask_y
            icon_x, icon_y,
            icon_size as u16, icon_size as u16,
        )?;

        self.conn.render_free_picture(icon_pic)?;
        Ok(())
    }

    /// Draw a small app icon in the bottom-right corner of the tile content area,
    /// semi-transparent, as an overlay on top of a thumbnail. Silently does nothing
    /// if the entry has no icon data or the overlay is disabled.
    fn draw_icon_overlay(
        &self,
        ctx: PictCtx,
        tile: TileGeom,
        entry: &WindowEntry,
    ) -> Result<(), Box<dyn Error>> {
        let (src_w, src_h, pixels) = match &entry.icon {
            Some(icon) => icon,
            None => return Ok(()),
        };
        let (src_w, src_h) = (*src_w, *src_h);
        if src_w == 0 || src_h == 0 { return Ok(()); }

        let ov_size = self.config.tile.icon_overlay_size.max(8);
        let pad = self.tile_pad();
        let avail_w = tile.w.saturating_sub(2 * pad);
        let avail_h = tile.h.saturating_sub(LABEL_H + 2 * pad);
        if avail_w < ov_size || avail_h < ov_size { return Ok(()); }

        // Bottom-right corner of the content area, with a small inset margin.
        let margin = 6i16;
        let ov_x = tile.x + pad as i16 + avail_w as i16 - ov_size as i16 - margin;
        let ov_y = tile.y + pad as i16 + avail_h as i16 - ov_size as i16 - margin;

        // Upload icon pixels into a temporary pixmap.
        let pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(32, pixmap, ctx.drawable, src_w as u16, src_h as u16)?;
        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0).background(0))?;
        let bytes: Vec<u8> = pixels.iter().flat_map(|&p| p.to_ne_bytes()).collect();
        self.conn.put_image(ImageFormat::Z_PIXMAP, pixmap, gc,
            src_w as u16, src_h as u16, 0, 0, 0, 32, &bytes)?;
        self.conn.free_gc(gc)?;

        let icon_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(icon_pic, pixmap, ctx.argb_fmt, &CreatePictureAux::new())?;
        self.conn.free_pixmap(pixmap)?;

        // Scale to ov_size if needed.
        if src_w != ov_size || src_h != ov_size {
            let sx = (src_w as i64 * 65536 / ov_size as i64) as i32;
            let sy = (src_h as i64 * 65536 / ov_size as i64) as i32;
            self.conn.render_set_picture_transform(icon_pic, Transform {
                matrix11: sx, matrix12: 0, matrix13: 0,
                matrix21: 0, matrix22: sy, matrix23: 0,
                matrix31: 0, matrix32: 0, matrix33: 65536,
            })?;
            self.conn.render_set_picture_filter(icon_pic, b"bilinear", &[])?;
        }

        // Build a 1×1 A8 alpha-mask picture with repeat, giving 80% opacity.
        let alpha_pix = self.conn.generate_id()?;
        self.conn.create_pixmap(8, alpha_pix, ctx.drawable, 1, 1)?;
        let alpha_pic = self.conn.generate_id()?;
        self.conn.render_create_picture(
            alpha_pic, alpha_pix, ctx.a8_fmt,
            &CreatePictureAux::new().repeat(Repeat::NORMAL),
        )?;
        self.conn.free_pixmap(alpha_pix)?;
        // 80% opacity: 204/255 * 65535 ≈ 52428 in 16-bit XRender alpha space.
        self.conn.render_fill_rectangles(PictOp::SRC, alpha_pic,
            RenderColor { red: 0, green: 0, blue: 0, alpha: 52428 },
            &[Rectangle { x: 0, y: 0, width: 1, height: 1 }],
        )?;

        // Composite icon over the thumbnail with the alpha mask.
        self.conn.render_composite(
            PictOp::OVER,
            icon_pic, alpha_pic, ctx.pic,
            0, 0, 0, 0,
            ov_x, ov_y,
            ov_size as u16, ov_size as u16,
        )?;

        self.conn.render_free_picture(alpha_pic)?;
        self.conn.render_free_picture(icon_pic)?;
        Ok(())
    }

    /// Draw a window's thumbnail from the pixel cache into the tile content area.
    ///
    /// This reads ONLY the cache (downscaled ARGB pixels captured by cache_thumb).
    /// The live capture — composite_name_window_pixmap + GetImage of the full-size
    /// backing pixmap — is expensive and runs off the critical path in the enrich
    /// pump, not here. Until a window's thumbnail has been captured, its icon is
    /// drawn as a placeholder (and replaced once the thumbnail streams in).
    ///
    /// Returns `true` if a thumbnail was drawn, `false` if it fell back to the icon
    /// (so the caller can skip the redundant corner icon-overlay on icon-only tiles).
    fn draw_thumb(
        &self,
        ctx: PictCtx,
        tile: TileGeom,
        entry: &WindowEntry,
        fg_argb: u32,
    ) -> Result<bool, Box<dyn Error>> {
        let pad = self.tile_pad();
        let avail_w = tile.w.saturating_sub(2 * pad);
        let avail_h = tile.h.saturating_sub(LABEL_H + 2 * pad);

        if avail_w == 0 || avail_h == 0 {
            self.draw_icon(ctx, tile, entry, fg_argb)?;
            return Ok(false);
        }

        if let Some((cw, ch, cpixels)) = self.thumb_cache.get(&entry.frame) {
            self.draw_pixels_scaled(ctx, cpixels, *cw, *ch,
                tile.x, tile.y, avail_w, avail_h, pad)?;
            return Ok(true);
        }

        // Not captured yet — show the icon as a placeholder until the enrich pump
        // caches the thumbnail and triggers a redraw.
        self.draw_icon(ctx, tile, entry, fg_argb)?;
        Ok(false)
    }

    /// Render the window title at the bottom of a tile.
    /// Uses Xft (antialiased, Fontconfig names) when available, bitmap fonts as fallback.
    fn draw_label(
        &self,
        drawable: Window,
        entry: &WindowEntry,
        tile: TileGeom,
        fg_argb: u32,
    ) -> Result<(), Box<dyn Error>> {
        let title = truncate_title(&entry.name, TITLE_MAX_CHARS);
        if title.is_empty() {
            return Ok(());
        }

        if let Some(ref xft) = self.xft {
            self.draw_label_xft(xft, drawable, &title, tile, fg_argb)
        } else {
            self.draw_label_bitmap(drawable, &title, tile, fg_argb)
        }
    }

    fn draw_label_xft(
        &self,
        xst: &xh::XftState,
        win: Window,
        title: &str,
        tile: TileGeom,
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
        let inner_w = tile.w.saturating_sub(2 * h_pad);
        let lines = wrap_text_xft(xst.display, font, title, inner_w);

        let line_h = unsafe { (*font).height.max(1) } as u32;
        let ascent = unsafe { (*font).ascent.max(0) } as i16;

        // Label area starts LABEL_H + v_pad px from the tile bottom
        let label_top = tile.y + (tile.h.saturating_sub(LABEL_H + v_pad)) as i16;

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
            let x = tile.x + (h_pad as i16) + (inner_w.saturating_sub(text_w) / 2) as i16;
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
        tile: TileGeom,
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
        let inner_w = tile.w.saturating_sub(2 * h_pad);
        let label_x = tile.x + (h_pad as i16) + (inner_w.saturating_sub(text_w) / 2) as i16;
        let label_y = tile.y + tile.h as i16 - 4 - v_pad as i16;

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
        self.enrich_queue.clear();
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
                )?;
            }
            "horizontal" => {
                // Left = transparent, right = background color.
                let p1 = Pointfix { x: 0, y: 0 };
                let p2 = Pointfix { x: pw as i32 * 65536, y: 0 };
                self.conn.render_create_linear_gradient(
                    grad_pic, p1, p2, stops, &[color_trans, color_bg],
                )?;
            }
            _ => {
                // "vertical" (default): transparent top → opaque bottom.
                let p1 = Pointfix { x: 0, y: 0 };
                let p2 = Pointfix { x: 0, y: ph as i32 * 65536 };
                self.conn.render_create_linear_gradient(
                    grad_pic, p1, p2, stops, &[color_trans, color_bg],
                )?;
            }
        }

        self.conn.render_composite(
            PictOp::OVER,
            grad_pic, 0u32, pix_pic,
            0, 0,  // src x, y
            0, 0,  // mask x, y
            0, 0,  // dst x, y
            pw, ph,
        )?;

        self.conn.render_free_picture(grad_pic)?;
        Ok(())
    }

    /// True while windows are still waiting to be enriched (icon + thumbnail).
    /// The event loop uses this to decide between blocking and non-blocking polling.
    pub fn has_pending_enrich(&self) -> bool {
        !self.enrich_queue.is_empty()
    }

    /// Enrich one window from the front of the queue: fetch its icon (if not yet
    /// loaded) and capture its thumbnail (in thumbnail mode), then redraw so the
    /// new content appears. Called from the event loop after polling for events,
    /// so the popup paints immediately and fills in one window per iteration.
    pub fn pump_one_enrich(&mut self) -> Result<(), Box<dyn Error>> {
        let idx = match self.enrich_queue.pop_front() {
            Some(i) => i,
            None => return Ok(()),
        };
        if idx >= self.windows.len() {
            return Ok(());
        }

        // Load the icon if we don't have one yet.
        if self.windows[idx].icon.is_none() {
            let id = self.windows[idx].id;
            let icon = self.fetch_icon(id);
            self.windows[idx].icon = icon;
        }

        // Capture the thumbnail (no-op outside thumbnail mode).
        let frame = self.windows[idx].frame;
        self.cache_thumb(frame);

        // Redraw so the freshly loaded icon/thumbnail appears.
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
        // Evict stale entries before inserting to keep the cache bounded.
        if self.thumb_cache.len() >= MAX_THUMB_CACHE {
            let live: std::collections::HashSet<Window> =
                self.windows.iter().map(|e| e.frame).collect();
            self.thumb_cache.retain(|k, _| live.contains(k));
            // If the cache is still at capacity (all entries are live windows),
            // remove one arbitrary entry to make room.
            if self.thumb_cache.len() >= MAX_THUMB_CACHE {
                if let Some(k) = self.thumb_cache.keys().next().copied() {
                    self.thumb_cache.remove(&k);
                }
            }
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
    // Wide positional signature (geometry + pixels); folding into a Rect struct is
    // tracked in TODO.md alongside the other drawing helpers.
    #[allow(clippy::too_many_arguments)]
    fn draw_pixels_scaled(
        &self,
        ctx: PictCtx,
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
        self.conn.create_pixmap(32, pixmap, ctx.drawable, src_w as u16, src_h as u16)?;
        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0).background(0))?;
        let bytes: Vec<u8> = pixels.iter().flat_map(|&p| p.to_ne_bytes()).collect();
        self.conn.put_image(
            ImageFormat::Z_PIXMAP, pixmap, gc,
            src_w as u16, src_h as u16, 0, 0, 0, 32, &bytes,
        )?;
        self.conn.free_gc(gc)?;

        let pic = self.conn.generate_id()?;
        self.conn.render_create_picture(pic, pixmap, ctx.argb_fmt, &CreatePictureAux::new())?;
        self.conn.free_pixmap(pixmap)?;

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
        })?;
        self.conn.render_set_picture_filter(pic, b"bilinear", &[])?;

        self.conn.render_composite(
            PictOp::OVER, pic, 0u32, ctx.pic,
            0, 0, 0, 0, dst_x, dst_y, dst_w as u16, dst_h as u16,
        )?;
        self.conn.render_free_picture(pic)?;
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
