//! Keeps logind from suspending the machine on lid close while an external display is
//! connected.
//!
//! cosmic-comp already takes a `handle-lid-switch` inhibitor in that situation, but it only
//! lives as long as a compositor process does. Two windows are left uncovered:
//!
//! * from boot until the greeter's compositor has configured its outputs, and
//! * between the greeter's compositor exiting and the user session's compositor starting.
//!
//! logind does not help there: it only counts an external connector as "docked" when the
//! connector is *enabled*, meaning a CRTC is driving it, and with no compositor running none is.
//! logind then falls back to `HandleLidSwitch=suspend`, and with the lid closed it suspends
//! the machine as soon as nothing inhibits it. Opening the lid resumes it.
//!
//! The daemon outlives both compositors, so it holds the inhibitor for as long as an external
//! connector is connected. cosmic-comp keeps its own inhibitor for the lid handling it does,
//! so the two do not conflict.

use std::fs;
use std::path::Path;
use std::time::Duration;

use tracing::{debug, warn};
use zbus::zvariant::OwnedFd;
use zbus::{Connection, proxy};

const DRM_SYSFS: &str = "/sys/class/drm";
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
}

/// Same definition of "internal" as `OutputExt::is_internal` in cosmic-comp.
fn is_internal_connector(connector: &str) -> bool {
    connector.starts_with("eDP-") || connector.starts_with("LVDS-") || connector.starts_with("DSI-")
}

/// Whether any non-internal connector under `drm_dir` reports `connected`.
fn external_display_connected(drm_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(drm_dir) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let name = entry.file_name();
        // Connectors are named `card<N>-<connector>`, e.g. `card1-HDMI-A-1`.
        let Some(connector) = name
            .to_str()
            .and_then(|name| name.strip_prefix("card"))
            .and_then(|name| name.split_once('-'))
            .map(|(_, connector)| connector)
        else {
            return false;
        };

        !is_internal_connector(connector)
            && fs::read_to_string(entry.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
    })
}

pub async fn run(conn: Connection) {
    let manager = match ManagerProxy::new(&conn).await {
        Ok(manager) => manager,
        Err(err) => {
            warn!(
                ?err,
                "Failed to connect to logind, not inhibiting lid switch"
            );
            return;
        }
    };

    let mut inhibitor: Option<OwnedFd> = None;
    let mut warned = false;
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let connected = external_display_connected(Path::new(DRM_SYSFS));
        match (connected, inhibitor.is_some()) {
            (true, false) => {
                match manager
                    .inhibit(
                        "handle-lid-switch",
                        "COSMIC Greeter",
                        "External display connected",
                        "block",
                    )
                    .await
                {
                    Ok(fd) => {
                        debug!("Inhibiting lid switch");
                        inhibitor = Some(fd);
                        warned = false;
                    }
                    Err(err) => {
                        if !warned {
                            warn!(?err, "Failed to inhibit lid switch");
                            warned = true;
                        }
                    }
                }
            }
            (false, true) => {
                debug!("Removing inhibitor-lock on lid switch");
                inhibitor = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_drm(test: &str, entries: &[(&str, Option<&str>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cosmic-greeter-lid-inhibit-{}-{}",
            std::process::id(),
            test
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (name, status) in entries {
            let path = dir.join(name);
            fs::create_dir_all(&path).unwrap();
            if let Some(status) = status {
                fs::write(path.join("status"), format!("{status}\n")).unwrap();
            }
        }
        dir
    }

    #[test]
    fn external_connected() {
        let dir = fake_drm(
            "external_connected",
            &[
                ("card1", None),
                ("card1-eDP-1", Some("connected")),
                ("card1-HDMI-A-1", Some("connected")),
            ],
        );
        assert!(external_display_connected(&dir));
    }

    #[test]
    fn internal_panel_only() {
        let dir = fake_drm(
            "internal_only",
            &[
                ("card1-eDP-1", Some("connected")),
                ("card1-LVDS-1", Some("connected")),
                ("card1-DSI-1", Some("connected")),
            ],
        );
        assert!(!external_display_connected(&dir));
    }

    #[test]
    fn external_disconnected() {
        let dir = fake_drm(
            "external_disconnected",
            &[
                ("card1-eDP-1", Some("connected")),
                ("card1-HDMI-A-1", Some("disconnected")),
                ("card1-DP-1", Some("unknown")),
            ],
        );
        assert!(!external_display_connected(&dir));
    }

    #[test]
    fn ignores_non_connector_entries() {
        let dir = fake_drm(
            "non_connector",
            &[("card1", Some("connected")), ("version", Some("connected"))],
        );
        assert!(!external_display_connected(&dir));
    }

    #[test]
    fn missing_directory() {
        assert!(!external_display_connected(Path::new(
            "/nonexistent/cosmic-greeter-lid-inhibit"
        )));
    }
}
