// Copyright 2026 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

//! Minimal `com.system76.CosmicSettingsDaemon` service for the greeter session.
//!
//! `Super+Space` (switch keyboard layout) is handled by cosmic-comp, which
//! reacts to the shortcut by spawning the configured `system_actions` command:
//!
//! ```text
//! busctl --user call com.system76.CosmicSettingsDaemon \
//!     /com/system76/CosmicSettingsDaemon \
//!     com.system76.CosmicSettingsDaemon InputSourceSwitch
//! ```
//!
//! In a normal user session that call is serviced by cosmic-settings-daemon,
//! which rotates the active keyboard layout. cosmic-settings-daemon does not run
//! inside the greeter session, so the call has no receiver and the shortcut does
//! nothing. We provide the `InputSourceSwitch` method ourselves and let the
//! greeter perform the same layout rotation it does for the dropdown.

use cosmic::iced::futures::SinkExt;
use cosmic::iced::futures::channel::mpsc;
use cosmic::iced::{Subscription, stream};
use std::any::TypeId;
use std::future::pending;
use zbus::connection::Builder;

/// D-Bus object exposing the subset of `com.system76.CosmicSettingsDaemon` that
/// cosmic-comp's `Super+Space` shortcut relies on.
struct InputSource {
    msg_tx: mpsc::Sender<()>,
}

#[zbus::interface(name = "com.system76.CosmicSettingsDaemon")]
impl InputSource {
    /// Cycle to the next keyboard layout. Forwarded to the greeter's update
    /// loop, which rotates the active layouts and applies the new xkb config.
    async fn input_source_switch(&self) {
        if let Err(err) = self.msg_tx.clone().send(()).await {
            tracing::warn!("failed to forward InputSourceSwitch: {}", err);
        }
    }
}

async fn serve(msg_tx: mpsc::Sender<()>) -> zbus::Result<()> {
    let _conn = Builder::session()?
        .name("com.system76.CosmicSettingsDaemon")?
        .serve_at("/com/system76/CosmicSettingsDaemon", InputSource { msg_tx })?
        .build()
        .await?;

    // Hold the connection (and therefore the bus name) for the greeter's
    // lifetime so the shortcut keeps working.
    pending::<()>().await;
    Ok(())
}

/// Serve a minimal `com.system76.CosmicSettingsDaemon` on the session bus so the
/// `Super+Space` keyboard-layout shortcut works inside the greeter. Each
/// `InputSourceSwitch` call yields a unit value.
pub fn subscription() -> Subscription<()> {
    struct InputSourceSubscription;

    Subscription::run_with(TypeId::of::<InputSourceSubscription>(), |_| {
        stream::channel(16, |msg_tx| async move {
            if let Err(err) = serve(msg_tx).await {
                tracing::warn!("input source switch service error: {}", err);
            }

            // Don't respawn the service on failure (e.g. the name is already
            // owned by a real cosmic-settings-daemon).
            pending::<()>().await
        })
    })
}
