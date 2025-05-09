// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

mod ipc;

use cosmic::app::{Core, Settings, Task};
use cosmic::cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity;
use cosmic::iced::{Point, Size};
use cosmic::iced_core::image;
use cosmic::iced_runtime::platform_specific::wayland::subsurface::SctkSubsurfaceSettings;
use cosmic::surface;
use cosmic::widget::text;
use cosmic::{
    Element,
    cosmic_config::{self, ConfigSet, CosmicConfigEntry},
    executor,
    iced::{
        self, Background, Border, Length, Subscription, alignment,
        event::{
            self,
            wayland::{Event as WaylandEvent, OutputEvent},
        },
        futures::SinkExt,
        platform_specific::{
            runtime::wayland::layer_surface::{IcedMargin, IcedOutput, SctkLayerSurfaceSettings},
            shell::wayland::commands::layer_surface::{
                Anchor, KeyboardInteractivity, Layer, destroy_layer_surface, get_layer_surface,
            },
        },
    },
    iced_runtime::core::window::Id as SurfaceId,
    theme, widget,
};
use cosmic_comp_config::CosmicCompConfig;
use cosmic_greeter_config::Config as CosmicGreeterConfig;
use cosmic_greeter_daemon::{UserData, WallpaperData};
use greetd_ipc::Request;
use std::{
    collections::{HashMap, hash_map},
    error::Error,
    fs, io,
    num::NonZeroU32,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time;
use wayland_client::{Proxy, protocol::wl_output::WlOutput};
use zbus::{Connection, proxy};

use crate::fl;

#[proxy(
    interface = "com.system76.CosmicGreeter",
    default_service = "com.system76.CosmicGreeter",
    default_path = "/com/system76/CosmicGreeter"
)]
trait Greeter {
    async fn get_user_data(&self) -> Result<String, zbus::Error>;
}

async fn user_data_dbus() -> Result<Vec<UserData>, Box<dyn Error>> {
    let connection = Connection::system().await?;

    // `dbus_proxy` macro creates `MyGreaterProxy` based on `Notifications` trait.
    let proxy = GreeterProxy::new(&connection).await?;
    let reply = proxy.get_user_data().await?;

    let user_datas: Vec<UserData> = ron::from_str(&reply)?;
    Ok(user_datas)
}

fn user_data_fallback() -> Vec<UserData> {
    // The pwd::Passwd method is unsafe (but not labelled as such) due to using global state (libc pwent functions).
    /* unsafe */
    {
        pwd::Passwd::iter()
            .filter(|user| {
                if user.uid < 1000 {
                    // Skip system accounts
                    return false;
                }

                match Path::new(&user.shell).file_name().and_then(|x| x.to_str()) {
                    // Skip shell ending in false
                    Some("false") => false,
                    // Skip shell ending in nologin
                    Some("nologin") => false,
                    _ => true,
                }
            })
            .map(UserData::from)
            .collect()
    }
}

