# hop — TODO

Open items only. Completed work (see git history): the switcher submodule split
(`mod.rs` + `icons.rs` / `render_util.rs` / `text_util.rs`), the bounded thumbnail
cache, config-driven keybindings, `window.position`, a cached keyboard mapping and
per-`show()` monitor/grid caching (hot-path round-trip removal), the
`parse_net_wm_icon` overflow fix, consistent screen handling, strict multi-modifier
binding matching, log-and-continue event-loop resilience, hardened picom-config
matching (comment/prefix-collision aware), config validation (`Config::validate()`
with fall-back-to-defaults), RAII guards for short-lived render resources
(`switcher/resource.rs`), and a unit-test suite covering the pure
color/icon/text/picom/validation logic.

---

## Features

- [ ] **Release packaging**
  Makefile with `install` target (copies binary to `~/.local/bin/hop`, config example to
  `~/.config/hop/`). Optional: AUR PKGBUILD.

---

## Nice-to-Have

- [ ] **Reduce remaining wide signatures**
  `draw_pixels_scaled`, `composite_color_through_mask`, and `draw_border_ring` still take
  ~8–10 positional args. The worst offenders were fixed by `TileGeom`/`PictCtx`; these
  could fold their geometry args into a small struct too.
