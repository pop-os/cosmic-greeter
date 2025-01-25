// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

mod ipc;

use cosmic::app::{message, Command, Core, Settings};
use cosmic::{
    cosmic_config::{self, ConfigSet, CosmicConfigEntry},
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
        Background, Border, Length, Subscription,
    },
    iced_runtime::core::window::Id as SurfaceId,
    style, theme, widget, Element,
};
use cosmic_comp_config::CosmicCompConfig;
use cosmic_greeter_config::Config as CosmicGreeterConfig;
use cosmic_greeter_daemon::{UserData, WallpaperData};
use greetd_ipc::Request;
use std::{
    collections::{hash_map, HashMap},
    error::Error,
    fs, io,
    num::NonZeroU32,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time;
use wayland_client::{protocol::wl_output::WlOutput, Proxy};
use zbus::{proxy, Connection};

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
                        .filter(|s| !s.is_empty())
                        .map(|gecos| gecos.split(',').next().unwrap_or_default().to_string()),
                    icon_opt,
                    theme_opt: None,
                    interface_font_opt: None,
                    wallpapers_opt: None,
                    xkb_config_opt: None,
                    clock_military_time: false,
                    // clock_show_seconds: false,
                }
            })
            .collect()
    }
}

pub fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    crate::localize::localize();

    let mut user_datas = match futures::executor::block_on(async { user_data_dbus().await }) {
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

    let tk_config_handler = match cosmic_config::Config::new("com.system76.CosmicTk", 1) {
        Ok(config_handler) => Some(config_handler),
        Err(err) => {
            log::error!("failed to create cosmic-tk config handler: {}", err);
            None
        }
    };

    let fallback_background =
        widget::image::Handle::from_memory(include_bytes!("../res/background.jpg"));

    let flags = Flags {
        user_datas,
        sessions,
        layouts_opt,
        comp_config_handler,
        tk_config_handler,
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
    tk_config_handler: Option<cosmic_config::Config>,
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
    // Sets channel used to communicate with the greetd IPC subscription.
    GreetdChannel(tokio::sync::mpsc::Sender<Request>),
    Heartbeat,
    KeyboardLayout(usize),
    LayerEvent(LayerEvent, SurfaceId),
    Login,
    NetworkIcon(Option<&'static str>),
    None,
    OutputEvent(OutputEvent, WlOutput),
    PowerInfo(Option<(String, f64)>),
    Prompt(String, bool, Option<String>),
    Reconnect,
    Restart,
    Session(String),
    Shutdown,
    Socket(SocketState),
    Suspend,
    Username(String),
}

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    flags: Flags,
    greetd_sender: Option<tokio::sync::mpsc::Sender<greetd_ipc::Request>>,
    surface_ids: HashMap<WlOutput, SurfaceId>,
    active_surface_id_opt: Option<SurfaceId>,
    surface_images: HashMap<SurfaceId, widget::image::Handle>,
    surface_names: HashMap<SurfaceId, String>,
    text_input_ids: HashMap<SurfaceId, widget::Id>,
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
}

impl App {
    /// Send a [`Request`] to the greetd IPC subscription.
    fn send_request(&self, request: Request) -> Command<Message> {
        if let Some(ref sender) = self.greetd_sender {
            let sender = sender.clone();
            return cosmic::command::future(async move {
                _ = sender.send(request).await;
                message::none()
            });
        }

        Command::none()
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

    fn set_interface_font(&self) {
        let user_data = match self
            .selected_username
            .data_idx
            .and_then(|i| self.flags.user_datas.get(i))
        {
            Some(some) => some,
            None => return,
        };

        if let Some(tk_config_handler) = &self.flags.tk_config_handler {
            if let Some(font) = &user_data.interface_font_opt {
                match tk_config_handler.set("interface_font", font) {
                    Ok(()) => log::info!("updated cosmic-tk interface_font"),
                    Err(err) => log::error!("failed to update cosmic-tk interface_font: {}", err),
                }
            }
        }
    }

    fn update_user_config(&mut self) -> Command<Message> {
        let user_data = match self
            .selected_username
            .data_idx
            .and_then(|i| self.flags.user_datas.get(i))
        {
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

        if let Some(font) = &user_data.interface_font_opt {
            self.set_interface_font();
        }

        match &user_data.theme_opt {
            Some(theme) => {
                cosmic::app::command::set_theme(cosmic::Theme::custom(Arc::new(theme.clone())))
            }
            None => Command::none(),
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
    fn init(mut core: Core, flags: Self::Flags) -> (Self, Command<Self::Message>) {
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        core.window.sharp_corners = true;
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
        };
        (app, Command::none())
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
                    OutputEvent::InfoUpdate(_output_info) => {
                        log::info!("output {}: info update", output.id());
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
                    SocketState::Open => {
                        // When socket is opened, send create session
                        return self.send_request(Request::CreateSession {
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
                        if let Some(text_input_id) = self.text_input_ids.get(&surface_id) {
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
                            return self.send_request(Request::CancelSession);
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
                    return Command::none();
                };

                let Some(handler) = self.flags.greeter_config_handler.as_mut() else {
                    log::error!(
                        "Failed to update config for {} (UID: {}): no config handler",
                        self.selected_username.username,
                        user_entry.key()
                    );
                    return Command::none();
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
                            return Command::none();
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
                return self.send_request(Request::PostAuthMessageResponse { response });
            }
            Message::Login => {
                self.prompt_opt = None;
                self.error_opt = None;
                match self.flags.sessions.get(&self.selected_session).cloned() {
                    Some((cmd, env)) => {
                        return Command::batch([
                            self.update(Message::ConfigUpdateUser),
                            self.send_request(Request::StartSession { cmd, env }),
                        ]);
                    }
                    None => todo!("session {:?} not found", self.selected_session),
                }
            }
            Message::Error(error) => {
                self.error_opt = Some(error);
                return self.send_request(Request::CancelSession);
            }
            Message::Reconnect => {
                return self.update_user_config();
            }
            Message::DialogCancel => {
                self.dialog_page_opt = None;
            }
            Message::DialogConfirm => match self.dialog_page_opt.take() {
                Some(DialogPage::Restart(_)) => {
                    #[cfg(feature = "logind")]
                    return cosmic::command::future(async move {
                        match crate::logind::reboot().await {
                            Ok(()) => (),
                            Err(err) => {
                                log::error!("failed to reboot: {:?}", err);
                            }
                        }
                        message::none()
                    });
                }
                Some(DialogPage::Shutdown(_)) => {
                    #[cfg(feature = "logind")]
                    return cosmic::command::future(async move {
                        match crate::logind::power_off().await {
                            Ok(()) => (),
                            Err(err) => {
                                log::error!("failed to power off: {:?}", err);
                            }
                        }
                        message::none()
                    });
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
                return cosmic::command::future(async move {
                    match crate::logind::suspend().await {
                        Ok(()) => (),
                        Err(err) => {
                            log::error!("failed to suspend: {:?}", err);
                        }
                    }
                    message::none()
                });
            }
            Message::Restart => {
                self.dialog_page_opt = Some(DialogPage::Restart(Instant::now()));
            }
            Message::Shutdown => {
                self.dialog_page_opt = Some(DialogPage::Shutdown(Instant::now()));
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
                    self.text_input_ids.remove(&surface_id);
                    commands.push(destroy_layer_surface(surface_id));
                }
                commands.push(Command::perform(async { process::exit(0) }, |x| x));
                return Command::batch(commands);
            }
            Message::GreetdChannel(sender) => {
                self.greetd_sender = Some(sender);
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
                let locale = *crate::localize::LANGUAGE_CHRONO;

                let date = dt.format_localized("%A, %B %-d", locale);
                column = column
                    .push(widget::text::title2(format!("{}", date)).style(style::Text::Accent));

                let (time, time_size) = if self
                    .selected_username
                    .data_idx
                    .and_then(|i| {
                        self.flags
                            .user_datas
                            .get(i)
                            .map(|user| user.clock_military_time)
                    })
                    .unwrap_or_default()
                {
                    (dt.format_localized("%R", locale), 112.0)
                } else {
                    // xxx format_localized doesn't seem to show am/pm for some languages, such as
                    // French or Hungarian. This is apparently correct
                    // Also, time size needs to be reduced a bit here so that it fits on one line
                    (dt.format_localized("%I:%M %p", locale), 75.0)
                };
                column = column.push(
                    widget::text(format!("{}", time))
                        .size(time_size)
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
                            .horizontal_alignment(iced::alignment::Horizontal::Left)
                            .into(),
                    ])
                    .on_press(message),
                )
            };
            let dropdown_menu = |items| {
                widget::container(widget::column::with_children(items))
                    .padding(1)
                    //TODO: move style to libcosmic
                    .style(theme::Container::custom(|theme| {
                        let cosmic = theme.cosmic();
                        let component = &cosmic.background.component;
                        widget::container::Appearance {
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
                widget::button(widget::icon::from_name("input-keyboard-symbolic"))
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
                widget::button(widget::icon::from_name("system-users-symbolic"))
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
                widget::button(widget::icon::from_name("application-menu-symbolic"))
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
                    fl!("keyboard-layout"),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(user_button, fl!("user"), widget::tooltip::Position::Top),
                widget::tooltip(
                    session_button,
                    fl!("session"),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(
                    widget::button(widget::icon::from_name("system-suspend-symbolic"))
                        .padding(12.0)
                        .on_press(Message::Suspend),
                    fl!("suspend"),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(
                    widget::button(widget::icon::from_name("system-reboot-symbolic"))
                        .padding(12.0)
                        .on_press(Message::Restart),
                    fl!("restart"),
                    widget::tooltip::Position::Top
                ),
                widget::tooltip(
                    widget::button(widget::icon::from_name("system-shutdown-symbolic"))
                        .padding(12.0)
                        .on_press(Message::Shutdown),
                    fl!("shutdown"),
                    widget::tooltip::Position::Top
                )
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
                SocketState::Open => {
                    for user_data in &self.flags.user_datas {
                        if &user_data.name == &self.selected_username.username {
                            match &user_data.icon_opt {
                                Some(icon) => {
                                    column = column.push(
                                        widget::container(
                                            widget::Image::new(
                                                //TODO: cache handle
                                                widget::image::Handle::from_memory(icon.clone()),
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
                    match &self.prompt_opt {
                        Some((prompt, secret, value_opt)) => match value_opt {
                            Some(value) => {
                                let mut text_input = widget::secure_input(
                                    prompt.clone(),
                                    value.clone(),
                                    Some(Message::Prompt(
                                        prompt.clone(),
                                        !*secret,
                                        Some(value.clone()),
                                    )),
                                    *secret,
                                )
                                .on_input(|value| {
                                    Message::Prompt(prompt.clone(), *secret, Some(value))
                                })
                                .on_submit(Message::Auth(Some(value.clone())));

                                if let Some(text_input_id) = self.text_input_ids.get(&surface_id) {
                                    text_input = text_input.id(text_input_id.clone());
                                }

                                if *secret {
                                    text_input = text_input.password()
                                }

                                column = column.push(text_input);
                            }
                            None => {
                                column = column
                                    .push(widget::button("Confirm").on_press(Message::Auth(None)));
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

        let content = crate::image_container::ImageContainer::new(
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
        .content_fit(iced::ContentFit::Cover);

        let popover = widget::popover(content).modal(true);
        match self.dialog_page_opt {
            Some(DialogPage::Restart(instant)) => {
                let remaining = DialogPage::remaining(instant).unwrap_or_default();
                popover
                    .popup(
                        widget::dialog(fl!("restart-now"))
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
                        widget::dialog(fl!("shutdown-now"))
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

    fn subscription(&self) -> Subscription<Self::Message> {
        struct HeartbeatSubscription;

        let mut subscriptions = vec![
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
                        msg_tx.send(Message::Heartbeat).await.unwrap();
                        time::sleep(time::Duration::new(1, 0)).await;
                    }
                },
            ),
            ipc::subscription(),
        ];

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
