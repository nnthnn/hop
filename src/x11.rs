//! X11 helpers: connection setup, EWMH, key codes, ARGB visual lookup.

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::protocol::xinerama::ConnectionExt as XineramaConnectionExt;
use x11rb::rust_connection::RustConnection;

use x11::xlib;

/// Xft state: a parallel Xlib Display + ARGB Visual used for antialiased text rendering.
/// Opened separately from the x11rb connection since Xft requires a raw Xlib Display*.
pub struct XftState {
    pub display: *mut xlib::Display,
    pub visual: *mut xlib::Visual,
    pub colormap: xlib::Colormap,
    pub screen_num: i32,
}

// Raw pointers are not Send by default, but our event loop is single-threaded.
unsafe impl Send for XftState {}

impl XftState {
    /// Try to initialise Xft. Returns None if the ARGB visual is unavailable.
    pub fn open() -> Option<Self> {
        let display = unsafe { xlib::XOpenDisplay(std::ptr::null()) };
        if display.is_null() {
            return None;
        }

        let screen_num = unsafe { xlib::XDefaultScreen(display) };

        // Look for a 32-bit TrueColor visual for ARGB compositing
        let mut vinfo: xlib::XVisualInfo = unsafe { std::mem::zeroed() };
        let found = unsafe {
            xlib::XMatchVisualInfo(display, screen_num, 32, xlib::TrueColor, &mut vinfo)
        };
        if found == 0 {
            unsafe { xlib::XCloseDisplay(display); }
            return None;
        }

        let root = unsafe { xlib::XRootWindow(display, screen_num) };
        let colormap = unsafe {
            xlib::XCreateColormap(display, root, vinfo.visual, xlib::AllocNone)
        };

        Some(XftState { display, visual: vinfo.visual, colormap, screen_num })
    }
}

impl Drop for XftState {
    fn drop(&mut self) {
        unsafe { xlib::XCloseDisplay(self.display); }
    }
}

/// All EWMH/ICCCM atoms hop interns, resolved once at connection time.
///
/// Atoms never change for the lifetime of an X11 connection, so interning them
/// repeatedly (as the old per-call `intern_atom` helpers did) wasted one blocking
/// round-trip per name per window. `load_windows` alone interned ~10 atoms per
/// window; caching them here removes that entire class of round-trips.
#[derive(Clone, Copy)]
pub struct Atoms {
    pub net_client_list_stacking: u32,
    pub net_client_list: u32,
    pub net_wm_window_type: u32,
    pub wt_desktop: u32,
    pub wt_dock: u32,
    pub wt_toolbar: u32,
    pub wt_splash: u32,
    pub wt_notification: u32,
    pub net_wm_name: u32,
    pub utf8_string: u32,
    pub net_wm_icon: u32,
}

impl Atoms {
    /// Intern every atom in one pipelined batch: issue all `InternAtom` requests
    /// first, then collect the replies, so the whole set costs ~1 round-trip.
    pub fn intern(conn: &RustConnection) -> Result<Self, Box<dyn Error>> {
        let names: &[&str] = &[
            "_NET_CLIENT_LIST_STACKING",
            "_NET_CLIENT_LIST",
            "_NET_WM_WINDOW_TYPE",
            "_NET_WM_WINDOW_TYPE_DESKTOP",
            "_NET_WM_WINDOW_TYPE_DOCK",
            "_NET_WM_WINDOW_TYPE_TOOLBAR",
            "_NET_WM_WINDOW_TYPE_SPLASH",
            "_NET_WM_WINDOW_TYPE_NOTIFICATION",
            "_NET_WM_NAME",
            "UTF8_STRING",
            "_NET_WM_ICON",
        ];
        // Phase 1: fire off all requests without waiting.
        let cookies: Vec<_> = names.iter()
            .map(|n| conn.intern_atom(false, n.as_bytes()))
            .collect::<Result<_, _>>()?;
        // Phase 2: collect replies (pipelined by the server).
        let mut a = [0u32; 11];
        for (i, c) in cookies.into_iter().enumerate() {
            a[i] = c.reply()?.atom;
        }
        Ok(Atoms {
            net_client_list_stacking: a[0],
            net_client_list:          a[1],
            net_wm_window_type:       a[2],
            wt_desktop:               a[3],
            wt_dock:                  a[4],
            wt_toolbar:               a[5],
            wt_splash:                a[6],
            wt_notification:          a[7],
            net_wm_name:              a[8],
            utf8_string:              a[9],
            net_wm_icon:              a[10],
        })
    }
}

