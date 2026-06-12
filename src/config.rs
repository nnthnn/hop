use serde::Deserialize;
use std::error::Error;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub tile: TileConfig,
    #[serde(default)]
    pub font: FontConfig,
    #[serde(default)]
    pub keys: KeysConfig,
}

/// Settings that apply to the popup window as a whole.
#[derive(Debug, Deserialize)]
pub struct WindowConfig {
    /// Where to place the popup: "center" or "x,y".
    #[serde(default = "default_position")]
    pub position: String,
    /// Width of the X11 border drawn around the outside of the popup (px). 0 = none.
    #[serde(default)]
    pub outer_border_width: u32,
    /// Color of the outer X11 border. Accepts "#rrggbb" (opaque) or "#rrggbbaa".
    #[serde(default = "default_border")]
    pub border: String,
    /// Background of the popup (the area tiles sit on: frame borders, gaps).
    /// Use "#rrggbbaa" to control transparency, e.g. "#282a36ff" = fully opaque.
    /// Default is "#00000000" (fully transparent — compositor shows through).
    #[serde(default = "default_window_bg")]
    pub background: String,
    /// Gradient direction for the window background.
    /// "none" = flat fill, "radial" = opaque center / transparent edges,
    /// "vertical" = transparent top → opaque bottom,
    /// "horizontal" = transparent left → opaque right.
    #[serde(default = "default_background_gradient")]
    pub background_gradient: String,
    /// Apply compositor blur behind the entire popup window.
    #[serde(default = "default_true")]
    pub blur: bool,
    /// picom blur-method to configure if hop enables blur in picom's config.
    /// Supported values: dual_kawase, gaussian, box, kernel.
    #[serde(default = "default_blur_method")]
    pub blur_method: String,
    /// picom blur-strength to configure if hop enables blur in picom's config.
    /// For dual_kawase the useful range is 1–20; higher = more blur.
    #[serde(default = "default_blur_strength")]
    pub blur_strength: u32,
    /// Whether hop may edit the user's picom config to match its blur/shadow/corner
    /// settings (and reload picom). Opt-in: defaults to false so hop never touches
    /// picom.conf unless explicitly enabled. When false, you manage your own picom
    /// exclude lists and blur-background setting.
    #[serde(default)]
    pub configure_picom: bool,
    /// Extra pixels of space between adjacent tiles (beyond the tile border).
    #[serde(default)]
    pub gap: u32,
    /// Padding around the outside of the tile area (px).
    /// Extends the window background on all four sides beyond the tiles.
    #[serde(default)]
    pub padding: u32,
    /// Alignment of tiles in the last row when it is not full.
    /// Accepted values: "left", "center", "right". Default: "center".
    #[serde(default = "default_last_row_position")]
    pub last_row_position: String,
    /// Show picom drop shadow on the popup. When false, hop adds itself to
    /// picom's shadow-exclude list. Default: false (no shadow).
    #[serde(default)]
    pub shadow: bool,
    /// Show picom rounded corners on the popup. When false, hop adds itself to
    /// picom's rounded-corners-exclude list. Default: false (no rounded corners).
    #[serde(default)]
    pub corners: bool,
    /// Corner radius for the popup window background (px). 0 = square corners.
    /// Clips the XRender framebuffer to a rounded shape so compositor transparency
    /// shows through at the corners.
    #[serde(default)]
    pub border_radius: u32,
}

/// Settings that apply to each individual tile.
#[derive(Debug, Deserialize)]
pub struct TileConfig {
    #[serde(default = "default_tile_width")]
    pub width: u32,
    #[serde(default = "default_tile_height")]
    pub height: u32,
    #[serde(default = "default_icon_size")]
    pub icon_size: u32,
    /// Thickness of the border drawn around each tile (px).
    #[serde(default = "default_border_width")]
    pub border_width: u32,
    /// Padding inside each tile between the tile edge and its content (icon + label).
    #[serde(default)]
    pub padding: u32,
    /// Tile background color. Use "#rrggbbaa" to control transparency.
    #[serde(default = "default_bg")]
    pub background: String,
    /// Text and icon placeholder color.
    #[serde(default = "default_fg")]
    pub foreground: String,
    /// Border color for the selected tile.
    #[serde(default = "default_frame")]
    pub frame: String,
    /// Border color for unselected tiles. Use "#rrggbbaa" for alpha, e.g. "#44475a00" = invisible.
    #[serde(default = "default_inactive")]
    pub inactive: String,
    /// Apply compositor blur only behind each tile region (not the gaps between tiles).
    /// Set window.blur = false and tile.blur = true for tile-only blur.
    #[serde(default)]
    pub blur: bool,
    /// Corner radius for tile borders (px). 0 = square corners.
    #[serde(default)]
    pub border_radius: u32,
    /// What to show inside each tile: "icon" (app icon) or "thumbnail" (window screenshot).
    /// Thumbnails require a compositor (picom) to be running.
    #[serde(default = "default_tile_content")]
    pub content: String,
    /// When content = "thumbnail", also draw a small app icon in the bottom-right
    /// corner of the thumbnail. Helps identify windows when thumbnails look similar.
    #[serde(default = "default_icon_overlay")]
    pub icon_overlay: bool,
    /// Size of the corner icon overlay in pixels (only used when icon_overlay = true).
    #[serde(default = "default_icon_overlay_size")]
    pub icon_overlay_size: u32,
}

