use cosmic_comp_config::output::randr;
use cosmic_config::CosmicConfigEntry;
use kdl::KdlDocument;
use std::{
    collections::BTreeMap,
    fs, iter,
    path::{Path, PathBuf},
};

pub use cosmic_applets_config::time::TimeAppletConfig;
pub use cosmic_bg_config::{
    Color, Config as BgConfig, Source as BgSource, state::State as BgState,
};
pub use cosmic_comp_config::{CosmicCompConfig, XkbConfig, ZoomConfig};
pub use cosmic_theme::{Theme, ThemeBuilder};

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct UserData {
    pub uid: u32,
    pub name: String,
    pub full_name: String,
    pub icon_opt: Option<Vec<u8>>,
    pub theme_opt: Option<Theme>,
    pub theme_builder_opt: Option<ThemeBuilder>,
    pub wallpapers: WallpaperData,
    pub xkb_config_opt: Option<XkbConfig>,
    pub time_applet_config: TimeAppletConfig,
    pub accessibility_zoom: ZoomConfig,
    pub kdl_output_lists: Vec<String>,
}

impl UserData {
    pub fn load_config_as_user(&mut self) {
        self.icon_opt = None;
        self.theme_opt = None;
        self.theme_builder_opt = None;
        self.wallpapers = WallpaperData::default();
        self.xkb_config_opt = None;
        self.time_applet_config = Default::default();

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
                    tracing::error!("failed to read icon {:?}: {:?}", icon_path, err);
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
                    tracing::error!("failed to load cosmic-theme config: {:?}", errs);
                    is_dark = theme_mode.is_dark;
                }
            },
            Err(err) => {
                tracing::error!("failed to create cosmic-theme mode helper: {:?}", err);
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
                    tracing::error!("failed to load cosmic-theme config: {:?}", errs);
                    self.theme_opt = Some(theme);
                }
            },
            Err(err) => {
                tracing::error!("failed to create cosmic-theme config helper: {:?}", err);
            }
        }

        match if is_dark {
            cosmic_theme::ThemeBuilder::dark_config()
        } else {
            cosmic_theme::ThemeBuilder::light_config()
        } {
            Ok(helper) => match cosmic_theme::ThemeBuilder::get_entry(&helper) {
                Ok(theme) => {
                    self.theme_builder_opt = Some(theme);
                }
                Err((errs, theme)) => {
                    tracing::error!("failed to load cosmic-theme builder config: {:?}", errs);
                    self.theme_builder_opt = Some(theme);
                }
            },
            Err(err) => {
                tracing::error!(
                    "failed to create cosmic-theme builder config helper: {:?}",
                    err
                );
            }
        }

        match cosmic_bg_config::state::State::state() {
            Ok(helper) => match cosmic_bg_config::state::State::get_entry(&helper) {
                Ok(state) => {
                    self.wallpapers.update_bg_state(state);
                }
                Err((errs, state)) => {
                    tracing::error!("failed to load cosmic-bg state: {:?}", errs);
                    self.wallpapers.update_bg_state(state);
                }
            },
            Err(err) => {
                tracing::error!("failed to create cosmic-bg state helper: {:?}", err);
            }
        }
        self.wallpapers.load_as_user();

        match cosmic_config::Config::new("com.system76.CosmicComp", CosmicCompConfig::VERSION) {
            Ok(config_handler) => {
                match CosmicCompConfig::get_entry(&config_handler) {
                    Ok(config) => {
                        self.xkb_config_opt = Some(config.xkb_config);
                        self.accessibility_zoom = config.accessibility_zoom;
                    }
                    Err((errs, config)) => {
                        tracing::error!("errors loading cosmic-comp config: {:?}", errs);
                        self.xkb_config_opt = Some(config.xkb_config);
                        self.accessibility_zoom = config.accessibility_zoom;
                    }
                };
            }
            Err(err) => {
                tracing::error!("failed to create cosmic-comp config handler: {}", err);
            }
        };

        let xdg = xdg::BaseDirectories::new();
        self.kdl_output_lists = xdg
            .get_state_home()
            .map(|mut s| {
                s.push("cosmic-comp/outputs.ron");
                let lists = randr::load_outputs(Some(&s));
                lists
                    .into_iter()
                    .map(|l| KdlDocument::from(l).to_string())
                    .collect()
            })
            .unwrap_or_default();

        match cosmic_config::Config::new("com.system76.CosmicAppletTime", TimeAppletConfig::VERSION)
        {
            Ok(config_handler) => match TimeAppletConfig::get_entry(&config_handler) {
                Ok(config) => {
                    self.time_applet_config = config;
                }
                Err((errs, config)) => {
                    tracing::error!("failed to load time applet config: {:?}", errs);
                    self.time_applet_config = config;
                }
            },
            Err(err) => {
                tracing::error!(
                    "failed to create CosmicAppletTime config handler: {:?}",
                    err
                );
            }
        };
    }
}

impl From<pwd::Passwd> for UserData {
    fn from(user: pwd::Passwd) -> Self {
        let mut full_name = user
            .gecos
            .as_ref()
            .and_then(|gecos| gecos.split(',').next())
            .map(|x| x.to_string())
            .unwrap_or_default();
        if full_name.is_empty() {
            full_name = user.name.clone();
        }
        Self {
            uid: user.uid,
            name: user.name.clone(),
            full_name,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct WallpaperData {
    default_bg: BgSource,
    bg_state: BgState,
    bg_path_data: BTreeMap<PathBuf, Vec<u8>>,
}

impl Default for WallpaperData {
    fn default() -> Self {
        Self {
            default_bg: BgConfig::default().default_background.source,
            bg_state: BgState::default(),
            bg_path_data: BTreeMap::default(),
        }
    }
}

impl WallpaperData {
    fn iter_sources(&self) -> impl Iterator<Item = &BgSource> {
        iter::once(&self.default_bg)
            .chain(self.bg_state.wallpapers.iter().map(|(_, source)| source))
    }

    pub fn get<'a>(&'a self, output_name: &str) -> Result<LoadedWallpaper<'a>, &'a Path> {
        let source = self
            .bg_state
            .wallpapers
            .iter()
            .find_map(|(name, source)| (output_name == name).then_some(source))
            .unwrap_or(&self.default_bg);

        match source {
            BgSource::Path(path) => self
                .bg_path_data
                .get(path)
                .map(|bytes| LoadedWallpaper::Bytes(bytes.as_slice()))
                .ok_or(path),
            BgSource::Color(color) => Ok(LoadedWallpaper::Color(color)),
        }
    }

    pub fn update_bg_state(&mut self, state: BgState) {
        self.bg_state = state;
    }

    pub fn load_as_user(&mut self) {
        let source_paths = self
            .iter_sources()
            .filter_map(|source| match source {
                BgSource::Path(path) => Some(path.to_owned()),
                BgSource::Color(_) => None,
            })
            .collect::<Vec<_>>();

        //TODO: reload changed background files?
        self.bg_path_data
            .retain(|path, _| source_paths.contains(path));

        for path in source_paths {
            if !self.bg_path_data.contains_key(&path) {
                match fs::read(&path) {
                    Ok(bytes) => {
                        self.bg_path_data.insert(path, bytes);
                    }
                    Err(err) => {
                        tracing::error!("failed to read wallpaper {:?}: {:?}", path, err);
                    }
                }
            }
        }
    }
}

pub enum LoadedWallpaper<'a> {
    Bytes(&'a [u8]),
    Color(&'a Color),
}
