use cosmic::{
    app::{Core, Task},
    iced::{
        self, Rectangle, Size, Subscription,
        core::SmolStr,
        event::{
            self,
            wayland::{Event as WaylandEvent, OutputEvent, SessionLockEvent},
        },
        keyboard::{Event as KeyEvent, Key, Modifiers},
    },
    iced_runtime::core::window::Id as SurfaceId,
    widget,
};
use cosmic_config::{ConfigSet, CosmicConfigEntry};
use cosmic_greeter_daemon::{BgSource, CosmicCompConfig, UserData};
use std::{collections::HashMap, sync::Arc};
use wayland_client::protocol::wl_output::WlOutput;

pub const DEFAULT_MENU_ITEM_HEIGHT: f32 = 36.;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ActiveLayout {
    pub layout: String,
    pub description: String,
    pub variant: String,
}

pub struct Common<M> {
    pub active_layouts: Vec<ActiveLayout>,
    pub active_surface_id_opt: Option<SurfaceId>,
    pub caps_lock: bool,
    pub comp_config_handler: Option<cosmic_config::Config>,
    pub core: Core,
    pub error_opt: Option<String>,
    pub fallback_background: widget::image::Handle,
    pub layouts_opt: Option<Arc<xkb_data::KeyboardLayouts>>,
    pub network_icon_opt: Option<widget::Icon>,
    pub on_output_event: Option<Box<dyn Fn(OutputEvent, WlOutput) -> M>>,
    pub on_session_lock_event: Option<Box<dyn Fn(SessionLockEvent) -> M>>,
    pub output_names: HashMap<WlOutput, String>,
    pub power_info_opt: Option<(widget::Icon, f64)>,
    pub prompt_opt: Option<(String, bool, Option<String>)>,
    pub subsurface_rects: HashMap<WlOutput, Rectangle>,
    pub surface_ids: HashMap<WlOutput, SurfaceId>,
    pub surface_images: HashMap<SurfaceId, widget::image::Handle>,
    pub surface_names: HashMap<SurfaceId, String>,
    pub text_input_ids: HashMap<String, widget::Id>,
    pub time: crate::time::Time,
    pub window_size: HashMap<SurfaceId, Size>,
}

#[derive(Clone, Debug)]
pub enum Message {
    CapsLock(bool),
    Focus(SurfaceId),
    Key(Modifiers, Key, Option<SmolStr>),
    NetworkIcon(Option<&'static str>),
    OutputEvent(OutputEvent, WlOutput),
    PowerInfo(Option<(String, f64)>),
    Prompt(String, bool, Option<String>),
    SessionLockEvent(SessionLockEvent),
    Tick,
    Tz(chrono_tz::Tz),
}

impl<M: From<Message> + Send + 'static> Common<M> {
    pub fn init(mut core: Core) -> (Self, Task<M>) {
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        // XXX must be false or define custom style to have transparent bg
        core.window.sharp_corners = false;
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.window.use_template = false;

        let comp_config_handler = match cosmic_config::Config::new(
            "com.system76.CosmicComp",
            CosmicCompConfig::VERSION,
        ) {
            Ok(config_handler) => Some(config_handler),
            Err(err) => {
                tracing::error!("failed to create cosmic-comp config handler: {}", err);
                None
            }
        };

        let layouts_opt = match xkb_data::all_keyboard_layouts() {
            Ok(ok) => Some(Arc::new(ok)),
            Err(err) => {
                tracing::warn!("failed to load keyboard layouts: {}", err);
                None
            }
        };

        let app = Self {
            active_layouts: Vec::new(),
            active_surface_id_opt: None,
            caps_lock: false,
            comp_config_handler,
            core,
            error_opt: None,
            fallback_background: widget::image::Handle::from_bytes(
                include_bytes!("../res/background.jpg").as_slice(),
            ),
            layouts_opt,
            network_icon_opt: None,
            on_output_event: None,
            on_session_lock_event: None,
            output_names: HashMap::new(),
            power_info_opt: None,
            prompt_opt: None,
            subsurface_rects: HashMap::new(),
            surface_ids: HashMap::new(),
            surface_images: HashMap::new(),
            surface_names: HashMap::new(),
            text_input_ids: HashMap::new(),
            time: crate::time::Time::new(),
            window_size: HashMap::new(),
        };
        (
            app,
            Task::batch(vec![
                crate::time::tick().map(|_| cosmic::Action::App(Message::Tick.into())),
                crate::time::tz_updates().map(|tz| cosmic::Action::App(Message::Tz(tz).into())),
            ]),
        )
    }

