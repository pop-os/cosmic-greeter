// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

//! Application API example

use cosmic::app::{message, Command, Core, Settings};
use cosmic::{executor, iced, widget, Element};
use greetd_ipc::{codec::SyncCodec, AuthMessageType, Request, Response};
use std::{env, io, sync::Arc};
use tokio::net::UnixStream;
use uzers::os::unix::UserExt;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // This method is unsafe due to using global state (libc pwent functions).
    let users: Vec<uzers::User> = unsafe {
        uzers::all_users()
            .filter(|user| {
                if user.uid() < 1000 {
                    // Skip system accounts
                    return false;
                }

                match user.shell().file_name().and_then(|x| x.to_str()) {
                    // Skip shell ending in false
                    Some("false") => false,
                    // Skip shell ending in nologin
                    Some("nologin") => false,
                    _ => true,
                }
            })
            .collect()
    };

    let settings = Settings::default()
        .antialiasing(true)
        .client_decorations(true)
        .debug(false)
        .default_icon_theme("Cosmic")
        .default_text_size(16.0)
        .scale_factor(1.0)
        .theme(cosmic::Theme::dark());

    cosmic::app::run::<App>(settings, users)?;

    Ok(())
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
    None,
    Username(String),
    Auth(bool, String, String),
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    None,
    Socket(SocketState),
    Input(InputState),
    Submit,
    Login,
}

async fn request(socket: Arc<UnixStream>, request: Request) -> Message {
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
            Ok(count) => {
                log::info!("read {} bytes", count);

                let mut cursor = io::Cursor::new(bytes);
                let response = Response::read_from(&mut cursor).unwrap();
                log::info!("{:?}", response);
                match response {
                    Response::AuthMessage {
                        auth_message_type,
                        auth_message,
                    } => match auth_message_type {
                        AuthMessageType::Secret => {
                            return Message::Input(InputState::Auth(
                                true,
                                auth_message,
                                String::new(),
                            ))
                        }
                        AuthMessageType::Visible => {
                            return Message::Input(InputState::Auth(
                                false,
                                auth_message,
                                String::new(),
                            ))
                        }
                        _ => todo!("unsupported auth_message_type {:?}", auth_message_type),
                    },
                    Response::Success => {
                        return Message::Login;
                    }
                    _ => {
                        log::error!("unhandled response");
                        break;
                    }
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

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    users: Vec<uzers::User>,
    socket_state: SocketState,
    input_state: InputState,
}

/// Implement [`cosmic::Application`] to integrate with COSMIC.
impl cosmic::Application for App {
    /// Default async executor to use with the app.
    type Executor = executor::Default;

    /// Argument received [`cosmic::Application::new`].
    type Flags = Vec<uzers::User>;

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
    fn init(mut core: Core, users: Self::Flags) -> (Self, Command<Self::Message>) {
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        core.window.sharp_corners = true;
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.window.use_template = false;

        (
            App {
                core,
                users,
                socket_state: SocketState::Pending,
                //TODO: set to pending until socket is open?
                input_state: InputState::Username(String::new()),
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
            }
            Message::Submit => match &self.socket_state {
                SocketState::Open(socket) => match &self.input_state {
                    InputState::None => {}
                    InputState::Username(username) => {
                        let socket = socket.clone();
                        let username = username.clone();
                        return Command::perform(
                            async move {
                                message::app(
                                    request(socket, Request::CreateSession { username }).await,
                                )
                            },
                            |x| x,
                        );
                    }
                    InputState::Auth(_secret, _prompt, value) => {
                        let socket = socket.clone();
                        let value = value.clone();
                        return Command::perform(
                            async move {
                                message::app(
                                    request(
                                        socket,
                                        Request::PostAuthMessageResponse {
                                            response: Some(value),
                                        },
                                    )
                                    .await,
                                )
                            },
                            |x| x,
                        );
                    }
                },
                _ => todo!("socket not open but input provided"),
            },
            Message::Login => match &self.socket_state {
                SocketState::Open(socket) => {
                    let socket = socket.clone();
                    return Command::perform(
                        async move {
                            message::app(
                                request(
                                    socket,
                                    //TODO: get session information from /usr/share/wayland-sessions
                                    Request::StartSession {
                                        cmd: vec!["start-cosmic".to_string()],
                                        env: vec![],
                                    },
                                )
                                .await,
                            )
                        },
                        |x| x,
                    );
                }
                _ => todo!("socket not open but attempting to log in"),
            },
        }
        Command::none()
    }

    /// Creates a view after each update.
    fn view(&self) -> Element<Self::Message> {
        let mut column = widget::column::with_capacity(2);

        match &self.socket_state {
            SocketState::Pending => {
                column = column.push(widget::text("Opening GREETD_SOCK"));
            }
            SocketState::Open(_) => match &self.input_state {
                InputState::None => {}
                InputState::Username(username) => {
                    column = column.push(widget::text("Username:"));
                    column = column.push(
                        widget::text_input("Username", &username)
                            .on_input(|username| Message::Input(InputState::Username(username)))
                            .on_submit(Message::Submit),
                    );

                    for user in &self.users {
                        match user.name().to_str() {
                            Some(user_name) => {
                                column = column.push(widget::button(user_name).on_press(
                                    Message::Input(InputState::Username(user_name.to_string())),
                                ));
                            }
                            None => {}
                        }
                    }
                }
                InputState::Auth(secret, prompt, value) => {
                    column = column.push(widget::text(prompt));
                    let text_input = widget::text_input("", &value)
                        .on_input(|value| {
                            Message::Input(InputState::Auth(*secret, prompt.clone(), value))
                        })
                        .on_submit(Message::Submit);
                    if *secret {
                        column = column.push(text_input.password());
                    } else {
                        column = column.push(text_input);
                    }
                }
            },
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

        let centered = widget::container(column.spacing(12.0).width(iced::Length::Fixed(400.0)))
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(iced::alignment::Vertical::Center);

        Element::from(centered)
    }
}
