use cosmic::cosmic_theme::{
    palette::{rgb::Rgb, Srgba},
    ThemeBuilder,
};

/**
 * Old Pop Os theme
 * gtk/src/light/gtk-3.0/_colors-pop.scss
 * gnome-shell/src/gnome-shell-sass/_common.scss
 */
pub fn get_theme() -> cosmic_greeter_daemon::Theme {
    ThemeBuilder::dark()
        // #303030
        .bg_color(Srgba::new(0.188, 0.188, 0.188, 1.0))
        // #cccccc
        .text_tint(Rgb::new(0.8, 0.8, 0.8))
        // #424242
        .neutral_tint(Rgb::new(0.208, 0.208, 0.208))
        // #94ebeb
        .accent(Rgb::new(0.58, 0.922, 0.922))
        // #90cfb0
        .success(Rgb::new(0.565, 0.812, 0.69))
        // #fff19e
        .warning(Rgb::new(1.0, 0.945, 0.62))
        // #ea9090
        .destructive(Rgb::new(0.918, 0.565, 0.565))
        .build()
}
