use anyhow::bail;
use async_fn_stream::StreamEmitter;
use cosmic::widget::{column, text};
use cosmic::{Element, Task, style};
use futures_util::StreamExt;
use icu::datetime::input::DateTime;
use icu::datetime::options::TimePrecision;
use icu::datetime::{DateTimeFormatter, DateTimeFormatterPreferences, fieldsets};
use icu::locale::Locale;
use icu::locale::preferences::extensions::unicode::keywords::HourCycle;
use jiff::tz::TimeZone;
use jiff_icu::ConvertFrom;
use std::time::Duration;
use timedate_zbus::TimeDateProxy;
use tokio::time;

#[derive(Debug, Clone)]
pub struct Time {
    locale: Locale,
    timezone: Option<TimeZone>,
    now: jiff::Zoned,
}

impl Time {
    pub fn new() -> Self {
        fn get_local() -> Locale {
            for var in ["LC_TIME", "LC_ALL", "LANG"] {
                if let Ok(locale_str) = std::env::var(var) {
                    let cleaned_locale = locale_str
                        .split('.')
                        .next()
                        .unwrap_or(&locale_str)
                        .replace('_', "-");

                    if let Ok(locale) = Locale::try_from_str(&cleaned_locale) {
                        return locale;
                    }

                    // Try language-only fallback (e.g., "en" from "en-US")
                    if let Some(lang) = cleaned_locale.split('-').next()
                        && let Ok(locale) = Locale::try_from_str(lang)
                    {
                        return locale;
                    }
                }
            }
            tracing::warn!("No valid locale found in environment, using fallback");
            Locale::try_from_str("en-US").expect("Failed to parse fallback locale 'en-US'")
        }

        let locale = get_local();
        let now = jiff::Zoned::now();

        Self {
            locale,
            timezone: None,
            now,
        }
    }

    pub fn set_tz(&mut self, tz: TimeZone) {
        self.timezone = Some(tz);
        self.tick();
    }

    pub fn tick(&mut self) {
        self.now = self
            .timezone
            .as_ref()
            .map(|tz| jiff::Timestamp::now().to_zoned(tz.clone()))
            .unwrap_or_else(jiff::Zoned::now);
    }

    pub fn format_date(&self) -> String {
        let prefs = DateTimeFormatterPreferences::from(&self.locale);
        let dtf = DateTimeFormatter::try_new(prefs, fieldsets::MDE::long()).unwrap();
        dtf.format(&DateTime::convert_from(self.now.datetime()))
            .to_string()
    }

    pub fn format_time(&self, military_time: bool) -> String {
        let mut prefs = DateTimeFormatterPreferences::from(&self.locale);
        prefs.hour_cycle = Some(if military_time {
            HourCycle::H23
        } else {
            HourCycle::H12
        });
        let dtf = DateTimeFormatter::try_new(
            prefs,
            fieldsets::T::medium().with_time_precision(TimePrecision::Minute),
        )
        .unwrap();
        dtf.format(&DateTime::convert_from(self.now.datetime()))
            .to_string()
    }

    pub fn date_time_widget<'a, M: 'a>(&self, military_time: bool) -> Element<'a, M> {
        Element::from(
            column::with_capacity(2)
                .padding(16.)
                .spacing(12.0)
                .push(text::title2(self.format_date()).class(style::Text::Accent))
                .push(
                    text(self.format_time(military_time))
                        .size(if military_time { 112. } else { 75. })
                        .class(style::Text::Accent),
                ),
        )
    }
}

pub fn tz_updates() -> Task<TimeZone> {
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
            let current = jiff::Zoned::now().second() as u64 % 60;
            if current != 0 {
                timer.reset_after(time::Duration::from_secs(60 - current));
            }
        }
    }))
}

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
