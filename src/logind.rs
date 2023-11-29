use logind_zbus::manager::ManagerProxy;
use zbus::{Connection, Result};

pub async fn power_off() -> Result<()> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    manager.reboot(false).await
}

pub async fn reboot() -> Result<()> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    manager.reboot(false).await
}

pub async fn suspend() -> Result<()> {
    let connection = Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    manager.suspend(false).await
}
