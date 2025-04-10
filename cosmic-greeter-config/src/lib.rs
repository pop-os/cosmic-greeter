// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

pub mod user;

use std::{collections::HashMap, num::NonZeroU32};

use cosmic_config::{cosmic_config_derive::CosmicConfigEntry, CosmicConfigEntry};
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "com.system76.CosmicGreeter";
pub const CONFIG_VERSION: u64 = 1;

#[derive(Debug, Clone, Default, PartialEq, CosmicConfigEntry, Deserialize, Serialize)]
#[version = 1]
#[id = "com.system76.CosmicGreeter"]
pub struct Config {
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub users: HashMap<NonZeroU32, user::UserState>,
}

impl Config {
    pub fn load() -> (Self, Option<cosmic_config::Config>) {
        crate::load()
    }
}

pub(crate) fn load<C>() -> (C, Option<cosmic_config::Config>)
where
    C: Default + CosmicConfigEntry,
{
    match cosmic_config::Config::new(APP_ID, CONFIG_VERSION) {
        Ok(handler) => {
            let config = C::get_entry(&handler)
                .inspect_err(|(errors, _)| {
                    for err in errors.iter().filter(|err| err.is_err()) {
                        log::error!("{err}")
                    }
                })
                .unwrap_or_else(|(_, config)| config);
            (config, Some(handler))
        }
        Err(e) => {
            log::error!("Failed to get settings for `{APP_ID}` (v {CONFIG_VERSION}): {e:?}");
            (C::default(), None)
        }
    }
}
