// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::app::{message, Command, Core, Settings};
use cosmic::{
    executor,
    iced::{
        self, alignment,
        event::{
            self,
            wayland::{Event as WaylandEvent, OutputEvent, SessionLockEvent},
        },
        futures::{self, SinkExt},
        subscription,
        wayland::session_lock::{destroy_lock_surface, get_lock_surface, lock, unlock},
        Length, Subscription,
    },
    iced_runtime::core::window::Id as SurfaceId,
    style, widget, Element,
};
use cosmic_config::CosmicConfigEntry;
use std::{
    any::TypeId,
    collections::HashMap,
    ffi::{CStr, CString},
    fs,
    os::fd::OwnedFd,
    path::Path,
    process,
    sync::Arc,
};
use tokio::{sync::mpsc, task, time};
use wayland_client::{protocol::wl_output::WlOutput, Proxy};

pub fn main(current_user: pwd::Passwd) -> Result<(), Box<dyn std::error::Error>> {
    //TODO: use accountsservice
    let icon_path = Path::new("/var/lib/AccountsService/icons").join(&current_user.name);
    let icon_opt = if icon_path.is_file() {
        match fs::read(&icon_path) {
            Ok(icon_data) => Some(widget::image::Handle::from_memory(icon_data)),
            Err(err) => {
                log::error!("failed to read {:?}: {:?}", icon_path, err);
                None
            }
        }
    } else {
        None
    };

    let mut wallpapers = Vec::new();
    match cosmic_bg_config::state::State::state() {
        Ok(helper) => match cosmic_bg_config::state::State::get_entry(&helper) {
            Ok(state) => {
                wallpapers = state.wallpapers;
            }
            Err(err) => {
                log::error!("failed to load cosmic-bg state: {:?}", err);
            }
        },
        Err(err) => {
            log::error!("failed to create cosmic-bg state helper: {:?}", err);
        }
    }

    let flags = Flags {
        current_user,
        icon_opt,
        wallpapers,
    };

    let settings = Settings::default().no_main_window(true);

    cosmic::app::run::<App>(settings, flags)?;

    Ok(())
}

pub fn pam_thread(username: String, conversation: Conversation) -> Result<(), pam_client::Error> {
    //TODO: send errors to GUI, restart process

    // Create PAM context
    //TODO: search for and use custom context?
    let mut context = pam_client::Context::new("login", Some(&username), conversation)?;

    // Authenticate the user (ask for password, 2nd-factor token, fingerprint, etc.)
    log::info!("authenticate");
    context.authenticate(pam_client::Flag::NONE)?;

    // Validate the account (is not locked, expired, etc.)
    log::info!("acct_mgmt");
    context.acct_mgmt(pam_client::Flag::NONE)?;

    Ok(())
}

pub struct Conversation {
    msg_tx: futures::channel::mpsc::Sender<Message>,
    value_rx: mpsc::Receiver<String>,
}

impl Conversation {
    fn prompt_value(
        &mut self,
        prompt_c: &CStr,
        secret: bool,
    ) -> Result<CString, pam_client::ErrorCode> {
        let prompt = prompt_c.to_str().map_err(|err| {
            log::error!("failed to convert prompt to UTF-8: {:?}", err);
            pam_client::ErrorCode::CONV_ERR
        })?;

        futures::executor::block_on(async {
            self.msg_tx
                .send(Message::Prompt(
                    prompt.to_string(),
                    secret,
                    Some(String::new()),
                ))
                .await
        })
        .map_err(|err| {
            log::error!("failed to send prompt: {:?}", err);
            pam_client::ErrorCode::CONV_ERR
        })?;

        let value = self.value_rx.blocking_recv().ok_or_else(|| {
            log::error!("failed to receive value: channel closed");
            pam_client::ErrorCode::CONV_ERR
        })?;

        CString::new(value).map_err(|err| {
            log::error!("failed to convert value to C string: {:?}", err);
            pam_client::ErrorCode::CONV_ERR
        })
    }

