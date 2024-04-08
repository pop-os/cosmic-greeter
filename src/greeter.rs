// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::app::{message, Command, Core, Settings};
use cosmic::Theme;
use cosmic::{
    executor,
    iced::{
        self, alignment,
        event::{
            self,
            wayland::{Event as WaylandEvent, LayerEvent, OutputEvent},
        },
        futures::{self, SinkExt},
        subscription,
        wayland::{
            actions::layer_surface::{IcedMargin, IcedOutput, SctkLayerSurfaceSettings},
            layer_surface::{
                destroy_layer_surface, get_layer_surface, Anchor, KeyboardInteractivity, Layer,
            },
        },
        Length, Subscription,
    },
    iced_runtime::core::window::Id as SurfaceId,
    style, widget, Element,
};
use cosmic_greeter_daemon::{UserData, WallpaperData};
use greetd_ipc::{codec::SyncCodec, AuthMessageType, Request, Response};
use std::{
    collections::HashMap,
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process,
    sync::Arc,
};
use tokio::{net::UnixStream, time};
use wayland_client::{protocol::wl_output::WlOutput, Proxy};
use zbus::{dbus_proxy, Connection};

use crate::theme::get_theme;

#[dbus_proxy(
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
            .map(|user| {
                //TODO: use accountsservice
                let icon_path = Path::new("/var/lib/AccountsService/icons").join(&user.name);
                let icon_opt = if icon_path.is_file() {
                    match fs::read(&icon_path) {
                        Ok(icon_data) => Some(icon_data),
                        Err(err) => {
                            log::error!("failed to read {:?}: {:?}", icon_path, err);
                            None
                        }
                    }
                } else {
                    None
                };

                UserData {
                    uid: user.uid,
                    name: user.name,
                    full_name_opt: user
                        .gecos
                        .map(|gecos| gecos.split(',').next().unwrap_or_default().to_string()),
                    icon_opt,
                    theme_opt: None,
                    wallpapers_opt: None,
                }
            })
            .collect()
    }
}

pub fn main() -> Result<(), Box<dyn Error>> {
    let mut user_datas = match futures::executor::block_on(async { user_data_dbus().await }) {
        Ok(ok) => ok,
        Err(err) => {
            log::error!("failed to load user data from daemon: {}", err);
            user_data_fallback()
        }
    };

    // Sort user data by uid
    user_datas.sort_by(|a, b| a.uid.cmp(&b.uid));

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

                let mut command = match session_type {
                    SessionType::X11 => {
                        //TODO: xinit may be better, but more complicated to set up
                        vec![
                            "startx".to_string(),
                            "/usr/bin/env".to_string(),
                            "XDG_SESSION_TYPE=x11".to_string(),
                        ]
                    }
                    SessionType::Wayland => {
                        vec![
                            "/usr/bin/env".to_string(),
                            "XDG_SESSION_TYPE=wayland".to_string(),
                        ]
                    }
                };

                if let Some(desktop_names) = entry.section("Desktop Entry").attr("DesktopNames") {
                    command.push(format!("XDG_CURRENT_DESKTOP={desktop_names}"));
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

                log::warn!("session {} using command {:?}", name, command);
                match sessions.insert(name.to_string(), command) {
                    Some(some) => {
                        log::warn!("session {} overwrote old command {:?}", name, some);
                    }
                    None => {}
                }
            }
        }
        sessions
    };

    let fallback_background =
        widget::image::Handle::from_memory(include_bytes!("../res/background.png"));

    let flags = Flags {
        user_datas,
        sessions,
        fallback_background,
    };

    let settings = Settings::default()
        .theme(Theme::custom(Arc::new(get_theme())))
        .no_main_window(true);

    cosmic::app::run::<App>(settings, flags)?;

    Ok(())
}

