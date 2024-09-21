use std::any::TypeId;

use cosmic::iced::{
    futures::{channel::mpsc, SinkExt, StreamExt},
    subscription, Subscription,
};
use serde::Deserialize;
use tokio::time;
use zbus::{Connection, Result};
use zbus_systemd::network1::ManagerProxy;

use crate::networkmanager::NetworkIcon;

pub fn subscription() -> Subscription<Option<&'static str>> {
    struct NetworkSubscription;

    subscription::channel(
        TypeId::of::<NetworkSubscription>(),
        16,
        |mut msg_tx| async move {
            match handler(&mut msg_tx).await {
                Ok(()) => {}
                Err(err) => {
                    log::warn!("systemd-networkd error: {}", err);
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
    let mp = ManagerProxy::new(&zbus).await?;

    let mut online_state_changed = mp.receive_online_state_changed().await;
    loop {
        match mp.online_state().await.unwrap_or_default().as_str() {
            "online" | "partial" => {
                // "partial" mean some links are online, let's assume this is good enough
                // see: https://www.freedesktop.org/software/systemd/man/latest/networkctl.html
            }
            _ => {
                continue;
            }
        };

        let mut icon = NetworkIcon::None;

        for (link_id, _, _) in mp.list_links().await.unwrap_or_default() {
            let link_json = mp.describe_link(link_id).await.unwrap_or_default();
            if let Ok(link) = serde_json::from_str::<Link>(&link_json) {
                if link.online_state != OnlineState::Online {
                    continue;
                }
                // Wired only overrides None
                if icon == NetworkIcon::None && link.r#type == LinkType::Ether {
                    icon = NetworkIcon::Wired;
                }
                // Wireless always overrides with the highest strength
                if link.r#type == LinkType::Wlan {
                    icon = NetworkIcon::Wireless(100);
                    // TODO: determine wireless signal strength
                }
            }
        }

        msg_tx.send(Some(icon.name())).await.unwrap();

        // Waits until active connections have changed and at least one second has passed
        tokio::join!(
            online_state_changed.next(),
            time::sleep(time::Duration::from_secs(3))
        );
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct Link {
    online_state: OnlineState,
    r#type: LinkType,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
enum LinkType {
    Ether,
    Loopback,
    Wlan,
    Other(String),
}

// see: https://www.freedesktop.org/software/systemd/man/latest/networkctl.html
#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
enum OnlineState {
    Partial,
    Offline,
    Online,
    Unknown,
    Other(String),
}
