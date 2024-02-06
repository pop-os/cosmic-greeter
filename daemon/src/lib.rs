pub use cosmic_bg_config::Color;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct UserData {
    pub uid: u32,
    pub name: String,
    pub full_name_opt: Option<String>,
    pub icon_opt: Option<Vec<u8>>,
    pub wallpapers_opt: Option<Vec<(String, WallpaperData)>>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub enum WallpaperData {
    Bytes(Vec<u8>),
    Color(Color),
}
