// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic_greeter::{greeter, locker};

use lexopt::{Parser, Arg};
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!("Usage: cosmic-greeter");
    println!("\nCOSMIC Greeter");
    println!("A login and lock screen manager designed for the COSMIC desktop environment. \nFor more information, visit the GitHub repository at https://github.com/pop-os/cosmic-greeter.");
    println!("\nOptions:");
    println!("  --help     Show this message");
    println!("  --version  Show the version of cosmic-greeter");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut parser = Parser::from_env();

    // Parse the arguments
    while let Some(arg) = parser.next()? {
        match arg {
            Arg::Long("help") => {
                print_help();
                return Ok(());
            }
            Arg::Long("version") => {
                println!("cosmic-greeter {}", APP_VERSION);
                return Ok(());
            }
            _ => {}
        }
    }
	
    match pwd::Passwd::current_user() {
        Some(current_user) => match current_user.name.as_str() {
            "cosmic-greeter" => greeter::main(),
            _ => locker::main(current_user),
        },
        _ => Err("failed to determine current user".into()),
    }

}
