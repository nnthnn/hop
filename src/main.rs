mod config;
mod x11;
mod switcher;

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;

use config::Config;
use x11::Display;
use switcher::{Switcher, NavDir};

// Return/Enter always commits, regardless of binding config.
const XK_RETURN: u32 = 0xff0d;
// Backspace deletes the last character of the type-to-filter query.
const XK_BACKSPACE: u32 = 0xff08;
// Arrow keys for 2D grid navigation.
const XK_LEFT:  u32 = 0xff51;
const XK_UP:    u32 = 0xff52;
const XK_RIGHT: u32 = 0xff53;
const XK_DOWN:  u32 = 0xff54;

fn main() -> Result<(), Box<dyn Error>> {
    let debug = std::env::var("HOP_DEBUG").is_ok();
    let config = Config::load()?;
    let display = Display::connect()?;

    if display.argb_visual.is_some() {
        eprintln!("hop: 32-bit ARGB visual available, transparency enabled");
    } else {
        eprintln!("hop: no ARGB visual, transparency disabled");
    }

    x11::grab_keys(
        &display.conn, display.root,
        &config.keys.modifier, &config.keys.next, &config.keys.prev,
    )?;
    // Same-app cycling keys (e.g. Super+Tab). Skipped when app_next is empty.
    x11::grab_keys(
        &display.conn, display.root,
        &config.keys.app_modifier, &config.keys.app_next, &config.keys.app_prev,
    )?;

    // Select SubstructureNotify on root so we receive MapNotify and DestroyNotify
    // events. These are used to keep the thumbnail cache up to date: MapNotify fires
    // when a window becomes visible (user switches to its desktop), DestroyNotify
    // fires when a window is closed.
    display.conn.change_window_attributes(
        display.root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
    )?.check()?;

    display.conn.flush()?;

    // Pre-compute binding info so we don't re-parse on every keypress.
    let primary_mask   = x11::modifier_mask(&config.keys.modifier);
    let (next_sym, next_extra) = x11::parse_key_binding(&config.keys.next);
    let (prev_sym, prev_extra) = x11::parse_key_binding(&config.keys.prev);
    let (cancel_sym, _)        = x11::parse_key_binding(&config.keys.cancel);
    let (close_sym, _)         = x11::parse_key_binding(&config.keys.close);
    let release_syms           = x11::modifier_release_keysyms(&config.keys.modifier);

    // Same-app cycling bindings (app_next == "" disables app-mode).
    let app_enabled            = !config.keys.app_next.is_empty();
    let app_primary_mask       = x11::modifier_mask(&config.keys.app_modifier);
    let (app_next_sym, app_next_extra) = x11::parse_key_binding(&config.keys.app_next);
    let (app_prev_sym, app_prev_extra) = x11::parse_key_binding(&config.keys.app_prev);
    let app_release_syms       = x11::modifier_release_keysyms(&config.keys.app_modifier);

    eprintln!("hop: listening for {}+{}...", config.keys.modifier, config.keys.next);
    if app_enabled {
        eprintln!("hop: same-app cycling on {}+{}", config.keys.app_modifier, config.keys.app_next);
    }

    // Load the keyboard mapping once; refreshed on MappingNotify. Avoids a
    // blocking get_keyboard_mapping round-trip on every key event.
    let mut keymap = x11::KeyMap::load(&display.conn)?;

    let mut switcher = Switcher::new(&display.conn, config, &display)?;
    let root = display.root;

    // The modifier-release keysyms that commit the *currently open* popup. Set when
    // the popup is shown (main vs app mode use different modifiers), read on KeyRelease.
    let mut opened_release: Vec<u32> = Vec::new();

    loop {
        // While thumbnails are loading, poll non-blocking so we can interleave
        // input handling with one GetImage download per iteration. When idle,
        // block until the next event to avoid busy-waiting.
        let maybe_event = if switcher.has_pending_enrich() {
            display.conn.poll_for_event()?
        } else {
            Some(display.conn.wait_for_event()?)
        };

        // Dispatch each event inside a closure so a transient X error (e.g. a
        // BadWindow race when a window vanishes mid-property-fetch) is logged and
        // the daemon keeps running, rather than propagating out of main() and
        // silently killing Alt+Tab. Connection-level failures from poll/wait above
        // stay fatal — a dead connection should exit.
        if let Some(event) = maybe_event {
        let handled: Result<(), Box<dyn Error>> = (|| { match event {
            Event::KeyPress(ev) => {
                let sym  = keymap.keysym(ev.detail);
                let mods = u32::from(ev.state);
                let primary_active = mods & primary_mask != 0;

                // is_prev: primary active + ALL prev extra mods active. Requiring the
                // full mask (== prev_extra) means a two-modifier binding like
                // Ctrl+Shift+Tab needs both bits, not just one. (prev_extra == 0
                // trivially satisfies the equality.)
                let is_prev = sym == prev_sym && primary_active
                    && (mods & prev_extra) == prev_extra;
                // is_next: primary active, not prev (when same base key), + all next extra mods active
                let is_next = sym == next_sym && primary_active && !is_prev
                    && (mods & next_extra) == next_extra;

                // Same-app cycling (e.g. Super+Tab), independent of the main modifier.
                let app_active = app_enabled && mods & app_primary_mask != 0;
                let app_is_prev = app_active && sym == app_prev_sym
                    && (mods & app_prev_extra) == app_prev_extra;
                let app_is_next = app_active && sym == app_next_sym && !app_is_prev
                    && (mods & app_next_extra) == app_next_extra;

                if is_next {
                    if !switcher.is_visible() {
                        switcher.show(root, false)?;
                        opened_release = release_syms.clone();
                    } else {
                        switcher.next()?;
                    }
                } else if is_prev {
                    if !switcher.is_visible() {
                        switcher.show(root, true)?;
                        opened_release = release_syms.clone();
                    } else {
                        switcher.prev()?;
                    }
                } else if app_is_next {
                    if !switcher.is_visible() {
                        switcher.show_app(root, false)?;
                        opened_release = app_release_syms.clone();
                    } else {
                        switcher.next()?;
                    }
                } else if app_is_prev {
                    if !switcher.is_visible() {
                        switcher.show_app(root, true)?;
                        opened_release = app_release_syms.clone();
                    } else {
                        switcher.prev()?;
                    }
                } else if sym == cancel_sym && switcher.is_visible() {
                    switcher.cancel()?;
                } else if sym == XK_LEFT && switcher.is_visible() {
                    switcher.navigate(NavDir::Left)?;
                } else if sym == XK_RIGHT && switcher.is_visible() {
                    switcher.navigate(NavDir::Right)?;
                } else if sym == XK_UP && switcher.is_visible() {
                    switcher.navigate(NavDir::Up)?;
                } else if sym == XK_DOWN && switcher.is_visible() {
                    switcher.navigate(NavDir::Down)?;
                } else if sym == close_sym && switcher.is_visible() {
                    switcher.close_selected(root)?;
                } else if sym == XK_RETURN && switcher.is_visible() {
                    switcher.commit(root)?;
                } else if sym == XK_BACKSPACE && switcher.is_visible() {
                    switcher.pop_filter()?;
                } else if switcher.is_visible() && switcher.quick_select_enabled()
                    && (0x31..=0x39).contains(&sym) {
                    // Digits 1–9 jump directly to that tile (quick-select). When
                    // quick-select is on, digits are reserved for this and don't filter.
                    switcher.quick_select(root, (sym - 0x31) as usize)?;
                } else if switcher.is_visible() && (0x20..=0x7e).contains(&sym) {
                    // Any other printable key (incl. space) is type-to-filter input.
                    // Navigation/control keys live in the 0xff00 range, so they're
                    // already handled above and never reach here.
                    if let Some(c) = char::from_u32(sym) {
                        switcher.push_filter(c)?;
                    }
                }
            }

            Event::KeyRelease(ev) => {
                let sym = keymap.keysym(ev.detail);
                // Commit when the modifier that opened this popup (main or app mode)
                // is released.
                if opened_release.contains(&sym) && switcher.is_visible() {
                    switcher.commit(root)?;
                }
            }

            // The keyboard layout changed (e.g. setxkbmap); refresh our cached mapping.
            Event::MappingNotify(_) => {
                keymap.reload(&display.conn);
            }

            Event::MotionNotify(ev) => {
                if switcher.is_visible() {
                    switcher.hover_at(ev.event_x, ev.event_y)?;
                }
            }

            Event::Expose(ev) => {
                if switcher.popup_window() == Some(ev.window) {
                    if debug { eprintln!("[hop] Expose count={}", ev.count); }
                    switcher.repaint()?;
                }
            }

            Event::ButtonPress(ev) => {
                if switcher.is_visible() {
                    match ev.detail {
                        2 => switcher.close_at(root, ev.event_x, ev.event_y)?,  // middle-click
                        4 => switcher.prev()?,   // scroll up
                        5 => switcher.next()?,   // scroll down
                        _ => switcher.click_at(root, ev.event_x, ev.event_y)?,
                    }
                }
            }

            // A window was mapped (made visible). This fires when the user switches
            // to a virtual desktop, causing xfwm4 to re-map the frame windows.
            // Update the thumbnail cache so off-desktop thumbnails stay fresh.
            Event::MapNotify(ev) => {
                switcher.cache_thumb(ev.window);
            }

            // A window was destroyed. Remove its cache entry to free memory.
            Event::DestroyNotify(ev) => {
                switcher.on_window_destroyed(ev.window);
            }

            _ => {}
        } Ok(()) })(); // end match event, invoke dispatch closure
            if let Err(e) = handled {
                eprintln!("hop: recovered from error handling event: {e}");
            }
        } // end if let Some(event)

        // Progressive enrichment: load one window's icon + thumbnail per loop
        // iteration. Runs only while the popup is visible and the queue is non-empty.
        // Errors here are non-fatal too — log and keep going.
        if switcher.has_pending_enrich() {
            if let Err(e) = switcher.pump_one_enrich() {
                eprintln!("hop: recovered from enrich error: {e}");
            }
        }
    }
}
