use cosmic::iced::{
    futures::{channel::mpsc, SinkExt, StreamExt},
    subscription, Subscription,
};
use logind_zbus::{manager::ManagerProxy, session::SessionProxy};
use std::{any::TypeId, error::Error, process};
use tokio::time;
use zbus::Connection;

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

pub fn subscription() -> Subscription<bool> {
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
pub async fn handler(msg_tx: &mut mpsc::Sender<bool>) -> Result<(), Box<dyn Error>> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    let session_path = manager.get_session_by_PID(process::id()).await?;
    let session = SessionProxy::builder(&connection)
        .path(&session_path)?
        .build()
        .await?;

    let mut lock = session.receive_lock().await?;
    let mut unlock = session.receive_unlock().await?;
    loop {
        // Waits until lock or unlock signals have been received
        tokio::select!(_ = lock.next() =>  {
            msg_tx.send(true).await?;
        }, _ = unlock.next() => {
            msg_tx.send(false).await?;
        });
    }
}
