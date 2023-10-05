// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::app::{message, Command, Core, Settings};
use cosmic::{
    executor,
    iced::{
        self,
        futures::{self, SinkExt},
        subscription, Subscription,
    },
    widget, Element,
};
use std::ffi::{CStr, CString};
use tokio::{sync::mpsc, task, time};

pub fn main(current_user: pwd::Passwd) -> Result<(), Box<dyn std::error::Error>> {
    let flags = Flags { current_user };

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

pub fn pam_thread(username: String, conversation: Conversation) -> Result<(), pam_client::Error> {
    //TODO: send errors to GUI, restart process

    // Create PAM context
    let mut context = pam_client::Context::new("cosmic-locker", Some(&username), conversation)?;

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
                .send(Message::Prompt(prompt.to_string(), secret, String::new()))
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
    fn text_info(&mut self, msg: &CStr) {
        log::warn!("TODO text_info: {:?}", msg);
    }
    fn error_msg(&mut self, msg: &CStr) {
        log::info!("TODO error_msg: {:?}", msg);
    }
}

#[derive(Clone)]
pub struct Flags {
    current_user: pwd::Passwd,
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    Channel(mpsc::Sender<String>),
    Prompt(String, bool, String),
    Submit,
    Error(String),
    Exit,
}

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    flags: Flags,
    value_tx_opt: Option<mpsc::Sender<String>>,
    prompt_opt: Option<(String, bool, String)>,
    error_opt: Option<String>,
    text_input_id: widget::Id,
    exited: bool,
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
                value_tx_opt: None,
                prompt_opt: None,
                error_opt: None,
                text_input_id: widget::Id::unique(),
                exited: false,
            },
            Command::none(),
        )
    }

    /// Handle application events here.
    fn update(&mut self, message: Self::Message) -> Command<Self::Message> {
        match message {
            Message::Channel(value_tx) => {
                self.value_tx_opt = Some(value_tx);
            }
            Message::Prompt(prompt, secret, value) => {
                self.prompt_opt = Some((prompt, secret, value));
                //TODO: only focus text input on changes to the page
                return widget::text_input::focus(self.text_input_id.clone());
            }
            Message::Submit => match self.prompt_opt.take() {
                Some((_prompt, _secret, value)) => match self.value_tx_opt.take() {
                    Some(value_tx) => {
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
                None => log::warn!("tried to submit without prompt"),
            },
            Message::Error(error) => {
                self.error_opt = Some(error);
            }
            Message::Exit => {
                self.exited = true;
                return iced::window::close();
            }
        }
        Command::none()
    }

    /// Creates a view after each update.
    fn view(&self) -> Element<Self::Message> {
        let mut column = widget::column::with_capacity(3).spacing(12.0);

        match &self.prompt_opt {
            Some((prompt, secret, value)) => {
                column = column.push(widget::text(prompt.clone()));

                let mut text_input = widget::text_input("", &value)
                    .id(self.text_input_id.clone())
                    .on_input(|value| Message::Prompt(prompt.clone(), *secret, value))
                    .on_submit(Message::Submit);

                if *secret {
                    text_input = text_input.password()
                }

                column = column.push(text_input);
            }
            None => {}
        }

        if let Some(error) = &self.error_opt {
            column = column.push(widget::text(error));
        }

        let centered = widget::container(column)
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(iced::alignment::Vertical::Center);

        Element::from(centered)
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        if self.exited {
            return Subscription::none();
        }

        struct SomeWorker;

        //TODO: how to avoid cloning this on every time subscription is called?
        let username = self.flags.current_user.name.clone();
        subscription::channel(
            std::any::TypeId::of::<SomeWorker>(),
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
                            msg_tx.send(Message::Exit).await.unwrap();
                            break;
                        }
                        Err(err) => {
                            log::info!("authentication error: {:?}", err);
                            msg_tx.send(Message::Error(err.to_string())).await.unwrap();
                        }
                    }
                }

                //TODO: how to properly kill this task?
                loop {
                    time::sleep(time::Duration::new(1, 0)).await;
                }
            },
        )
    }
}