pub fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    crate::localize::localize();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut user_datas = match runtime.block_on(user_data_dbus()) {
        Ok(ok) => ok,
        Err(err) => {
            log::error!("failed to load user data from daemon: {}", err);
            user_data_fallback()
        }
    };

    // Sort user data by uid
    user_datas.sort_by(|a, b| a.uid.cmp(&b.uid));

    let (mut greeter_config, greeter_config_handler) = CosmicGreeterConfig::load();
    // Filter out users that were removed from the system since the last time we loaded config
    greeter_config.users.retain(|uid, _| {
        user_datas
            .binary_search_by(|probe| probe.uid.cmp(&uid.get()))
            .is_ok()
    });

    enum SessionType {
        X11,
        Wayland,
    }

    let session_dirs = xdg::BaseDirectories::with_prefix("wayland-sessions")
        .map_or(
            vec![PathBuf::from("/usr/share/wayland-sessions")],
            |xdg_dirs| xdg_dirs.get_data_dirs(),
        )
        .into_iter()
        .map(|dir| (dir, SessionType::Wayland))
        .chain(
            xdg::BaseDirectories::with_prefix("xsessions")
                .map_or(vec![PathBuf::from("/usr/share/xsessions")], |xdg_dirs| {
                    xdg_dirs.get_data_dirs()
                })
                .into_iter()
                .map(|dir| (dir, SessionType::X11)),
        );

    let sessions = {
        let mut sessions = HashMap::new();
        for (session_dir, session_type) in session_dirs {
            let read_dir = match fs::read_dir(&session_dir) {
                Ok(ok) => ok,
                Err(err) => {
                    log::warn!(
                        "failed to read session directory {:?}: {:?}",
                        session_dir,
                        err
                    );
                    continue;
                }
            };

            for dir_entry_res in read_dir {
                let dir_entry = match dir_entry_res {
                    Ok(ok) => ok,
                    Err(err) => {
                        log::warn!(
                            "failed to read session directory {:?} entry: {:?}",
                            session_dir,
                            err
                        );
                        continue;
                    }
                };

                let entry = match freedesktop_entry_parser::parse_entry(dir_entry.path()) {
                    Ok(ok) => ok,
                    Err(err) => {
                        log::warn!(
                            "failed to read session file {:?}: {:?}",
                            dir_entry.path(),
                            err
                        );
                        continue;
                    }
                };

                let name = match entry.section("Desktop Entry").attr("Name") {
                    Some(some) => some,
                    None => {
                        log::warn!(
                            "failed to read session file {:?}: no Desktop Entry/Name attribute",
                            dir_entry.path()
                        );
                        continue;
                    }
                };

                let exec = match entry.section("Desktop Entry").attr("Exec") {
                    Some(some) => some,
                    None => {
                        log::warn!(
                            "failed to read session file {:?}: no Desktop Entry/Exec attribute",
                            dir_entry.path()
                        );
                        continue;
                    }
                };

                let mut command = Vec::new();
                let mut env = Vec::new();
                match session_type {
                    SessionType::X11 => {
                        //TODO: xinit may be better, but more complicated to set up
                        command.push("startx".to_string());
                        env.push("XDG_SESSION_TYPE=x11".to_string());
                    }
                    SessionType::Wayland => {
                        env.push("XDG_SESSION_TYPE=wayland".to_string());
                    }
                };

                if let Some(desktop_names) = entry.section("Desktop Entry").attr("DesktopNames") {
                    env.push(format!("XDG_CURRENT_DESKTOP={desktop_names}"));
                    if let Some(name) = desktop_names.split(':').next() {
                        env.push(format!("XDG_SESSION_DESKTOP={name}"));
                    }
                }

                // Session exec may contain environmental variables
                command.push("/usr/bin/env".to_string());

                // To ensure the env is set correctly, we also set it in the session command
                for arg in env.iter() {
                    command.push(arg.clone());
                }

                match shlex::split(exec) {
                    Some(args) => {
                        for arg in args {
                            command.push(arg)
                        }
                    }
                    None => {
                        log::warn!(
                            "failed to parse session file {:?} Exec field {:?}",
                            dir_entry.path(),
                            exec
                        );
                        continue;
                    }
                };

                log::info!("session {} using command {:?} env {:?}", name, command, env);
                match sessions.insert(name.to_string(), (command, env)) {
                    Some(some) => {
                        log::warn!("session {} overwrote old command {:?}", name, some);
                    }
                    None => {}
                }
            }
        }
        sessions
    };

    let layouts_opt = match xkb_data::all_keyboard_layouts() {
        Ok(ok) => Some(Arc::new(ok)),
        Err(err) => {
            log::warn!("failed to load keyboard layouts: {}", err);
            None
        }
    };

    let comp_config_handler =
        match cosmic_config::Config::new("com.system76.CosmicComp", CosmicCompConfig::VERSION) {
            Ok(config_handler) => Some(config_handler),
            Err(err) => {
                log::error!("failed to create cosmic-comp config handler: {}", err);
                None
            }
        };

    let fallback_background =
        widget::image::Handle::from_bytes(include_bytes!("../res/background.jpg").as_slice());

    let flags = Flags {
        user_datas,
        sessions,
        layouts_opt,
        comp_config_handler,
        greeter_config,
        greeter_config_handler,
        fallback_background,
    };

    let settings = Settings::default().no_main_window(true);

    cosmic::app::run::<App>(settings, flags)?;

    Ok(())
}

#[derive(Clone)]
pub struct Flags {
    user_datas: Vec<UserData>,
    sessions: HashMap<String, (Vec<String>, Vec<String>)>,
    layouts_opt: Option<Arc<xkb_data::KeyboardLayouts>>,
    comp_config_handler: Option<cosmic_config::Config>,
    greeter_config: CosmicGreeterConfig,
    greeter_config_handler: Option<cosmic_config::Config>,
    fallback_background: widget::image::Handle,
}

