// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use anyhow::bail;
use async_fn_stream::StreamEmitter;
use futures_util::StreamExt;
use jiff::tz::TimeZone;
use timedate1_zbus::TimeDateProxy;

pub async fn tz_stream(emitter: &StreamEmitter<TimeZone>) -> anyhow::Result<()> {
    let Ok(conn) = zbus::Connection::system().await else {
        bail!("No zbus system connection.");
    };
    let Ok(proxy) = TimeDateProxy::new(&conn).await else {
        bail!("No timezone proxy");
    };

    // The stream always returns the current timezone as its first item even if it wasn't
    // updated. If the proxy is recreated in a loop somehow, the resulting stream will
    // always yield an update immediately which could lead to spammed false updates.
    let mut s = proxy.receive_timezone_changed().await;

    while let Some(property) = s.next().await {
        let Ok(tz) = property.get().await else {
            bail!("Failed to get property");
        };
        let Ok(tz) = TimeZone::get(&tz) else {
            bail!("Failed to parse timezone.");
        };
        emitter.emit(tz).await;
    }
    bail!("Timezone property stream ended.");
}