async fn request_message(socket: Arc<UnixStream>, request: Request) -> Message {
    //TODO: handle errors
    socket.writable().await.unwrap();
    {
        let mut bytes = Vec::<u8>::new();
        request.write_to(&mut bytes).unwrap();
        socket.try_write(&bytes).unwrap();
    }

    //TODO: handle responses at any time?
    loop {
        socket.readable().await.unwrap();

        let mut bytes = Vec::<u8>::with_capacity(4096);
        match socket.try_read_buf(&mut bytes) {
            Ok(0) => break,
            Ok(_count) => {
                let mut cursor = io::Cursor::new(bytes);
                let response = Response::read_from(&mut cursor).unwrap();
                log::info!("{:?}", response);
                match response {
                    Response::AuthMessage {
                        auth_message_type,
                        auth_message,
                    } => match auth_message_type {
                        AuthMessageType::Secret => {
                            return Message::Prompt(auth_message, true, Some(String::new()));
                        }
                        AuthMessageType::Visible => {
                            return Message::Prompt(auth_message, false, Some(String::new()));
                        }
                        //TODO: treat error type differently?
                        AuthMessageType::Info | AuthMessageType::Error => {
                            return Message::Prompt(auth_message, false, None);
                        }
                    },
                    Response::Error {
                        error_type: _,
                        description,
                    } => {
                        //TODO: use error_type?
                        return Message::Error(socket, description);
                    }
                    Response::Success => match request {
                        Request::CreateSession { .. } => {
                            // User has no auth required, proceed to login
                            return Message::Login(socket);
                        }
                        Request::PostAuthMessageResponse { .. } => {
                            // All auth is completed, proceed to login
                            return Message::Login(socket);
                        }
                        Request::StartSession { .. } => {
                            // Session has been started, exit greeter
                            return Message::Exit;
                        }
                        Request::CancelSession => {
                            // Reconnect to socket
                            return Message::Reconnect;
                        }
                    },
                }
            }
            Err(err) => match err.kind() {
                io::ErrorKind::WouldBlock => continue,
                _ => {
                    log::error!("failed to read socket: {:?}", err);
                    break;
                }
            },
        }
    }

    Message::None
}

fn request_command(socket: Arc<UnixStream>, request: Request) -> Command<Message> {
    Command::perform(
        async move { message::app(request_message(socket, request).await) },
        |x| x,
    )
}

#[derive(Clone)]
pub struct Flags {
    user_datas: Vec<UserData>,
    sessions: HashMap<String, Vec<String>>,
    fallback_background: widget::image::Handle,
}

#[derive(Clone, Debug)]
pub enum SocketState {
    /// Opening GREETD_SOCK
    Pending,
    /// GREETD_SOCK is open
    Open(Arc<UnixStream>),
    /// No GREETD_SOCK variable set
    NotSet,
    /// Failed to open GREETD_SOCK
    Error(Arc<io::Error>),
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    None,
    OutputEvent(OutputEvent, WlOutput),
    LayerEvent(LayerEvent, SurfaceId),
    Socket(SocketState),
    NetworkIcon(Option<&'static str>),
    PowerInfo(Option<(String, f64)>),
    Prompt(String, bool, Option<String>),
    Session(String),
    Username(Arc<UnixStream>, String),
    Auth(Arc<UnixStream>, Option<String>),
    Login(Arc<UnixStream>),
    Error(Arc<UnixStream>, String),
    Reconnect,
    Suspend,
    Exit,
}

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    flags: Flags,
    surface_ids: HashMap<WlOutput, SurfaceId>,
    active_surface_id_opt: Option<SurfaceId>,
    surface_images: HashMap<SurfaceId, widget::image::Handle>,
    surface_names: HashMap<SurfaceId, String>,
    text_input_ids: HashMap<SurfaceId, widget::Id>,
    network_icon_opt: Option<&'static str>,
    power_info_opt: Option<(String, f64)>,
    socket_state: SocketState,
    username_opt: Option<String>,
    prompt_opt: Option<(String, bool, Option<String>)>,
    session_names: Vec<String>,
    selected_session: String,
    error_opt: Option<String>,
}