#[derive(Clone, Debug)]
pub enum SocketState {
    /// Opening GREETD_SOCK
    Pending,
    /// GREETD_SOCK is open
    Open,
    /// No GREETD_SOCK variable set
    NotSet,
    /// Failed to open GREETD_SOCK
    Error(Arc<io::Error>),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ActiveLayout {
    layout: String,
    description: String,
    variant: String,
}

#[derive(Clone, Copy, Debug)]
pub enum DialogPage {
    Restart(Instant),
    Shutdown(Instant),
}

impl DialogPage {
    fn remaining(instant: Instant) -> Option<Duration> {
        let elapsed = instant.elapsed();
        let timeout = Duration::new(60, 0);
        if elapsed < timeout {
            Some(timeout - elapsed)
        } else {
            None
        }
    }
}

///TODO: this is custom code that should be better handled by libcosmic
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dropdown {
    Keyboard,
    User,
    Session,
}

struct NameIndexPair {
    /// Selected username
    username: String,
    /// Index of the [`UserData`] for the selected username
    data_idx: Option<usize>,
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    Auth(Option<String>),
    ConfigUpdateUser,
    DialogCancel,
    DialogConfirm,
    DropdownToggle(Dropdown),
    Error(String),
    Exit,
    Focus(SurfaceId),
    // Sets channel used to communicate with the greetd IPC subscription.
    GreetdChannel(tokio::sync::mpsc::Sender<Request>),
    Heartbeat,
    KeyboardLayout(usize),
    Login,
    NetworkIcon(Option<&'static str>),
    OutputEvent(OutputEvent, WlOutput),
    PowerInfo(Option<(String, f64)>),
    Prompt(String, bool, Option<String>),
    Reconnect,
    Restart,
    Session(String),
    Shutdown,
    Socket(SocketState),
    Surface(surface::Action),
    Suspend,
    Tick,
    Tz(chrono_tz::Tz),
    Username(String),
}

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    flags: Flags,
    greetd_sender: Option<tokio::sync::mpsc::Sender<greetd_ipc::Request>>,
    surface_ids: HashMap<WlOutput, SurfaceId>,
    active_surface_id_opt: Option<SurfaceId>,
    surface_images: HashMap<SurfaceId, image::Handle>,
    surface_names: HashMap<SurfaceId, String>,
    text_input_ids: HashMap<String, widget::Id>,
    network_icon_opt: Option<&'static str>,
    power_info_opt: Option<(String, f64)>,
    socket_state: SocketState,
    usernames: Vec<(String, String)>,
    selected_username: NameIndexPair,
    prompt_opt: Option<(String, bool, Option<String>)>,
    session_names: Vec<String>,
    selected_session: String,
    active_layouts: Vec<ActiveLayout>,
    error_opt: Option<String>,
    dialog_page_opt: Option<DialogPage>,
    dropdown_opt: Option<Dropdown>,
    window_size: HashMap<SurfaceId, Size>,
    heartbeat_handle: Option<cosmic::iced::task::Handle>,
    time: crate::time::Time,
}

impl App {
    fn menu(&self, id: SurfaceId) -> Element<Message> {
        let window_width = self.window_size.get(&id).map(|s| s.width).unwrap_or(800.);
        let menu_width = if window_width > 800. {
            800.
        } else {
            window_width
        };
        let left_element = {
            let military_time = self
                .selected_username
                .data_idx
                .and_then(|i| {
                    self.flags
                        .user_datas
                        .get(i)
                        .and_then(|user| user.clock_military_time_opt)
                })
                .unwrap_or_default();
            let date_time_column = self.time.date_time_widget(military_time);

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

            //TODO: move code for custom dropdowns to libcosmic
            let menu_checklist = |label, value, message| {
                Element::from(
                    widget::menu::menu_button(vec![
                        if value {
                            widget::icon::from_name("object-select-symbolic")
                                .size(16)
                                .icon()
                                .width(Length::Fixed(16.0))
                                .into()
                        } else {
                            widget::Space::with_width(Length::Fixed(17.0)).into()
                        },
                        widget::Space::with_width(Length::Fixed(8.0)).into(),
                        widget::text(label)
                            .align_x(iced::alignment::Horizontal::Left)
                            .into(),
                    ])
                    .on_press(message),
                )
            };
            let dropdown_menu = |items| {
                widget::container(widget::column::with_children(items))
                    .padding(1)
                    //TODO: move style to libcosmic
                    .class(theme::Container::custom(|theme| {
                        let cosmic = theme.cosmic();
                        let component = &cosmic.background.component;
                        widget::container::Style {
                            icon_color: Some(component.on.into()),
                            text_color: Some(component.on.into()),
                            background: Some(Background::Color(component.base.into())),
                            border: Border {
                                radius: 8.0.into(),
                                width: 1.0,
                                color: component.divider.into(),
                            },
                            ..Default::default()
                        }
                    }))
                    .width(Length::Fixed(240.0))
            };

            let mut input_button = widget::popover(
                widget::button::custom(widget::icon::from_name("input-keyboard-symbolic"))
                    .padding(12.0)
                    .on_press(Message::DropdownToggle(Dropdown::Keyboard)),
            )
            .position(widget::popover::Position::Bottom);
            if matches!(self.dropdown_opt, Some(Dropdown::Keyboard)) {
                let mut items = Vec::with_capacity(self.active_layouts.len());
                for (i, layout) in self.active_layouts.iter().enumerate() {
                    items.push(menu_checklist(
                        &layout.description,
                        i == 0,
                        Message::KeyboardLayout(i),
                    ));
                }
                input_button = input_button.popup(dropdown_menu(items));
            }

            let mut user_button = widget::popover(
                widget::button::custom(widget::icon::from_name("system-users-symbolic"))
                    .padding(12.0)
                    .on_press(Message::DropdownToggle(Dropdown::User)),
            )
            .position(widget::popover::Position::Bottom);
            if matches!(self.dropdown_opt, Some(Dropdown::User)) {
                let mut items = Vec::with_capacity(self.usernames.len());
                for (name, full_name) in self.usernames.iter() {
                    items.push(menu_checklist(
                        full_name,
                        name == &self.selected_username.username,
                        Message::Username(name.clone()),
                    ));
                }
                user_button = user_button.popup(dropdown_menu(items));
            }

            let mut session_button = widget::popover(
                widget::button::custom(widget::icon::from_name("application-menu-symbolic"))
                    .padding(12.0)
                    .on_press(Message::DropdownToggle(Dropdown::Session)),
            )
            .position(widget::popover::Position::Bottom);
            if matches!(self.dropdown_opt, Some(Dropdown::Session)) {
                let mut items = Vec::with_capacity(self.session_names.len());
                for session_name in self.session_names.iter() {
                    items.push(menu_checklist(
                        session_name,
                        session_name == &self.selected_session,
                        Message::Session(session_name.clone()),
                    ));
                }
                session_button = session_button.popup(dropdown_menu(items));
            }

            let button_row = iced::widget::row![
                /*TODO: greeter accessibility options
                widget::button(widget::icon::from_name(
                    "applications-accessibility-symbolic"
                ))
                .padding(12.0)
                .on_press(Message::None),
                */
                widget::tooltip(
                    input_button,
                    text(fl!("keyboard-layout")),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(
                    user_button,
                    text(fl!("user")),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(
                    session_button,
                    text(fl!("session")),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(
                    widget::button::custom(widget::icon::from_name("system-suspend-symbolic"))
                        .padding(12.0)
                        .on_press(Message::Suspend),
                    text(fl!("suspend")),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(
                    widget::button::custom(widget::icon::from_name("system-reboot-symbolic"))
                        .padding(12.0)
                        .on_press(Message::Restart),
                    text(fl!("restart")),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(
                    widget::button::custom(widget::icon::from_name("system-shutdown-symbolic"))
                        .padding(12.0)
                        .on_press(Message::Shutdown),
                    text(fl!("shutdown")),
                    widget::tooltip::Position::Top
                )
            ]
            .padding([16.0, 0.0, 0.0, 0.0])
            .spacing(8.0);

            widget::container(iced::widget::column![
                date_time_column,
                widget::divider::horizontal::default().width(Length::Fixed(menu_width / 2. - 16.)),
                status_row,
                widget::divider::horizontal::default().width(Length::Fixed(menu_width / 2. - 16.)),
                button_row,
            ])
            .align_x(alignment::Horizontal::Left)
        };

        let right_element = {
            let mut column = widget::column::with_capacity(2)
                .spacing(12.0)
                .max_width(280.0);

            match &self.socket_state {
                SocketState::Pending => {
                    column = column.push(widget::text("Opening GREETD_SOCK"));
                }
                SocketState::Open => {
                    for user_data in &self.flags.user_datas {
                        if user_data.name == self.selected_username.username {
                            match &user_data.icon_opt {
                                Some(icon) => {
                                    column = column.push(
                                        widget::container(
                                            widget::Image::new(
                                                //TODO: cache handle
                                                widget::image::Handle::from_bytes(icon.clone()),
                                            )
                                            .width(Length::Fixed(78.0))
                                            .height(Length::Fixed(78.0)),
                                        )
                                        .width(Length::Fill)
                                        .align_x(alignment::Horizontal::Center),
                                    )
                                }
                                None => {}
                            }
                            column = column.push(
                                widget::container(widget::text::title4(
                                    user_data.full_name_or_name(),
                                ))
                                .width(Length::Fill)
                                .align_x(alignment::Horizontal::Center),
                            );
                        }
                    }
                    match &self.prompt_opt {
                        Some((prompt, secret, value_opt)) => match value_opt {
                            Some(value) => {
                                let text_input_id = self
                                    .surface_names
                                    .get(&id)
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
                                .on_submit(|v| Message::Auth(Some(v)));

                                if let Some(text_input_id) = self
                                    .surface_names
                                    .get(&id)
                                    .and_then(|id| self.text_input_ids.get(id))
                                {
                                    text_input = text_input.id(text_input_id.clone());
                                }

                                if *secret {
                                    text_input = text_input.password()
                                }

                                column = column.push(text_input);
                            }
                            None => {
                                column = column.push(
                                    widget::button::custom("Confirm").on_press(Message::Auth(None)),
                                );
                            }
                        },
                        None => {}
                    }
                }
                SocketState::NotSet => {
                    column = column.push(widget::text("GREETD_SOCK variable not set"));
                }
                SocketState::Error(err) => {
                    column = column.push(widget::text(format!(
                        "Failed to open GREETD_SOCK: {:?}",
                        err
                    )))
                }
            }

            if let Some(error) = &self.error_opt {
                column = column.push(widget::text(error));
            }

            widget::container(column)
                .align_x(alignment::Horizontal::Center)
                .width(Length::Fill)
        };
        let menu = widget::container(
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
                    appearance.border = iced::Border::default().rounded(16);
                    appearance
                },
            )))
            .class(cosmic::theme::Container::Background)
            .width(Length::Fixed(800.0)),
        )
        .padding([32.0, 0.0, 0.0, 0.0])
        .width(Length::Fill)
        .height(Length::Shrink)
        .align_x(alignment::Horizontal::Center)
        .align_y(alignment::Vertical::Top);

        let popover = widget::popover(menu).modal(true);
        match self.dialog_page_opt {
            Some(DialogPage::Restart(instant)) => {
                let remaining = DialogPage::remaining(instant).unwrap_or_default();
                popover
                    .popup(
                        widget::dialog()
                            .title(fl!("restart-now"))
                            .icon(widget::icon::from_name("system-reboot-symbolic").size(64))
                            .body(fl!("restart-timeout", seconds = remaining.as_secs()))
                            .primary_action(
                                widget::button::suggested(fl!("restart"))
                                    .on_press(Message::DialogConfirm),
                            )
                            .secondary_action(
                                widget::button::standard(fl!("cancel"))
                                    .on_press(Message::DialogCancel),
                            ),
                    )
                    .into()
            }
            Some(DialogPage::Shutdown(instant)) => {
                let remaining = DialogPage::remaining(instant).unwrap_or_default();
                popover
                    .popup(
                        widget::dialog()
                            .title(fl!("shutdown-now"))
                            .icon(widget::icon::from_name("system-shutdown-symbolic").size(64))
                            .body(fl!("shutdown-timeout", seconds = remaining.as_secs()))
                            .primary_action(
                                widget::button::suggested(fl!("shutdown"))
                                    .on_press(Message::DialogConfirm),
                            )
                            .secondary_action(
                                widget::button::standard(fl!("cancel"))
                                    .on_press(Message::DialogCancel),
                            ),
                    )
                    .into()
            }
            None => popover.into(),
        }
    }
    /// Send a [`Request`] to the greetd IPC subscription.
    fn send_request(&self, request: Request) {
        if let Some(ref sender) = self.greetd_sender {
            let sender = sender.clone();
            tokio::task::spawn(async move {
                _ = sender.send(request).await;
            });
        }
    }

    fn set_xkb_config(&self) {
        let user_data = match self
            .selected_username
            .data_idx
            .and_then(|i| self.flags.user_datas.get(i))
        {
            Some(some) => some,
            None => return,
        };

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
            if let Some(comp_config_handler) = &self.flags.comp_config_handler {
                match comp_config_handler.set("xkb_config", xkb_config) {
                    Ok(()) => log::info!("updated cosmic-comp xkb_config"),
                    Err(err) => log::error!("failed to update cosmic-comp xkb_config: {}", err),
                }
            }
        }
    }

