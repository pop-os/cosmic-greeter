// Copyright 2025 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

//! When the greeter is launched with the cosmic-initial-setup user, the greeter
//! will check if the system needs to perform an initial setup. If no user accounts
//! are found, or the initial setup mode was explicitly requested, a temporary
//! COSMIC environment will be launched with the cosmic-initial-setup user to create
//! a user account for the system.

use cosmic_greeter_daemon::UserData;
use std::os::unix::process::CommandExt;

use super::greeter;
use std::path::Path;

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        if condition_met().await? {
            unsafe {
                std::env::set_var("XDG_SESSION_TYPE", "wayland");
                std::env::set_var("XDG_CURRENT_DESKTOP", "COSMIC");
                std::env::set_var("XDG_SESSION_DESKTOP", "COSMIC");
            }

            std::fs::create_dir_all(Path::new("/run/cosmic-initial-setup/.config/cosmic")).unwrap();

            _ = std::process::Command::new("bash")
                .args(&["-c", "start-cosmic --in-login-shell"])
                .exec();
        }

        Ok(())
    })
}

/// Check if the initial setup session should execute
async fn condition_met() -> zbus::Result<bool> {
    if kernel_cmdline_enabled() {
        return Ok(true);
    }

    user_accounts_found().await
}

/// If user accounts are found, the initial setup can be skipped.
async fn user_accounts_found() -> zbus::Result<bool> {
    let connection = zbus::Connection::system().await?;
    let mut proxy = crate::greeter::GreeterProxy::new(&connection).await?;

    _ = proxy.initial_setup_start().await;

    let reply = proxy.get_user_data().await?;

    let user_datas = match ron::from_str::<Vec<UserData>>(&reply) {
        Ok(ok) => ok,
        Err(err) => {
            log::error!("failed to load user data from daemon: {}", err);
            greeter::user_data_fallback()
        }
    };

    Ok(user_datas.is_empty())
}

/// Check if the initial setup mode was requested via the kernel command line.
fn kernel_cmdline_enabled() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .unwrap_or_default()
        .contains("cosmic.initial-setup=1")
}