pub struct Display {
    pub conn: RustConnection,
    pub screen_num: usize,
    pub root: Window,
    pub screen_width: u16,
    pub screen_height: u16,
    pub argb_visual: Option<Visualid>,
    pub argb_colormap: Option<u32>,
    pub atoms: Atoms,
}

impl Display {
    pub fn connect() -> Result<Self, Box<dyn Error>> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let screen = &conn.setup().roots[screen_num].clone();
        let root = screen.root;
        let screen_width = screen.width_in_pixels;
        let screen_height = screen.height_in_pixels;

        let (argb_visual, argb_colormap) = find_argb_visual(&conn, screen)?;
        let atoms = Atoms::intern(&conn)?;

        Ok(Display {
            conn,
            screen_num,
            root,
            screen_width,
            screen_height,
            argb_visual,
            argb_colormap,
            atoms,
        })
    }

    pub fn screen(&self) -> &x11rb::protocol::xproto::Screen {
        &self.conn.setup().roots[self.screen_num]
    }
}

/// Find a 32-bit TrueColor (ARGB) visual on the given screen.
/// Returns (visual_id, colormap) if found.
fn find_argb_visual(
    conn: &RustConnection,
    screen: &x11rb::protocol::xproto::Screen,
) -> Result<(Option<Visualid>, Option<u32>), Box<dyn Error>> {
    for depth in &screen.allowed_depths {
        if depth.depth != 32 {
            continue;
        }
        for visual in &depth.visuals {
            if visual.class == VisualClass::TRUE_COLOR {
                // Create a colormap for this visual
                let cmap = conn.generate_id()?;
                conn.create_colormap(
                    ColormapAlloc::NONE,
                    cmap,
                    screen.root,
                    visual.visual_id,
                )?
                .check()?;
                return Ok((Some(visual.visual_id), Some(cmap)));
            }
        }
    }
    Ok((None, None))
}

/// Parse a modifier name ("Alt", "Super", "Ctrl", "Shift") into its mask value (as u32).
pub fn modifier_mask(s: &str) -> u32 {
    use x11rb::protocol::xproto::ModMask;
    match s.trim() {
        "Super" | "Win" | "Mod4" => u32::from(ModMask::M4),
        "Ctrl"  | "Control"      => u32::from(ModMask::CONTROL),
        "Shift"                  => u32::from(ModMask::SHIFT),
        _                        => u32::from(ModMask::M1),  // "Alt" and anything else
    }
}

/// The keysyms for both sides of the given modifier key,
/// used to detect modifier release in KeyRelease events.
pub fn modifier_release_keysyms(modifier: &str) -> Vec<u32> {
    match modifier.trim() {
        "Super" | "Win"      => vec![0xffeb, 0xffec],  // Super_L, Super_R
        "Ctrl"  | "Control"  => vec![0xffe3, 0xffe4],  // Control_L, Control_R
        "Shift"              => vec![0xffe1, 0xffe2],  // Shift_L, Shift_R
        _                    => vec![0xffe9, 0xffea],  // Alt_L, Alt_R
    }
}

/// Parse a key binding string like "Tab", "Shift+Tab", "grave".
/// Returns `(keysym, extra_modifier_mask)` where the mask is a u32 suitable
/// for bitwise AND against `u32::from(ev.state)`.
pub fn parse_key_binding(s: &str) -> (u32, u32) {
    let parts: Vec<&str> = s.split('+').collect();
    let key_name = parts.last().copied().unwrap_or(s);
    let mut extra: u32 = 0;
    for mod_str in &parts[..parts.len().saturating_sub(1)] {
        extra |= modifier_mask(mod_str);
    }
    (keysym_from_name(key_name), extra)
}

