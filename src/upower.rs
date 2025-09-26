use cosmic::iced::{
    Subscription,
    futures::{SinkExt, StreamExt, channel::mpsc},
};
use std::{any::TypeId, time::Duration};
use upower_dbus::UPowerProxy;
use zbus::{Connection, Result};

pub fn subscription() -> Subscription<Option<(String, f64)>> {
    struct PowerSubscription;

    Subscription::run_with_id(
        TypeId::of::<PowerSubscription>(),
        cosmic::iced_futures::stream::channel(16, |mut msg_tx| async move {
            match handler(&mut msg_tx).await {
                Ok(()) => {}
                Err(err) => {
                    tracing::warn!("upower error: {}", err);
                    //TODO: send error
                }
            }

            // If reading power status failed, clear power icon
            msg_tx.send(None).await.unwrap();

            //TODO: should we retry on error?
            futures_util::future::pending().await
        }),
    )
}

//TODO: use never type?
pub async fn handler(msg_tx: &mut mpsc::Sender<Option<(String, f64)>>) -> Result<()> {
    let zbus = Connection::system().await?;
    let upower = UPowerProxy::new(&zbus).await?;
    let dev = upower.get_display_device().await?;

    let mut icon_name_changed = dev.receive_icon_name_changed().await;
    let mut percentage_changed = dev.receive_percentage_changed().await;
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        let mut info_opt = None;

        if let Ok(percent) = dev.percentage().await {
            if let Ok(icon_name) = dev.icon_name().await {
                if !icon_name.is_empty() && !icon_name.eq("battery-missing-symbolic") {
                    info_opt = Some((icon_name, percent));
                }
            }
        }

        msg_tx.send(info_opt).await.unwrap();

        // Waits until icon or percentage have changed, and at least one second has passed.
        futures_util::future::select(icon_name_changed.next(), percentage_changed.next()).await;
        interval.tick().await;
    }
}
