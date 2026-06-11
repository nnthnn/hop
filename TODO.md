# hop — TODO

Open items only. Completed bugs, cleanups, and the switcher submodule split
(`mod.rs` + `icons.rs` / `render_util.rs` / `text_util.rs`), the bounded
thumbnail cache, config-driven keybindings, and `window.position` are all done —
see git history.

---

## Features

- [ ] **Release packaging**
  Makefile with `install` target (copies binary to `~/.local/bin/hop`, config example to
  `~/.config/hop/`). Optional: AUR PKGBUILD.

---

## Nice-to-Have

- [ ] **Config validation**
  Add `Config::validate()` that checks sanity of numeric fields (e.g. `icon_size > 0`,
  `border_width` not absurdly large) and returns `Err` with a helpful message.

- [ ] **RAII wrappers for X11 resources**
  In error paths inside `draw_icon()` and similar functions, a pixmap or GC could be
  leaked if an early return fires before the matching `free_*` call. Low risk (X11 cleans
  up on disconnect) but a small RAII guard would make it bulletproof.

- [ ] **Reduce remaining wide signatures**
  `draw_pixels_scaled`, `composite_color_through_mask`, and `draw_border_ring` still take
  ~8–10 positional args. The worst offenders were fixed by `TileGeom`/`PictCtx`; these
  could fold their geometry args into a small struct too.