    fn message(&mut self, prompt_c: &CStr) -> Result<(), pam_client::ErrorCode> {
        let prompt = prompt_c.to_str().map_err(|err| {
            log::error!("failed to convert prompt to UTF-8: {:?}", err);
            pam_client::ErrorCode::CONV_ERR
        })?;

        futures::executor::block_on(async {
            self.msg_tx
                .send(Message::Prompt(prompt.to_string(), false, None))
                .await
        })
        .map_err(|err| {
            log::error!("failed to send prompt: {:?}", err);
            pam_client::ErrorCode::CONV_ERR
        })
    }
}

impl pam_client::ConversationHandler for Conversation {
    fn prompt_echo_on(&mut self, prompt_c: &CStr) -> Result<CString, pam_client::ErrorCode> {
        log::info!("prompt_echo_on {:?}", prompt_c);
        self.prompt_value(prompt_c, false)
    }
    fn prompt_echo_off(&mut self, prompt_c: &CStr) -> Result<CString, pam_client::ErrorCode> {
        log::info!("prompt_echo_off {:?}", prompt_c);
        self.prompt_value(prompt_c, true)
    }
    fn text_info(&mut self, prompt_c: &CStr) {
        log::info!("text_info {:?}", prompt_c);
        match self.message(prompt_c) {
            Ok(()) => (),
            Err(err) => {
                log::warn!("failed to send text_info: {:?}", err);
            }
        }
    }
    fn error_msg(&mut self, prompt_c: &CStr) {
        //TODO: treat error type differently?
        log::info!("error_msg {:?}", prompt_c);
        match self.message(prompt_c) {
            Ok(()) => (),
            Err(err) => {
                log::warn!("failed to send error_msg: {:?}", err);
            }
        }
    }
}

#[derive(Clone)]
pub struct Flags {
    current_user: pwd::Passwd,
    icon_opt: Option<widget::image::Handle>,
    wallpapers: Vec<(String, cosmic_bg_config::Source)>,
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    None,
    OutputEvent(OutputEvent, WlOutput),
    SessionLockEvent(SessionLockEvent),
    Channel(mpsc::Sender<String>),
    BackgroundState(cosmic_bg_config::state::State),
    Inhibit(Arc<OwnedFd>),
    NetworkIcon(Option<&'static str>),
    PowerInfo(Option<(String, f64)>),
    Prompt(String, bool, Option<String>),
    Submit,
    Suspend,
    Error(String),
    Lock,
    Unlock,
}

#[derive(Clone, Debug)]
enum State {
    Locking,
    Locked,
    Unlocking,
    Unlocked,
}

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    flags: Flags,
    state: State,
    surface_ids: HashMap<WlOutput, SurfaceId>,
    active_surface_id_opt: Option<SurfaceId>,
    surface_images: HashMap<SurfaceId, widget::image::Handle>,
    surface_names: HashMap<SurfaceId, String>,
    text_input_ids: HashMap<SurfaceId, widget::Id>,
    inhibit_opt: Option<Arc<OwnedFd>>,
    network_icon_opt: Option<&'static str>,
    power_info_opt: Option<(String, f64)>,
    value_tx_opt: Option<mpsc::Sender<String>>,
    prompt_opt: Option<(String, bool, Option<String>)>,
    error_opt: Option<String>,
}

impl App {
    //TODO: cache wallpapers by source?
    fn update_wallpapers(&mut self) {
        for (output, surface_id) in self.surface_ids.iter() {
            if self.surface_images.contains_key(surface_id) {
                continue;
            }

            let output_name = match self.surface_names.get(surface_id) {
                Some(some) => some,
                None => continue,
            };

            log::info!("updating wallpaper for {:?}", output_name);

            for (wallpaper_output_name, wallpaper_source) in self.flags.wallpapers.iter() {
                if wallpaper_output_name == output_name {
                    match wallpaper_source {
                        cosmic_bg_config::Source::Path(path) => {
                            match fs::read(path) {
                                Ok(bytes) => {
                                    let image = widget::image::Handle::from_memory(bytes);
                                    self.surface_images.insert(*surface_id, image);
                                    //TODO: what to do about duplicates?
                                    break;
                                }
                                Err(err) => {
                                    log::warn!(
                                        "output {}: failed to load wallpaper {:?}: {:?}",
                                        output.id(),
                                        path,
                                        err
                                    );
                                }
                            }
                        }
                        cosmic_bg_config::Source::Color(color) => {
                            //TODO: support color sources
                            log::warn!("output {}: unsupported source {:?}", output.id(), color);
                        }
                    }
                }
            }
        }
    }
}

