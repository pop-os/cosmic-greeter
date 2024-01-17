use cosmic::iced::{
    futures::{channel::mpsc, SinkExt, StreamExt},
    subscription, Subscription,
};
use cosmic_dbus_networkmanager::{device::SpecificDevice, nm::NetworkManager};
use std::{any::TypeId, cmp};
use tokio::time;
use zbus::{Connection, Result};

#[derive(Clone, Copy, Debug)]
pub enum NetworkIcon {
    None,
    Wired,
    Wireless(u8),
}

impl NetworkIcon {
    pub fn name(&self) -> &'static str {
        match self {
            NetworkIcon::None => "network-wired-disconnected-symbolic",
            NetworkIcon::Wired => "network-wired-symbolic",
            NetworkIcon::Wireless(strength) => {
                if *strength < 25 {
                    "network-wireless-signal-weak-symbolic"
                } else if *strength < 50 {
                    "network-wireless-signal-ok-symbolic"
                } else if *strength < 75 {
                    "network-wireless-signal-good-symbolic"
                } else {
                    "network-wireless-signal-excellent-symbolic"
                }
            }
        }
    }
}

pub fn subscription() -> Subscription<Option<&'static str>> {
    struct NetworkSubscription;

    subscription::channel(
        TypeId::of::<NetworkSubscription>(),
        16,
        |mut msg_tx| async move {
            match handler(&mut msg_tx).await {
                Ok(()) => {}
                Err(err) => {
                    log::warn!("networkmanager error: {}", err);
                    //TODO: send error
                }
            }

            // If reading network status failed, clear network icon
            msg_tx.send(None).await.unwrap();

            //TODO: should we retry on error?
            loop {
                time::sleep(time::Duration::new(60, 0)).await;
            }
        },
    )
}

//TODO: use never type?
pub async fn handler(msg_tx: &mut mpsc::Sender<Option<&'static str>>) -> Result<()> {
    let zbus = Connection::system().await?;
    let nm = NetworkManager::new(&zbus).await?;

    let mut active_conns_changed = nm.receive_active_connections_changed().await;
    loop {
        let mut icon = NetworkIcon::None;

        for conn in nm.active_connections().await.unwrap_or_default() {
            for dev in conn.devices().await.unwrap_or_default() {
                match dev.downcast_to_device().await.unwrap_or_default() {
                    //TODO: more specific devices
                    Some(SpecificDevice::Wired(_)) => {
                        // Wired only overrides None
                        icon = match icon {
                            NetworkIcon::None => NetworkIcon::Wired,
                            other => other,
                        };
                    }
                    Some(SpecificDevice::Wireless(wireless)) => {
                        if let Ok(ap) = wireless.active_access_point().await {
                            if let Ok(strength) = ap.strength().await {
                                // Wireless always overrides with the highest strength
                                icon = match icon {
                                    NetworkIcon::Wireless(other_strength) => {
                                        NetworkIcon::Wireless(cmp::max(strength, other_strength))
                                    }
                                    _ => NetworkIcon::Wireless(strength),
                                };
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        msg_tx.send(Some(icon.name())).await.unwrap();

        // Waits until active connections have changed and at least one second has passed
        tokio::join!(
            active_conns_changed.next(),
            time::sleep(time::Duration::from_secs(1))
        );
    }
}
