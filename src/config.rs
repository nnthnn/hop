use serde::Deserialize;
use std::error::Error;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub colors: ColorsConfig,
    #[serde(default)]
    pub font: FontConfig,
    #[serde(default)]
    pub blur: BlurConfig,
    #[serde(default)]
    pub keys: KeysConfig,
}

#[derive(Debug, Deserialize)]
pub struct WindowConfig {
    #[serde(default = "default_tile_width")]
    pub tile_width: u32,
    #[serde(default = "default_tile_height")]
    pub tile_height: u32,
    #[serde(default = "default_icon_size")]
    pub icon_size: u32,
    #[serde(default = "default_border_width")]
    pub border_width: u32,
    #[serde(default = "default_position")]
    pub position: String,
}

#[derive(Debug, Deserialize)]
pub struct ColorsConfig {
    #[serde(default = "default_bg")]
    pub background: String,
    #[serde(default = "default_bg_alpha")]
    pub bg_alpha: f64,
    #[serde(default = "default_fg")]
    pub foreground: String,
    #[serde(default = "default_frame")]
    pub frame: String,
    #[serde(default = "default_inactive")]
    pub inactive: String,
    #[serde(default = "default_border")]
    pub border: String,
}

#[derive(Debug, Deserialize)]
pub struct FontConfig {
    #[serde(default = "default_font_name")]
    pub name: String,
    #[serde(default = "default_font_size")]
    pub size: u32,
}

#[derive(Debug, Deserialize)]
pub struct BlurConfig {
    #[serde(default = "default_blur_enabled")]
    pub enabled: bool,
    /// Blur radius hint passed to the compositor via _KDE_NET_WM_BLUR_BEHIND_REGION.
    /// Actual blur appearance is controlled by picom's blur settings;
    /// setting this to 0 disables the blur hint entirely.
    #[serde(default = "default_blur_radius")]
    pub radius: u32,
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

fn default_tile_width() -> u32 { 200 }
fn default_tile_height() -> u32 { 150 }
fn default_icon_size() -> u32 { 64 }
fn default_border_width() -> u32 { 4 }
fn default_position() -> String { "center".into() }

fn default_bg() -> String { "#282a36".into() }
fn default_bg_alpha() -> f64 { 0.80 }
fn default_fg() -> String { "#f8f8f2".into() }
fn default_frame() -> String { "#bd93f9".into() }
fn default_inactive() -> String { "#44475a".into() }
fn default_border() -> String { "#6272a4".into() }

fn default_font_name() -> String { "sans".into() }
fn default_font_size() -> u32 { 11 }

fn default_blur_enabled() -> bool { true }
fn default_blur_radius() -> u32 { 10 }

fn default_modifier() -> String { "Alt".into() }
fn default_next_key() -> String { "Tab".into() }
fn default_prev_key() -> String { "Shift+Tab".into() }
fn default_cancel_key() -> String { "Escape".into() }

impl Default for Config {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

impl Default for WindowConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for ColorsConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for FontConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for BlurConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for KeysConfig {
    fn default() -> Self { toml::from_str("").unwrap() }
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn Error>> {
        if let Some(path) = Self::config_path() {
            let content = std::fs::read_to_string(&path)?;
            eprintln!("xwitch: loaded config from {}", path.display());
            Ok(toml::from_str(&content)?)
        } else {
            eprintln!("xwitch: no config found, using defaults");
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
        let path = base.join("xwitch").join("config.toml");
        path.exists().then_some(path)
    }

    /// Parse a hex color string like "#rrggbb" into (r, g, b) bytes.
    pub fn parse_color(hex: &str) -> Option<(u8, u8, u8)> {
        let h = hex.trim_start_matches('#');
        if h.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&h[0..2], 16).ok()?;
        let g = u8::from_str_radix(&h[2..4], 16).ok()?;
        let b = u8::from_str_radix(&h[4..6], 16).ok()?;
        Some((r, g, b))
    }

    /// Returns the background as a premultiplied ARGB u32 pixel value.
    pub fn bg_argb(&self) -> u32 {
        let (r, g, b) = Self::parse_color(&self.colors.background)
            .unwrap_or((0x28, 0x2a, 0x36));
        let a = (self.colors.bg_alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
        (a << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
    }

    /// Returns a fully opaque ARGB pixel for the given hex color.
    pub fn color_argb(hex: &str) -> u32 {
        let (r, g, b) = Self::parse_color(hex).unwrap_or((0, 0, 0));
        0xFF000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
    }
}