/// Implement [`cosmic::Application`] to integrate with COSMIC.
impl cosmic::Application for App {
    /// Default async executor to use with the app.
    type Executor = executor::Default;

    /// Argument received [`cosmic::Application::new`].
    type Flags = Flags;

    /// Message type specific to our [`App`].
    type Message = Message;

    /// The unique application ID to supply to the window manager.
    const APP_ID: &'static str = "com.system76.CosmicGreeter";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    /// Creates the application, and optionally emits command on initialize.
    fn init(mut core: Core, flags: Self::Flags) -> (Self, Command<Self::Message>) {
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        core.window.sharp_corners = true;
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.window.use_template = false;

        (
            App {
                core,
                flags,
                state: State::Unlocked,
                surface_ids: HashMap::new(),
                active_surface_id_opt: None,
                surface_images: HashMap::new(),
                surface_names: HashMap::new(),
                text_input_ids: HashMap::new(),
                inhibit_opt: None,
                network_icon_opt: None,
                power_info_opt: None,
                value_tx_opt: None,
                prompt_opt: None,
                error_opt: None,
            },
            if cfg!(feature = "logind") {
                // When logind feature is used, wait for lock signal
                Command::none()
            } else {
                // When logind feature not used, lock immediately
                lock()
            },
        )
    }

