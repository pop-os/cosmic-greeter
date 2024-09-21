use std::any::TypeId;

use cosmic::iced::{
    futures::{channel::mpsc, SinkExt, StreamExt},
    subscription, Subscription,
};
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
        let icon = match mp.online_state().await.unwrap_or_default().as_str() {
            // TODO: traverse systemd-networkd's links/networks to determine wireless-versus-wired
            // "partial" mean some links are online, let's assume this is good enough
            // see: https://www.freedesktop.org/software/systemd/man/latest/networkctl.html
            "online" | "partial" => NetworkIcon::Wired,
            _ => NetworkIcon::None,
        };

        msg_tx.send(Some(icon.name())).await.unwrap();

        // Waits until active connections have changed and at least one second has passed
        tokio::join!(
            online_state_changed.next(),
            time::sleep(time::Duration::from_secs(3))
        );
    }
}