    fn update_user_config(&mut self) -> Task<Message> {
        let user_data = match self
            .selected_username
            .data_idx
            .and_then(|i| self.flags.user_datas.get(i))
        {
            Some(some) => some,
            None => {
                return Task::none();
            }
        };

        if let Some(wallpapers) = &user_data.wallpapers_opt {
            for (output, surface_id) in self.surface_ids.iter() {
                if self.surface_images.contains_key(surface_id) {
                    continue;
                }

                let output_name = match self.surface_names.get(surface_id) {
                    Some(some) => some,
                    None => continue,
                };

                log::info!("updating wallpaper for {:?}", output_name);

                for (wallpaper_output_name, wallpaper_data) in wallpapers.iter() {
                    if wallpaper_output_name == output_name {
                        match wallpaper_data {
                            WallpaperData::Bytes(bytes) => {
                                self.surface_images
                                    .insert(*surface_id, image::Handle::from_bytes(bytes.clone()));

                                //TODO: what to do about duplicates?
                                break;
                            }
                            WallpaperData::Color(color) => {
                                //TODO: support color sources
                                log::warn!(
                                    "output {}: unsupported source {:?}",
                                    output.id(),
                                    color
                                );
                            }
                        }
                    }
                }
            }
        }
        // From cosmic-applet-input-sources
        if let Some(keyboard_layouts) = &self.flags.layouts_opt {
            if let Some(xkb_config) = &user_data.xkb_config_opt {
                self.active_layouts.clear();
                let config_layouts = xkb_config.layout.split_terminator(',');
                let config_variants = xkb_config
                    .variant
                    .split_terminator(',')
                    .chain(std::iter::repeat(""));
                for (config_layout, config_variant) in config_layouts.zip(config_variants) {
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
                            continue;
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
                        }
                    }
                }
                log::info!("{:?}", self.active_layouts);

                // Ensure that user's xkb config is used
                self.set_xkb_config();
            }
        }

