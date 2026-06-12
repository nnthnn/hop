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

// Return/Enter always commits, regardless of binding config.
const XK_RETURN: u32 = 0xff0d;

fn main() -> Result<(), Box<dyn Error>> {
    let debug = std::env::var("HOP_DEBUG").is_ok();
    let config = Config::load()?;
    maybe_configure_picom(&config);
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
    let release_syms           = x11::modifier_release_keysyms(&config.keys.modifier);

    eprintln!("hop: listening for {}+{}...", config.keys.modifier, config.keys.next);

    // Load the keyboard mapping once; refreshed on MappingNotify. Avoids a
    // blocking get_keyboard_mapping round-trip on every key event.
    let mut keymap = x11::KeyMap::load(&display.conn)?;

    let mut switcher = Switcher::new(&display.conn, config, &display)?;
    let root = display.root;

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

                if is_next {
                    if !switcher.is_visible() {
                        switcher.show(root, false)?;
                    } else {
                        switcher.next()?;
                    }
                } else if is_prev {
                    if !switcher.is_visible() {
                        switcher.show(root, true)?;
                    } else {
                        switcher.prev()?;
                    }
                } else if sym == cancel_sym && switcher.is_visible() {
                    switcher.cancel()?;
                } else if sym == XK_RETURN && switcher.is_visible() {
                    switcher.commit(root)?;
                }
            }

            Event::KeyRelease(ev) => {
                let sym = keymap.keysym(ev.detail);
                if release_syms.contains(&sym) && switcher.is_visible() {
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

/// Sync picom's config with hop's settings for blur, shadow, and rounded corners.
fn maybe_configure_picom(config: &Config) {
    // Opt-out: when configure_picom = false, never touch the user's picom.conf.
    if !config.window.configure_picom {
        return;
    }

    let want_blur    = config.window.blur || config.tile.blur;
    let want_shadow  = config.window.shadow;
    let want_corners = config.window.corners;

    let Some(_pid) = find_process_pid("picom") else { return; };
    let Some(path) = find_picom_config() else {
        let need_exclude = !want_blur || !want_shadow || !want_corners;
        if need_exclude {
            eprintln!(
                "hop: picom detected but config not found; \
                 add \"class_g = 'hop'\" to the relevant exclude lists manually"
            );
        }
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else { return; };

    let mut new_content = content.clone();
    let mut changed = false;

    // Sync each exclude list independently.
    for (want, list_key, label) in [
        (want_blur,    "blur-background-exclude",  "blur-background-exclude"),
        (want_shadow,  "shadow-exclude",            "shadow-exclude"),
        (want_corners, "rounded-corners-exclude",   "rounded-corners-exclude"),
    ] {
        let excluded = is_in_picom_exclude(&new_content, list_key);
        if want && excluded {
            new_content = remove_picom_exclude(&new_content, list_key);
            changed = true;
            eprintln!("hop: removed hop from picom {label} in {}", path.display());
        } else if !want && !excluded {
            new_content = patch_picom_exclude(&new_content, list_key);
            changed = true;
            eprintln!("hop: added hop to picom {label} in {}", path.display());
        }
    }

    // If blur is wanted, also ensure picom has blur-background = true.
    if want_blur {
        if let Some(patched) = ensure_picom_blur_on(
            &new_content,
            &config.window.blur_method,
            config.window.blur_strength,
        ) {
            new_content = patched;
            changed = true;
            eprintln!(
                "hop: enabled blur-background = true in {} (method={}, strength={})",
                path.display(), config.window.blur_method, config.window.blur_strength,
            );
        }
    }

    if changed {
        match std::fs::write(&path, &new_content) {
            Ok(()) => {
                let _ = std::process::Command::new("pkill").args(["-USR1", "picom"]).status();
            }
            Err(e) => eprintln!("hop: couldn't update {}: {e}", path.display()),
        }
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

/// Find the byte offset of a real `list_key = [ ... ]` assignment in `content`.
///
/// Skips commented-out lines and substring matches inside comments, and guards
/// against prefix collisions (e.g. `blur-background` must not match
/// `blur-background-exclude`) by requiring the next non-space char to be `=`.
/// Returns the offset of the key itself.
fn find_list_assignment(content: &str, list_key: &str) -> Option<usize> {
    let mut line_start = 0;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            if let Some(rest) = trimmed.strip_prefix(list_key) {
                if rest.trim_start().starts_with('=') {
                    let indent = line.len() - trimmed.len();
                    return Some(line_start + indent);
                }
            }
        }
        line_start += line.len() + 1; // +1 for '\n'
    }
    None
}

/// Check whether hop's rule is currently in a named picom exclude list.
fn is_in_picom_exclude(content: &str, list_key: &str) -> bool {
    // Check for a block we appended.
    if content.contains(&format!("\n# Added by hop\n{list_key} = [")) {
        return true;
    }
    // Check inside an existing user-written list (real assignment only, not a comment).
    if let Some(kw_pos) = find_list_assignment(content, list_key) {
        let after = &content[kw_pos..];
        if let Some(open_rel) = after.find('[') {
            let open_abs = kw_pos + open_rel + 1;
            if let Some(close_rel) = content[open_abs..].find(']') {
                let inner = &content[open_abs..open_abs + close_rel];
                return inner.contains("class_g = 'hop'")
                    || inner.contains("class_g = \"hop\"");
            }
        }
    }
    false
}

/// Insert `"class_g = 'hop'"` into a named picom exclude list,
/// or append a new list block if the key is absent.
fn patch_picom_exclude(content: &str, list_key: &str) -> String {
    let rule = "\"class_g = 'hop'\"";

    if let Some(kw_pos) = find_list_assignment(content, list_key) {
        let after = &content[kw_pos..];
        if let Some(open_rel) = after.find('[') {
            let open_abs = kw_pos + open_rel + 1;
            if let Some(close_rel) = content[open_abs..].find(']') {
                let close_abs = open_abs + close_rel;
                let inner = content[open_abs..close_abs].trim_end();
                let sep = if inner.is_empty() || inner.ends_with(',') { "\n  " } else { ",\n  " };
                let mut out = content[..close_abs].to_string();
                out.push_str(sep);
                out.push_str(rule);
                out.push_str(&content[close_abs..]);
                return out;
            }
        }
    }

    let mut out = content.to_string();
    if !out.ends_with('\n') { out.push('\n'); }
    out.push_str(&format!("\n# Added by hop\n{list_key} = [\n  {rule}\n];\n"));
    out
}

/// Remove `"class_g = 'hop'"` from a named picom exclude list.
/// Removes the entire `# Added by hop` block if hop wrote it, otherwise
/// strips just the rule line from a user-written list.
fn remove_picom_exclude(content: &str, list_key: &str) -> String {
    // First try: remove the entire "# Added by hop\n{list_key} = [...];\n" block.
    let block_prefix = format!("\n# Added by hop\n{list_key} = [");
    if let Some(bs) = content.find(&block_prefix) {
        let from = bs + block_prefix.len() - 1; // rewind to the '['
        if let Some(rel) = content[from..].find("];\n") {
            let be = from + rel + 3;
            let mut out = content[..bs].to_string();
            out.push_str(&content[be..]);
            return out;
        }
    }

    // Second try: remove just the rule from a user-written list.
    let sq = "\"class_g = 'hop'\"";
    let dq = "\"class_g = \\\"hop\\\"\"";
    let patterns = [
        format!(",\n  {sq}"), format!("{sq},\n  "), format!("\n  {sq}"), sq.to_string(),
        format!(",\n  {dq}"), format!("{dq},\n  "), format!("\n  {dq}"), dq.to_string(),
    ];
    for pat in &patterns {
        if let Some(pos) = content.find(pat.as_str()) {
            let mut out = content[..pos].to_string();
            out.push_str(&content[pos + pat.len()..]);
            return out;
        }
    }

    content.to_string()
}

/// Sync picom's blur settings to match hop's config.
/// Sets blur-background = true, blur-method, and blur-strength — updating existing values
/// or appending them if absent. Returns `Some(new_content)` if anything changed.
fn ensure_picom_blur_on(content: &str, method: &str, strength: u32) -> Option<String> {
    let mut result = content.to_string();
    let mut changed = false;

    changed |= set_picom_setting(&mut result, "blur-background", "true");
    changed |= set_picom_setting(&mut result, "blur-method", &format!("\"{method}\""));
    changed |= set_picom_setting(&mut result, "blur-strength", &strength.to_string());

    if changed { Some(result) } else { None }
}

/// Set `key = value;` in a picom config string.
/// Finds and replaces an existing non-commented assignment for the key,
/// or appends the setting if not found. Returns true if the content changed.
/// Safely handles keys that share a prefix (e.g. "blur-background" vs "blur-background-exclude")
/// by requiring the character after the key to be `=` or whitespace.
fn set_picom_setting(content: &mut String, key: &str, value: &str) -> bool {
    let desired = format!("{key} = {value};");

    // Find the byte range of the line that currently sets this key.
    let replace_range = {
        let mut line_start = 0;
        let mut found = None;
        for line in content.lines() {
            let line_end = line_start + line.len();
            let trimmed = line.trim();
            if !trimmed.starts_with('#') {
                if let Some(rest) = trimmed.strip_prefix(key) {
                    if rest.trim_start().starts_with('=') {
                        // Accept with or without trailing semicolon as "already correct".
                        if trimmed == desired || trimmed == format!("{key} = {value}") {
                            return false;
                        }
                        found = Some(line_start..line_end);
                        break;
                    }
                }
            }
            line_start = line_end + 1; // +1 for '\n'
        }
        found
    };

    if let Some(range) = replace_range {
        content.replace_range(range, &desired);
    } else {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&desired);
        content.push('\n');
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_patch_then_detect_then_remove_roundtrip() {
        let key = "blur-background-exclude";
        let patched = patch_picom_exclude("", key);
        assert!(is_in_picom_exclude(&patched, key));
        assert!(patched.contains("class_g = 'hop'"));

        let removed = remove_picom_exclude(&patched, key);
        assert!(!is_in_picom_exclude(&removed, key));
    }

    #[test]
    fn exclude_patch_into_existing_list_preserves_other_rules() {
        let existing = "shadow-exclude = [\n  \"class_g = 'foo'\"\n];\n";
        let patched = patch_picom_exclude(existing, "shadow-exclude");
        assert!(patched.contains("class_g = 'hop'"));
        assert!(patched.contains("class_g = 'foo'")); // not clobbered
        assert!(is_in_picom_exclude(&patched, "shadow-exclude"));
    }

    #[test]
    fn set_setting_appends_when_absent_and_is_idempotent() {
        let mut c = String::new();
        assert!(set_picom_setting(&mut c, "blur-method", "\"dual_kawase\""));
        assert!(c.contains("blur-method = \"dual_kawase\";"));
        // Second call with the same value reports no change.
        assert!(!set_picom_setting(&mut c, "blur-method", "\"dual_kawase\""));
    }

    #[test]
    fn set_setting_replaces_existing_value() {
        let mut c = String::from("blur-strength = 3;\n");
        assert!(set_picom_setting(&mut c, "blur-strength", "5"));
        assert!(c.contains("blur-strength = 5;"));
        assert!(!c.contains("= 3;"));
    }

    #[test]
    fn set_setting_does_not_match_prefix_collision() {
        // "blur-background" must not match the "blur-background-exclude" line.
        let mut c = String::from("blur-background-exclude = [];\n");
        assert!(set_picom_setting(&mut c, "blur-background", "true"));
        assert!(c.contains("blur-background = true;"));
        assert!(c.contains("blur-background-exclude = [];")); // untouched
    }

    #[test]
    fn exclude_ignores_commented_out_list() {
        // A commented exclude line must not count as a real assignment.
        let commented = "# blur-background-exclude = [ \"class_g = 'hop'\" ];\n";
        assert!(!is_in_picom_exclude(commented, "blur-background-exclude"));
        // Patching appends a fresh block rather than editing the comment.
        let patched = patch_picom_exclude(commented, "blur-background-exclude");
        assert!(patched.contains("# Added by hop"));
        assert!(patched.starts_with(commented)); // original comment left intact
        assert!(is_in_picom_exclude(&patched, "blur-background-exclude"));
    }

    #[test]
    fn find_list_assignment_rejects_prefix_collision() {
        // "blur-background" must not match the "-exclude" assignment.
        let content = "blur-background-exclude = [];\n";
        assert_eq!(find_list_assignment(content, "blur-background"), None);
        assert!(find_list_assignment(content, "blur-background-exclude").is_some());
    }

    #[test]
    fn ensure_blur_on_sets_all_three_then_noop() {
        let out = ensure_picom_blur_on("", "dual_kawase", 5).expect("should change empty config");
        assert!(out.contains("blur-background = true;"));
        assert!(out.contains("blur-method = \"dual_kawase\";"));
        assert!(out.contains("blur-strength = 5;"));
        // Already-correct config → no change.
        assert!(ensure_picom_blur_on(&out, "dual_kawase", 5).is_none());
    }
}

