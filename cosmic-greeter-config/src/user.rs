// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

/// Per user state for Greeter.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct UserState {
    #[serde(skip_serializing_if = "invalid_uid")]
    pub uid: NonZeroU32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_session: Option<String>,
}

// Only serialize users not system accounts
const fn invalid_uid(uid: &NonZeroU32) -> bool {
    uid.get() < 1000
}
