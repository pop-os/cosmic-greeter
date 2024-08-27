// SPDX-License-Identifier: GPL-3.0-only

use std::{
    str::FromStr,
    sync::{LazyLock, OnceLock},
};

use i18n_embed::{
    fluent::{fluent_language_loader, FluentLanguageLoader},
    DefaultLocalizer, LanguageLoader, Localizer,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

pub static LANGUAGE_LOADER: OnceLock<FluentLanguageLoader> = OnceLock::new();
pub static LANGUAGE_CHRONO: LazyLock<chrono::Locale> = LazyLock::new(|| {
    std::env::var("LC_TIME")
        .ok()
        .or_else(|| std::env::var("LANG").ok())
        .and_then(|locale_full| {
            // Split LANG because it may be set to a locale such as en_US.UTF8
            locale_full
                .split('.')
                .next()
                .and_then(|locale| chrono::Locale::from_str(locale).ok())
        })
        .unwrap_or(chrono::Locale::en_US)
});

#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        i18n_embed_fl::fl!($crate::localize::LANGUAGE_LOADER.get().unwrap(), $message_id)
    }};

    ($message_id:literal, $($args:expr),*) => {{
        i18n_embed_fl::fl!($crate::localize::LANGUAGE_LOADER.get().unwrap(), $message_id, $($args), *)
    }};
}

// Get the `Localizer` to be used for localizing this library.
pub fn localizer() -> Box<dyn Localizer> {
    LANGUAGE_LOADER.get_or_init(|| {
        let loader: FluentLanguageLoader = fluent_language_loader!();

        loader
            .load_fallback_language(&Localizations)
            .expect("Error while loading fallback language");

        loader
    });

    Box::from(DefaultLocalizer::new(
        LANGUAGE_LOADER.get().unwrap(),
        &Localizations,
    ))
}

pub fn localize() {
    let localizer = localizer();
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();

    if let Err(error) = localizer.select(&requested_languages) {
        eprintln!("Error while loading language for App List {}", error);
    }
}
