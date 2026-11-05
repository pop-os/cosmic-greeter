// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use core::{mem::MaybeUninit, ptr, str};
use std::thread;

use async_fn_stream::StreamEmitter;
use jiff::tz::TimeZone;
use rustix::{
    event::kqueue::{Event, EventFilter, EventFlags, VnodeEvents, kevent, kqueue},
    fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    fs::{self, Mode, OFlags, SeekFrom},
    io::{self, Errno},
};
use tokio::sync::watch::{Sender, channel};
use tracing::warn;

pub async fn tz_stream(emitter: &StreamEmitter<TimeZone>) -> anyhow::Result<()> {
    let (sender, mut receiver) = channel(TimeZone::unknown());
    let handle = thread::spawn(move || watch_zone_info(sender));

    loop {
        if receiver.changed().await.is_err() {
            break;
        }
        let tz = receiver.borrow().clone();
        emitter.emit(tz).await;
    }

    handle.join().unwrap().map_err(anyhow::Error::new)
}

fn watch_zone_info(sender: Sender<TimeZone>) -> Result<(), Errno> {
    // FreeBSD writes the plain text timezone to /var/db/zoneinfo. There is also /etc/localtime
    // which is a symlink to the exact timezone.
    let mut zonefd = open_zone_info()?;
    let kq = kqueue()?;
    let mut watch = zone_info_event(zonefd.as_fd());
    let mut events = [MaybeUninit::<Event>::uninit(); 1];

    let mut tz_cur = TimeZone::system();
    let mut tz_buf = [0u8; 64];

    // Send the initial TimeZone to match Linux.
    sender.send_replace(tz_cur.clone());

    loop {
        // SAFETY: `zonefd` is owned by this function so it isn't closed before kevent returns.
        let ([event], []) = (unsafe { kevent(&kq, &[watch], &mut events[..], None)? }) else {
            continue;
        };
        let EventFilter::Vnode { flags, .. } = event.filter() else {
            unreachable!();
        };

        // If zoneinfo is renamed or deleted, which can happen if it's atomically replaced, then
        // zonefd must be reopened to point to the correct inode. Ditto for the watch event which
        // needs the new file descriptor.
        if flags.intersects(VnodeEvents::DELETE | VnodeEvents::RENAME) {
            zonefd = open_zone_info()?;
            watch = zone_info_event(zonefd.as_fd());
        }

        if fs::seek(&zonefd, SeekFrom::Start(0))
            .inspect_err(|e| warn!(%e, "failed to seek /var/db/zoneinfo to start"))
            .is_ok()
            && let Ok(tz_nrd) = io::read(&zonefd, &mut tz_buf)
                .inspect_err(|e| warn!(%e, "failed to read from /var/db/zoneinfo"))
            && let Ok(tz_str) = str::from_utf8(&tz_buf[..tz_nrd])
            && let Ok(tz_jif) = TimeZone::get(tz_str.trim())
            && tz_cur != tz_jif
        {
            // Close gracefully on shutdown.
            if sender.send(tz_jif.clone()).is_err() {
                break;
            }
            tz_cur = tz_jif;
        }
    }

    Ok(())
}

fn open_zone_info() -> Result<OwnedFd, Errno> {
    fs::open(
        c"/var/db/zoneinfo",
        OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDONLY,
        Mode::empty(),
    )
}

fn zone_info_event(zonefd: BorrowedFd<'_>) -> Event {
    Event::new(
        EventFilter::Vnode {
            vnode: zonefd.as_raw_fd(),
            flags: VnodeEvents::DELETE | VnodeEvents::RENAME | VnodeEvents::WRITE,
        },
        EventFlags::ADD | EventFlags::CLEAR,
        ptr::null_mut(),
    )
}
