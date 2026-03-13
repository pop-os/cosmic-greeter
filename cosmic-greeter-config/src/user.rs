// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

/// Per user state for Greeter.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct UserState {
    pub uid: NonZeroU32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_session: Option<String>,
}