    /// Handle application events here.
    fn update(&mut self, message: Self::Message) -> Command<Self::Message> {
        match message {
            Message::None => {}
            Message::OutputEvent(output_event, output) => {
                match output_event {
                    OutputEvent::Created(output_info_opt) => {
                        log::info!("output {}: created", output.id());

                        let surface_id = SurfaceId::unique();
                        match self.surface_ids.insert(output.clone(), surface_id) {
                            Some(old_surface_id) => {
                                //TODO: remove old surface?
                                log::warn!(
                                    "output {}: already had surface ID {:?}",
                                    output.id(),
                                    old_surface_id
                                );
                            }
                            None => {}
                        }

                        match output_info_opt {
                            Some(output_info) => match output_info.name {
                                Some(output_name) => {
                                    self.surface_names.insert(surface_id, output_name.clone());
                                    self.surface_images.remove(&surface_id);
                                    self.update_wallpapers();
                                }
                                None => {
                                    log::warn!("output {}: no output name", output.id());
                                }
                            },
                            None => {
                                log::warn!("output {}: no output info", output.id());
                            }
                        }

                        let text_input_id = widget::Id::unique();
                        self.text_input_ids
                            .insert(surface_id, text_input_id.clone());

                        if matches!(self.state, State::Locked) {
                            return Command::batch([
                                get_lock_surface(surface_id, output),
                                widget::text_input::focus(text_input_id),
                            ]);
                        }
                    }
                    OutputEvent::Removed => {
                        log::info!("output {}: removed", output.id());
                        match self.surface_ids.remove(&output) {
                            Some(surface_id) => {
                                self.surface_images.remove(&surface_id);
                                self.surface_names.remove(&surface_id);
                                self.text_input_ids.remove(&surface_id);
                                if matches!(self.state, State::Locked) {
                                    return destroy_lock_surface(surface_id);
                                }
                            }
                            None => {
                                log::warn!("output {}: no surface found", output.id());
                            }
                        }
                    }
                    OutputEvent::InfoUpdate(_output_info) => {
                        log::info!("output {}: info update", output.id());
                    }
                }
            }
            Message::SessionLockEvent(session_lock_event) => match session_lock_event {
                SessionLockEvent::Focused(_, surface_id) => {
                    log::info!("focus surface {:?}", surface_id);
                    self.active_surface_id_opt = Some(surface_id);
                    if let Some(text_input_id) = self.text_input_ids.get(&surface_id) {
                        return widget::text_input::focus(text_input_id.clone());
                    }
                }
                SessionLockEvent::Locked => {
                    log::info!("session locked");
                    self.state = State::Locked;
                    // Allow suspend
                    self.inhibit_opt = None;
                    let mut commands = Vec::with_capacity(self.surface_ids.len());
                    for (output, surface_id) in self.surface_ids.iter() {
                        commands.push(get_lock_surface(*surface_id, output.clone()));
                    }
                    return Command::batch(commands);
                }
                SessionLockEvent::Unlocked => {
                    log::info!("session unlocked");
                    self.state = State::Unlocked;
                    if cfg!(feature = "logind") {
                        // When using logind feature, stick around for more lock signals
                    } else {
                        // When not using logind feature, exit immediately after unlocking
                        //TODO: cleaner method to exit?
                        process::exit(0);
                    }
                }
                //TODO: handle finished signal
                _ => {}
            },
            Message::Channel(value_tx) => {
                self.value_tx_opt = Some(value_tx);
            }
            Message::BackgroundState(background_state) => {
                self.flags.wallpapers = background_state.wallpapers;
                self.surface_images.clear();
                self.update_wallpapers();
            }
            Message::Inhibit(inhibit) => {
                self.inhibit_opt = Some(inhibit);
            }
            Message::NetworkIcon(network_icon_opt) => {
                self.network_icon_opt = network_icon_opt;
            }
            Message::PowerInfo(power_info_opt) => {
                self.power_info_opt = power_info_opt;
            }
            Message::Prompt(prompt, secret, value_opt) => {
                let prompt_was_none = self.prompt_opt.is_none();
                self.prompt_opt = Some((prompt, secret, value_opt));
                if prompt_was_none {
                    if let Some(surface_id) = self.active_surface_id_opt {
                        if let Some(text_input_id) = self.text_input_ids.get(&surface_id) {
                            return widget::text_input::focus(text_input_id.clone());
                        }
                    }
                }
            }
            Message::Submit => match self.prompt_opt.take() {
                Some((_prompt, _secret, value_opt)) => match value_opt {
                    Some(value) => match self.value_tx_opt.take() {
                        Some(value_tx) => {
                            // Clear errors
                            self.error_opt = None;
                            return Command::perform(
                                async move {
                                    value_tx.send(value).await.unwrap();
                                    message::app(Message::Channel(value_tx))
                                },
                                |x| x,
                            );
                        }
                        None => log::warn!("tried to submit when value_tx_opt not set"),
                    },
                    None => log::warn!("tried to submit without value"),
                },
                None => log::warn!("tried to submit without prompt"),
            },
            Message::Suspend => {
                #[cfg(feature = "logind")]
                return Command::perform(
                    async move {
                        match crate::logind::suspend().await {
                            Ok(()) => message::none(),
                            Err(err) => {
                                log::error!("failed to suspend: {:?}", err);
                                message::app(Message::Error(err.to_string()))
                            }
                        }
                    },
                    |x| x,
                );
            }
            Message::Error(error) => {
                self.error_opt = Some(error);
            }
            Message::Lock => match self.state {
                State::Unlocked => {
                    log::info!("session locking");
                    self.state = State::Locking;
                    // Clear errors
                    self.error_opt = None;
                    // Clear value_tx
                    self.value_tx_opt = None;
                    return lock();
                }
                State::Unlocking => {
                    log::info!("session still unlocking");
                }
                State::Locking | State::Locked => {
                    log::info!("session already locking or locked");
                }
            },
            Message::Unlock => {
                match self.state {
                    State::Locked => {
                        log::info!("sessing unlocking");
                        self.state = State::Unlocking;
                        // Clear errors
                        self.error_opt = None;
                        // Clear value_tx
                        self.value_tx_opt = None;
                        let mut commands = Vec::with_capacity(self.surface_ids.len() + 1);
                        for (_output, surface_id) in self.surface_ids.iter() {
                            commands.push(destroy_lock_surface(*surface_id));
                        }
                        commands.push(unlock());
                        // Wait to exit until `Unlocked` event, when server has processed unlock
                        return Command::batch(commands);
                    }
                    State::Locking => {
                        log::info!("session still locking");
                    }
                    State::Unlocking | State::Unlocked => {
                        log::info!("session already unlocking or unlocked");
                    }
                }
            }
        }
        Command::none()
    }

    // Not used for layer surface window
    fn view(&self) -> Element<Self::Message> {
        unimplemented!()
    }

