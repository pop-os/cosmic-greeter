use cosmic::iced::{
    futures::{channel::mpsc, SinkExt, StreamExt},
    subscription, Subscription,
};
use std::any::TypeId;
use tokio::time;
use upower_dbus::UPowerProxy;
use zbus::{Connection, Result};

pub fn subscription() -> Subscription<Option<(String, f64)>> {
    struct PowerSubscription;

    subscription::channel(
        TypeId::of::<PowerSubscription>(),
        16,
        |mut msg_tx| async move {
            match handler(&mut msg_tx).await {
                Ok(()) => {}
                Err(err) => {
                    log::warn!("upower error: {}", err);
                    //TODO: send error
                }
            }

            // If reading power status failed, clear power icon
            msg_tx.send(None).await.unwrap();

            //TODO: should we retry on error?
            loop {
                time::sleep(time::Duration::new(60, 0)).await;
            }
        },
    )
}

//TODO: use never type?
pub async fn handler(msg_tx: &mut mpsc::Sender<Option<(String, f64)>>) -> Result<()> {
    let zbus = Connection::system().await?;
    let upower = UPowerProxy::new(&zbus).await?;
    let dev = upower.get_display_device().await?;

    let mut icon_name_changed = dev.receive_icon_name_changed().await;
    let mut percentage_changed = dev.receive_percentage_changed().await;
    loop {
        let mut info_opt = None;

        if let Ok(percent) = dev.percentage().await {
            if let Ok(icon_name) = dev.icon_name().await {
                if !icon_name.is_empty() {
                    info_opt = Some((icon_name, percent));
                }
            }
        }

        msg_tx.send(info_opt).await.unwrap();

        // Waits until icon or percentage have changed
        tokio::select!(_ = icon_name_changed.next() => (), _ = percentage_changed.next() => ());
    }
}
