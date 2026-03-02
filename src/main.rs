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
    maybe_configure_picom(&config);
    let display = Display::connect()?;

    if display.argb_visual.is_some() {
        eprintln!("hop: 32-bit ARGB visual available, transparency enabled");
    } else {
        eprintln!("hop: no ARGB visual, transparency disabled");
    }

    x11::grab_keys(&display.conn, display.root)?;
    display.conn.flush()?;

    eprintln!("hop: listening for Alt+Tab...");

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

/// If picom is running and blur is disabled, ensure hop is in picom's
/// blur-background-exclude list and reload picom so the change takes effect.
fn maybe_configure_picom(config: &Config) {
    if config.window.blur || config.tile.blur {
        return;
    }
    let Some(pid) = find_process_pid("picom") else { return; };
    let Some(path) = find_picom_config() else {
        eprintln!(
            "hop: picom detected (pid {pid}) but config not found; \
             add \"class_g = 'hop'\" to blur-background-exclude manually"
        );
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else { return; };

    // Already configured — nothing to do.
    if content.contains("class_g = 'hop'") || content.contains("class_g = \"hop\"") {
        return;
    }

    let new_content = patch_picom_blur_exclude(&content);
    match std::fs::write(&path, &new_content) {
        Ok(()) => {
            eprintln!("hop: added hop to picom blur-background-exclude in {}", path.display());
            // Ask picom to reload its config.
            let _ = std::process::Command::new("pkill").args(["-USR1", "picom"]).status();
        }
        Err(e) => eprintln!(
            "hop: couldn't update {}: {e}; \
             add \"class_g = 'hop'\" to blur-background-exclude manually",
            path.display()
        ),
    }
}

/// Find the PID of the first process whose comm name matches `name`.
fn find_process_pid(name: &str) -> Option<u32> {
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let fname = entry.file_name();
        let fname_str = fname.to_string_lossy();
        if !fname_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            if comm.trim() == name {
                if let Ok(pid) = fname_str.parse::<u32>() {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// Search standard locations for picom's config file.
fn find_picom_config() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| format!("{home}/.config"));
    for p in &[
        format!("{xdg}/picom.conf"),
        format!("{xdg}/picom/picom.conf"),
        format!("{home}/.picom.conf"),
    ] {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Insert `"class_g = 'hop'"` into an existing `blur-background-exclude` list,
/// or append a new one if the key is absent.
fn patch_picom_blur_exclude(content: &str) -> String {
    let rule = "\"class_g = 'hop'\"";

    // Try to find an existing blur-background-exclude = [ ... ] block.
    if let Some(kw_pos) = content.find("blur-background-exclude") {
        let after_kw = &content[kw_pos..];
        if let Some(open_rel) = after_kw.find('[') {
            let open_abs = kw_pos + open_rel + 1;
            if let Some(close_rel) = content[open_abs..].find(']') {
                let close_abs = open_abs + close_rel;
                let inner = content[open_abs..close_abs].trim_end();
                let sep = if inner.is_empty() || inner.ends_with(',') {
                    "\n  "
                } else {
                    ",\n  "
                };
                let mut out = content[..close_abs].to_string();
                out.push_str(sep);
                out.push_str(rule);
                out.push_str(&content[close_abs..]);
                return out;
            }
        }
    }

    // No existing key — append a new block.
    let mut out = content.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("\n# Added by hop\nblur-background-exclude = [\n  {rule}\n];\n"));
    out
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
