// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::app::{message, Command, Core, Settings};
use cosmic::{executor, iced, widget, Element};
use greetd_ipc::{codec::SyncCodec, AuthMessageType, Request, Response};
use std::{collections::HashMap, env, fs, io, path::Path, sync::Arc};
use tokio::net::UnixStream;

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The pwd::Passwd method is unsafe (but not labelled as such) due to using global state (libc pwent functions).
    let users: Vec<_> = /* unsafe */ {
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
                        Ok(icon_data) => Some(widget::image::Handle::from_memory(icon_data)),
                        Err(err) => {
                            log::error!("failed to read {:?}: {:?}", icon_path, err);
                            None
                        }
                    }
                } else {
                    None
                };
                (user, icon_opt)
            })
            .collect()
    };

    //TODO: allow custom directories?
    let session_dirs = &[
        Path::new("/usr/share/wayland-sessions"),
        Path::new("/usr/share/xsessions"),
    ];

    let sessions = {
        let mut sessions = HashMap::new();
        for session_dir in session_dirs {
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

                let split = match shlex::split(exec) {
                    Some(some) => some,
                    None => {
                        log::warn!(
                            "failed to parse session file {:?} Exec field {:?}",
                            dir_entry.path(),
                            exec
                        );
                        continue;
                    }
                };

                match sessions.insert(name.to_string(), split) {
                    Some(some) => {
                        log::warn!("session overwritten with command {:?}", some);
                    }
                    None => {}
                }
            }
        }
        sessions
    };

    let flags = Flags { users, sessions };

    let settings = Settings::default()
        .antialiasing(true)
        .client_decorations(true)
        .debug(false)
        .default_icon_theme("Cosmic")
        .default_text_size(16.0)
        .scale_factor(1.0)
        .theme(cosmic::Theme::dark());

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
                            return Message::Input(InputState::Auth {
                                prompt: auth_message,
                                value_opt: Some(String::new()),
                                secret: true,
                            })
                        }
                        AuthMessageType::Visible => {
                            return Message::Input(InputState::Auth {
                                prompt: auth_message,
                                value_opt: Some(String::new()),
                                secret: false,
                            })
                        }
                        //TODO: treat error type differently?
                        AuthMessageType::Info | AuthMessageType::Error => {
                            return Message::Input(InputState::Auth {
                                prompt: auth_message,
                                value_opt: None,
                                secret: false,
                            })
                        }
                    },
                    Response::Error {
                        error_type: _,
                        description,
                    } => {
                        //TODO: use error_type?
                        return Message::Error(description);
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
                            //TODO: restart whole process
                            return Message::None;
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
    users: Vec<(pwd::Passwd, Option<widget::image::Handle>)>,
    sessions: HashMap<String, Vec<String>>,
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

#[derive(Clone, Debug)]
pub enum InputState {
    Username,
    Auth {
        prompt: String,
        value_opt: Option<String>,
        secret: bool,
    },
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    None,
    Socket(SocketState),
    Input(InputState),
    Session(String),
    Error(String),
    Username(Arc<UnixStream>, String),
    Auth(Arc<UnixStream>, Option<String>),
    Login(Arc<UnixStream>),
    Exit,
}

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    flags: Flags,
    socket_state: SocketState,
    input_state: InputState,
    session_names: Vec<String>,
    selected_session: String,
    error_opt: Option<String>,
    text_input_id: widget::Id,
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

        (
            App {
                core,
                flags,
                socket_state: SocketState::Pending,
                //TODO: set to pending until socket is open?
                input_state: InputState::Username,
                session_names,
                selected_session,
                error_opt: None,
                text_input_id: widget::Id::unique(),
            },
            Command::perform(
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
            ),
        )
    }

    /// Handle application events here.
    fn update(&mut self, message: Self::Message) -> Command<Self::Message> {
        match message {
            Message::None => {}
            Message::Socket(socket_state) => {
                self.socket_state = socket_state;
            }
            Message::Input(input_state) => {
                self.input_state = input_state;
                //TODO: only focus text input on changes to the page
                return widget::text_input::focus(self.text_input_id.clone());
            }
            Message::Session(selected_session) => {
                self.selected_session = selected_session;
            }
            Message::Error(error) => {
                self.error_opt = Some(error);
            }
            Message::Username(socket, username) => {
                return request_command(socket, Request::CreateSession { username });
            }
            Message::Auth(socket, response) => {
                return request_command(socket, Request::PostAuthMessageResponse { response });
            }
            Message::Login(socket) => {
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
            Message::Exit => {
                return iced::window::close();
            }
        }
        Command::none()
    }

    /// Creates a view after each update.
    fn view(&self) -> Element<Self::Message> {
        let content: Element<_> = match &self.socket_state {
            SocketState::Pending => widget::text("Opening GREETD_SOCK").into(),
            SocketState::Open(socket) => match &self.input_state {
                InputState::Username => {
                    let mut row = widget::row::with_capacity(self.flags.users.len()).spacing(12.0);
                    for (user, icon_opt) in &self.flags.users {
                        let mut column = widget::column::with_capacity(2).spacing(12.0);
                        match icon_opt {
                            Some(icon) => {
                                column = column.push(
                                    widget::Image::new(icon.clone())
                                        .width(iced::Length::Fixed(256.0))
                                        .height(iced::Length::Fixed(256.0)),
                                )
                            }
                            None => {}
                        }
                        match &user.gecos {
                            Some(gecos) => {
                                column = column.push(widget::text(gecos));
                            }
                            None => {}
                        }
                        row = row.push(
                            widget::MouseArea::new(
                                widget::cosmic_container::container(column)
                                    .layer(cosmic::cosmic_theme::Layer::Primary)
                                    .padding(16)
                                    .style(cosmic::theme::Container::Primary),
                            )
                            .on_press(Message::Username(socket.clone(), user.name.clone())),
                        );
                    }
                    row.into()
                }
                InputState::Auth {
                    prompt,
                    value_opt,
                    secret,
                } => {
                    let mut column = widget::column::with_capacity(2)
                        .spacing(12.0)
                        .width(iced::Length::Fixed(400.0));
                    column = column.push(widget::text(prompt));

                    match value_opt {
                        Some(value) => {
                            let text_input = widget::text_input("", &value)
                                .id(self.text_input_id.clone())
                                .on_input(|value| {
                                    Message::Input(InputState::Auth {
                                        prompt: prompt.clone(),
                                        value_opt: Some(value),
                                        secret: *secret,
                                    })
                                })
                                .on_submit(Message::Auth(socket.clone(), Some(value.clone())));
                            if *secret {
                                column = column.push(text_input.password());
                            } else {
                                column = column.push(text_input);
                            }
                        }
                        None => {
                            column = column.push(
                                widget::button("Confirm")
                                    .on_press(Message::Auth(socket.clone(), None)),
                            );
                        }
                    }

                    column.into()
                }
            },
            SocketState::NotSet => widget::text("GREETD_SOCK variable not set").into(),
            SocketState::Error(err) => {
                widget::text(format!("Failed to open GREETD_SOCK: {:?}", err)).into()
            }
        };

        let session_picker = widget::pick_list(
            &self.session_names,
            Some(self.selected_session.clone()),
            Message::Session,
        );

        let mut column = widget::column::with_capacity(3)
            .push(content)
            .push(session_picker)
            .spacing(12.0);

        if let Some(error) = &self.error_opt {
            column = column.push(widget::text(error.clone()));
        }

        let centered = widget::container(column)
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(iced::alignment::Vertical::Center);

        Element::from(centered)
    }
}