    /// Creates a view after each update.
    fn view_window(&self, surface_id: SurfaceId) -> Element<Self::Message> {
        let left_element = {
            let date_time_column = {
                let mut column = widget::column::with_capacity(2).padding(16.0);

                let dt = chrono::Local::now();

                //TODO: localized format
                let date = dt.format("%A, %B %-d");
                column = column
                    .push(widget::text::title2(format!("{}", date)).style(style::Text::Accent));

                //TODO: localized format
                let time = dt.format("%R");
                column = column.push(
                    widget::text(format!("{}", time))
                        .size(112.0)
                        .style(style::Text::Accent),
                );

                column
            };

            let mut status_row = widget::row::with_capacity(2).padding(16.0).spacing(12.0);

            if let Some(network_icon) = self.network_icon_opt {
                status_row = status_row.push(widget::icon::from_name(network_icon));
            }

            if let Some((power_icon, power_percent)) = &self.power_info_opt {
                status_row = status_row.push(iced::widget::row![
                    widget::icon::from_name(power_icon.clone()),
                    widget::text(format!("{:.0}%", power_percent)),
                ]);
            }

            //TODO: implement these buttons
            let button_row = iced::widget::row![
                widget::button(widget::icon::from_name(
                    "applications-accessibility-symbolic"
                ))
                .padding(12.0)
                .on_press(Message::None),
                widget::button(widget::icon::from_name("input-keyboard-symbolic"))
                    .padding(12.0)
                    .on_press(Message::None),
                widget::button(widget::icon::from_name("system-users-symbolic"))
                    .padding(12.0)
                    .on_press(Message::None),
                widget::button(widget::icon::from_name("system-suspend-symbolic"))
                    .padding(12.0)
                    .on_press(Message::Suspend),
            ]
            .padding([16.0, 0.0, 0.0, 0.0])
            .spacing(8.0);

            widget::container(iced::widget::column![
                date_time_column,
                widget::divider::horizontal::default(),
                status_row,
                widget::divider::horizontal::default(),
                button_row,
            ])
            .width(Length::Fill)
            .align_x(alignment::Horizontal::Left)
        };

        let right_element = {
            let mut column = widget::column::with_capacity(2)
                .spacing(12.0)
                .max_width(280.0);

            match &self.flags.icon_opt {
                Some(icon) => {
                    column = column.push(
                        widget::container(
                            widget::Image::new(icon.clone())
                                .width(Length::Fixed(78.0))
                                .height(Length::Fixed(78.0)),
                        )
                        .width(Length::Fill)
                        .align_x(alignment::Horizontal::Center),
                    )
                }
                None => {}
            }
            match &self.flags.current_user.gecos {
                Some(gecos) => {
                    let full_name = gecos.split(",").next().unwrap_or("");
                    column = column.push(
                        widget::container(widget::text::title4(full_name))
                            .width(Length::Fill)
                            .align_x(alignment::Horizontal::Center),
                    );
                }
                None => {}
            }

            match &self.prompt_opt {
                Some((prompt, secret, value_opt)) => match value_opt {
                    Some(value) => {
                        let mut text_input = widget::text_input(prompt.clone(), value.clone())
                            .leading_icon(
                                widget::icon::from_name("system-lock-screen-symbolic").into(),
                            )
                            .trailing_icon(
                                widget::icon::from_name("document-properties-symbolic").into(),
                            )
                            .on_input(|value| Message::Prompt(prompt.clone(), *secret, Some(value)))
                            .on_submit(Message::Submit);

                        if let Some(text_input_id) = self.text_input_ids.get(&surface_id) {
                            text_input = text_input.id(text_input_id.clone());
                        }

                        if *secret {
                            text_input = text_input.password()
                        }

                        column = column.push(text_input);
                    }
                    None => {
                        column = column.push(widget::text(prompt));
                    }
                },
                None => {}
            }

            if let Some(error) = &self.error_opt {
                column = column.push(widget::text(error));
            }

            widget::container(column)
                .align_x(alignment::Horizontal::Center)
                .width(Length::Fill)
        };

        crate::image_container::ImageContainer::new(
            widget::container(
                widget::layer_container(
                    iced::widget::row![left_element, right_element]
                        .align_items(alignment::Alignment::Center),
                )
                .layer(cosmic::cosmic_theme::Layer::Background)
                .padding(16)
                .style(cosmic::theme::Container::Custom(Box::new(
                    |theme: &cosmic::Theme| {
                        // Use background appearance as the base
                        let mut appearance = widget::container::StyleSheet::appearance(
                            theme,
                            &cosmic::theme::Container::Background,
                        );
                        appearance.border = iced::Border::with_radius(16.0);
                        appearance
                    },
                )))
                .width(Length::Fixed(800.0)),
            )
            .padding([32.0, 0.0, 0.0, 0.0])
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(alignment::Horizontal::Center)
            .align_y(alignment::Vertical::Top)
            .style(cosmic::theme::Container::Transparent),
        )
        .image(match self.surface_images.get(&surface_id) {
            Some(some) => some.clone(),
            //TODO: default image
            None => widget::image::Handle::from_pixels(1, 1, vec![0x00, 0x00, 0x00, 0xFF]),
        })
        .content_fit(iced::ContentFit::Cover)
        .into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subscriptions = Vec::with_capacity(7);

        subscriptions.push(event::listen_with(|event, _| match event {
            iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
                wayland_event,
            )) => match wayland_event {
                WaylandEvent::Output(output_event, output) => {
                    Some(Message::OutputEvent(output_event, output))
                }
                WaylandEvent::SessionLock(evt) => Some(Message::SessionLockEvent(evt)),
                _ => None,
            },
            _ => None,
        }));

        struct BackgroundSubscription;
        subscriptions.push(
            cosmic_config::config_state_subscription(
                TypeId::of::<BackgroundSubscription>(),
                cosmic_bg_config::NAME.into(),
                cosmic_bg_config::state::State::version(),
            )
            .map(|res| {
                if !res.errors.is_empty() {
                    log::info!("errors loading background state: {:?}", res.errors);
                }
                Message::BackgroundState(res.config)
            }),
        );

        if matches!(self.state, State::Locked) {
            struct HeartbeatSubscription;
            subscriptions.push(subscription::channel(
                TypeId::of::<HeartbeatSubscription>(),
                16,
                |mut msg_tx| async move {
                    loop {
                        // Send heartbeat once a second to update time
                        //TODO: only send this when needed
                        msg_tx.send(Message::None).await.unwrap();
                        time::sleep(time::Duration::new(1, 0)).await;
                    }
                },
            ));

            struct PamSubscription;
            //TODO: how to avoid cloning this on every time subscription is called?
            let username = self.flags.current_user.name.clone();
            subscriptions.push(subscription::channel(
                TypeId::of::<PamSubscription>(),
                16,
                |mut msg_tx| async move {
                    loop {
                        let (value_tx, value_rx) = mpsc::channel(16);
                        msg_tx.send(Message::Channel(value_tx)).await.unwrap();

                        let pam_res = {
                            let username = username.clone();
                            let msg_tx = msg_tx.clone();
                            task::spawn_blocking(move || {
                                pam_thread(username, Conversation { msg_tx, value_rx })
                            })
                            .await
                            .unwrap()
                        };

                        match pam_res {
                            Ok(()) => {
                                log::info!("successfully authenticated");
                                msg_tx.send(Message::Unlock).await.unwrap();
                                break;
                            }
                            Err(err) => {
                                log::warn!("authentication error: {}", err);
                                msg_tx.send(Message::Error(err.to_string())).await.unwrap();
                            }
                        }
                    }

                    loop {
                        time::sleep(time::Duration::new(60, 0)).await;
                    }
                },
            ));
        }

        #[cfg(feature = "logind")]
        {
            subscriptions.push(crate::logind::subscription());
        }

        #[cfg(feature = "networkmanager")]
        {
            subscriptions.push(
                crate::networkmanager::subscription()
                    .map(|icon_opt| Message::NetworkIcon(icon_opt)),
            );
        }

        #[cfg(feature = "upower")]
        {
            subscriptions
                .push(crate::upower::subscription().map(|info_opt| Message::PowerInfo(info_opt)));
        }

        Subscription::batch(subscriptions)
    }
}
