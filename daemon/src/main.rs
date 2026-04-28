use color_eyre::eyre::Context;
use cosmic_greeter_daemon::{UserData, UserFilter};
use std::error::Error;
use std::ffi::CString;
use std::future::pending;
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
    {
        let name_c = CString::new(&*user.name).expect("invalid username");
        initgroups(&name_c, Gid::from_raw(user.gid))
            .expect("failed to set user supplementary groups");
    }
    setegid(Gid::from_raw(user.gid)).expect("failed to set user gid");
    seteuid(Uid::from_raw(user.uid)).expect("failed to set user uid");

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
    fn get_user_data(&mut self) -> Result<String, GreeterError> {
        let user_filter = UserFilter::new();

        // The pwd::Passwd method is unsafe (but not labelled as such) due to using global state (libc pwent functions).
        // To prevent issues, this should only be called once in the entire process space at a time
        let users: Vec<_> = /* unsafe */ {
             pwd::Passwd::iter()
                .filter(|user| user_filter.filter(user))
                .collect()
            };

        let mut user_datas = Vec::new();
        for user in users {
            let mut user_data = UserData::from(user.clone());

            //IMPORTANT: Assume the identity of the user to ensure we don't read user file data as root
            run_as_user(&user, || user_data.load_config_as_user())
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
