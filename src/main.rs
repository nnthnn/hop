mod config;
mod x11;
mod switcher;

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;

use config::Config;
use x11::Display;
use switcher::Switcher;

// Keysym values
const XK_TAB: u32    = 0xff09;
const XK_ESCAPE: u32 = 0xff1b;
const XK_RETURN: u32 = 0xff0d;

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::load()?;
    let display = Display::connect()?;

    if display.argb_visual.is_some() {
        eprintln!("xwitch: 32-bit ARGB visual available, transparency enabled");
    } else {
        eprintln!("xwitch: no ARGB visual, transparency disabled");
    }

    x11::grab_keys(&display.conn, display.root)?;
    display.conn.flush()?;

    eprintln!("xwitch: listening for Alt+Tab...");

    let mut switcher = Switcher::new(&display.conn, &config, &display)?;
    let root = display.root;

    loop {
        let event = display.conn.wait_for_event()?;

        match event {
            Event::KeyPress(ev) => {
                let sym = keycode_to_keysym(&display.conn, ev.detail, ev.state)?;
                let mods = u32::from(ev.state);
                let alt   = mods & u32::from(ModMask::M1)  != 0;
                let shift = mods & u32::from(ModMask::SHIFT) != 0;

                if sym == XK_TAB && alt {
                    if !switcher.is_visible() {
                        switcher.show(root, shift)?;
                    } else if shift {
                        switcher.prev()?;
                    } else {
                        switcher.next()?;
                    }
                } else if sym == XK_ESCAPE && switcher.is_visible() {
                    switcher.cancel()?;
                } else if sym == XK_RETURN && switcher.is_visible() {
                    switcher.commit(root)?;
                }
            }

            Event::KeyRelease(ev) => {
                let sym = keycode_to_keysym(&display.conn, ev.detail, ev.state)?;
                // Alt_L = 0xffe9, Alt_R = 0xffea
                if (sym == 0xffe9 || sym == 0xffea) && switcher.is_visible() {
                    switcher.commit(root)?;
                }
            }

            Event::Expose(ev) => {
                if switcher.popup_window() == Some(ev.window) {
                    switcher.redraw()?;
                }
            }

            Event::ButtonPress(_) => {
                if switcher.is_visible() {
                    switcher.commit(root)?;
                }
            }

            _ => {}
        }
    }
}

/// Translate a keycode + modifier state into a keysym.
fn keycode_to_keysym(
    conn: &x11rb::rust_connection::RustConnection,
    keycode: u8,
    state: KeyButMask,
) -> Result<u32, Box<dyn Error>> {
    let mapping = conn.get_keyboard_mapping(keycode, 1)?.reply()?;
    let kpk = mapping.keysyms_per_keycode as usize;
    if mapping.keysyms.is_empty() {
        return Ok(0);
    }
    let shift = u32::from(state) & u32::from(ModMask::SHIFT) != 0;
    let col = usize::from(shift && kpk > 1);
    Ok(mapping.keysyms[col])
}
