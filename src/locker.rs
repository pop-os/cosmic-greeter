// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::app::{Core, Settings, Task};
use cosmic::cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity;
use cosmic::iced::{Point, Rectangle, Size};
use cosmic::iced_runtime::platform_specific::wayland::subsurface::SctkSubsurfaceSettings;
use cosmic::surface;
use cosmic::{
    executor,
    iced::{
        self, alignment,
        event::{
            self,
            wayland::{Event as WaylandEvent, OutputEvent, SessionLockEvent},
        },
        futures::{self, SinkExt},
        platform_specific::shell::wayland::commands::session_lock::{
            destroy_lock_surface, get_lock_surface, lock, unlock,
        },
        Length, Subscription,
    },
    iced_runtime::core::window::Id as SurfaceId,
    style, widget, Element,
};
use cosmic_config::CosmicConfigEntry;
use std::{
    any::TypeId,
    collections::HashMap,
    env,
    ffi::{CStr, CString},
    fs,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    process,
    sync::Arc,
};
use tokio::{sync::mpsc, task, time};
use wayland_client::{protocol::wl_output::WlOutput, Proxy};

fn lockfile_opt() -> Option<PathBuf> {
    let runtime_dir = dirs::runtime_dir()?;
    let session_id_str = env::var("XDG_SESSION_ID").ok()?;
    let session_id = match session_id_str.parse::<u64>() {
        Ok(ok) => ok,
        Err(err) => {
            log::warn!("failed to parse session ID {:?}: {}", session_id_str, err);
            return None;
        }
    };
    Some(runtime_dir.join(format!("cosmic-greeter-{}.lock", session_id)))
}

