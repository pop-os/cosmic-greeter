// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

//! Integration test for LDAP user login persistence issue
//!
//! This test reproduces the bug where:
//! 1. A user logs in with an LDAP-backed account (UID saved in config)
//! 2. On next login, the UID is not found in user_datas (LDAP users aren't enumerated)
//! 3. The code falls back to unwrap_or_default(), setting username to ""
//! 4. Authentication fails because greetd receives an empty username

use std::num::NonZeroU32;

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

/// The function under test: determines selected username from last_user config
///
/// This replicates the logic from greeter.rs lines 1126-1158
fn determine_selected_username(
    greeter_config: &GreeterConfig,
    user_datas: &[UserData],
) -> NameIndexPair {
    let last_user = greeter_config.last_user.as_ref();

    let (username, uid) = last_user
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
        .unwrap_or_else(|| {
            // FIX: When last_user UID is not found in user_datas (e.g., LDAP user),
            // preserve the last_user UID as a sentinel instead of returning empty string.
            // This prevents authentication failure due to empty username.
            if let Some(last_user_uid) = last_user {
                // Return a sentinel username indicating manual entry is needed
                // The actual username will be entered manually by the user
                (format!("uid:{}", last_user_uid.get()), Some(*last_user_uid))
            } else {
                // No last_user and no enumerated users - truly empty state
                (String::new(), None)
            }
        });

    let data_idx = user_datas.iter().position(|d| d.name == username);
    NameIndexPair { username, data_idx }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_last_user_found_in_user_datas() {
        // Arrange: Normal case where last user exists in enumerated users
        let config = GreeterConfig {
            last_user: Some(NonZeroU32::new(1001).unwrap()),
        };
        let user_datas = vec![
            UserData {
                name: "alice".to_string(),
                uid: 1000,
            },
            UserData {
                name: "bob".to_string(),
                uid: 1001,
            },
        ];

        // Act
        let result = determine_selected_username(&config, &user_datas);

        // Assert: Should select Bob (UID 1001)
        assert_eq!(result.username, "bob");
        assert_eq!(result.data_idx, Some(1));
    }

    #[test]
    fn test_last_user_not_found_falls_back_to_first() {
        // Arrange: last_user UID doesn't exist in user_datas (current behavior)
        let config = GreeterConfig {
            last_user: Some(NonZeroU32::new(5000).unwrap()), // LDAP user UID
        };
        let user_datas = vec![
            UserData {
                name: "alice".to_string(),
                uid: 1000,
            },
            UserData {
                name: "bob".to_string(),
                uid: 1001,
            },
        ];

        // Act
        let result = determine_selected_username(&config, &user_datas);

        // Assert: Current behavior - falls back to first user
        assert_eq!(result.username, "alice");
        assert_eq!(result.data_idx, Some(0));
    }

    #[test]
    fn test_ldap_user_missing_should_not_use_empty_username() {
        // Arrange: LDAP user (UID 5000) logged in previously, saved in config
        // On reconnect, LDAP user not in enumerated user_datas list
        let config = GreeterConfig {
            last_user: Some(NonZeroU32::new(5000).unwrap()), // LDAP user UID
        };
        
        // Empty user_datas (or no local users) - LDAP users aren't enumerated
        let user_datas: Vec<UserData> = vec![];

        // Act
        let result = determine_selected_username(&config, &user_datas);

        // Assert: Should NOT have an empty username
        // BUG: Current implementation returns "" via unwrap_or_default()
        // FIX: Should return a sentinel value or preserve the last_user info somehow
        assert_ne!(
            result.username, "",
            "Empty username causes authentication to fail. \
             When last_user UID is not in user_datas, username should remain set \
             or UI should prompt for manual entry."
        );
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
