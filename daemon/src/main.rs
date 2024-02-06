use cosmic_bg_config::Source;
use cosmic_greeter_daemon::{UserData, WallpaperData};
use std::{error::Error, fs, future::pending, io, path::Path};
use zbus::{dbus_interface, ConnectionBuilder, DBusError};

//IMPORTANT: this function is critical to the security of this proxy. It must ensure that the
// callback is executed with the permissions of the specified user id. A good test is to see if
// the /etc/shadow file can be read with a non-root user, it should fail with EPERM.
fn run_as_user<F: FnOnce() -> T, T>(user: &pwd::Passwd, f: F) -> Result<T, io::Error> {
    if unsafe { libc::seteuid(user.uid) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let t = f();

    if unsafe { libc::seteuid(0) } != 0 {
        panic!("failed to restore root user id")
    }

    Ok(t)
}

#[derive(DBusError, Debug)]
#[dbus_error(prefix = "com.system76.CosmicGreeter")]
enum GreeterError {
    #[dbus_error(zbus_error)]
    ZBus(zbus::Error),
    Ron(String),
    RunAsUser(String),
}

struct GreeterProxy;

#[dbus_interface(name = "com.system76.CosmicGreeter")]
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

            //IMPORTANT: Assume the identity of the user to ensure we don't read wallpaper file data as root
            let wallpapers_opt = run_as_user(&user, || {
                //TODO: use libcosmic to find this path
                let wallpapers_path = Path::new(&user.dir)
                    .join(".local/state/cosmic/com.system76.CosmicBackground/v1/wallpapers");
                if wallpapers_path.is_file() {
                    match fs::read_to_string(&wallpapers_path) {
                        Ok(wallpapers_ron) => {
                            match ron::from_str::<Vec<(String, Source)>>(&wallpapers_ron) {
                                Ok(sources) => {
                                    let mut wallpaper_datas = Vec::new();
                                    for (output, source) in sources {
                                        match source {
                                            Source::Path(path) => match fs::read(&path) {
                                                Ok(bytes) => {
                                                    wallpaper_datas.push((
                                                        output,
                                                        WallpaperData::Bytes(bytes),
                                                    ));
                                                }
                                                Err(err) => {
                                                    log::error!(
                                                        "failed to read wallpaper {:?}: {:?}",
                                                        path,
                                                        err
                                                    );
                                                }
                                            },
                                            Source::Color(color) => {
                                                wallpaper_datas
                                                    .push((output, WallpaperData::Color(color)));
                                            }
                                        }
                                    }
                                    Some(wallpaper_datas)
                                }
                                Err(err) => {
                                    log::error!(
                                        "failed to parse wallpapers {:?}: {:?}",
                                        wallpapers_path,
                                        err
                                    );
                                    None
                                }
                            }
                        }
                        Err(err) => {
                            log::error!(
                                "failed to read wallpapers {:?}: {:?}",
                                wallpapers_path,
                                err
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            })
            .map_err(|err| GreeterError::RunAsUser(err.to_string()))?;

            user_datas.push(UserData {
                uid: user.uid,
                name: user.name,
                full_name_opt: user
                    .gecos
                    .map(|gecos| gecos.split(',').next().unwrap_or_default().to_string()),
                icon_opt,
                //TODO: should wallpapers come from a per-user call?
                wallpapers_opt,
            });
        }

        //TODO: is ron the best choice for passing around background data?
        ron::to_string(&user_datas).map_err(|err| GreeterError::Ron(err.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let _conn = ConnectionBuilder::system()?
        .name("com.system76.CosmicGreeter")?
        .serve_at("/com/system76/CosmicGreeter", GreeterProxy)?
        .build()
        .await?;

    pending::<()>().await;

    Ok(())
}
