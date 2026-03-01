/// X11 helpers: connection setup, EWMH, key codes, ARGB visual lookup.

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::rust_connection::RustConnection;

pub struct Display {
    pub conn: RustConnection,
    pub screen_num: usize,
    pub root: Window,
    pub screen_width: u16,
    pub screen_height: u16,
    pub argb_visual: Option<Visualid>,
    pub argb_colormap: Option<u32>,
}

impl Display {
    pub fn connect() -> Result<Self, Box<dyn Error>> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let screen = &conn.setup().roots[screen_num].clone();
        let root = screen.root;
        let screen_width = screen.width_in_pixels;
        let screen_height = screen.height_in_pixels;

        let (argb_visual, argb_colormap) = find_argb_visual(&conn, screen)?;

        Ok(Display {
            conn,
            screen_num,
            root,
            screen_width,
            screen_height,
            argb_visual,
            argb_colormap,
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

/// Grab Alt+Tab and Alt+Shift+Tab on the root window.
pub fn grab_keys(conn: &RustConnection, root: Window) -> Result<(), Box<dyn Error>> {
    use x11rb::protocol::xproto::{GrabMode, ModMask};

    // Map Tab and Escape key symbols to keycodes
    let tab_code = keyname_to_keycode(conn, "Tab")?;
    let esc_code = keyname_to_keycode(conn, "Escape")?;

    if let Some(tab) = tab_code {
        // Alt+Tab (forward)
        for extra_mod in offending_modifiers(conn)? {
            conn.grab_key(
                true,
                root,
                ModMask::M1 | extra_mod,
                tab,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .check()?;
            // Alt+Shift+Tab (backward)
            conn.grab_key(
                true,
                root,
                ModMask::M1 | ModMask::SHIFT | extra_mod,
                tab,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .check()?;
        }
    }

    if let Some(esc) = esc_code {
        for extra_mod in offending_modifiers(conn)? {
            conn.grab_key(
                true,
                root,
                ModMask::M1 | extra_mod,
                esc,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .check()?;
        }
    }

    Ok(())
}

/// Return all modifier combinations to grab (handles NumLock, CapsLock, ScrollLock).
fn offending_modifiers(conn: &RustConnection) -> Result<Vec<ModMask>, Box<dyn Error>> {
    // For simplicity, grab with and without common lock modifiers
    let locks = [ModMask::from(0u16), ModMask::LOCK, ModMask::M2, ModMask::LOCK | ModMask::M2];
    Ok(locks.to_vec())
}

/// Look up a keysym name and return a keycode.
fn keyname_to_keycode(conn: &RustConnection, name: &str) -> Result<Option<Keycode>, Box<dyn Error>> {
    use x11rb::protocol::xproto::Keycode;
    let sym = keysym_from_name(name)?;
    if sym == 0 {
        return Ok(None);
    }
    let mapping = conn.get_keyboard_mapping(
        conn.setup().min_keycode,
        conn.setup().max_keycode - conn.setup().min_keycode + 1,
    )?.reply()?;

    let keysyms_per_code = mapping.keysyms_per_keycode as usize;
    for (i, chunk) in mapping.keysyms.chunks(keysyms_per_code).enumerate() {
        if chunk.contains(&sym) {
            let code = conn.setup().min_keycode + i as u8;
            return Ok(Some(code));
        }
    }
    Ok(None)
}

/// Minimal keysym lookup for the keys we care about.
fn keysym_from_name(name: &str) -> Result<u32, Box<dyn Error>> {
    // XF86 keysym values — just what we need
    let sym = match name {
        "Tab"    => 0xff09,
        "Escape" => 0xff1b,
        "Return" => 0xff0d,
        _        => 0,
    };
    Ok(sym)
}

/// Get the list of mapped windows from EWMH _NET_CLIENT_LIST_STACKING.
pub fn get_window_list(
    conn: &RustConnection,
    root: Window,
) -> Result<Vec<Window>, Box<dyn Error>> {
    use x11rb::protocol::xproto::AtomEnum;

    let atom = intern_atom(conn, "_NET_CLIENT_LIST_STACKING")?;
    if atom == 0 {
        // Fall back to _NET_CLIENT_LIST
        let atom2 = intern_atom(conn, "_NET_CLIENT_LIST")?;
        return get_window_list_atom(conn, root, atom2);
    }
    get_window_list_atom(conn, root, atom)
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

/// Get a window's _NET_WM_NAME or WM_NAME.
pub fn get_window_name(conn: &RustConnection, window: Window) -> Result<String, Box<dyn Error>> {
    let net_wm_name = intern_atom(conn, "_NET_WM_NAME")?;
    let utf8_string = intern_atom(conn, "UTF8_STRING")?;

    let reply = conn.get_property(false, window, net_wm_name, utf8_string, 0, 256)?.reply();
    if let Ok(r) = reply {
        if !r.value.is_empty() {
            return Ok(String::from_utf8_lossy(&r.value).into_owned());
        }
    }

    // Fall back to WM_NAME
    let reply = conn.get_property(
        false, window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        0, 256,
    )?.reply()?;
    Ok(String::from_utf8_lossy(&reply.value).into_owned())
}

/// Get _NET_WM_ICON raw ARGB data, picking the best size for `target_size`.
pub fn get_window_icon(
    conn: &RustConnection,
    window: Window,
    target_size: u32,
) -> Result<Option<(u32, u32, Vec<u32>)>, Box<dyn Error>> {
    let atom = intern_atom(conn, "_NET_WM_ICON")?;
    let reply = conn.get_property(false, window, atom, AtomEnum::CARDINAL, 0, u32::MAX / 4)?
        .reply()?;

    if reply.format != 32 || reply.value.is_empty() {
        return Ok(None);
    }

    let data: Vec<u32> = reply.value32().unwrap().collect();
    let mut i = 0;
    let mut best: Option<(u32, u32, usize)> = None; // (w, h, offset)

    while i + 2 <= data.len() {
        let w = data[i];
        let h = data[i + 1];
        i += 2;
        if i + (w * h) as usize > data.len() {
            break;
        }
        let is_better = best.map_or(true, |(bw, bh, _)| {
            let cur_diff = (w as i64 - target_size as i64).unsigned_abs();
            let best_diff = (bw as i64 - target_size as i64).unsigned_abs();
            cur_diff < best_diff || (cur_diff == best_diff && w > bw)
        });
        if is_better {
            best = Some((w, h, i));
        }
        i += (w * h) as usize;
    }

    if let Some((w, h, offset)) = best {
        let pixels = data[offset..offset + (w * h) as usize].to_vec();
        Ok(Some((w, h, pixels)))
    } else {
        Ok(None)
    }
}

/// Hint to the compositor (picom/KWin) to blur behind this window.
pub fn set_blur_hint(conn: &RustConnection, window: Window, radius: u32) -> Result<(), Box<dyn Error>> {
    if radius == 0 {
        return Ok(());
    }
    let atom = intern_atom(conn, "_KDE_NET_WM_BLUR_BEHIND_REGION")?;
    if atom == 0 {
        return Ok(());
    }
    // Empty region = blur the whole window
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atom,
        AtomEnum::CARDINAL,
        &[],
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

/// Set the active window via _NET_ACTIVE_WINDOW.
pub fn activate_window(
    conn: &RustConnection,
    root: Window,
    window: Window,
) -> Result<(), Box<dyn Error>> {
    use x11rb::protocol::xproto::{EventMask, ClientMessageEvent, CLIENT_MESSAGE_EVENT};
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
