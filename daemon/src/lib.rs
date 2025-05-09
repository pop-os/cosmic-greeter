use cosmic_config::{ConfigGet, CosmicConfigEntry};
use std::{fs, path::Path};

pub use cosmic_bg_config::{Color, Source};
pub use cosmic_comp_config::{CosmicCompConfig, XkbConfig};
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
    pub clock_military_time_opt: Option<bool>,
}

impl UserData {
    pub fn full_name_or_name(&self) -> &str {
        if let Some(full_name) = &self.full_name_opt {
            if !full_name.is_empty() {
                return full_name.as_str();
            }
        }
        self.name.as_str()
    }

    pub fn load_wallpapers_as_user(&mut self, wallpaper_state: &cosmic_bg_config::state::State) {
        let mut wallpaper_datas = Vec::new();
        for (output, source) in wallpaper_state.wallpapers.iter() {
            match source {
                Source::Path(path) => match fs::read(&path) {
                    Ok(bytes) => {
                        wallpaper_datas.push((output.clone(), WallpaperData::Bytes(bytes)));
                    }
                    Err(err) => {
                        log::error!("failed to read wallpaper {:?}: {:?}", path, err);
                    }
                },
                Source::Color(color) => {
                    wallpaper_datas.push((output.clone(), WallpaperData::Color(color.clone())));
                }
            }
        }
        self.wallpapers_opt = Some(wallpaper_datas);
    }

    pub fn load_config_as_user(&mut self) {
        self.icon_opt = None;
        self.theme_opt = None;
        self.wallpapers_opt = None;
        self.xkb_config_opt = None;
        self.clock_military_time_opt = None;

        //TODO: use accountsservice?
        //IMPORTANT: This file is owned by root and safe to read (it won't be a link to /etc/shadow for example)
        // It may not exist if the user uses one of the system icons. In that case, we should read the
        // information in /var/lib/AccountsService/users, and then read the icon path as the user
        let icon_path = Path::new("/var/lib/AccountsService/icons").join(&self.name);
        if icon_path.is_file() {
            match fs::read(&icon_path) {
                Ok(icon_data) => {
                    self.icon_opt = Some(icon_data);
                }
                Err(err) => {
                    log::error!("failed to read icon {:?}: {:?}", icon_path, err);
                }
            }
        }

        let mut is_dark = true;
        match cosmic_theme::ThemeMode::config() {
            Ok(helper) => match cosmic_theme::ThemeMode::get_entry(&helper) {
                Ok(theme_mode) => {
                    is_dark = theme_mode.is_dark;
                }
                Err((errs, theme_mode)) => {
                    log::error!("failed to load cosmic-theme config: {:?}", errs);
                    is_dark = theme_mode.is_dark;
                }
            },
            Err(err) => {
                log::error!("failed to create cosmic-theme mode helper: {:?}", err);
            }
        }

        match if is_dark {
            cosmic_theme::Theme::dark_config()
        } else {
            cosmic_theme::Theme::light_config()
        } {
            Ok(helper) => match cosmic_theme::Theme::get_entry(&helper) {
                Ok(theme) => {
                    self.theme_opt = Some(theme);
                }
                Err((errs, theme)) => {
                    log::error!("failed to load cosmic-theme config: {:?}", errs);
                    self.theme_opt = Some(theme);
                }
            },
            Err(err) => {
                log::error!("failed to create cosmic-theme config helper: {:?}", err);
            }
        }

        //TODO: fallback to background config if background state is not set?
        let mut wallpaper_state_opt = None;
        match cosmic_bg_config::state::State::state() {
            Ok(helper) => match cosmic_bg_config::state::State::get_entry(&helper) {
                Ok(state) => {
                    wallpaper_state_opt = Some(state);
                }
                Err((errs, state)) => {
                    log::error!("failed to load cosmic-bg state: {:?}", errs);
                    wallpaper_state_opt = Some(state);
                }
            },
            Err(err) => {
                log::error!("failed to create cosmic-bg state helper: {:?}", err);
            }
        }

        if let Some(wallpaper_state) = wallpaper_state_opt {
            self.load_wallpapers_as_user(&wallpaper_state);
        }

        match cosmic_config::Config::new("com.system76.CosmicComp", CosmicCompConfig::VERSION) {
            Ok(config_handler) => match CosmicCompConfig::get_entry(&config_handler) {
                Ok(config) => {
                    self.xkb_config_opt = Some(config.xkb_config);
                }
                Err((errs, config)) => {
                    log::error!("errors loading cosmic-comp config: {:?}", errs);
                    self.xkb_config_opt = Some(config.xkb_config);
                }
            },
            Err(err) => {
                log::error!("failed to create cosmic-comp config handler: {}", err);
            }
        };

        match cosmic_config::Config::new("com.system76.CosmicAppletTime", 1) {
            Ok(config_handler) => match config_handler.get("military_time") {
                Ok(value) => {
                    self.clock_military_time_opt = Some(value);
                }
                Err(err) => {
                    log::error!("failed to load military time config: {:?}", err);
                }
            },
            Err(err) => {
                log::error!(
                    "failed to create CosmicAppletTime config handler: {:?}",
                    err
                );
            }
        };
    }
}

impl From<pwd::Passwd> for UserData {
    fn from(user: pwd::Passwd) -> Self {
        Self {
            uid: user.uid,
            name: user.name.clone(),
            full_name_opt: user
                .gecos
                .as_ref()
                .and_then(|gecos| gecos.split(',').next())
                .map(|x| x.to_string()),
            icon_opt: None,
            theme_opt: None,
            wallpapers_opt: None,
            xkb_config_opt: None,
            clock_military_time_opt: None,
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub enum WallpaperData {
    Bytes(Vec<u8>),
    Color(Color),
}
