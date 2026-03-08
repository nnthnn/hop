# hop — TODO

Tracked items from CLAUDE.md, the code review, and direct source inspection.
Ordered by priority within each category.

---

## Bugs

- [x] **`wrapping_div` in bitmap label centering** (`switcher.rs:1186`)
  `inner_w.saturating_sub(text_w).wrapping_div(2)` — `wrapping_div` is pointless after
  `saturating_sub` (result is already ≥ 0). The Xft path on line ~1124 uses plain `/`.
  Inconsistency suggests a copy-paste error. Fix: use regular `/` for both.

---

## Code Quality / Cleanup

- [x] **`Config::default()` uses `.unwrap()`** (`config.rs:190-203`)
  All five `Default` impls do `toml::from_str("").unwrap()`. Won't panic today, but is
  fragile. At minimum change to `.expect("empty TOML must parse")` to get a useful
  message if it ever breaks. Better: construct the structs directly without TOML.

- [x] **Unused `conn` parameter in `offending_modifiers`** (`x11.rs:195`)
  The function signature takes `conn: &RustConnection` but never uses it. Remove the
  parameter and update the two call sites.

- [x] **Unused variable `bh`** (`x11.rs:323`)
  `best.map_or(true, |(bw, bh, _)| { ... })` — `bh` is bound but never read. Rename to
  `_bh` or restructure the pattern to `(bw, _, _)`.

- [x] **Dead field `WindowEntry::visual`** (`switcher.rs:30`)
  Populated in `load_windows()` but never read anywhere. Remove the field and its
  population code (`switcher.rs:140-143`).

- [x] **Unused import `AtomEnum` in `get_window_list`** (`x11.rs:239`)
  `use x11rb::protocol::xproto::AtomEnum;` inside the function body — `AtomEnum` is used
  in `get_window_list_atom` but the scoped import is redundant since `xproto::*` is
  already in scope at the module level.

- [x] **Manual `div_ceil` arithmetic** (`switcher.rs:219, 238`)
  `(n + n_cols - 1) / n_cols` should be `n.div_ceil(n_cols)`. Same pattern appears at
  least twice.

- [x] **`truncate_title` O(n) char count** (`switcher.rs:1649` approx)
  `s.chars().count() <= max_chars` iterates the entire string to count characters. Use
  `char_indices()` to stop early once `max_chars` characters are consumed.

---

## Features (from CLAUDE.md)

- [ ] **Config-driven `window.position`** (`config.rs:21`, `switcher.rs:popup_dims`)
  `window.position` is parsed from config ("center" or "x,y") but `popup_dims()` always
  centers. Wire the "x,y" branch to use the literal coordinates.

- [ ] **Config-driven keybindings fully working**
  `keys.modifier`, `keys.next`, `keys.prev`, `keys.cancel` are now parsed and used in
  `grab_keys()` and the main event loop — verify all combinations work correctly including
  `cancel` key. Confirm `cancel_sym` logic in `main.rs:49` is correct (currently ignores
  `_` from `parse_key_binding`).

- [ ] **Release packaging**
  Makefile with `install` target (copies binary to `~/.local/bin/hop`, config example to
  `~/.config/hop/`). Optional: AUR PKGBUILD.

---

## Architecture

- [ ] **Unbounded thumbnail cache** (`switcher.rs` — `thumb_cache`)
  `HashMap<Window, (u32, u32, Vec<u32>)>` with no eviction policy. Fine for normal use
  but grows indefinitely if windows open/close frequently over a long session. Add LRU
  eviction or cap at N entries.

- [ ] **Functions with too many parameters** (`switcher.rs`)
  Several functions exceed 10 parameters. Introduce a `TileCtx` (or similar) struct to
  bundle positional/dimensional arguments passed repeatedly to drawing functions:
  - `draw_border_ring()` — 13 params
  - `draw_pixels_scaled()` — 12 params
  - `draw_thumb()` — 11 params
  - `composite_color_through_mask()` — 11 params
  - `draw_icon()` / `draw_icon_overlay()` — 10 params each

- [ ] **`switcher.rs` is 2000+ lines**
  Consider splitting into submodules:
  - `switcher/layout.rs` — `grid_layout()`, `tile_pos()`, `popup_dims()`
  - `switcher/render.rs` — XRender drawing functions
  - `switcher/icons.rs` — icon loading, `load_icon_file()`, `load_png_file()`
  - `switcher/text.rs` — Xft + bitmap label rendering

---

## Nice-to-Have

- [ ] **Config validation**
  Add `Config::validate()` that checks sanity of numeric fields (e.g. `icon_size > 0`,
  `border_width` not absurdly large) and returns `Err` with a helpful message.

- [ ] **RAII wrappers for X11 resources**
  In error paths inside `draw_icon()` and similar functions, a pixmap or GC could be
  leaked if an early return fires before the matching `free_*` call. Low risk (X11 cleans
  up on disconnect) but a small RAII guard would make it bulletproof.