/// Grab the configured trigger keys on the root window.
/// Grabs `modifier+next_key` and `modifier+prev_key` (with all lock-key combos).
pub fn grab_keys(
    conn: &RustConnection,
    root: Window,
    modifier: &str,
    next_key: &str,
    prev_key: &str,
) -> Result<(), Box<dyn Error>> {
    use x11rb::protocol::xproto::{GrabMode, ModMask};

    let primary     = ModMask::from(modifier_mask(modifier) as u16);
    let (next_sym, next_extra_u32) = parse_key_binding(next_key);
    let (prev_sym, prev_extra_u32) = parse_key_binding(prev_key);
    let next_extra  = ModMask::from(next_extra_u32 as u16);
    let prev_extra  = ModMask::from(prev_extra_u32 as u16);

    if let Some(next_code) = keysym_to_keycode(conn, next_sym)? {
        for lock in offending_modifiers() {
            conn.grab_key(true, root, primary | next_extra | lock,
                next_code, GrabMode::ASYNC, GrabMode::ASYNC)?.check()?;
        }
    }

    // Grab prev separately only when it differs from next in keycode or modifiers.
    if prev_sym != next_sym || prev_extra_u32 != next_extra_u32 {
        if let Some(prev_code) = keysym_to_keycode(conn, prev_sym)? {
            for lock in offending_modifiers() {
                conn.grab_key(true, root, primary | prev_extra | lock,
                    prev_code, GrabMode::ASYNC, GrabMode::ASYNC)?.check()?;
            }
        }
    }

    Ok(())
}

/// Return all modifier combinations to grab (handles NumLock, CapsLock, ScrollLock).
fn offending_modifiers() -> Vec<ModMask> {
    // For simplicity, grab with and without common lock modifiers
    [ModMask::from(0u16), ModMask::LOCK, ModMask::M2, ModMask::LOCK | ModMask::M2].to_vec()
}

/// Map a key name to its X11 keysym. Returns 0 for unrecognised names.
fn keysym_from_name(name: &str) -> u32 {
    match name {
        "Tab"               => 0xff09,
        "Escape" | "Esc"    => 0xff1b,
        "Return" | "Enter"  => 0xff0d,
        "space"  | "Space"  => 0x0020,
        "grave"  | "quoteleft" => 0x0060,  // backtick / tilde key
        "F1"  => 0xffbe, "F2"  => 0xffbf, "F3"  => 0xffc0, "F4"  => 0xffc1,
        "F5"  => 0xffc2, "F6"  => 0xffc3, "F7"  => 0xffc4, "F8"  => 0xffc5,
        "F9"  => 0xffc6, "F10" => 0xffc7, "F11" => 0xffc8, "F12" => 0xffc9,
        // Single ASCII character — covers letters, digits, punctuation
        s if s.len() == 1   => s.chars().next().map_or(0, |c| c as u32),
        _                   => 0,
    }
}

/// Look up the keycode for a given keysym by scanning the keyboard mapping.
fn keysym_to_keycode(conn: &RustConnection, keysym: u32) -> Result<Option<Keycode>, Box<dyn Error>> {
    if keysym == 0 { return Ok(None); }
    let mapping = conn.get_keyboard_mapping(
        conn.setup().min_keycode,
        conn.setup().max_keycode - conn.setup().min_keycode + 1,
    )?.reply()?;
    let kpk = mapping.keysyms_per_keycode as usize;
    for (i, chunk) in mapping.keysyms.chunks(kpk).enumerate() {
        if chunk.contains(&keysym) {
            return Ok(Some(conn.setup().min_keycode + i as u8));
        }
    }
    Ok(None)
}

/// Get the list of mapped windows from EWMH _NET_CLIENT_LIST_STACKING.
pub fn get_window_list(
    conn: &RustConnection,
    root: Window,
    atoms: &Atoms,
) -> Result<Vec<Window>, Box<dyn Error>> {
    if atoms.net_client_list_stacking == 0 {
        // Fall back to _NET_CLIENT_LIST
        return get_window_list_atom(conn, root, atoms.net_client_list);
    }
    get_window_list_atom(conn, root, atoms.net_client_list_stacking)
}

fn get_window_list_atom(
    conn: &RustConnection,
    root: Window,
    atom: u32,
) -> Result<Vec<Window>, Box<dyn Error>> {
    let reply = conn.get_property(
        false,
        root,
        atom,
        AtomEnum::WINDOW,
        0,
        u32::MAX / 4,
    )?.reply()?;

    if reply.format != 32 {
        return Ok(vec![]);
    }

    let windows: Vec<Window> = reply.value32()
        .map(|iter| iter.collect())
        .unwrap_or_default();

    // Reverse so most recently focused is first
    Ok(windows.into_iter().rev().collect())
}

