// Copyright 2025 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

//! Detection helpers for fprintd (`net.reactivated.Fprint`)
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, proxy};

#[proxy(
    interface = "net.reactivated.Fprint.Manager",
    default_service = "net.reactivated.Fprint",
    default_path = "/net/reactivated/Fprint/Manager"
)]
trait FprintManager {
    /// Returns the default reader, or errors if there is no device.
    fn get_default_device(&self) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "net.reactivated.Fprint.Device",
    default_service = "net.reactivated.Fprint"
)]
trait FprintDevice {
    /// Fingers enrolled for `username`, errors if none are enrolled.
    fn list_enrolled_fingers(&self, username: &str) -> zbus::Result<Vec<String>>;
}

/// True if a default reader exists and `username` has at least one enrolled
/// finger.
pub async fn fingerprint_available(username: &str) -> bool {
    match check(username).await {
        Ok(available) => available,
        Err(err) => {
            tracing::info!("fingerprint unavailable: {}", err);
            false
        }
    }
}

async fn check(username: &str) -> zbus::Result<bool> {
    let connection = Connection::system().await?;

    let manager = FprintManagerProxy::new(&connection).await?;
    let device_path = manager.get_default_device().await?;

    let device = FprintDeviceProxy::builder(&connection)
        .path(device_path)?
        .build()
        .await?;

    let fingers = match device.list_enrolled_fingers(username).await {
        Ok(fingers) => fingers,
        Err(err) => {
            tracing::info!("no enrolled fingerprints for {}: {}", username, err);
            return Ok(false);
        }
    };

    Ok(!fingers.is_empty())
}