#[derive(Debug, Deserialize)]
pub struct FontConfig {
    #[serde(default = "default_font_name")]
    pub name: String,
    #[serde(default = "default_font_size")]
    pub size: u32,
    /// Draw a drop shadow behind tile labels. Helps readability on light tile backgrounds.
    #[serde(default)]
    pub shadow: bool,
    /// Shadow color. Only used when shadow = true.
    #[serde(default = "default_text_shadow_color")]
    pub shadow_color: String,
    /// Shadow offset in pixels. Only used when shadow = true.
    #[serde(default = "default_text_shadow_offset")]
    pub shadow_offset: u32,
}

#[derive(Debug, Deserialize)]
pub struct KeysConfig {
    #[serde(default = "default_modifier")]
    pub modifier: String,
    #[serde(default = "default_next_key")]
    pub next: String,
    #[serde(default = "default_prev_key")]
    pub prev: String,
    #[serde(default = "default_cancel_key")]
    pub cancel: String,
}

// -- defaults --

fn default_true() -> bool { true }

fn default_position() -> String { "center".into() }
fn default_last_row_position() -> String { "center".into() }
fn default_window_bg() -> String { "#00000000".into() }  // fully transparent
fn default_border() -> String { "#6272a4ff".into() }

fn default_tile_width() -> u32 { 200 }
fn default_tile_height() -> u32 { 150 }
fn default_icon_size() -> u32 { 64 }
fn default_border_width() -> u32 { 4 }
fn default_bg() -> String { "#282a36cc".into() }  // 0xcc = ~80% opaque
fn default_fg() -> String { "#f8f8f2ff".into() }
fn default_frame() -> String { "#bd93f9ff".into() }
fn default_inactive() -> String { "#44475aff".into() }

fn default_background_gradient() -> String { "none".into() }

fn default_blur_method() -> String { "dual_kawase".into() }
fn default_blur_strength() -> u32 { 5 }

fn default_font_name() -> String { "sans".into() }
fn default_font_size() -> u32 { 11 }
fn default_text_shadow_color() -> String { "auto".into() }
fn default_text_shadow_offset() -> u32 { 1 }

fn default_tile_content() -> String { "icon".into() }
fn default_icon_overlay() -> bool { true }
fn default_icon_overlay_size() -> u32 { 32 }

fn default_modifier() -> String { "Alt".into() }
fn default_next_key() -> String { "Tab".into() }
fn default_prev_key() -> String { "Shift+Tab".into() }
fn default_cancel_key() -> String { "Escape".into() }

