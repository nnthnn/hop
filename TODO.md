# hop — TODO

Open items only. Completed work (see git history): the switcher submodule split
(`mod.rs` + `icons.rs` / `render_util.rs` / `text_util.rs`), the bounded thumbnail
cache, config-driven keybindings, `window.position`, a cached keyboard mapping and
per-`show()` monitor/grid caching (hot-path round-trip removal), the
`parse_net_wm_icon` overflow fix, consistent screen handling, strict multi-modifier
binding matching, log-and-continue event-loop resilience, removal of the fragile
opt-in picom-config editing (hop now only sets the blur hint; compositor setup is
documented in the README), config validation (`Config::validate()` with
fall-back-to-defaults), RAII guards for short-lived render resources
(`switcher/resource.rs`), folding the render helpers' wide positional geometry
args into a shared `Rect` struct (no more `too_many_arguments` allows), and a
unit-test suite covering the pure color/icon/text/validation logic.

---

## Features

- [ ] **Release packaging**
  Makefile with `install` target (copies binary to `~/.local/bin/hop`, config example to
  `~/.config/hop/`). Optional: AUR PKGBUILD.
