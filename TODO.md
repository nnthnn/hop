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
args into a shared `Rect` struct (no more `too_many_arguments` allows), a
unit-test suite covering the pure color/icon/text/validation logic, and the
type-to-filter / quick-select / close-window feature set, 2D arrow-key grid
navigation, same-app window cycling (Super+Tab), and non-fatal key grabs.

---

## Features

### Backlog

- [ ] **App exclude list** — `[filter] exclude_classes = ["..."]` to skip specific apps by
  `WM_CLASS`, layered onto the existing window-type skip filter.
- [ ] **Urgent-window highlight** — draw tiles with `_NET_WM_STATE_DEMANDS_ATTENTION` in a
  distinct border color.
- [ ] **Filter scope toggle** — current-desktop-only / current-monitor-only modes (config
  default + a runtime toggle key).
- [ ] **True MRU ordering** — maintain a most-recently-used stack from focus changes
  instead of relying on `_NET_CLIENT_LIST_STACKING`.
- [ ] **App grouping** — two-level switching (apps, then windows within an app).

- [ ] **Release packaging**
  Makefile with `install` target (copies binary to `~/.local/bin/hop`, config example to
  `~/.config/hop/`). Optional: AUR PKGBUILD.