impl App {
    fn update_user_config(&mut self) -> Command<Message> {
        let username = match &self.username_opt {
            Some(some) => some,
            None => return Command::none(),
        };

        let user_data = match self.flags.user_datas.iter().find(|x| &x.name == username) {
            Some(some) => some,
            None => return Command::none(),
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
                                let image = widget::image::Handle::from_memory(bytes.clone());
                                self.surface_images.insert(*surface_id, image);
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

        match &user_data.theme_opt {
            Some(theme) => {
                cosmic::app::command::set_theme(cosmic::Theme::custom(Arc::new(theme.clone())))
            }
            None => Command::none(),
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

        let mut session_names: Vec<_> = flags.sessions.keys().map(|x| x.to_string()).collect();
        session_names.sort();

        //TODO: determine default session?
        let selected_session = session_names.first().cloned().unwrap_or(String::new());

        let mut app = App {
            core,
            flags,
            surface_ids: HashMap::new(),
            active_surface_id_opt: None,
            surface_images: HashMap::new(),
            surface_names: HashMap::new(),
            text_input_ids: HashMap::new(),
            network_icon_opt: None,
            power_info_opt: None,
            socket_state: SocketState::Pending,
            username_opt: None,
            prompt_opt: None,
            session_names,
            selected_session,
            error_opt: None,
        };
        let command = app.update(Message::Reconnect);
        (app, command)
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

                        return Command::batch([
                            self.update_user_config(),
                            get_layer_surface(SctkLayerSurfaceSettings {
                                id: surface_id,
                                layer: Layer::Overlay,
                                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                                pointer_interactivity: true,
                                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
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
                            widget::text_input::focus(text_input_id),
                        ]);
                    }
                    OutputEvent::Removed => {
                        log::info!("output {}: removed", output.id());
                        match self.surface_ids.remove(&output) {
                            Some(surface_id) => {
                                self.surface_images.remove(&surface_id);
                                self.surface_names.remove(&surface_id);
                                self.text_input_ids.remove(&surface_id);
                                return destroy_layer_surface(surface_id);
                            }
                            None => {
                                log::warn!("output {}: no surface found", output.id());
                            }
                        }
                    }
                    OutputEvent::InfoUpdate(output_info) => {
                        log::info!("output {}: info update {:#?}", output.id(), output_info);
                    }
                }
            }
            Message::LayerEvent(layer_event, surface_id) => match layer_event {
                LayerEvent::Focused => {
                    log::info!("focus surface {:?}", surface_id);
                    self.active_surface_id_opt = Some(surface_id);
                    if let Some(text_input_id) = self.text_input_ids.get(&surface_id) {
                        return widget::text_input::focus(text_input_id.clone());
                    }
                }
                _ => {}
            },
            Message::Socket(socket_state) => {
                self.socket_state = socket_state;
                match &self.socket_state {
                    SocketState::Open(socket) => {
                        // When socket is opened, request default user
                        //TODO: choose last used user
                        match self.flags.user_datas.first().map(|x| x.name.clone()) {
                            Some(username) => {
                                let socket = socket.clone();
                                return self.update(Message::Username(socket, username));
                            }
                            None => {}
                        }
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
                        if let Some(text_input_id) = self.text_input_ids.get(&surface_id) {
                            return widget::text_input::focus(text_input_id.clone());
                        }
                    }
                }
            }
            Message::Session(selected_session) => {
                self.selected_session = selected_session;
            }
            Message::Username(socket, username) => {
                self.username_opt = Some(username.clone());
                self.prompt_opt = None;
                self.surface_images.clear();
                return Command::batch([
                    self.update_user_config(),
                    request_command(socket, Request::CreateSession { username }),
                ]);
            }
            Message::Auth(socket, response) => {
                self.prompt_opt = None;
                self.error_opt = None;
                return request_command(socket, Request::PostAuthMessageResponse { response });
            }
            Message::Login(socket) => {
                self.prompt_opt = None;
                self.error_opt = None;
                match self.flags.sessions.get(&self.selected_session).cloned() {
                    Some(cmd) => {
                        return request_command(
                            socket,
                            Request::StartSession {
                                cmd,
                                env: Vec::new(),
                            },
                        );
                    }
                    None => todo!("session {:?} not found", self.selected_session),
                }
            }
            Message::Error(socket, error) => {
                self.error_opt = Some(error);
                return request_command(socket, Request::CancelSession);
            }
            Message::Reconnect => {
                return Command::perform(
                    async {
                        message::app(Message::Socket(match env::var_os("GREETD_SOCK") {
                            Some(socket_path) => match UnixStream::connect(&socket_path).await {
                                Ok(socket) => SocketState::Open(Arc::new(socket)),
                                Err(err) => SocketState::Error(Arc::new(err)),
                            },
                            None => SocketState::NotSet,
                        }))
                    },
                    |x| x,
                );
            }
            Message::Suspend => {
                #[cfg(feature = "logind")]
                return Command::perform(
                    async move {
                        match crate::logind::suspend().await {
                            Ok(()) => (),
                            Err(err) => {
                                log::error!("failed to suspend: {:?}", err);
                            }
                        }
                        message::none()
                    },
                    |x| x,
                );
            }
            Message::Exit => {
                let mut commands = Vec::new();
                for (_output, surface_id) in self.surface_ids.drain() {
                    self.surface_images.remove(&surface_id);
                    self.surface_names.remove(&surface_id);
                    self.text_input_ids.remove(&surface_id);
                    commands.push(destroy_layer_surface(surface_id));
                }
                commands.push(Command::perform(async { process::exit(0) }, |x| x));
                return Command::batch(commands);
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
                let mut column = widget::column::with_capacity(2).padding(16.0).spacing(12.0);

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
                widget::button(widget::icon::from_name("application-menu-symbolic"))
                    .padding(12.0)
                    .on_press(Message::None),
                widget::button(widget::icon::from_name("system-suspend-symbolic"))
                    .padding(12.0)
                    .on_press(Message::Suspend),
                widget::button(widget::icon::from_name("system-reboot-symbolic"))
                    .padding(12.0)
                    .on_press(Message::None),
                widget::button(widget::icon::from_name("system-shutdown-symbolic"))
                    .padding(12.0)
                    .on_press(Message::None),
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

            match &self.socket_state {
                SocketState::Pending => {
                    column = column.push(widget::text("Opening GREETD_SOCK"));
                }
                SocketState::Open(socket) => {
                    match &self.username_opt {
                        Some(username) => {
                            for user_data in &self.flags.user_datas {
                                if &user_data.name == username {
                                    match &user_data.icon_opt {
                                        Some(icon) => {
                                            column = column.push(
                                                widget::container(
                                                    widget::Image::new(
                                                        //TODO: cache handle
                                                        widget::image::Handle::from_memory(
                                                            icon.clone(),
                                                        ),
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
                                    match &user_data.full_name_opt {
                                        Some(full_name) => {
                                            column = column.push(
                                                widget::container(widget::text::title4(full_name))
                                                    .width(Length::Fill)
                                                    .align_x(alignment::Horizontal::Center),
                                            );
                                        }
                                        None => {}
                                    }
                                }
                            }
                        }
                        None => {
                            let mut row = widget::row::with_capacity(self.flags.user_datas.len())
                                .spacing(12.0);
                            for user_data in &self.flags.user_datas {
                                let mut column = widget::column::with_capacity(2).spacing(12.0);

                                match &user_data.icon_opt {
                                    Some(icon) => {
                                        column = column.push(
                                            widget::container(
                                                widget::Image::new(
                                                    //TODO: cache handle
                                                    widget::image::Handle::from_memory(
                                                        icon.clone(),
                                                    ),
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
                                match &user_data.full_name_opt {
                                    Some(full_name) => {
                                        column = column.push(
                                            widget::container(widget::text::title4(full_name))
                                                .width(Length::Fill)
                                                .align_x(alignment::Horizontal::Center),
                                        );
                                    }
                                    None => {}
                                }

                                row = row.push(
                                    widget::MouseArea::new(
                                        widget::layer_container(column)
                                            .layer(cosmic::cosmic_theme::Layer::Primary)
                                            .padding(16)
                                            .style(cosmic::theme::Container::Card),
                                    )
                                    .on_press(
                                        Message::Username(socket.clone(), user_data.name.clone()),
                                    ),
                                );
                            }
                            column = column.push(row);
                        }
                    }
                    match &self.prompt_opt {
                        Some((prompt, secret, value_opt)) => match value_opt {
                            Some(value) => {
                                let mut text_input =
                                    widget::text_input(prompt.clone(), value.clone())
                                        .leading_icon(
                                            widget::icon::from_name("system-lock-screen-symbolic")
                                                .into(),
                                        )
                                        .trailing_icon(
                                            widget::icon::from_name("document-properties-symbolic")
                                                .into(),
                                        )
                                        .on_input(|value| {
                                            Message::Prompt(prompt.clone(), *secret, Some(value))
                                        })
                                        .on_submit(Message::Auth(
                                            socket.clone(),
                                            Some(value.clone()),
                                        ));

                                if let Some(text_input_id) = self.text_input_ids.get(&surface_id) {
                                    text_input = text_input.id(text_input_id.clone());
                                }

                                if *secret {
                                    text_input = text_input.password()
                                }

                                column = column.push(text_input);
                            }
                            None => {
                                column = column.push(
                                    widget::button("Confirm")
                                        .on_press(Message::Auth(socket.clone(), None)),
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

            column = column.push(
                //TODO: use button
                iced::widget::pick_list(
                    &self.session_names,
                    Some(self.selected_session.clone()),
                    Message::Session,
                ),
            );

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
            None => self.flags.fallback_background.clone(),
        })
        .content_fit(iced::ContentFit::Cover)
        .into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        struct HeartbeatSubscription;

        //TODO: just use one vec for all subscriptions
        let mut extra_suscriptions = Vec::with_capacity(2);

        #[cfg(feature = "networkmanager")]
        {
            extra_suscriptions.push(
                crate::networkmanager::subscription()
                    .map(|icon_opt| Message::NetworkIcon(icon_opt)),
            );
        }

        #[cfg(feature = "upower")]
        {
            extra_suscriptions
                .push(crate::upower::subscription().map(|info_opt| Message::PowerInfo(info_opt)));
        }

        Subscription::batch([
            event::listen_with(|event, _| match event {
                iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
                    wayland_event,
                )) => match wayland_event {
                    WaylandEvent::Output(output_event, output) => {
                        Some(Message::OutputEvent(output_event, output))
                    }
                    WaylandEvent::Layer(layer_event, _surface, surface_id) => {
                        Some(Message::LayerEvent(layer_event, surface_id))
                    }
                    _ => None,
                },
                _ => None,
            }),
            subscription::channel(
                std::any::TypeId::of::<HeartbeatSubscription>(),
                16,
                |mut msg_tx| async move {
                    loop {
                        // Send heartbeat once a second to update time
                        //TODO: only send this when needed
                        msg_tx.send(Message::None).await.unwrap();
                        time::sleep(time::Duration::new(1, 0)).await;
                    }
                },
            ),
            Subscription::batch(extra_suscriptions),
        ])
    }
}