    pub fn set_xkb_config(&self, user_data: &UserData) {
        if let Some(mut xkb_config) = user_data.xkb_config_opt.clone() {
            xkb_config.layout = String::new();
            xkb_config.variant = String::new();
            for (i, layout) in self.active_layouts.iter().enumerate() {
                if i > 0 {
                    xkb_config.layout.push(',');
                    xkb_config.variant.push(',');
                }
                xkb_config.layout.push_str(&layout.layout);
                xkb_config.variant.push_str(&layout.variant);
            }
            if let Some(comp_config_handler) = &self.comp_config_handler {
                match comp_config_handler.set("xkb_config", xkb_config) {
                    Ok(()) => tracing::info!("updated cosmic-comp xkb_config"),
                    Err(err) => tracing::error!("failed to update cosmic-comp xkb_config: {}", err),
                }
            }
        }
    }

    pub fn update_wallpapers(&mut self, user_data: &UserData) {
        for (_output, surface_id) in self.surface_ids.iter() {
            if self.surface_images.contains_key(surface_id) {
                continue;
            }

            let Some(output_name) = self.surface_names.get(surface_id) else {
                continue;
            };

            tracing::info!("updating wallpaper for {:?}", output_name);

            for (wallpaper_output_name, wallpaper_source) in user_data.bg_state.wallpapers.iter() {
                if wallpaper_output_name == output_name {
                    match wallpaper_source {
                        BgSource::Path(path) => {
                            match user_data.bg_path_data.get(path) {
                                Some(bytes) => {
                                    let image = widget::image::Handle::from_bytes(bytes.clone());
                                    self.surface_images.insert(*surface_id, image);
                                    //TODO: what to do about duplicates?
                                }
                                None => {
                                    tracing::warn!(
                                        "output {}: failed to find wallpaper data for source {:?}",
                                        output_name,
                                        path
                                    );
                                }
                            }
                            break;
                        }
                        BgSource::Color(color) => {
                            //TODO: support color sources
                            tracing::warn!(
                                "output {}: unsupported source {:?}",
                                output_name,
                                color
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn update_user_data(&mut self, user_data: &UserData) {
        self.update_wallpapers(user_data);

        // From cosmic-applet-input-sources
        if let Some(keyboard_layouts) = &self.layouts_opt {
            if let Some(xkb_config) = &user_data.xkb_config_opt {
                self.active_layouts.clear();
                let config_layouts = xkb_config.layout.split_terminator(',');
                let config_variants = xkb_config
                    .variant
                    .split_terminator(',')
                    .chain(std::iter::repeat(""));
                'outer: for (config_layout, config_variant) in config_layouts.zip(config_variants) {
                    for xkb_layout in keyboard_layouts.layouts() {
                        if config_layout != xkb_layout.name() {
                            continue;
                        }
                        if config_variant.is_empty() {
                            let active_layout = ActiveLayout {
                                description: xkb_layout.description().to_owned(),
                                layout: config_layout.to_owned(),
                                variant: config_variant.to_owned(),
                            };
                            self.active_layouts.push(active_layout);
                            continue 'outer;
                        }

                        let Some(xkb_variants) = xkb_layout.variants() else {
                            continue;
                        };
                        for xkb_variant in xkb_variants {
                            if config_variant != xkb_variant.name() {
                                continue;
                            }
                            let active_layout = ActiveLayout {
                                description: xkb_variant.description().to_owned(),
                                layout: config_layout.to_owned(),
                                variant: config_variant.to_owned(),
                            };
                            self.active_layouts.push(active_layout);
                            continue 'outer;
                        }
                    }
                }
                tracing::info!("{:?}", self.active_layouts);
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Task<M> {
        match message {
            Message::CapsLock(caps_lock) => {
                self.caps_lock = caps_lock;
            }
            Message::Focus(surface_id) => {
                self.active_surface_id_opt = Some(surface_id);
                if let Some(text_input_id) = self
                    .surface_names
                    .get(&surface_id)
                    .and_then(|id| self.text_input_ids.get(id))
                {
                    return widget::text_input::focus(text_input_id.clone());
                }
            }
            Message::Key(modifiers, key, text) => {
                // Uncaptured keys with only shift modifiers go to the password box
                if !modifiers.logo()
                    && !modifiers.control()
                    && !modifiers.alt()
                    && matches!(key, Key::Character(_))
                {
                    if let Some(text) = text {
                        if let Some((_, _, Some(value))) = &mut self.prompt_opt {
                            value.push_str(&text);
                        }
                    }

                    if let Some(surface_id) = self.active_surface_id_opt {
                        if let Some(text_input_id) = self
                            .surface_names
                            .get(&surface_id)
                            .and_then(|id| self.text_input_ids.get(id))
                        {
                            return widget::text_input::focus(text_input_id.clone());
                        }
                    }
                }
            }
            Message::NetworkIcon(network_icon_opt) => {
                self.network_icon_opt =
                    network_icon_opt.map(|name| widget::icon::from_name(name).into());
            }
            Message::OutputEvent(output_event, output) => {
                if let Some(on_output_event) = &self.on_output_event {
                    return Task::done(cosmic::Action::App(on_output_event(output_event, output)));
                }
            }
            Message::PowerInfo(power_info_opt) => {
                self.power_info_opt = power_info_opt
                    .map(|(name, level)| (widget::icon::from_name(name).into(), level));
            }
            Message::Prompt(prompt, secret, value_opt) => {
                let prompt_was_none = self.prompt_opt.is_none();
                self.prompt_opt = Some((prompt, secret, value_opt));
                if prompt_was_none {
                    if let Some(surface_id) = self.active_surface_id_opt {
                        if let Some(text_input_id) = self
                            .surface_names
                            .get(&surface_id)
                            .and_then(|id| self.text_input_ids.get(id))
                        {
                            tracing::info!("focus surface found id {:?}", text_input_id);
                            return widget::text_input::focus(text_input_id.clone());
                        }
                    }
                }
            }
            Message::SessionLockEvent(lock_event) => {
                if let Some(on_session_lock_event) = &self.on_session_lock_event {
                    return Task::done(cosmic::Action::App(on_session_lock_event(lock_event)));
                }
            }
            Message::Tick => {
                self.time.tick();
            }
            Message::Tz(tz) => {
                self.time.set_tz(tz);
            }
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = Vec::with_capacity(3);

        subscriptions.push(event::listen_with(|event, status, id| match event {
            iced::Event::Keyboard(KeyEvent::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) => match status {
                event::Status::Ignored => Some(Message::Key(modifiers, key, text)),
                event::Status::Captured => None,
            },
            iced::Event::Keyboard(KeyEvent::ModifiersChanged(modifiers)) => {
                Some(Message::CapsLock(modifiers.contains(Modifiers::CAPS_LOCK)))
            }
            iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
                wayland_event,
            )) => match wayland_event {
                WaylandEvent::Output(output_event, output) => {
                    Some(Message::OutputEvent(output_event, output))
                }
                WaylandEvent::SessionLock(lock_event) => {
                    Some(Message::SessionLockEvent(lock_event))
                }
                _ => None,
            },
            iced::Event::Window(iced::window::Event::Focused) => Some(Message::Focus(id)),
            _ => None,
        }));

        #[cfg(feature = "networkmanager")]
        {
            subscriptions.push(crate::networkmanager::subscription().map(Message::NetworkIcon));
        }

        #[cfg(feature = "upower")]
        {
            subscriptions.push(crate::upower::subscription().map(Message::PowerInfo));
        }

        Subscription::batch(subscriptions)
    }
}
