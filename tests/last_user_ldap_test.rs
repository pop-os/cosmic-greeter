// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

//! Integration test for LDAP user login persistence issue
//!
//! This test reproduces the bug where:
//! 1. A user logs in with an LDAP-backed account (UID saved in config)
//! 2. On next login, the UID is not found in user_datas (LDAP users aren't enumerated)
//! 3. The code should use pwd::Passwd::from_uid() to look up the user directly
//! 4. This works for LDAP users that aren't in the enumerated list

use std::num::NonZeroU32;
use pwd::Passwd;

/// Simulates the user data structure from the daemon
#[derive(Clone, Debug)]
struct UserData {
    name: String,
    uid: u32,
}

/// Simulates the greeter config with last_user
#[derive(Clone, Debug, Default)]
struct GreeterConfig {
    last_user: Option<NonZeroU32>,
}

/// Represents the selected username state
#[derive(Clone, Debug, Default, PartialEq)]
struct NameIndexPair {
    username: String,
    data_idx: Option<usize>,
}

/// ORIGINAL BUGGY VERSION: determines selected username from last_user config
///
/// This replicates the BUGGY logic from greeter.rs lines 1126-1142
/// that returns empty username when last_user UID is not in user_datas
#[allow(dead_code)]
fn determine_selected_username_buggy(
    greeter_config: &GreeterConfig,
    user_datas: &[UserData],
) -> NameIndexPair {
    let last_user = greeter_config.last_user.as_ref();

    let (username, _uid) = last_user
        .and_then(|last_user| {
            user_datas
                .iter()
                .find(|d| d.uid == last_user.get())
                .map(|x| (x.name.clone(), NonZeroU32::new(x.uid)))
        })
        .or_else(|| {
            user_datas
                .first()
                .map(|x| (x.name.clone(), NonZeroU32::new(x.uid)))
        })
        .unwrap_or_default(); // BUG: Returns ("", None) when no match!

    let data_idx = user_datas.iter().position(|d| d.name == username);
    NameIndexPair { username, data_idx }
}

/// FIXED VERSION: determines selected username using pwd::Passwd::from_uid()
///
/// This demonstrates the CORRECT approach: when last_user UID is not found
/// in user_datas, query the system user database directly via passwd
fn determine_selected_username(
    greeter_config: &GreeterConfig,
    user_datas: &[UserData],
) -> NameIndexPair {
    let last_user = greeter_config.last_user.as_ref();

    let (username, _uid) = last_user
        .and_then(|last_user| {
            // First try to find in enumerated user_datas
            user_datas
                .iter()
                .find(|d| d.uid == last_user.get())
                .map(|x| (x.name.clone(), NonZeroU32::new(x.uid)))
        })
        .or_else(|| {
            // If not in user_datas but we have a last_user UID,
            // query passwd directly (this handles LDAP users!)
            last_user.and_then(|last_user_uid| {
                Passwd::from_uid(last_user_uid.get())
                    .map(|passwd| (passwd.name, Some(*last_user_uid)))
            })
        })
        .or_else(|| {
            // Final fallback: first enumerated user
            user_datas
                .first()
                .map(|x| (x.name.clone(), NonZeroU32::new(x.uid)))
        })
        .unwrap_or_default(); // Only empty if truly no users anywhere

    let data_idx = user_datas.iter().position(|d| d.name == username);
    NameIndexPair { username, data_idx }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test data constants
    const LOCAL_USER_1_UID: u32 = 1000;
    const LOCAL_USER_2_UID: u32 = 1001;
    const LDAP_USER_UID: u32 = 5000; // LDAP users typically have higher UIDs

    /// Helper to create standard test user data
    fn create_test_users() -> Vec<UserData> {
        vec![
            UserData {
                name: "alice".to_string(),
                uid: LOCAL_USER_1_UID,
            },
            UserData {
                name: "bob".to_string(),
                uid: LOCAL_USER_2_UID,
            },
        ]
    }

    #[test]
    fn test_last_user_found_in_user_datas() {
        // Arrange: Normal case where last user exists in enumerated users
        let config = GreeterConfig {
            last_user: NonZeroU32::new(LOCAL_USER_2_UID),
        };
        let user_datas = create_test_users();

        // Act
        let result = determine_selected_username(&config, &user_datas);

        // Assert: Should select Bob (UID 1001)
        assert_eq!(result.username, "bob");
        assert_eq!(result.data_idx, Some(1));
    }

    #[test]
    fn test_last_user_not_found_falls_back_to_first() {
        // Arrange: last_user UID doesn't exist in user_datas but other users present
        let config = GreeterConfig {
            last_user: NonZeroU32::new(LDAP_USER_UID),
        };
        let user_datas = create_test_users();

        // Act
        let result = determine_selected_username(&config, &user_datas);

        // Assert: Falls back to first user when LDAP user not found but locals exist
        assert_eq!(result.username, "alice");
        assert_eq!(result.data_idx, Some(0));
    }

    #[test]
    fn test_uid_not_found_anywhere_returns_empty() {
        // Arrange: UID that doesn't exist in user_datas OR passwd database
        let config = GreeterConfig {
            last_user: NonZeroU32::new(LDAP_USER_UID), // UID 5000 unlikely to exist
        };
        
        // Empty user_datas - simulates LDAP users not being enumerated
        let user_datas: Vec<UserData> = vec![];

        // Act
        let result = determine_selected_username(&config, &user_datas);

        // Assert: When UID exists nowhere, falls back to empty string as last resort
        // This is acceptable because:
        // 1. It tried user_datas (not found)
        // 2. It tried passwd lookup (not found)
        // 3. No users are available to fall back to
        // The real fix is tested in test_real_system_user_lookup_via_passwd
        assert_eq!(
            result.username, "",
            "When UID doesn't exist anywhere and no users available, \
             should return empty string as last resort."
        );
        assert_eq!(result.data_idx, None);
    }

    #[test]
    fn test_real_system_user_lookup_via_passwd() {
        // Arrange: Use current user's UID (which WILL exist in passwd)
        let current_user = Passwd::current_user()
            .expect("Failed to get current user for test");
        
        let config = GreeterConfig {
            last_user: NonZeroU32::new(current_user.uid),
        };
        
        // Empty user_datas - simulate LDAP user not enumerated
        let user_datas: Vec<UserData> = vec![];

        // Act
        let result = determine_selected_username(&config, &user_datas);

        // Assert: Should successfully look up via passwd
        assert_eq!(result.username, current_user.name);
        assert_eq!(result.data_idx, None); // Not in user_datas
    }

    #[test]
    fn test_buggy_version_returns_empty_username() {
        // This test documents the BUG in the original implementation
        let config = GreeterConfig {
            last_user: NonZeroU32::new(LDAP_USER_UID),
        };
        let user_datas: Vec<UserData> = vec![];

        // Act with BUGGY version
        let result = determine_selected_username_buggy(&config, &user_datas);

        // Assert: Buggy version DOES return empty username (this is the problem!)
        assert_eq!(result.username, "");
        assert_eq!(result.data_idx, None);
    }

    #[test]
    fn test_no_last_user_no_enumerated_users() {
        // Arrange: Fresh install, no last user, no enumerated users
        let config = GreeterConfig { last_user: None };
        let user_datas: Vec<UserData> = vec![];

        // Act
        let result = determine_selected_username(&config, &user_datas);

        // Assert: Empty is acceptable here since it's a fresh state
        assert_eq!(result.username, "");
        assert_eq!(result.data_idx, None);
    }
}
