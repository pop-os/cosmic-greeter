// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic_greeter::{greeter, locker};

use lexopt::{Arg, Parser};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut parser = Parser::from_env();

    // Parse the arguments
    while let Some(arg) = parser.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => {
                print_help(env!("CARGO_PKG_VERSION"), env!("VERGEN_GIT_SHA"));
                return Ok(());
            }
            Arg::Short('v') | Arg::Long("version") => {
                println!(
                    "cosmic-greeter {} (git commit {})",
                    env!("CARGO_PKG_VERSION"),
                    env!("VERGEN_GIT_SHA")
                );
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

fn print_help(version: &str, git_rev: &str) {
    println!(
        r#"cosmic-greeter {version} (git commit {git_rev})
System76 <info@system76.com>

Designed for the COSMIC™ desktop environment, cosmic-greeter is a libcosmic
frontend for greetd which can be run inside of cosmic-comp.

Project home page: https://github.com/pop-os/cosmic-greeter

Options:
  -h, --help     Show this message
  -v, --version  Show the version of cosmic-greeter"#
    );
}
