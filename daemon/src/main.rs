use color_eyre::eyre::Context;
use cosmic_greeter_daemon::{AccountsProxy, AccountsUserProxy, UserData, UserFilter};
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::CString;
use std::future::pending;
use std::os::unix::fs::MetadataExt;
use std::{env, io};
use tracing::metadata::LevelFilter;
use tracing::warn;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};
use zbus::DBusError;
use zbus::connection::Builder;

//IMPORTANT: this function is critical to the security of this proxy. It must ensure that the
// callback is executed with the permissions of the specified user id. A good test is to see if
// the /etc/shadow file can be read with a non-root user, it should fail with EPERM.
fn run_as_user<F: FnOnce() -> T, T>(user: &pwd::Passwd, f: F) -> Result<T, io::Error> {
    use nix::unistd::{Gid, Uid, getgroups, initgroups, setegid, seteuid, setgroups};

    // Save root HOME
    let root_home_opt = env::var_os("HOME");

    // Save root groups
    let root_groups = getgroups().expect("failed to get root groups");

    // Switch to user HOME
    unsafe {
        env::set_var("HOME", &user.dir);
    }

    // Switch to user identity
    if let Ok(name_c) = CString::new(&*user.name) {
        // Ignore initgroups failure since AD / domain users might not have local group records
        let _ = initgroups(&name_c, Gid::from_raw(user.gid));
    }
    if let Err(e) = setegid(Gid::from_raw(user.gid)) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()));
    }
    if let Err(e) = seteuid(Uid::from_raw(user.uid)) {
        let _ = setegid(Gid::from_raw(0));
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()));
    }

    let t = f();

    // Restore root identity
    seteuid(Uid::from_raw(0)).expect("failed to restore root uid");
    setegid(Gid::from_raw(0)).expect("failed to restore root gid");
    setgroups(&root_groups).expect("failed to restore root supplementary groups");

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
    // Caches a user with AccountsService (`org.freedesktop.Accounts.CacheUser`).
    //
    // Required for remote domain accounts:
    // AccountsService does not automatically enumerate remote domain accounts to prevent
    // massive LDAP queries. When a user logs in successfully, calling this registers their
    // username in `/var/lib/AccountsService/users/<username>` so they appear as user card on login screen.
    async fn cache_user(
        &mut self,
        #[zbus(connection)] conn: &zbus::Connection,
        name: String,
    ) -> Result<(), GreeterError> {
        match AccountsProxy::new(conn).await {
            Ok(accounts_proxy) => match accounts_proxy.cache_user(&name).await {
                Ok(path) => {
                    tracing::warn!("Successfully cached user {} in AccountsService: {:?}", name, path);
                }
                Err(err) => {
                    tracing::error!("Failed to cache user {} in AccountsService: {:?}", name, err);
                }
            },
            Err(err) => {
                tracing::error!("Failed to create AccountsProxy: {:?}", err);
            }
        }
        Ok(())
    }

    async fn get_user_data(
        &mut self,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> Result<String, GreeterError> {
        let user_filter = UserFilter::new().await;
        // User map
        let mut user_map: BTreeMap<String, (pwd::Passwd, Option<String>, Option<String>)> =
            BTreeMap::new();

        // Query AccountsService for cached users
        if let Ok(accounts_proxy) = AccountsProxy::new(conn).await {
            if let Ok(user_paths) = accounts_proxy.list_cached_users().await {
                for user_path in user_paths {
                    if let Ok(builder) = AccountsUserProxy::builder(conn).path(user_path) {
                        if let Ok(user_proxy) = builder.build().await {
                            let is_system = user_proxy.system_account().await.unwrap_or(false);
                            let is_locked = user_proxy.locked().await.unwrap_or(false);
                            if is_system || is_locked {
                                continue;
                            }

                            let Ok(uid_raw) = user_proxy.uid().await else {
                                continue;
                            };
                            if uid_raw > u32::MAX as u64 {
                                continue;
                            }
                            let uid = uid_raw as u32;

                            let user_name = user_proxy.user_name().await.unwrap_or_default();
                            let real_name =
                                user_proxy.real_name().await.ok().filter(|s| !s.is_empty());
                            let icon_file =
                                user_proxy.icon_file().await.ok().filter(|s| !s.is_empty());

                            // Resolve POSIX entry (queries SSSD / NSS via getpwuid / getpwnam)
                            let passwd_opt = pwd::Passwd::from_uid(uid).or_else(|| {
                                if !user_name.is_empty() {
                                    pwd::Passwd::from_name(&user_name).ok().flatten()
                                } else {
                                    None
                                }
                            });

                            if let Some(user) = passwd_opt {
                                if user_filter.filter_cached(&user) {
                                    user_map.insert(user.name.clone(), (user, real_name, icon_file));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Enumerate local users from /etc/passwd (ensuring un-cached local users are also shown)
        // The pwd::Passwd method is unsafe (but not labelled as such) due to using global state (libc pwent functions).
        // To prevent issues, this should only be called once in the entire process space at a time
        let local_users: Vec<_> = pwd::Passwd::iter()
            .filter(|user| user_filter.filter_local(user))
            .collect();

        for user in local_users {
            user_map
                .entry(user.name.clone())
                .or_insert_with(|| (user, None, None));
        }

        let mut user_datas = Vec::new();
        for (_, (user, real_name_opt, icon_file_opt)) in user_map {
            let mut user_data = UserData::from(user.clone());
            if let Some(real_name) = real_name_opt {
                user_data.full_name = real_name;
            }

            // An active home is owned by the user; the fallback is not. Only load
            // config when we would be reading the user's own directory.
            let home_is_users = std::fs::metadata(&user.dir)
                .map(|meta| meta.uid() == user.uid)
                .unwrap_or(false);
            if !home_is_users {
                tracing::debug!(
                    "skipping config for {}: {:?} is not their home (locked or uncreated?)",
                    user.name,
                    user.dir
                );
                user_datas.push(user_data);
                continue;
            }

            // IMPORTANT: Assume the identity of the user to ensure we don't read user file data as root
            if let Err(err) = run_as_user(&user, || {
                user_data.load_config_as_user_with_icon(icon_file_opt.as_deref())
            }) {
                tracing::warn!("failed to run as user {}: {}", user.name, err);
            }

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
