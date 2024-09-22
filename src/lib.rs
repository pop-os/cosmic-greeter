// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

pub mod greeter;
pub mod locker;

mod image_container;
mod localize;

#[cfg(feature = "logind")]
mod logind;

#[cfg(feature = "networkmanager")]
mod networkmanager;

#[cfg(feature = "systemd")]
mod systemd;

#[cfg(feature = "upower")]
mod upower;