/// Pick the best-matching icon from raw `_NET_WM_ICON` CARDINAL data for
/// `target_size`. The property is a sequence of `[w, h, w*h ARGB pixels]` blocks.
pub fn parse_net_wm_icon(data: &[u32], target_size: u32) -> Option<(u32, u32, Vec<u32>)> {
    let mut i = 0;
    let mut best: Option<(u32, u32, usize)> = None; // (w, h, offset)

    while i + 2 <= data.len() {
        let w = data[i];
        let h = data[i + 1];
        i += 2;
        if i + (w * h) as usize > data.len() {
            break;
        }
        let is_better = best.is_none_or(|(bw, _, _)| {
            let cur_diff = (w as i64 - target_size as i64).unsigned_abs();
            let best_diff = (bw as i64 - target_size as i64).unsigned_abs();
            cur_diff < best_diff || (cur_diff == best_diff && w > bw)
        });
        if is_better {
            best = Some((w, h, i));
        }
        i += (w * h) as usize;
    }

    best.map(|(w, h, offset)| {
        let pixels = data[offset..offset + (w * h) as usize].to_vec();
        (w, h, pixels)
    })
}

/// Hint to the compositor (picom/KWin) to blur behind specific regions of this window.
///
/// Pass an empty slice to blur the entire window area.
/// Pass a non-empty slice of `(x, y, w, h)` rectangles to blur only those regions.
pub fn set_blur_hint(
    conn: &RustConnection,
    window: Window,
    rects: &[(i16, i16, u16, u16)],
) -> Result<(), Box<dyn Error>> {
    let atom = intern_atom(conn, "_KDE_NET_WM_BLUR_BEHIND_REGION")?;
    if atom == 0 {
        return Ok(());
    }
    let data: Vec<u32> = if rects.is_empty() {
        // Empty property = blur the whole window
        vec![]
    } else {
        rects.iter()
            .flat_map(|&(x, y, w, h)| [x as u32, y as u32, w as u32, h as u32])
            .collect()
    };
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atom,
        AtomEnum::CARDINAL,
        &data,
    )?.check()?;
    Ok(())
}

/// Helper: intern an atom by name.
pub fn intern_atom(conn: &RustConnection, name: &str) -> Result<u32, Box<dyn Error>> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Set _NET_WM_WINDOW_TYPE to _NET_WM_WINDOW_TYPE_DIALOG.
pub fn set_window_type_dialog(conn: &RustConnection, window: Window) -> Result<(), Box<dyn Error>> {
    let wt = intern_atom(conn, "_NET_WM_WINDOW_TYPE")?;
    let td = intern_atom(conn, "_NET_WM_WINDOW_TYPE_DIALOG")?;
    conn.change_property32(PropMode::REPLACE, window, wt, AtomEnum::ATOM, &[td])?.check()?;
    Ok(())
}

/// Skip the window in taskbar.
pub fn set_skip_taskbar(conn: &RustConnection, window: Window) -> Result<(), Box<dyn Error>> {
    let st = intern_atom(conn, "_NET_WM_STATE")?;
    let sk = intern_atom(conn, "_NET_WM_STATE_SKIP_TASKBAR")?;
    if st != 0 && sk != 0 {
        conn.change_property32(PropMode::REPLACE, window, st, AtomEnum::ATOM, &[sk])?.check()?;
    }
    Ok(())
}

/// Return true if any window type in `window_types` marks a window hop should
/// skip: panels, docks, the desktop, splashes, and notification windows.
/// Operates on already-fetched `_NET_WM_WINDOW_TYPE` atoms (see `load_windows`).
pub fn is_skip_window_type(window_types: &[u32], atoms: &Atoms) -> bool {
    window_types.iter().any(|&t| {
        t == atoms.wt_desktop || t == atoms.wt_dock || t == atoms.wt_toolbar
            || t == atoms.wt_splash || t == atoms.wt_notification
    })
}

/// Parse the class component out of raw `WM_CLASS` property bytes.
/// `WM_CLASS` is `"instance\0class\0"`; we want the class (second part).
pub fn parse_wm_class(value: &[u8]) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let parts: Vec<&[u8]> = value.splitn(3, |&b| b == 0).collect();
    let class_bytes = if parts.len() >= 2 && !parts[1].is_empty() {
        parts[1]
    } else {
        parts[0]
    };
    if class_bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(class_bytes).into_owned())
}

