// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic_greeter::{greeter, locker};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Add CLI arguments managements with `clap`
    let matches = clap::Command::new("cosmic-greeter")
        .version(env!("CARGO_PKG_VERSION"))
        .about("COSMIC Greeter")
        .long_about("A login and lock screen manager designed for the COSMIC desktop environment. \nFor more information, visit the GitHub repository at https://github.com/pop-os/cosmic-greeter.")
        .get_matches();

    // Argument verification
    if matches.contains_id("version") {
        println!("cosmic-greeter {}", APP_VERSION);
        return Ok(());
    }
	
    match pwd::Passwd::current_user() {
        Some(current_user) => match current_user.name.as_str() {
            "cosmic-greeter" => greeter::main(),
            _ => locker::main(current_user),
        },
        _ => Err("failed to determine current user".into()),
    }
}
