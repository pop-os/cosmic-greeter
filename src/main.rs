// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

mod greeter;
mod image_container;
mod locker;

#[cfg(feature = "logind")]
mod logind;

#[cfg(feature = "networkmanager")]
mod networkmanager;

mod theme;
#[cfg(feature = "upower")]
mod upower;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    match pwd::Passwd::current_user() {
        Some(current_user) => match current_user.name.as_str() {
            "cosmic-greeter" => greeter::main(),
            _ => locker::main(current_user),
        },
        _ => Err("failed to determine current user".into()),
    }
}
