use cosmic::iced::{
    Subscription,
    futures::{SinkExt, StreamExt, channel::mpsc},
    stream,
};
use futures_util::select;
use std::{any::TypeId, time::Duration};
use upower_dbus::{BatteryState, BatteryType, UPowerProxy};
use zbus::{Connection, Result};

pub fn subscription() -> Subscription<Option<(f64, bool, bool)>> {
    struct PowerSubscription;

    Subscription::run_with(TypeId::of::<PowerSubscription>(), |_| {
        stream::channel(16, |mut msg_tx| async move {
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
        })
    })
}

//TODO: use never type?
pub async fn handler(msg_tx: &mut mpsc::Sender<Option<(f64, bool, bool)>>) -> Result<()> {
    let zbus = Connection::system().await?;
    let upower = UPowerProxy::new(&zbus).await?;
    let dev = upower.get_display_device().await?;

    let mut percentage_changed = dev.receive_percentage_changed().await.boxed().fuse();
    let mut state_changed = dev.receive_state_changed().await.boxed().fuse();
    let mut charge_threshold_enabled_changed = dev
        .receive_charge_threshold_enabled_changed()
        .await
        .boxed()
        .fuse();
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    let has_battery = dev.type_().await? == BatteryType::Battery && dev.power_supply().await?;
    if !has_battery {
        return Ok(());
    }
    loop {
        let mut info_opt = None;

        if let Ok(mut percent) = dev.percentage().await {
            if let Ok(state) = dev.state().await {
                let threshold_enabled = dev.charge_threshold_enabled().await.unwrap_or_default();
                let mut capacity = dev.capacity().await.unwrap_or(100.);
                if capacity <= 1. {
                    capacity = 100.;
                }

                // compensate for declining battery capacity
                percent = percent * 100. / capacity;
                if matches!(state, BatteryState::FullyCharged) || percent >= 100. {
                    percent = 100.;
                }

                info_opt = Some((
                    percent,
                    state == BatteryState::Discharging,
                    threshold_enabled,
                ));
            }
        }

        msg_tx.send(info_opt).await.unwrap();

        // Waits until icon or percentage have changed, and at least one second has passed.
        select! {
            _ = state_changed.next() => {},
            _ = percentage_changed.next() => {},
            _ = charge_threshold_enabled_changed.next() => {}
        }
        interval.tick().await;
    }
}
