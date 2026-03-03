use serde::Deserialize;
use std::error::Error;

#[derive(Debug, Deserialize)]
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

fn default_modifier() -> String { "Alt".into() }
fn default_next_key() -> String { "Tab".into() }
fn default_prev_key() -> String { "Shift+Tab".into() }
fn default_cancel_key() -> String { "Escape".into() }

impl Default for Config {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for WindowConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for TileConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for FontConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for KeysConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn Error>> {
        if let Some(path) = Self::config_path() {
            let content = std::fs::read_to_string(&path)?;
            eprintln!("hop: loaded config from {}", path.display());
            Ok(toml::from_str(&content)?)
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
