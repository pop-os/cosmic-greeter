use color_eyre::eyre::Context;
use cosmic_config::{ConfigSet, CosmicConfigEntry};
use cosmic_greeter_daemon::{CosmicCompConfig, UserData, UserFilter, XkbConfig};
use std::error::Error;
use std::ffi::{CString, OsString};
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

    // Guard that restores the root identity (uid, gid, supplementary groups and
    // HOME) when it is dropped. Arming this before we drop privileges means a
    // panic anywhere in the switch or in `f` still restores root on unwind,
    // rather than leaving the process stuck with another identity for the next
    // request it serves.
    struct RestoreRoot {
        root_groups: Vec<Gid>,
        root_home_opt: Option<OsString>,
    }
    impl Drop for RestoreRoot {
        fn drop(&mut self) {
            // Restore uid/gid first so we have the privilege to restore groups.
            seteuid(Uid::from_raw(0)).expect("failed to restore root uid");
            setegid(Gid::from_raw(0)).expect("failed to restore root gid");
            setgroups(&self.root_groups).expect("failed to restore root supplementary groups");
            match self.root_home_opt.take() {
                Some(root_home) => unsafe {
                    env::set_var("HOME", root_home);
                },
                None => unsafe {
                    env::remove_var("HOME");
                },
            }
        }
    }

    // Save root HOME and groups, then arm the restore guard before touching the
    // process identity.
    let _restore = RestoreRoot {
        root_groups: getgroups().expect("failed to get root groups"),
        root_home_opt: env::var_os("HOME"),
    };

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

    // `_restore` is dropped here, restoring the root identity and HOME.
    Ok(t)
}

#[derive(DBusError, Debug)]
#[zbus(prefix = "com.system76.CosmicGreeter")]
enum GreeterError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Ron(String),
    RunAsUser(String),
    UnknownUser(String),
    InvalidXkbConfig(String),
}

// Reject an `XkbConfig` that would corrupt the user's keyboard setup or that
// looks like abuse rather than a real layout selection. Called on the daemon
// side because the only client that should reach `set_xkb_config` is the
// greeter, but the D-Bus policy cannot enforce that, so the payload is treated
// as untrusted.
fn validate_xkb_config(xkb_config: &XkbConfig) -> Result<(), GreeterError> {
    const MAX_FIELD_LEN: usize = 4096;
    const MAX_LAYOUTS: usize = 64;

    // An empty layout blanks the user's keyboard config, which makes cosmic-comp
    // fall back to US and silently drops every configured layout. This is
    // exactly the data loss of issue #258, so refuse to write it.
    if xkb_config.layout.is_empty() {
        return Err(GreeterError::InvalidXkbConfig("empty layout".to_string()));
    }

    // `layout` and `variant` are parallel comma-separated lists; a mismatch
    // means the variants would bind to the wrong layouts.
    let layout_count = xkb_config.layout.split(',').count();
    let variant_count = xkb_config.variant.split(',').count();
    if layout_count != variant_count {
        return Err(GreeterError::InvalidXkbConfig(format!(
            "layout/variant count mismatch ({layout_count} vs {variant_count})"
        )));
    }
    if layout_count > MAX_LAYOUTS {
        return Err(GreeterError::InvalidXkbConfig(format!(
            "too many layouts ({layout_count})"
        )));
    }

    // Bound every field so a caller cannot bloat the user's config file.
    for (name, value) in [
        ("rules", &xkb_config.rules),
        ("model", &xkb_config.model),
        ("layout", &xkb_config.layout),
        ("variant", &xkb_config.variant),
    ] {
        if value.len() > MAX_FIELD_LEN {
            return Err(GreeterError::InvalidXkbConfig(format!(
                "{name} too long ({} bytes)",
                value.len()
            )));
        }
    }
    if let Some(options) = &xkb_config.options
        && options.len() > MAX_FIELD_LEN
    {
        return Err(GreeterError::InvalidXkbConfig(format!(
            "options too long ({} bytes)",
            options.len()
        )));
    }

    Ok(())
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

    // Persist a keyboard layout chosen in the greeter into the target user's
    // cosmic-comp config, so the layout carries over into the started session.
    //
    // The greeter process runs as the unprivileged `cosmic-greeter` user and
    // cannot write into another user's home directory; only this root daemon
    // can, via `run_as_user`. Access to this interface is restricted to the
    // `cosmic-greeter` group and root by the D-Bus policy, the same trust level
    // as `get_user_data`.
    //
    // THREAT MODEL: this method has no notion of authentication. It trusts the
    // D-Bus policy to limit callers to the `cosmic-greeter` group and root, and
    // it trusts the greeter to only call it for a user who has just
    // authenticated (see `App::persist_xkb_config_and_start`, gated on
    // `Message::Login`).
    // Any process running as `cosmic-greeter` can therefore rewrite any login
    // user's keyboard layout without authenticating. That is an accepted,
    // bounded risk: the write is confined to a single, validated `XkbConfig`
    // value in one config key, written as the target user (never root), with no
    // path or content under the caller's control beyond the layout itself, so
    // the worst case is keyboard-layout tampering, not code execution or
    // privilege escalation. We still validate the payload below rather than
    // trusting the caller to have produced something sane.
    fn set_xkb_config(
        &mut self,
        username: String,
        xkb_config_ron: String,
    ) -> Result<(), GreeterError> {
        // Bound the payload before parsing so a caller cannot drive memory/CPU
        // use with a giant RON string. A real xkb_config is well under 1 KiB.
        const MAX_RON_LEN: usize = 64 * 1024;
        if xkb_config_ron.len() > MAX_RON_LEN {
            return Err(GreeterError::InvalidXkbConfig(format!(
                "payload too large ({} bytes)",
                xkb_config_ron.len()
            )));
        }

        let xkb_config: XkbConfig =
            ron::from_str(&xkb_config_ron).map_err(|err| GreeterError::Ron(err.to_string()))?;

        // The greeter's own `build_xkb_config` already upholds these invariants,
        // but this daemon must not trust the caller to have done so.
        validate_xkb_config(&xkb_config)?;

        // Only accept real login users (skips system accounts, nologin, etc.),
        // and never trust the caller-supplied name without matching a passwd
        // entry, which is what gates which files we write as root.
        let user_filter = UserFilter::new();
        let user = /* unsafe */ {
            pwd::Passwd::iter()
                .filter(|user| user_filter.filter(user))
                .find(|user| user.name == username)
        }
        .ok_or_else(|| GreeterError::UnknownUser(format!("unknown user {:?}", username)))?;

        //IMPORTANT: Assume the identity of the user so the config is written as
        // that user and never as root.
        run_as_user(&user, || {
            let config =
                cosmic_config::Config::new("com.system76.CosmicComp", CosmicCompConfig::VERSION)
                    .map_err(|err| format!("failed to open cosmic-comp config: {}", err))?;
            config
                .set("xkb_config", xkb_config)
                .map_err(|err| format!("failed to write xkb_config: {}", err))
        })
        .map_err(|err| GreeterError::RunAsUser(err.to_string()))?
        .map_err(GreeterError::RunAsUser)?;

        Ok(())
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
