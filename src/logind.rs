use cosmic::iced::{
    futures::{channel::mpsc, SinkExt, StreamExt},
    subscription, Subscription,
};
use logind_zbus::{
    manager::{InhibitType, ManagerProxy},
    session::SessionProxy,
};
use std::{
    any::TypeId,
    error::Error,
    os::fd::{FromRawFd, IntoRawFd, OwnedFd},
    process,
    sync::Arc,
};
use tokio::time;
use zbus::Connection;

use crate::locker::Message;

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
    Ok(unsafe { OwnedFd::from_raw_fd(fd.into_raw_fd()) })
}

pub fn subscription() -> Subscription<Message> {
    struct LogindSubscription;

    subscription::channel(
        TypeId::of::<LogindSubscription>(),
        16,
        |mut msg_tx| async move {
            match handler(&mut msg_tx).await {
                Ok(()) => {}
                Err(err) => {
                    log::warn!("logind error: {}", err);
                    //TODO: send error
                }
            }

            //TODO: should we retry on error?
            loop {
                time::sleep(time::Duration::new(60, 0)).await;
            }
        },
    )
}

//TODO: use never type?
pub async fn handler(msg_tx: &mut mpsc::Sender<Message>) -> Result<(), Box<dyn Error>> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    let session_path = manager.get_session_by_PID(process::id()).await?;
    let session = SessionProxy::builder(&connection)
        .path(&session_path)?
        .build()
        .await?;

    let mut inhibit_opt = Some(inhibit(&manager).await?);
    let mut prepare_for_sleep = manager.receive_prepare_for_sleep().await?;
    let mut lock = session.receive_lock().await?;
    let mut unlock = session.receive_unlock().await?;
    loop {
        // Waits until a signal has been received
        tokio::select!(
            signal_opt = prepare_for_sleep.next() => {
                match signal_opt {
                    Some(signal) => match signal.args() {
                        Ok(args) => {
                            if args.start {
                                log::info!("logind prepare for sleep");
                                if let Some(inhibit) = inhibit_opt.take() {
                                    msg_tx.send(Message::Inhibit(Arc::new(inhibit))).await?;
                                }
                                msg_tx.send(Message::Lock).await?;
                            } else {
                                log::info!("logind resume");
                                if inhibit_opt.is_none() {
                                    inhibit_opt = Some(inhibit(&manager).await?);
                                }
                            }
                        },
                        Err(err) => {
                            log::warn!("logind prepare to sleep invalid data: {}", err);
                        }
                    },
                    None => {
                        log::warn!("logind prepare to sleep missing data");
                    }
                }
            },
            _ = lock.next() =>  {
            log::info!("logind lock");
            msg_tx.send(Message::Lock).await?;
        }, _ = unlock.next() => {
            log::info!("logind unlock");
            msg_tx.send(Message::Unlock).await?;
        });
    }
}
