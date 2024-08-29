pub use cosmic_bg_config::Color;
pub use cosmic_comp_config::XkbConfig;
pub use cosmic_theme::Theme;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct UserData {
    pub uid: u32,
    pub name: String,
    pub full_name_opt: Option<String>,
    pub icon_opt: Option<Vec<u8>>,
    pub theme_opt: Option<Theme>,
    pub wallpapers_opt: Option<Vec<(String, WallpaperData)>>,
    pub xkb_config_opt: Option<XkbConfig>,
    pub clock_military_time: bool,
    // pub clock_show_seconds: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub enum WallpaperData {
    Bytes(Vec<u8>),
    Color(Color),
}
