// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

mod greeter;
mod image_container;
mod locker;

#[cfg(feature = "logind")]
mod logind;

#[cfg(feature = "networkmanager")]
mod networkmanager;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    match pwd::Passwd::current_user() {
        Some(current_user) => match current_user.name.as_str() {
            "greeter" => greeter::main(),
            _ => locker::main(current_user),
        },
        _ => Err("failed to determine current user".into()),
    }
}
