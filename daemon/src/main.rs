use cosmic_bg_config::Source;
use cosmic_comp_config::CosmicCompConfig;
use cosmic_config::{ConfigGet, CosmicConfigEntry};
use cosmic_greeter_daemon::{UserData, WallpaperData};
use std::{env, error::Error, fs, future::pending, io, path::Path};
use zbus::{ConnectionBuilder, DBusError};

//IMPORTANT: this function is critical to the security of this proxy. It must ensure that the
// callback is executed with the permissions of the specified user id. A good test is to see if
// the /etc/shadow file can be read with a non-root user, it should fail with EPERM.
fn run_as_user<F: FnOnce() -> T, T>(user: &pwd::Passwd, f: F) -> Result<T, io::Error> {
    // Save root HOME
    let root_home_opt = env::var_os("HOME");

    // Switch to user HOME
    env::set_var("HOME", &user.dir);

    // Switch to user UID
    if unsafe { libc::seteuid(user.uid) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let t = f();

    // Restore root UID
    if unsafe { libc::seteuid(0) } != 0 {
        panic!("failed to restore root user id")
    }

    // Restore root HOME
    match root_home_opt {
        Some(root_home) => env::set_var("HOME", root_home),
        None => env::remove_var("HOME"),
    }

    Ok(t)
}

#[derive(DBusError, Debug)]
#[zbus(prefix = "com.system76.CosmicGreeter")]
enum GreeterError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Ron(String),
    RunAsUser(String),
}

struct GreeterProxy;

#[zbus::interface(name = "com.system76.CosmicGreeter")]
impl GreeterProxy {
    fn get_user_data(&mut self) -> Result<String, GreeterError> {
        // The pwd::Passwd method is unsafe (but not labelled as such) due to using global state (libc pwent functions).
        // To prevent issues, this should only be called once in the entire process space at a time
        let users: Vec<_> = /* unsafe */ {
             pwd::Passwd::iter()
                .filter(|user| {
                    if user.uid < 1000 {
                        // Skip system accounts
                        return false;
                    }

                    match Path::new(&user.shell).file_name().and_then(|x| x.to_str()) {
                        // Skip shell ending in false
                        Some("false") => false,
                        // Skip shell ending in nologin
                        Some("nologin") => false,
                        _ => true,
                    }
                })
                .collect()
            };

        let mut user_datas = Vec::new();
        for user in users {
            if user.uid < 1000 {
                // Skip system accounts
                continue;
            }

            match Path::new(&user.shell).file_name().and_then(|x| x.to_str()) {
                // Skip shell ending in false
                Some("false") => continue,
                // Skip shell ending in nologin
                Some("nologin") => continue,
                _ => (),
            }

            //TODO: use accountsservice
            //IMPORTANT: This file is owned by root and safe to read (it won't be a link to /etc/shadow for example)
            // It may not exist if the user uses one of the system icons. In that case, we should read the
            // information in /var/lib/AccountsService/users, and then read the icon path as the user
            let icon_path = Path::new("/var/lib/AccountsService/icons").join(&user.name);
            let icon_opt = if icon_path.is_file() {
                match fs::read(&icon_path) {
                    Ok(icon_data) => Some(icon_data),
                    Err(err) => {
                        log::error!("failed to read icon {:?}: {:?}", icon_path, err);
                        None
                    }
                }
            } else {
                None
            };

            let mut user_data = UserData {
                uid: user.uid,
                name: user.name.clone(),
                full_name_opt: user
                    .gecos
                    .as_ref()
                    .map(|gecos| gecos.split(',').next().unwrap_or_default().to_string()),
                icon_opt,
                theme_opt: None,
                //TODO: should wallpapers come from a per-user call?
                wallpapers_opt: None,
                xkb_config_opt: None,
                clock_military_time: false,
                // clock_show_seconds: false,
            };

            //IMPORTANT: Assume the identity of the user to ensure we don't read wallpaper file data as root
            run_as_user(&user, || {
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
                            user_data.theme_opt = Some(theme);
                        }
                        Err((errs, theme)) => {
                            log::error!("failed to load cosmic-theme config: {:?}", errs);
                            user_data.theme_opt = Some(theme);
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
                    let mut wallpaper_datas = Vec::new();
                    for (output, source) in wallpaper_state.wallpapers {
                        match source {
                            Source::Path(path) => match fs::read(&path) {
                                Ok(bytes) => {
                                    wallpaper_datas.push((output, WallpaperData::Bytes(bytes)));
                                }
                                Err(err) => {
                                    log::error!("failed to read wallpaper {:?}: {:?}", path, err);
                                }
                            },
                            Source::Color(color) => {
                                wallpaper_datas.push((output, WallpaperData::Color(color)));
                            }
                        }
                    }
                    user_data.wallpapers_opt = Some(wallpaper_datas);
                }

                match cosmic_config::Config::new(
                    "com.system76.CosmicComp",
                    CosmicCompConfig::VERSION,
                ) {
                    Ok(config_handler) => match CosmicCompConfig::get_entry(&config_handler) {
                        Ok(config) => {
                            user_data.xkb_config_opt = Some(config.xkb_config);
                        }
                        Err((errs, config)) => {
                            log::error!("errors loading cosmic-comp config: {:?}", errs);
                            user_data.xkb_config_opt = Some(config.xkb_config);
                        }
                    },
                    Err(err) => {
                        log::error!("failed to create cosmic-comp config handler: {}", err);
                    }
                };

                match cosmic_config::Config::new("com.system76.CosmicAppletTime", 1) {
                    Ok(config_handler) => {
                        user_data.clock_military_time =
                            config_handler.get("military_time").unwrap_or_default();
                        // user_data.clock_show_seconds =
                        //     config_handler.get("show_seconds").unwrap_or_default();
                    }
                    Err(err) => {
                        log::error!(
                            "failed to create CosmicAppletTime config handler: {:?}",
                            err
                        );
                    }
                };
            })
            .map_err(|err| GreeterError::RunAsUser(err.to_string()))?;

            user_datas.push(user_data);
        }

        //TODO: is ron the best choice for passing around background data?
        ron::to_string(&user_datas).map_err(|err| GreeterError::Ron(err.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let _conn = ConnectionBuilder::system()?
        .name("com.system76.CosmicGreeter")?
        .serve_at("/com/system76/CosmicGreeter", GreeterProxy)?
        .build()
        .await?;

    pending::<()>().await;

    Ok(())
}