pub fn main(current_user: pwd::Passwd) -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    crate::localize::localize();

    //TODO: use accountsservice
    let icon_path = Path::new("/var/lib/AccountsService/icons").join(&current_user.name);
    let icon_opt = if icon_path.is_file() {
        match fs::read(&icon_path) {
            Ok(icon_data) => Some(widget::image::Handle::from_bytes(icon_data)),
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
    let fallback_background =
        widget::image::Handle::from_bytes(include_bytes!("../res/background.jpg").as_slice());

    let flags = Flags {
        current_user,
        icon_opt,
        lockfile_opt: lockfile_opt(),
        wallpapers,
        fallback_background,
    };

    let settings = Settings::default().no_main_window(true);

    cosmic::app::run::<App>(settings, flags)?;

    Ok(())
}

pub fn pam_thread(username: String, conversation: Conversation) -> Result<(), pam_client::Error> {
    //TODO: send errors to GUI, restart process

    // Create PAM context
    let mut context = pam_client::Context::new("cosmic-greeter", Some(&username), conversation)?;

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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(async {
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
    lockfile_opt: Option<PathBuf>,
    wallpapers: Vec<(String, cosmic_bg_config::Source)>,
    fallback_background: widget::image::Handle,
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    None,
    OutputEvent(OutputEvent, WlOutput),
    SessionLockEvent(SessionLockEvent),
    Channel(mpsc::Sender<String>),
    BackgroundState(cosmic_bg_config::state::State),
    Focus(SurfaceId),
    Inhibit(Arc<OwnedFd>),
    NetworkIcon(Option<&'static str>),
    PowerInfo(Option<(String, f64)>),
    Prompt(String, bool, Option<String>),
    Submit(String),
    Surface(surface::Action),
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
    output_names: HashMap<WlOutput, String>,
    surface_ids: HashMap<WlOutput, SurfaceId>,
    subsurface_rects: HashMap<WlOutput, Rectangle>,
    active_surface_id_opt: Option<SurfaceId>,
    surface_images: HashMap<SurfaceId, widget::image::Handle>,
    surface_names: HashMap<SurfaceId, String>,
    text_input_ids: HashMap<String, widget::Id>,
    inhibit_opt: Option<Arc<OwnedFd>>,
    network_icon_opt: Option<&'static str>,
    power_info_opt: Option<(String, f64)>,
    value_tx_opt: Option<mpsc::Sender<String>>,
    prompt_opt: Option<(String, bool, Option<String>)>,
    error_opt: Option<String>,
}

impl App {
    fn menu<'a>(&'a self, surface_id: SurfaceId) -> Element<'a, Message> {
        let left_element = {
            let date_time_column = {
                let mut column = widget::column::with_capacity(2).padding(16.0);

                let dt = chrono::Local::now();
                let locale = *crate::localize::LANGUAGE_CHRONO;

                let date = dt.format_localized("%A, %B %-d", locale);
                column = column
                    .push(widget::text::title2(format!("{}", date)).class(style::Text::Accent));

                let time = dt.format_localized("%R", locale);
                column = column.push(
                    widget::text(format!("{}", time))
                        .size(112.0)
                        .class(style::Text::Accent),
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
                widget::button::custom(widget::icon::from_name(
                    "applications-accessibility-symbolic"
                ))
                .padding(12.0)
                .on_press(Message::None),
                widget::button::custom(widget::icon::from_name("input-keyboard-symbolic"))
                    .padding(12.0)
                    .on_press(Message::None),
                widget::button::custom(widget::icon::from_name("system-users-symbolic"))
                    .padding(12.0)
                    .on_press(Message::None),
                widget::button::custom(widget::icon::from_name("system-suspend-symbolic"))
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
            match self
                .flags
                .current_user
                .gecos
                .as_ref()
                .filter(|s| !s.is_empty())
            {
                Some(gecos) => {
                    let full_name = gecos.split(",").next().unwrap_or_default();
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
                        let text_input_id = self
                            .surface_names
                            .get(&surface_id)
                            .and_then(|id| self.text_input_ids.get(id))
                            .cloned()
                            .unwrap_or_else(|| cosmic::widget::Id::new("text_input"));

                        let mut text_input = widget::secure_input(
                            prompt.clone(),
                            "",
                            Some(Message::Prompt(
                                prompt.clone(),
                                !*secret,
                                Some(value.clone()),
                            )),
                            *secret,
                        )
                        .id(text_input_id)
                        .manage_value(true)
                        .on_submit(|v| Message::Submit(v));

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

        widget::container(
            widget::layer_container(
                iced::widget::row![left_element, right_element]
                    .align_y(alignment::Alignment::Center),
            )
            .layer(cosmic::cosmic_theme::Layer::Background)
            .padding(16)
            .class(cosmic::theme::Container::Custom(Box::new(
                |theme: &cosmic::Theme| {
                    // Use background appearance as the base
                    let mut appearance = widget::container::Catalog::style(
                        theme,
                        &cosmic::theme::Container::Background,
                    );
                    appearance.border = iced::Border::default().rounded(16.0);
                    appearance
                },
            )))
            .width(Length::Fill)
            .height(Length::Shrink),
        )
        .padding([32.0, 0.0, 0.0, 0.0])
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .align_y(alignment::Vertical::Top)
        .class(cosmic::theme::Container::Transparent)
        .into()
    }

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
                                    let image = widget::image::Handle::from_bytes(bytes);
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
    fn init(mut core: Core, flags: Self::Flags) -> (Self, Task<Self::Message>) {
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        // XXX must be false or define custom style to have transparent bg
        core.window.sharp_corners = false;
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.window.use_template = false;

        let already_locked = match flags.lockfile_opt {
            Some(ref lockfile) => lockfile.exists(),
            None => false,
        };

        let mut app = App {
            core,
            flags,
            state: State::Unlocked,
            surface_ids: HashMap::new(),
            active_surface_id_opt: None,
            output_names: HashMap::new(),
            surface_images: HashMap::new(),
            surface_names: HashMap::new(),
            text_input_ids: HashMap::new(),
            subsurface_rects: HashMap::new(),
            inhibit_opt: None,
            network_icon_opt: None,
            power_info_opt: None,
            value_tx_opt: None,
            prompt_opt: None,
            error_opt: None,
        };

        let command = if cfg!(feature = "logind") {
            if already_locked {
                // Recover previously locked state
                log::info!("recovering previous locked state");
                app.state = State::Locking;
                lock()
            } else {
                // When logind feature is used, wait for lock signal
                Task::none()
            }
        } else {
            // When logind feature not used, lock immediately
            log::info!("locking immediately");
            app.state = State::Locking;
            lock()
        };

        (app, command)
    }

    /// Handle application events here.
    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::None => {}
            Message::OutputEvent(output_event, output) => {
                match output_event {
                    OutputEvent::Created(output_info_opt) => {
                        log::info!("output {}: created", output.id());

                        let surface_id = SurfaceId::unique();
                        let subsurface_id = SurfaceId::unique();

                        match self.surface_ids.insert(output.clone(), surface_id) {
                            Some(old_surface_id) => {
                                //TODO: remove old surface?
                                log::warn!(
                                    "output {}: already had surface ID {:?}",
                                    output.id(),
                                    old_surface_id
                                );
                                return Task::none();
                            }
                            None => {}
                        }
                        let size = if let Some((w, h)) =
                            output_info_opt.as_ref().and_then(|info| info.logical_size)
                        {
                            Some((Some(w as u32), Some(h as u32)))
                        } else {
                            Some((None, None))
                        };
                        match output_info_opt {
                            Some(output_info) => match output_info.name {
                                Some(output_name) => {
                                    self.output_names
                                        .insert(output.clone(), output_name.clone());
                                    self.surface_names.insert(surface_id, output_name.clone());
                                    self.surface_names
                                        .insert(subsurface_id, output_name.clone());
                                    self.surface_images.remove(&surface_id);
                                    self.update_wallpapers();
                                    let text_input_id =
                                        widget::Id::new(format!("input-{output_name}",));
                                    self.text_input_ids
                                        .insert(output_name.clone(), text_input_id.clone());
                                }
                                None => {
                                    log::warn!("output {}: no output name", output.id());
                                }
                            },
                            None => {
                                log::warn!("output {}: no output info", output.id());
                            }
                        }

                        let unwrapped_size = size
                            .map(|s| (s.0.unwrap_or(1920), s.1.unwrap_or(1080)))
                            .unwrap_or((1920, 1080));
                        let (loc, sub_size) = if unwrapped_size.0 > 800 {
                            (
                                Point::new(unwrapped_size.0 as f32 / 2. - 400., 32.),
                                Size::new(800., unwrapped_size.1 as f32 - 32.),
                            )
                        } else {
                            (
                                Point::new(0., 32.),
                                Size::new(unwrapped_size.0 as f32, unwrapped_size.1 as f32 - 32.),
                            )
                        };
                        self.subsurface_rects
                            .insert(output.clone(), Rectangle::new(loc, sub_size));

                        let msg = cosmic::surface::action::subsurface(
                            move |_: &mut App| SctkSubsurfaceSettings {
                                parent: surface_id,
                                id: subsurface_id,
                                loc,
                                size: Some(sub_size),
                                z: 10,
                                steal_keyboard_focus: true,
                                gravity: Gravity::BottomRight,
                                offset: (0, 0),
                                input_zone: None,
                            },
                            Some(Box::new(move |app: &App| {
                                app.menu(subsurface_id).map(cosmic::Action::App)
                            })),
                        );

                        if matches!(self.state, State::Locked) {
                            return Task::batch([
                                get_lock_surface(surface_id, output),
                                cosmic::task::message(cosmic::Action::Cosmic(
                                    cosmic::app::Action::Surface(msg),
                                )),
                            ]);
                        }
                    }
                    OutputEvent::Removed => {
                        log::info!("output {}: removed", output.id());
                        match self.surface_ids.remove(&output) {
                            Some(surface_id) => {
                                self.surface_images.remove(&surface_id);
                                self.surface_names.remove(&surface_id);
                                if let Some(n) = self.surface_names.remove(&surface_id) {
                                    self.text_input_ids.remove(&n);
                                }
                                if matches!(self.state, State::Locked) {
                                    return destroy_lock_surface(surface_id);
                                }
                            }
                            None => {
                                log::warn!("output {}: no surface found", output.id());
                            }
                        }
                    }
                    OutputEvent::InfoUpdate(info) => {
                        let size = if let Some((w, h)) = info.logical_size {
                            Some((Some(w as u32), Some(h as u32)))
                        } else {
                            Some((None, None))
                        };
                        let unwrapped_size = size
                            .map(|s| (s.0.unwrap_or(1920), s.1.unwrap_or(1080)))
                            .unwrap_or((1920, 1080));
                        let (loc, sub_size) = if unwrapped_size.0 > 800 {
                            (
                                Point::new(unwrapped_size.0 as f32 / 2. - 400., 32.),
                                Size::new(800., unwrapped_size.1 as f32 - 32.),
                            )
                        } else {
                            (Point::ORIGIN, Size::new(1920., 1080.))
                        };
                        self.subsurface_rects
                            .insert(output.clone(), Rectangle::new(loc, sub_size));

                        log::info!("output {}: info update", output.id());
                    }
                }
            }
            Message::SessionLockEvent(session_lock_event) => match session_lock_event {
                SessionLockEvent::Focused(..) => {}
                SessionLockEvent::Locked => {
                    log::info!("session locked");
                    if matches!(self.state, State::Locked) {
                        return Task::none();
                    }
                    self.state = State::Locked;
                    // Allow suspend
                    self.inhibit_opt = None;
                    // Create lock surfaces

                    let mut commands = Vec::with_capacity(self.surface_ids.len());
                    for (output, surface_id) in self.surface_ids.iter() {
                        commands.push(get_lock_surface(*surface_id, output.clone()));

                        if let Some((rect, name)) = self
                            .subsurface_rects
                            .get(output)
                            .copied()
                            .zip(self.output_names.get(output))
                        {
                            let subsurface_id = SurfaceId::unique();
                            let surface_id = *surface_id;
                            self.surface_names.insert(surface_id, name.clone());
                            self.surface_names.insert(subsurface_id, name.clone());
                            let msg = cosmic::surface::action::subsurface(
                                move |_: &mut App| SctkSubsurfaceSettings {
                                    parent: surface_id,
                                    id: subsurface_id,
                                    loc: Point::new(rect.x, rect.y),
                                    size: Some(Size::new(rect.width, rect.height)),
                                    z: 10,
                                    steal_keyboard_focus: true,
                                    gravity: Gravity::BottomRight,
                                    offset: (0, 0),
                                    input_zone: None,
                                },
                                Some(Box::new(move |app: &App| {
                                    app.menu(subsurface_id).map(cosmic::Action::App)
                                })),
                            );
                            commands.push(cosmic::task::message(cosmic::Action::Cosmic(
                                cosmic::app::Action::Surface(msg),
                            )));
                        } else {
                            log::error!("no rectangle for subsurface...");
                        }
                    }
                    return Task::batch(commands);
                }
                SessionLockEvent::Unlocked => {
                    log::info!("session unlocked");
                    self.state = State::Unlocked;

                    let mut commands = Vec::new();
                    for (_output, surface_id) in self.surface_ids.iter() {
                        self.surface_names.remove(surface_id);

                        commands.push(destroy_lock_surface(*surface_id));
                    }
                    if cfg!(feature = "logind") {
                        return Task::batch(commands);
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
            Message::Focus(surface_id) => {
                self.active_surface_id_opt = Some(surface_id);
                self.active_surface_id_opt = Some(surface_id);
                if let Some(text_input_id) = self
                    .surface_names
                    .get(&surface_id)
                    .and_then(|id| self.text_input_ids.get(id))
                {
                    return widget::text_input::focus(text_input_id.clone());
                }
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
                            log::error!("focus surface found id {:?}", text_input_id);

                            return widget::text_input::focus(text_input_id.clone());
                        }
                    }
                }
            }
            Message::Submit(value) => match self.value_tx_opt.take() {
                Some(value_tx) => {
                    // Clear errors
                    self.error_opt = None;
                    return cosmic::task::future(async move {
                        value_tx.send(value).await.unwrap();
                        Message::Channel(value_tx)
                    });
                }
                None => log::warn!("tried to submit when value_tx_opt not set"),
            },
            Message::Suspend => {
                #[cfg(feature = "logind")]
                return cosmic::task::future(async move {
                    match crate::logind::suspend().await {
                        Ok(()) => cosmic::action::none(),
                        Err(err) => {
                            log::error!("failed to suspend: {:?}", err);
                            cosmic::Action::App(Message::Error(err.to_string()))
                        }
                    }
                });
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
                    // Try to create lockfile when locking
                    if let Some(ref lockfile) = self.flags.lockfile_opt {
                        if let Err(err) = fs::File::create(lockfile) {
                            log::warn!("failed to create lockfile {:?}: {}", lockfile, err);
                        }
                    }
                    // Tell compositor to lock
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
                        // Try to delete lockfile when unlocking
                        if let Some(ref lockfile) = self.flags.lockfile_opt {
                            if let Err(err) = fs::remove_file(lockfile) {
                                log::warn!("failed to remove lockfile {:?}: {}", lockfile, err);
                            }
                        }

                        // Destroy lock surfaces
                        let mut commands = Vec::with_capacity(self.surface_ids.len() + 1);
                        // Tell compositor to unlock
                        commands.push(unlock());

                        // Wait to exit until `Unlocked` event, when server has processed unlock
                        return Task::batch(commands);
                    }
                    State::Locking => {
                        log::info!("session still locking");
                    }
                    State::Unlocking | State::Unlocked => {
                        log::info!("session already unlocking or unlocked");
                    }
                }
            }
            Message::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(a),
                ));
            }
        }
        Task::none()
    }

    // Not used for layer surface window
    fn view(&self) -> Element<Self::Message> {
        unimplemented!()
    }

    /// Creates a view after each update.
    fn view_window(&self, surface_id: SurfaceId) -> Element<Self::Message> {
        let img = self
            .surface_images
            .get(&surface_id)
            .unwrap_or(&self.flags.fallback_background);
        widget::image(img)
            .content_fit(iced::ContentFit::Cover)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subscriptions = Vec::with_capacity(7);

        subscriptions.push(event::listen_with(|event, _, id| match event {
            iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
                wayland_event,
            )) => match wayland_event {
                WaylandEvent::Output(output_event, output) => {
                    Some(Message::OutputEvent(output_event, output))
                }
                WaylandEvent::SessionLock(evt) => Some(Message::SessionLockEvent(evt)),
                _ => None,
            },
            iced::Event::Window(iced::window::Event::Focused) => Some(Message::Focus(id)),
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
            subscriptions.push(Subscription::run_with_id(
                TypeId::of::<HeartbeatSubscription>(),
                cosmic::iced_futures::stream::channel(16, |mut msg_tx| async move {
                    loop {
                        // Send heartbeat once a second to update time
                        //TODO: only send this when needed
                        msg_tx.send(Message::None).await.unwrap();
                        time::sleep(time::Duration::new(1, 0)).await;
                    }
                }),
            ));

            struct PamSubscription;
            //TODO: how to avoid cloning this on every time subscription is called?
            let username = self.flags.current_user.name.clone();
            subscriptions.push(Subscription::run_with_id(
                TypeId::of::<PamSubscription>(),
                cosmic::iced_futures::stream::channel(16, |mut msg_tx| async move {
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
                }),
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
