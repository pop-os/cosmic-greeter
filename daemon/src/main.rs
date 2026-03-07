use color_eyre::eyre::Context;
use cosmic_greeter_daemon::UserData;
use std::{env, error::Error, fs, future::pending, io, path::{Path, PathBuf}};
use tracing::metadata::LevelFilter;
use tracing::warn;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};
use zbus::{DBusError, connection::Builder};

/// Read the Icon= path from the AccountsService user file.
/// Must be called as root (before seteuid) since /var/lib/AccountsService/users/ is root-only.
fn accountsservice_icon_path(username: &str) -> Option<PathBuf> {
    let user_file = Path::new("/var/lib/AccountsService/users").join(username);
    let content = fs::read_to_string(&user_file).ok()?;
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("Icon=") {
            let path = path.trim();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

//IMPORTANT: this function is critical to the security of this proxy. It must ensure that the
// callback is executed with the permissions of the specified user id. A good test is to see if
// the /etc/shadow file can be read with a non-root user, it should fail with EPERM.
fn run_as_user<F: FnOnce() -> T, T>(user: &pwd::Passwd, f: F) -> Result<T, io::Error> {
    // Save root HOME
    let root_home_opt = env::var_os("HOME");

    // Switch to user HOME
    unsafe {
        env::set_var("HOME", &user.dir);
    }

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
        Some(root_home) => unsafe {
            env::set_var("HOME", root_home);
        },
        None => unsafe {
            env::remove_var("HOME");
        },
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

            let mut user_data = UserData::from(user.clone());

            // Read icon path from AccountsService as root (before seteuid)
            let icon_path = accountsservice_icon_path(&user.name);

            //IMPORTANT: Assume the identity of the user to ensure we don't read user file data as root
            run_as_user(&user, || user_data.load_config_as_user(icon_path.as_deref()))
                .map_err(|err| GreeterError::RunAsUser(err.to_string()))?;

            user_datas.push(user_data);
        }

        //TODO: is ron the best choice for passing around background data?
        ron::to_string(&user_datas).map_err(|err| GreeterError::Ron(err.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    color_eyre::install().wrap_err("failed to install color_eyre error handler")?;

    let trace = tracing_subscriber::registry();
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .from_env_lossy();

    #[cfg(feature = "systemd")]
    if let Ok(journald) = tracing_journald::layer() {
        trace
            .with(journald)
            .with(env_filter)
            .try_init()
            .wrap_err("failed to initialize logger")?;
    } else {
        trace
            .with(fmt::layer())
            .with(env_filter)
            .try_init()
            .wrap_err("failed to initialize logger")?;
        warn!("failed to connect to journald")
    }

    #[cfg(not(feature = "systemd"))]
    trace
        .with(fmt::layer())
        .with(env_filter)
        .try_init()
        .wrap_err("failed to initialize logger")?;

    let _conn = Builder::system()?
        .name("com.system76.CosmicGreeter")?
        .serve_at("/com/system76/CosmicGreeter", GreeterProxy)?
        .build()
        .await?;

    pending::<()>().await;

    Ok(())
}
