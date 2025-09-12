use cosmic::iced::{
    Subscription,
    futures::{SinkExt, StreamExt, channel::mpsc},
};
use logind_zbus::{
    manager::{InhibitType, ManagerProxy},
    session::SessionProxy,
};
use std::{any::TypeId, error::Error, os::fd::OwnedFd, sync::Arc, time::Duration};
use zbus::Connection;

use crate::{common, locker::Message};

pub async fn power_off() -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    manager.power_off(false).await
}

pub async fn reboot() -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    manager.reboot(false).await
}

pub async fn suspend() -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    manager.suspend(false).await
}

async fn inhibit(manager: &ManagerProxy<'_>) -> zbus::Result<OwnedFd> {
    let what = InhibitType::Sleep;
    let who = "COSMIC Greeter";
    let why = "COSMIC Greeter needs to display a lock screen";
    let mode = "delay";
    //TODO: update logind-zbus to fix inhibit signature
    let fd: zbus::zvariant::OwnedFd = manager
        .inner()
        .call("Inhibit", &(what, who, why, mode))
        .await?;
    // Have to convert to std type to avoid leaking zbus dependency
    Ok(fd.into())
}

pub fn subscription() -> Subscription<Message> {
    struct LogindSubscription;

    Subscription::run_with_id(
        TypeId::of::<LogindSubscription>(),
        cosmic::iced_futures::stream::channel(16, |mut msg_tx| async move {
            match handler(&mut msg_tx).await {
                Ok(()) => {}
                Err(err) => {
                    tracing::warn!("logind error: {}", err);
                    //TODO: send error
                }
            }

            std::process::exit(1);
        }),
    )
}

//TODO: use never type?
pub async fn handler(msg_tx: &mut mpsc::Sender<Message>) -> Result<(), Box<dyn Error>> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    let session_path = manager
        .get_session_by_PID(std::os::unix::process::parent_id())
        .await?;
    let session = SessionProxy::builder(&connection)
        .path(&session_path)?
        .build()
        .await?;

    let mut inhibit_opt = Some(inhibit(&manager).await?);
    let mut prepare_for_sleep = manager.receive_prepare_for_sleep().await?;
    let mut lock = session.receive_lock().await?;
    let mut unlock = session.receive_unlock().await?;

    let mut interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        // Waits until a signal has been received
        tokio::select!(
            signal_opt = prepare_for_sleep.next() => {
                match signal_opt {
                    Some(signal) => match signal.args() {
                        Ok(args) => {
                            if args.start {
                                tracing::info!("logind prepare for sleep");
                                if let Some(inhibit) = inhibit_opt.take() {
                                    msg_tx.send(Message::Inhibit(Arc::new(inhibit))).await?;
                                }
                                msg_tx.send(Message::Lock).await?;
                            } else {
                                tracing::info!("logind resume");
                                if inhibit_opt.is_none() {
                                    inhibit_opt = Some(inhibit(&manager).await?);
                                }
                                // Immediately update time when resuming from sleep.
                                msg_tx.send(Message::Common(common::Message::Tick)).await?;
                            }
                        },
                        Err(err) => {
                            tracing::warn!("logind prepare to sleep invalid data: {}", err);
                        }
                    },
                    None => {
                        tracing::warn!("logind prepare to sleep missing data");
                    }
                }
            },
            _ = lock.next() =>  {
            tracing::info!("logind lock");
            msg_tx.send(Message::Lock).await?;
        }, _ = unlock.next() => {
            tracing::info!("logind unlock");
            msg_tx.send(Message::Unlock).await?;
        });

        interval.tick().await;
    }
}