/// Switch to the desktop that `window` lives on, then raise and focus it.
/// If the window is sticky (desktop == 0xFFFF_FFFF) we skip the desktop switch.
pub fn activate_window(
    conn: &RustConnection,
    root: Window,
    window: Window,
) -> Result<(), Box<dyn Error>> {
    use x11rb::protocol::xproto::{EventMask, ClientMessageEvent, CLIENT_MESSAGE_EVENT};

    // Switch workspace if the window is on a different one.
    if let Some(target_desk) = get_window_desktop(conn, window) {
        if target_desk != 0xFFFF_FFFF {
            let current_desk = get_current_desktop(conn, root).unwrap_or(target_desk);
            if current_desk != target_desk {
                switch_to_desktop(conn, root, target_desk)?;
            }
        }
    }

    let naw = intern_atom(conn, "_NET_ACTIVE_WINDOW")?;
    let event = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window,
        type_: naw,
        data: [2, x11rb::CURRENT_TIME, 0, 0, 0].into(),
    };
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?.check()?;
    conn.flush()?;
    Ok(())
}

/// Return the `_NET_WM_DESKTOP` index for `window`, or `None` if unset.
fn get_window_desktop(conn: &RustConnection, window: Window) -> Option<u32> {
    let atom = intern_atom(conn, "_NET_WM_DESKTOP").ok()?;
    if atom == 0 {
        return None;
    }
    let Ok(cookie) = conn.get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1) else {
        return None;
    };
    let Ok(reply) = cookie.reply() else { return None; };
    if reply.format != 32 {
        return None;
    }
    let vals: Vec<u32> = reply.value32()?.collect();
    vals.into_iter().next()
}

/// Return the index of the currently-visible desktop (`_NET_CURRENT_DESKTOP`).
fn get_current_desktop(conn: &RustConnection, root: Window) -> Option<u32> {
    let atom = intern_atom(conn, "_NET_CURRENT_DESKTOP").ok()?;
    if atom == 0 {
        return None;
    }
    let Ok(cookie) = conn.get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1) else {
        return None;
    };
    let Ok(reply) = cookie.reply() else { return None; };
    if reply.format != 32 {
        return None;
    }
    let vals: Vec<u32> = reply.value32()?.collect();
    vals.into_iter().next()
}

/// Ask the window manager to switch to `desktop` via `_NET_CURRENT_DESKTOP`.
fn switch_to_desktop(conn: &RustConnection, root: Window, desktop: u32) -> Result<(), Box<dyn Error>> {
    use x11rb::protocol::xproto::{EventMask, ClientMessageEvent, CLIENT_MESSAGE_EVENT};
    let ncd = intern_atom(conn, "_NET_CURRENT_DESKTOP")?;
    let event = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: root,
        type_: ncd,
        data: [desktop, x11rb::CURRENT_TIME, 0, 0, 0].into(),
    };
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?.check()?;
    conn.flush()?;
    Ok(())
}

/// Monitor geometry rectangle.
#[derive(Debug, Clone, Copy)]
pub struct MonitorGeom {
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
}

/// Query per-monitor geometry via Xinerama.
/// Falls back to the root screen dimensions if Xinerama is unavailable.
pub fn query_monitors(conn: &RustConnection, screen_w: u16, screen_h: u16) -> Vec<MonitorGeom> {
    let active = conn.xinerama_is_active()
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.state != 0)
        .unwrap_or(false);

    if active {
        let Ok(cookie) = conn.xinerama_query_screens() else {
            return vec![MonitorGeom { x: 0, y: 0, w: screen_w, h: screen_h }];
        };
        let Ok(reply) = cookie.reply() else {
            return vec![MonitorGeom { x: 0, y: 0, w: screen_w, h: screen_h }];
        };
        let monitors: Vec<MonitorGeom> = reply.screen_info.iter().map(|s| MonitorGeom {
            x: s.x_org,
            y: s.y_org,
            w: s.width,
            h: s.height,
        }).collect();
        if !monitors.is_empty() {
            return monitors;
        }
    }

    vec![MonitorGeom { x: 0, y: 0, w: screen_w, h: screen_h }]
}

/// Get the current pointer position relative to the root window.
pub fn pointer_position(conn: &RustConnection, root: Window) -> (i16, i16) {
    conn.query_pointer(root)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| (r.root_x, r.root_y))
        .unwrap_or((0, 0))
}

/// Find which monitor contains the given point.
pub fn monitor_at(monitors: &[MonitorGeom], x: i16, y: i16) -> MonitorGeom {
    monitors.iter()
        .find(|m| {
            x >= m.x && x < m.x + m.w as i16
                && y >= m.y && y < m.y + m.h as i16
        })
        .copied()
        .unwrap_or(monitors[0])
}
