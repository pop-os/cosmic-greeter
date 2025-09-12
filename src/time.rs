use std::{str::FromStr, time::Duration};

use anyhow::bail;
use async_fn_stream::StreamEmitter;
use chrono::{Datelike, Timelike};
use cosmic::{
    Task,
    iced_core::Element,
    style,
    widget::{self, column, text::title2},
};
use futures_util::StreamExt;
use icu::{
    calendar::DateTime,
    datetime::{
        DateTimeFormatter, DateTimeFormatterOptions,
        options::{
            components::{self, Bag},
            preferences,
        },
    },
    locid::Locale,
};
use timedate_zbus::TimeDateProxy;
use tokio::time;

#[derive(Debug, Clone)]
pub struct Time {
    locale: Locale,
    timezone: Option<chrono_tz::Tz>,
    now: chrono::DateTime<chrono::FixedOffset>,
}

impl Time {
    pub fn new() -> Self {
        fn get_local() -> Result<Locale, Box<dyn std::error::Error>> {
            let locale = std::env::var("LC_TIME").or_else(|_| std::env::var("LANG"))?;
            let locale = locale
                .split('.')
                .next()
                .ok_or(format!("Can't split the locale {locale}"))?;

            let locale = Locale::from_str(locale).map_err(|e| format!("{e:?}"))?;
            Ok(locale)
        }

        let locale = match get_local() {
            Ok(locale) => locale,
            Err(e) => {
                tracing::error!("can't get locale {e}");
                Locale::default()
            }
        };
        let now = chrono::Local::now().fixed_offset();

        Self {
            locale,
            timezone: None,
            now,
        }
    }

    pub fn set_tz(&mut self, tz: chrono_tz::Tz) {
        self.timezone = Some(tz);
        self.tick();
    }

    pub fn tick(&mut self) {
        self.now = self
            .timezone
            .map(|tz| chrono::Local::now().with_timezone(&tz).fixed_offset())
            .unwrap_or_else(|| chrono::Local::now().into());
    }

    pub fn format<D: Datelike>(&self, bag: Bag, date: &D) -> String {
        let options = DateTimeFormatterOptions::Components(bag);

        let dtf =
            DateTimeFormatter::try_new_experimental(&self.locale.clone().into(), options).unwrap();

        let datetime = DateTime::try_new_gregorian_datetime(
            date.year(),
            date.month() as u8,
            date.day() as u8,
            // hack cause we know that we will only use "now"
            // when we need hours (NaiveDate don't support this functions)
            self.now.hour() as u8,
            self.now.minute() as u8,
            self.now.second() as u8,
        )
        .unwrap()
        .to_iso()
        .to_any();

        dtf.format(&datetime)
            .expect("can't format value")
            .to_string()
    }

    pub fn date_time_widget<'a, M: 'a>(&self, military_time: bool) -> cosmic::Element<'a, M> {
        let mut top_bag = Bag::empty();

        top_bag.weekday = Some(components::Text::Long);

        top_bag.day = Some(components::Day::NumericDayOfMonth);
        top_bag.month = Some(components::Month::Long);

        let mut bottom_bag = Bag::empty();

        bottom_bag.hour = Some(components::Numeric::Numeric);
        bottom_bag.minute = Some(components::Numeric::Numeric);

        let hour_cycle = if military_time {
            preferences::HourCycle::H23
        } else {
            preferences::HourCycle::H12
        };

        bottom_bag.preferences = Some(preferences::Bag::from_hour_cycle(hour_cycle));

        Element::from(
            column()
                .padding(16.)
                .spacing(12.0)
                .push(title2(self.format(top_bag, &self.now)).class(style::Text::Accent))
                .push(
                    widget::text(self.format(bottom_bag, &self.now))
                        .size(if military_time { 112. } else { 75. })
                        .class(style::Text::Accent),
                ),
        )
    }
}

pub fn tz_updates() -> Task<chrono_tz::Tz> {
    Task::stream(async_fn_stream::fn_stream(|emitter| async move {
        loop {
            if let Err(err) = tz_stream(&emitter).await {
                tracing::error!("{err:?}");
            }
            _ = time::sleep(Duration::from_secs(60)).await;
        }
    }))
}

pub fn tick() -> Task<()> {
    Task::stream(async_fn_stream::fn_stream(|emitter| async move {
        let mut timer = time::interval(time::Duration::from_secs(60));
        timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        emitter.emit(()).await;
        loop {
            timer.tick().await;
            emitter.emit(()).await;

            // Calculate a delta if we're ticking per minute to keep ticks stable
            // Based on i3status-rust
            let current = chrono::Local::now().second() as u64 % 60;
            if current != 0 {
                timer.reset_after(time::Duration::from_secs(60 - current));
            }
        }
    }))
}

pub async fn tz_stream(emitter: &StreamEmitter<chrono_tz::Tz>) -> anyhow::Result<()> {
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
        let Ok(tz) = tz.parse::<chrono_tz::Tz>() else {
            bail!("Failed to parse timezone.");
        };
        emitter.emit(tz).await;
    }
    bail!("Timezone property stream ended.");
}