impl Default for WindowConfig {
    fn default() -> Self {
        WindowConfig {
            position:            default_position(),
            outer_border_width:  0,
            border:              default_border(),
            background:          default_window_bg(),
            background_gradient: default_background_gradient(),
            blur:                true,
            blur_method:         default_blur_method(),
            blur_strength:       default_blur_strength(),
            configure_picom:     false,
            gap:                 0,
            padding:             0,
            last_row_position:   default_last_row_position(),
            shadow:              false,
            corners:             false,
            border_radius:       0,
        }
    }
}
impl Default for TileConfig {
    fn default() -> Self {
        TileConfig {
            width:             default_tile_width(),
            height:            default_tile_height(),
            icon_size:         default_icon_size(),
            border_width:      default_border_width(),
            padding:           0,
            background:        default_bg(),
            foreground:        default_fg(),
            frame:             default_frame(),
            inactive:          default_inactive(),
            blur:              false,
            border_radius:     0,
            content:           default_tile_content(),
            icon_overlay:      default_icon_overlay(),
            icon_overlay_size: default_icon_overlay_size(),
        }
    }
}
impl Default for FontConfig {
    fn default() -> Self {
        FontConfig {
            name:          default_font_name(),
            size:          default_font_size(),
            shadow:        false,
            shadow_color:  default_text_shadow_color(),
            shadow_offset: default_text_shadow_offset(),
        }
    }
}
impl Default for KeysConfig {
    fn default() -> Self {
        KeysConfig {
            modifier: default_modifier(),
            next:     default_next_key(),
            prev:     default_prev_key(),
            cancel:   default_cancel_key(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn Error>> {
        if let Some(path) = Self::config_path() {
            let content = std::fs::read_to_string(&path)?;
            let cfg: Config = toml::from_str(&content)?;
            match cfg.validate() {
                Ok(()) => {
                    eprintln!("hop: loaded config from {}", path.display());
                    Ok(cfg)
                }
                Err(msg) => {
                    // Keep hop usable: warn loudly and fall back to defaults rather
                    // than refusing to start over a bad field.
                    eprintln!("hop: invalid config at {}:\n  {msg}", path.display());
                    eprintln!("hop: using built-in defaults instead");
                    Ok(Config::default())
                }
            }
        } else {
            eprintln!("hop: no config found, using defaults");
            Ok(Config::default())
        }
    }

    fn config_path() -> Option<std::path::PathBuf> {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_default();
                std::path::PathBuf::from(home).join(".config")
            });
        let path = base.join("hop").join("config.toml");
        path.exists().then_some(path)
    }

    /// Returns the popup window background as a packed 0xAARRGGBB value.
    pub fn window_bg_argb(&self) -> u32 {
        Self::color_argb(&self.window.background)
    }

    /// Returns the tile background as a packed 0xAARRGGBB value.
    pub fn bg_argb(&self) -> u32 {
        Self::color_argb(&self.tile.background)
    }

    /// True if `s` is a syntactically valid color: "#rrggbb" or "#rrggbbaa"
    /// (the leading `#` is optional), all hex digits.
    fn is_valid_color(s: &str) -> bool {
        let h = s.trim_start_matches('#');
        matches!(h.len(), 6 | 8) && h.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// Validate the config after deserialization. Returns `Err` with a message
    /// listing every problem found (one per line). Catches values that would
    /// otherwise panic (zero divisors for tile/icon scaling) and surfaces likely
    /// typos in enum-like string fields and colors, which would silently fall back
    /// to a default and confuse the user.
    pub fn validate(&self) -> Result<(), String> {
        let mut errs: Vec<String> = Vec::new();

        // Numeric fields that act as divisors or dimensions — zero would break
        // layout or panic during icon/thumbnail scaling.
        if self.tile.width == 0 {
            errs.push("tile.width must be greater than 0".into());
        }
        if self.tile.height == 0 {
            errs.push("tile.height must be greater than 0".into());
        }
        if self.tile.icon_size == 0 {
            errs.push("tile.icon_size must be greater than 0".into());
        }
        if self.font.size == 0 {
            errs.push("font.size must be greater than 0".into());
        }

        // Enum-like string fields. The renderer falls back to a default for an
        // unknown value, so flag it as a likely typo rather than silently ignoring.
        let check_one = |field: &str, val: &str, allowed: &[&str], errs: &mut Vec<String>| {
            if !allowed.contains(&val) {
                errs.push(format!("{field} = \"{val}\" is invalid (expected one of: {})", allowed.join(", ")));
            }
        };
        check_one("window.background_gradient", &self.window.background_gradient,
            &["none", "radial", "vertical", "horizontal"], &mut errs);
        check_one("window.last_row_position", &self.window.last_row_position,
            &["left", "center", "right"], &mut errs);
        check_one("window.blur_method", &self.window.blur_method,
            &["dual_kawase", "gaussian", "box", "kernel"], &mut errs);
        check_one("tile.content", &self.tile.content,
            &["icon", "thumbnail"], &mut errs);

        // Position: either "center" or a parseable "x,y" pair.
        let pos = self.window.position.trim();
        let pos_ok = pos == "center"
            || pos.split_once(',').is_some_and(|(x, y)| {
                x.trim().parse::<i16>().is_ok() && y.trim().parse::<i16>().is_ok()
            });
        if !pos_ok {
            errs.push(format!("window.position = \"{pos}\" is invalid (expected \"center\" or \"x,y\")"));
        }

        // Colors. shadow_color additionally accepts the special value "auto".
        for (field, val) in [
            ("window.background", &self.window.background),
            ("window.border", &self.window.border),
            ("tile.background", &self.tile.background),
            ("tile.foreground", &self.tile.foreground),
            ("tile.frame", &self.tile.frame),
            ("tile.inactive", &self.tile.inactive),
        ] {
            if !Self::is_valid_color(val) {
                errs.push(format!("{field} = \"{val}\" is not a valid color (expected #rrggbb or #rrggbbaa)"));
            }
        }
        if self.font.shadow_color != "auto" && !Self::is_valid_color(&self.font.shadow_color) {
            errs.push(format!(
                "font.shadow_color = \"{}\" is invalid (expected \"auto\", #rrggbb, or #rrggbbaa)",
                self.font.shadow_color
            ));
        }

        if errs.is_empty() { Ok(()) } else { Err(errs.join("\n  ")) }
    }

    /// Parse any color string into a packed 0xAARRGGBB value.
    /// Accepts "#rrggbb" (fully opaque) or "#rrggbbaa" (with explicit alpha).
    /// Any other format falls back to opaque black.
    pub fn color_argb(hex: &str) -> u32 {
        let h = hex.trim_start_matches('#');
        match h.len() {
            8 => {
                let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
                let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
                let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
                let a = u8::from_str_radix(&h[6..8], 16).unwrap_or(0xff);
                ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
            }
            6 => {
                let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
                let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
                let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
                0xFF000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
            }
            _ => 0xFF000000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_argb_rrggbbaa() {
        // Packed as 0xAARRGGBB: rr=28 gg=2a bb=36 aa=ff
        assert_eq!(Config::color_argb("#282a36ff"), 0xFF282A36);
        assert_eq!(Config::color_argb("#28323ccc"), 0xCC28323C);
        // Fully transparent.
        assert_eq!(Config::color_argb("#00000000"), 0x0000_0000);
    }

    #[test]
    fn color_argb_rrggbb_is_opaque() {
        assert_eq!(Config::color_argb("#282a36"), 0xFF282A36);
        assert_eq!(Config::color_argb("#ffffff"), 0xFFFFFFFF);
    }

    #[test]
    fn color_argb_leading_hash_optional() {
        assert_eq!(Config::color_argb("282a36"), Config::color_argb("#282a36"));
        assert_eq!(Config::color_argb("282a36ff"), Config::color_argb("#282a36ff"));
    }

    #[test]
    fn color_argb_invalid_falls_back_to_opaque_black() {
        assert_eq!(Config::color_argb(""), 0xFF000000);
        assert_eq!(Config::color_argb("#fff"), 0xFF000000); // wrong length
        assert_eq!(Config::color_argb("not-a-color"), 0xFF000000);
    }

    #[test]
    fn color_argb_bad_hex_digits_default_per_channel() {
        // Non-hex channels parse to 0; alpha defaults to 0xff for the 8-digit form.
        assert_eq!(Config::color_argb("#zzzzzz"), 0xFF000000);
    }

    #[test]
    fn defaults_load_without_a_config_file() {
        // Default config should be constructible and have sane keybindings.
        let c = Config::default();
        assert_eq!(c.keys.modifier, "Alt");
        assert_eq!(c.keys.next, "Tab");
        assert_eq!(c.tile.width, 200);
    }

    #[test]
    fn validate_accepts_defaults() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_dimensions() {
        let mut c = Config::default();
        c.tile.icon_size = 0;
        let err = c.validate().unwrap_err();
        assert!(err.contains("tile.icon_size"));
    }

    #[test]
    fn validate_rejects_unknown_enum_value() {
        let mut c = Config::default();
        c.tile.content = "screenshot".into(); // not "icon"/"thumbnail"
        let err = c.validate().unwrap_err();
        assert!(err.contains("tile.content"));
    }

    #[test]
    fn validate_position_center_or_xy() {
        let mut c = Config::default();
        c.window.position = "center".into();
        assert!(c.validate().is_ok());
        c.window.position = "100,200".into();
        assert!(c.validate().is_ok());
        c.window.position = "top-left".into();
        assert!(c.validate().unwrap_err().contains("window.position"));
    }

    #[test]
    fn validate_colors_and_auto_shadow() {
        let mut c = Config::default();
        c.font.shadow_color = "auto".into();
        assert!(c.validate().is_ok());
        c.tile.frame = "purple".into();
        assert!(c.validate().unwrap_err().contains("tile.frame"));
    }

    #[test]
    fn validate_reports_multiple_problems_at_once() {
        let mut c = Config::default();
        c.tile.width = 0;
        c.window.blur_method = "blurry".into();
        let err = c.validate().unwrap_err();
        assert!(err.contains("tile.width"));
        assert!(err.contains("window.blur_method"));
    }

    #[test]
    fn valid_color_helper() {
        assert!(Config::is_valid_color("#282a36"));
        assert!(Config::is_valid_color("#282a36ff"));
        assert!(Config::is_valid_color("282a36")); // # optional
        assert!(!Config::is_valid_color("#fff"));   // wrong length
        assert!(!Config::is_valid_color("#zzzzzz")); // non-hex
    }
}
