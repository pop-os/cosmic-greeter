// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

mod ipc;

use cosmic::app::{Core, Settings, Task};
use cosmic::cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity;
use cosmic::iced::{Point, Size};
use cosmic::iced_runtime::platform_specific::wayland::subsurface::SctkSubsurfaceSettings;
use cosmic::surface;
use cosmic::widget::text;
use cosmic::{
    Element,
    cosmic_config::{self, ConfigSet},
    executor,
    iced::{
        self, Background, Border, Length, Subscription, alignment,
        event::wayland::OutputEvent,
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
use cosmic_greeter_config::Config as CosmicGreeterConfig;
use cosmic_greeter_daemon::UserData;
use greetd_ipc::Request;
use std::sync::LazyLock;
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

use crate::{
    common::{self, Common, DEFAULT_MENU_ITEM_HEIGHT},
    fl,
};

static USERNAME_ID: LazyLock<iced::id::Id> = LazyLock::new(|| iced::id::Id::new("username-id"));

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

    let flags = Flags {
        user_datas,
        sessions,
        greeter_config,
        greeter_config_handler,
    };

    let settings = Settings::default().no_main_window(true);

    cosmic::app::run::<App>(settings, flags)?;

    Ok(())
}

#[derive(Clone)]
pub struct Flags {
    user_datas: Vec<UserData>,
    sessions: HashMap<String, (Vec<String>, Vec<String>)>,
    greeter_config: CosmicGreeterConfig,
    greeter_config_handler: Option<cosmic_config::Config>,
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
    Common(common::Message),
    OutputEvent(OutputEvent, WlOutput),
    Auth(Option<String>),
    ConfigUpdateUser,
    DialogCancel,
    DialogConfirm,
    DropdownToggle(Dropdown),
    Error(String),
    Exit,
    // Sets channel used to communicate with the greetd IPC subscription.
    GreetdChannel(tokio::sync::mpsc::Sender<Request>),
    Heartbeat,
    KeyboardLayout(usize),
    Login,
    Reconnect,
    Restart,
    Session(String),
    Shutdown,
    Socket(SocketState),
    Surface(surface::Action),
    Suspend,
    Username(String),
    EnterUser(bool, String),
}

impl From<common::Message> for Message {
    fn from(message: common::Message) -> Self {
        Self::Common(message)
    }
}

/// The [`App`] stores application-specific state.
pub struct App {
    common: Common<Message>,
    flags: Flags,
    greetd_sender: Option<tokio::sync::mpsc::Sender<greetd_ipc::Request>>,
    socket_state: SocketState,
    usernames: Vec<(String, String)>,
    selected_username: NameIndexPair,
    session_names: Vec<String>,
    selected_session: String,
    dialog_page_opt: Option<DialogPage>,
    dropdown_opt: Option<Dropdown>,
    heartbeat_handle: Option<cosmic::iced::task::Handle>,
    entering_name: bool,
}

impl App {
    fn menu(&self, id: SurfaceId) -> Element<Message> {
        let window_width = self
            .common
            .window_size
            .get(&id)
            .map(|s| s.width)
            .unwrap_or(800.);
        let menu_width = if window_width > 800. {
            800.
        } else {
            window_width
        };
        let left_element = {
            let military_time = self
                .selected_username
                .data_idx
                .and_then(|i| self.flags.user_datas.get(i))
                .map(|user_data| user_data.time_applet_config.military_time)
                .unwrap_or_default();
            let date_time_column = self.common.time.date_time_widget(military_time);

            let mut status_row = widget::row::with_capacity(2).padding(16.0).spacing(12.0);

            if let Some(network_icon) = self.common.network_icon_opt {
                status_row = status_row.push(widget::icon::from_name(network_icon));
            }

            if let Some((power_icon, power_percent)) = &self.common.power_info_opt {
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
            let dropdown_menu = |items: Vec<_>| {
                let item_cnt = items.len();

                let items = widget::column::with_children(items);
                let items = if item_cnt > 7 {
                    Element::from(
                        widget::scrollable(items)
                            .height(Length::Fixed(DEFAULT_MENU_ITEM_HEIGHT * 7.)),
                    )
                } else {
                    Element::from(items)
                };

                widget::container(items)
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
                let mut items = Vec::with_capacity(self.common.active_layouts.len());
                for (i, layout) in self.common.active_layouts.iter().enumerate() {
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
                let item_cnt = items.len();
                let menu_button = widget::menu::menu_button(vec![
                    Element::from(widget::Space::with_width(Length::Fixed(25.0))),
                    widget::text(fl!("enter-user"))
                        .align_x(iced::alignment::Horizontal::Left)
                        .into(),
                ])
                .on_press(Message::EnterUser(true, String::new()))
                .into();
                let items = if item_cnt >= 6 {
                    dropdown_menu(vec![
                        widget::scrollable(widget::column::with_children(items))
                            .height(Length::Fixed(DEFAULT_MENU_ITEM_HEIGHT * 6.))
                            .into(),
                        widget::divider::horizontal::light().into(),
                        menu_button,
                    ])
                } else {
                    items.push(menu_button);
                    dropdown_menu(items)
                };

                user_button = user_button.popup(items);
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
                        if !self.entering_name && user_data.name == self.selected_username.username
                        {
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
                                widget::container(widget::text::title4(&user_data.full_name))
                                    .width(Length::Fill)
                                    .align_x(alignment::Horizontal::Center),
                            );
                        }
                    }
                    if self.entering_name {
                        column = column.push(
                            widget::text_input(
                                fl!("type-username"),
                                self.selected_username.username.as_str(),
                            )
                            .id(USERNAME_ID.clone())
                            .on_input(|input| Message::EnterUser(false, input))
                            .on_submit(|v| Message::Username(v)),
                        )
                    }
                    match &self.common.prompt_opt {
                        Some((prompt, secret, value_opt)) => match value_opt {
                            Some(value) => {
                                let text_input_id = self
                                    .common
                                    .surface_names
                                    .get(&id)
                                    .and_then(|id| self.common.text_input_ids.get(id))
                                    .cloned()
                                    .unwrap_or_else(|| cosmic::widget::Id::new("text_input"));
                                let mut text_input = widget::secure_input(
                                    prompt.clone(),
                                    value.as_str(),
                                    Some(
                                        common::Message::Prompt(
                                            prompt.clone(),
                                            !*secret,
                                            Some(value.clone()),
                                        )
                                        .into(),
                                    ),
                                    *secret,
                                )
                                .id(text_input_id)
                                .on_input(|input| {
                                    common::Message::Prompt(prompt.clone(), *secret, Some(input))
                                        .into()
                                })
                                .on_submit(|v| Message::Auth(Some(v)));

                                if let Some(text_input_id) = self
                                    .common
                                    .surface_names
                                    .get(&id)
                                    .and_then(|id| self.common.text_input_ids.get(id))
                                {
                                    text_input = text_input.id(text_input_id.clone());
                                }

                                if *secret {
                                    text_input = text_input.password()
                                }

                                column = column.push(text_input);

                                if self.common.caps_lock {
                                    column = column.push(widget::text(fl!("caps-lock")));
                                }
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

            if let Some(error) = &self.common.error_opt {
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

        self.common.set_xkb_config(&user_data);
    }

    fn update_user_data(&mut self) -> Task<Message> {
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

        self.common.update_user_data(&user_data);

        // Ensure that user's xkb config is used
        self.common.set_xkb_config(&user_data);

        match &user_data.theme_opt {
            Some(theme) => {
                cosmic::command::set_theme(cosmic::Theme::custom(Arc::new(theme.clone())))
            }
            None => Task::none(),
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
        &self.common.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.common.core
    }

    /// Creates the application, and optionally emits command on initialize.
    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Message>) {
        let (mut common, common_task) = Common::init(core);
        common.on_output_event = Some(Box::new(|output_event, output| {
            Message::OutputEvent(output_event, output)
        }));

        //TODO: use full_name?
        let mut usernames: Vec<_> = flags
            .user_datas
            .iter()
            .map(|x| (x.name.clone(), x.full_name.clone()))
            .collect();
        usernames.sort_by(|a, b| a.1.cmp(&b.1));

        let last_user = flags.greeter_config.last_user.as_ref();

        let (username, uid) = last_user
            .and_then(|last_user| {
                flags
                    .user_datas
                    .iter()
                    .find(|d| d.uid == last_user.get())
                    .map(|x| (x.name.clone(), NonZeroU32::new(x.uid)))
            })
            .or_else(|| {
                flags
                    .user_datas
                    .first()
                    .map(|x| (x.name.clone(), NonZeroU32::new(x.uid)))
            })
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
            common,
            flags,
            greetd_sender: None,
            socket_state: SocketState::Pending,
            usernames,
            selected_username,
            session_names,
            selected_session,
            dialog_page_opt: None,
            dropdown_opt: None,
            heartbeat_handle: None,
            entering_name: false,
        };
        (app, common_task)
    }

    /// Handle application events here.
    fn update(&mut self, message: Self::Message) -> Task<Message> {
        match message {
            Message::Common(common_message) => {
                return self.common.update(common_message);
            }
            Message::OutputEvent(output_event, output) => {
                match output_event {
                    OutputEvent::Created(output_info_opt) => {
                        log::info!("output {}: created", output.id());

                        let surface_id = SurfaceId::unique();
                        let subsurface_id = SurfaceId::unique();

                        match self.common.surface_ids.insert(output.clone(), surface_id) {
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
                                    self.common
                                        .surface_names
                                        .insert(surface_id, output_name.clone());
                                    self.common
                                        .surface_names
                                        .insert(subsurface_id, output_name.clone());
                                    self.common.surface_images.remove(&surface_id);
                                    let text_input_id =
                                        widget::Id::new(format!("input-{output_name}",));
                                    self.common
                                        .text_input_ids
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
                        self.common.window_size.insert(
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
                            self.update_user_data(),
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
                        match self.common.surface_ids.remove(&output) {
                            Some(surface_id) => {
                                self.common.surface_images.remove(&surface_id);
                                self.common.window_size.remove(&surface_id);
                                if let Some(n) = self.common.surface_names.remove(&surface_id) {
                                    self.common.text_input_ids.remove(&n);
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
            Message::Session(selected_session) => {
                self.selected_session = selected_session;
                if self.dropdown_opt == Some(Dropdown::Session) {
                    self.dropdown_opt = None;
                }
            }
            Message::EnterUser(focus_input, username) => {
                if self.dropdown_opt == Some(Dropdown::User) {
                    self.dropdown_opt = None;
                }
                self.entering_name = true;
                self.selected_username = NameIndexPair {
                    data_idx: self
                        .flags
                        .user_datas
                        .iter()
                        .position(|d| d.name == username),
                    username,
                };
                if focus_input {
                    return widget::text_input::focus(USERNAME_ID.clone());
                }
            }
            Message::Username(username) => {
                if self.dropdown_opt == Some(Dropdown::User) {
                    self.dropdown_opt = None;
                }
                if self.entering_name || username != self.selected_username.username {
                    self.entering_name = false;
                    let data_idx = self
                        .flags
                        .user_datas
                        .iter()
                        .position(|d| d.name == username);
                    self.selected_username = NameIndexPair { username, data_idx };
                    self.common.surface_images.clear();
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
                            self.common.prompt_opt = None;
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
                    log::error!(
                        "Couldn't find user: {:?} {:?}",
                        self.selected_username.username,
                        self.selected_username.data_idx,
                    );
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
                self.flags.greeter_config.last_user = Some(uid);
                if let Err(err) = handler.set("last_user", &self.flags.greeter_config.last_user) {
                    log::error!(
                        "Failed to set {:?} as last user: {:?}",
                        self.flags.greeter_config.last_user,
                        err
                    );
                }
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
                self.common.prompt_opt = None;
                self.common.error_opt = None;
                self.send_request(Request::PostAuthMessageResponse { response });
            }
            Message::Login => {
                self.common.prompt_opt = None;
                self.common.error_opt = None;
                match self.flags.sessions.get(&self.selected_session).cloned() {
                    Some((cmd, env)) => {
                        self.send_request(Request::StartSession { cmd, env });
                        return self.update(Message::ConfigUpdateUser);
                    }
                    None => todo!("session {:?} not found", self.selected_session),
                }
            }
            Message::Error(error) => {
                self.common.error_opt = Some(error);
                self.send_request(Request::CancelSession);
            }
            Message::Reconnect => {
                return self.update_user_data();
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
                if layout_i < self.common.active_layouts.len() {
                    self.common.active_layouts.swap(0, layout_i);
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
                for (_output, surface_id) in self.common.surface_ids.drain() {
                    self.common.surface_images.remove(&surface_id);
                    self.common.surface_names.remove(&surface_id);
                    if let Some(n) = self.common.surface_names.remove(&surface_id) {
                        self.common.text_input_ids.remove(&n);
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
            .common
            .surface_images
            .get(&surface_id)
            .unwrap_or(&self.common.fallback_background);
        widget::image(img)
            .content_fit(iced::ContentFit::Cover)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::batch([
            self.common.subscription().map(Message::from),
            ipc::subscription(),
        ])
    }
}