        match &user_data.theme_opt {
            Some(theme) => {
                cosmic::command::set_theme(cosmic::Theme::custom(Arc::new(theme.clone())))
            }
            None => Task::none(),
        }
    }

    fn user_data_index(user_datas: &[UserData], username: &str) -> Option<usize> {
        user_datas
            .binary_search_by(|probe| probe.name.as_str().cmp(username))
            .ok()
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
    fn init(mut core: Core, flags: Self::Flags) -> (Self, Task<Message>) {
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        // XXX must be false or define custom style to have transparent bg
        core.window.sharp_corners = false;
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.window.use_template = false;

        //TODO: use full_name_opt
        let mut usernames: Vec<_> = flags
            .user_datas
            .iter()
            .map(|x| {
                let name = x.name.clone();
                let full_name = x.full_name_opt.clone().unwrap_or_else(|| name.clone());
                (name, full_name)
            })
            .collect();
        usernames.sort_by(|a, b| a.1.cmp(&b.1));

        //TODO: use last selected user
        let (username, uid) = flags
            .user_datas
            .first()
            .map(|x| (x.name.clone(), NonZeroU32::new(x.uid)))
            .unwrap_or_default();

        let mut session_names: Vec<_> = flags.sessions.keys().map(|x| x.to_string()).collect();
        session_names.sort();

        let selected_session = uid
            .and_then(|uid| {
                flags
                    .greeter_config
                    .users
                    .get(&uid)
                    .and_then(|user| user.last_session.clone())
            })
            .or_else(|| session_names.first().cloned())
            .unwrap_or_default();
        let data_idx = Some(0);
        let selected_username = NameIndexPair { username, data_idx };

        let app = App {
            core,
            flags,
            greetd_sender: None,
            surface_ids: HashMap::new(),
            active_surface_id_opt: None,
            surface_images: HashMap::new(),
            surface_names: HashMap::new(),
            text_input_ids: HashMap::new(),
            network_icon_opt: None,
            power_info_opt: None,
            socket_state: SocketState::Pending,
            usernames,
            selected_username,
            prompt_opt: None,
            session_names,
            selected_session,
            active_layouts: Vec::new(),
            error_opt: None,
            dialog_page_opt: None,
            dropdown_opt: None,
            window_size: HashMap::new(),
            heartbeat_handle: None,
            time: crate::time::Time::new(),
        };
        (
            app,
            Task::batch(vec![
                crate::time::tick().map(|_| cosmic::Action::App(Message::Tick)),
                crate::time::tz_updates().map(|tz| cosmic::Action::App(Message::Tz(tz))),
            ]),
        )
    }

    /// Handle application events here.
    fn update(&mut self, message: Self::Message) -> Task<Message> {
        match message {
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
                                    self.surface_names.insert(surface_id, output_name.clone());
                                    self.surface_names
                                        .insert(subsurface_id, output_name.clone());
                                    self.surface_images.remove(&surface_id);
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
                        self.window_size.insert(
                            surface_id,
                            Size::new(unwrapped_size.0 as f32, unwrapped_size.1 as f32),
                        );

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
                        return Task::batch([
                            self.update_user_config(),
                            get_layer_surface(SctkLayerSurfaceSettings {
                                id: surface_id,
                                layer: Layer::Overlay,
                                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                                pointer_interactivity: true,
                                anchor: Anchor::TOP | Anchor::LEFT | Anchor::BOTTOM | Anchor::RIGHT,
                                output: IcedOutput::Output(output),
                                namespace: "cosmic-locker".into(),
                                size: Some((None, None)),
                                margin: IcedMargin {
                                    top: 0,
                                    bottom: 0,
                                    left: 0,
                                    right: 0,
                                },
                                exclusive_zone: -1,
                                size_limits: iced::Limits::NONE.min_width(1.0).min_height(1.0),
                            }),
                            cosmic::task::message(cosmic::Action::Cosmic(
                                cosmic::app::Action::Surface(msg),
                            )),
                        ]);
                    }
                    OutputEvent::Removed => {
                        log::info!("output {}: removed", output.id());
                        match self.surface_ids.remove(&output) {
                            Some(surface_id) => {
                                self.surface_images.remove(&surface_id);
                                self.window_size.remove(&surface_id);
                                if let Some(n) = self.surface_names.remove(&surface_id) {
                                    self.text_input_ids.remove(&n);
                                }
                                return destroy_layer_surface(surface_id);
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
            Message::Socket(socket_state) => {
                self.socket_state = socket_state;
                match &self.socket_state {
                    SocketState::Open => {
                        // When socket is opened, send create session
                        self.send_request(Request::CreateSession {
                            username: self.selected_username.username.clone(),
                        });
                    }
                    _ => {}
                }
            }
            Message::NetworkIcon(network_icon_opt) => {
                self.network_icon_opt = network_icon_opt;
            }
            Message::PowerInfo(power_info_opt) => {
                self.power_info_opt = power_info_opt;
            }
            Message::Prompt(prompt, secret, value_opt) => {
                let value_was_some = self
                    .prompt_opt
                    .as_ref()
                    .map_or(false, |(_, _, x)| x.is_some());
                let value_is_some = value_opt.is_some();
                self.prompt_opt = Some((prompt, secret, value_opt));
                if value_is_some && !value_was_some {
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
            Message::Session(selected_session) => {
                self.selected_session = selected_session;
                if self.dropdown_opt == Some(Dropdown::Session) {
                    self.dropdown_opt = None;
                }
            }
            Message::Username(username) => {
                if self.dropdown_opt == Some(Dropdown::User) {
                    self.dropdown_opt = None;
                }
                if username != self.selected_username.username {
                    let data_idx = Self::user_data_index(&self.flags.user_datas, &username);
                    self.selected_username = NameIndexPair { username, data_idx };
                    self.surface_images.clear();
                    if let Some(session) = data_idx.and_then(|i| {
                        self.flags
                            .user_datas
                            .get(i)
                            .and_then(|UserData { uid, .. }| {
                                NonZeroU32::new(*uid).and_then(|uid| {
                                    self.flags
                                        .greeter_config
                                        .users
                                        .get(&uid)
                                        .and_then(|conf| conf.last_session.as_deref())
                                })
                            })
                    }) {
                        session.clone_into(&mut self.selected_session);
                    };
                    match &self.socket_state {
                        SocketState::Open => {
                            self.prompt_opt = None;
                            self.send_request(Request::CancelSession);
                        }
                        _ => {}
                    }
                }
            }
            Message::ConfigUpdateUser => {
                let Some(user_entry) = self.selected_username.data_idx.and_then(|i| {
                    self.flags
                        .user_datas
                        .get(i)
                        .and_then(|UserData { uid, .. }| {
                            NonZeroU32::new(*uid)
                                .map(|uid| self.flags.greeter_config.users.entry(uid))
                        })
                }) else {
                    log::error!("Couldn't find user: {:?}", self.selected_username.username);
                    return Task::none();
                };

                let Some(handler) = self.flags.greeter_config_handler.as_mut() else {
                    log::error!(
                        "Failed to update config for {} (UID: {}): no config handler",
                        self.selected_username.username,
                        user_entry.key()
                    );
                    return Task::none();
                };

                let uid = *user_entry.key();
                match user_entry {
                    hash_map::Entry::Vacant(entry) => {
                        let last_session = Some(self.selected_session.clone());
                        entry.insert(cosmic_greeter_config::user::UserState { uid, last_session });
                    }
                    hash_map::Entry::Occupied(mut entry) => {
                        let last_session = entry.get_mut().last_session.as_mut();
                        if last_session
                            .as_ref()
                            .is_some_and(|session| session.as_str() == self.selected_session)
                        {
                            return Task::none();
                        }
                        if let Some(session) = last_session {
                            self.selected_session.clone_into(session);
                        } else {
                            let last_session = Some(self.selected_session.clone());
                            entry.insert(cosmic_greeter_config::user::UserState {
                                uid,
                                last_session,
                            });
                        }
                    }
                }

                // xxx Not sure why this doesn't work unless the handler is used directly
                // if let Err(err) = self
                //     .flags
                //     .greeter_config
                //     .set_users(&handler, self.flags.greeter_config.users.clone())
                if let Err(err) = handler.set("users", &self.flags.greeter_config.users) {
                    log::error!(
                        "Failed to set {} as last selected session for {} (UID: {}): {:?}",
                        self.selected_session,
                        self.selected_username.username,
                        uid,
                        err
                    );
                }
            }
            Message::Auth(response) => {
                self.prompt_opt = None;
                self.error_opt = None;
                self.send_request(Request::PostAuthMessageResponse { response });
            }
            Message::Login => {
                self.prompt_opt = None;
                self.error_opt = None;
                match self.flags.sessions.get(&self.selected_session).cloned() {
                    Some((cmd, env)) => {
                        self.send_request(Request::StartSession { cmd, env });
                        return self.update(Message::ConfigUpdateUser);
                    }
                    None => todo!("session {:?} not found", self.selected_session),
                }
            }
            Message::Error(error) => {
                self.error_opt = Some(error);
                self.send_request(Request::CancelSession);
            }
            Message::Reconnect => {
                return self.update_user_config();
            }
            Message::DialogCancel => {
                self.dialog_page_opt = None;
                if let Some(handle) = self.heartbeat_handle.take() {
                    handle.abort();
                }
            }
            Message::DialogConfirm => match self.dialog_page_opt.take() {
                Some(DialogPage::Restart(_)) => {
                    #[cfg(feature = "logind")]
                    return cosmic::task::future::<(), ()>(async move {
                        match crate::logind::reboot().await {
                            Ok(()) => (),
                            Err(err) => {
                                log::error!("failed to reboot: {:?}", err);
                            }
                        }
                    })
                    .discard();
                }
                Some(DialogPage::Shutdown(_)) => {
                    #[cfg(feature = "logind")]
                    return cosmic::task::future::<(), ()>(async move {
                        match crate::logind::power_off().await {
                            Ok(()) => (),
                            Err(err) => {
                                log::error!("failed to power off: {:?}", err);
                            }
                        }
                    })
                    .discard();
                }
                None => {}
            },
            Message::DropdownToggle(dropdown) => {
                if self.dropdown_opt == Some(dropdown) {
                    self.dropdown_opt = None;
                } else {
                    self.dropdown_opt = Some(dropdown);
                }
            }
            Message::KeyboardLayout(layout_i) => {
                if layout_i < self.active_layouts.len() {
                    self.active_layouts.swap(0, layout_i);
                    self.set_xkb_config();
                }
                if self.dropdown_opt == Some(Dropdown::Keyboard) {
                    self.dropdown_opt = None
                }
            }
            Message::Suspend => {
                #[cfg(feature = "logind")]
                return cosmic::task::future::<(), ()>(async move {
                    match crate::logind::suspend().await {
                        Ok(()) => (),
                        Err(err) => {
                            log::error!("failed to suspend: {:?}", err);
                        }
                    }
                })
                .discard();
            }
            Message::Restart | Message::Shutdown => {
                let instant = Instant::now();

                self.dialog_page_opt = Some(if matches!(message, Message::Restart) {
                    DialogPage::Restart(instant)
                } else {
                    DialogPage::Shutdown(instant)
                });

                if self.heartbeat_handle.is_none() {
                    let (heartbeat, handle) = cosmic::task::stream(
                        cosmic::iced_futures::stream::channel(1, |mut msg_tx| async move {
                            let mut interval = time::interval(Duration::from_secs(1));

                            loop {
                                // Send heartbeat once a second to update time
                                msg_tx
                                    .send(cosmic::Action::App(Message::Heartbeat))
                                    .await
                                    .unwrap();

                                interval.tick().await;
                            }
                        }),
                    )
                    .abortable();

                    self.heartbeat_handle = Some(handle);
                    return heartbeat;
                }
            }
            Message::Heartbeat => match self.dialog_page_opt {
                Some(DialogPage::Restart(instant)) | Some(DialogPage::Shutdown(instant)) => {
                    if DialogPage::remaining(instant).is_none() {
                        return self.update(Message::DialogConfirm);
                    }
                }
                None => {}
            },
            Message::Exit => {
                let mut commands = Vec::new();
                for (_output, surface_id) in self.surface_ids.drain() {
                    self.surface_images.remove(&surface_id);
                    self.surface_names.remove(&surface_id);
                    if let Some(n) = self.surface_names.remove(&surface_id) {
                        self.text_input_ids.remove(&n);
                    }
                    commands.push(destroy_layer_surface(surface_id));
                }
                commands.push(Task::perform(async { process::exit(0) }, |x| x));
                return Task::batch(commands);
            }
            Message::GreetdChannel(sender) => {
                self.greetd_sender = Some(sender);
            }
            Message::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(a),
                ));
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
            Message::Tick => {
                self.time.tick();
            }
            Message::Tz(tz) => {
                self.time.set_tz(tz);
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
        let mut subscriptions = vec![
            event::listen_with(|event, _, id| match event {
                iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
                    wayland_event,
                )) => match wayland_event {
                    WaylandEvent::Output(output_event, output) => {
                        Some(Message::OutputEvent(output_event, output))
                    }

                    _ => None,
                },
                iced::Event::Window(iced::window::Event::Focused) => Some(Message::Focus(id)),
                _ => None,
            }),
            ipc::subscription(),
        ];

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
