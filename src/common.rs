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
use cosmic_greeter_daemon::{BgSource, UserData};
use std::collections::HashMap;
use wayland_client::protocol::wl_output::WlOutput;

pub struct Common<M> {
    pub active_surface_id_opt: Option<SurfaceId>,
    pub core: Core,
    pub error_opt: Option<String>,
    pub input: String,
    pub network_icon_opt: Option<&'static str>,
    pub on_output_event: Option<Box<dyn Fn(OutputEvent, WlOutput) -> M>>,
    pub on_session_lock_event: Option<Box<dyn Fn(SessionLockEvent) -> M>>,
    pub output_names: HashMap<WlOutput, String>,
    pub power_info_opt: Option<(String, f64)>,
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
    Focus(SurfaceId),
    Input(String),
    Key(Modifiers, Key, Option<SmolStr>),
    NetworkIcon(Option<&'static str>),
    OutputEvent(OutputEvent, WlOutput),
    PowerInfo(Option<(String, f64)>),
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

        let app = Self {
            active_surface_id_opt: None,
            core,
            error_opt: None,
            input: String::new(),
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

    pub fn update_wallpapers(&mut self, user_data: &UserData) {
        for (_output, surface_id) in self.surface_ids.iter() {
            if self.surface_images.contains_key(surface_id) {
                continue;
            }

            let Some(output_name) = self.surface_names.get(surface_id) else {
                continue;
            };

            log::info!("updating wallpaper for {:?}", output_name);

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
                                    log::warn!(
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
                            log::warn!("output {}: unsupported source {:?}", output_name, color);
                        }
                    }
                }
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Task<M> {
        match message {
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
            Message::Input(input) => {
                self.input = input;
            }
            Message::Key(modifiers, key, text) => {
                // Uncaptured keys with only shift modifiers go to the password box
                if !modifiers.logo()
                    && !modifiers.control()
                    && !modifiers.alt()
                    && matches!(key, Key::Character(_))
                {
                    if let Some(text) = text {
                        self.input.push_str(&text);
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
                self.network_icon_opt = network_icon_opt;
            }
            Message::OutputEvent(output_event, output) => {
                if let Some(on_output_event) = &self.on_output_event {
                    return Task::done(cosmic::Action::App(on_output_event(output_event, output)));
                }
            }
            Message::PowerInfo(power_info_opt) => {
                self.power_info_opt = power_info_opt;
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
