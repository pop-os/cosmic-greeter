// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

//! Application API example

use cosmic::app::{message, Command, Core, Settings};
use cosmic::prelude::*;
use cosmic::{executor, iced, widget, ApplicationExt, Element};
use std::{env, io, sync::Arc};
use tokio::fs::{File, OpenOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = Settings::default()
        .antialiasing(true)
        .client_decorations(true)
        .debug(false)
        .default_icon_theme("Cosmic")
        .default_text_size(16.0)
        .scale_factor(1.0)
        .theme(cosmic::Theme::dark());

    cosmic::app::run::<App>(settings, ())?;

    Ok(())
}

#[derive(Clone, Debug)]
pub enum SocketState {
    /// Opening GREETD_SOCK
    Pending,
    /// GREETD_SOCK is open
    Open(Arc<File>),
    /// No GREETD_SOCK variable set
    NotSet,
    /// Failed to open GREETD_SOCK
    Error(Arc<io::Error>),
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    Socket(SocketState),
    Username(String),
}

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    socket_state: SocketState,
    username: String,
}

/// Implement [`cosmic::Application`] to integrate with COSMIC.
impl cosmic::Application for App {
    /// Default async executor to use with the app.
    type Executor = executor::Default;

    /// Argument received [`cosmic::Application::new`].
    type Flags = ();

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
    fn init(mut core: Core, _flags: Self::Flags) -> (Self, Command<Self::Message>) {
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        core.window.sharp_corners = true;
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.window.use_template = false;

        (
            App {
                core,
                socket_state: SocketState::Pending,
                username: String::new(),
            },
            Command::perform(
                async {
                    message::app(Message::Socket(match env::var_os("GREETD_SOCK") {
                        Some(socket_path) => match OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(&socket_path)
                            .await
                        {
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
            Message::Socket(socket_state) => {
                self.socket_state = socket_state;
            }
            Message::Username(username) => {
                self.username = username;
            }
        }
        Command::none()
    }

    /// Creates a view after each update.
    fn view(&self) -> Element<Self::Message> {
        let text = widget::text(match &self.socket_state {
            SocketState::Pending => format!("Opening GREETD_SOCK"),
            SocketState::Open(_) => format!("GREETD_SOCK open"),
            SocketState::NotSet => format!("GREETD_SOCK variable not set"),
            SocketState::Error(err) => format!("Failed to open GREETD_SOCK: {:?}", err),
        });

        let column = widget::column::with_capacity(3)
            .push(text)
            .push(widget::text("Username:"))
            .push(widget::text_input("Username", &self.username).on_input(Message::Username));

        let centered = widget::container(column)
            .width(iced::Length::Fill)
            .height(iced::Length::Shrink)
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(iced::alignment::Vertical::Center);

        Element::from(centered)
    }
}
