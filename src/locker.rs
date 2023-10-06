// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::app::{message, Command, Core, Settings};
use cosmic::{
    executor,
    iced::{
        self, alignment,
        event::wayland::{Event as WaylandEvent, OutputEvent},
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
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    fs,
    path::Path,
    process,
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

    //TODO: use background config
    let background = widget::image::Handle::from_memory(include_bytes!("../res/background.png"));

    let flags = Flags {
        current_user,
        icon_opt,
        background,
    };

    let settings = Settings::default()
        .antialiasing(true)
        .client_decorations(true)
        .debug(false)
        .default_icon_theme("Cosmic")
        .default_text_size(16.0)
        .no_main_window(true)
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
    icon_opt: Option<widget::image::Handle>,
    background: widget::image::Handle,
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    None,
    OutputEvent(OutputEvent, WlOutput),
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
    next_surface_id: SurfaceId,
    surface_ids: HashMap<WlOutput, SurfaceId>,
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
                next_surface_id: SurfaceId(1),
                surface_ids: HashMap::new(),
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
            Message::None => {}
            Message::OutputEvent(output_event, output) => match output_event {
                OutputEvent::Created(_output_info_opt) => {
                    log::info!("output {}: created", output.id());

                    //TODO: COVER ALL OUTPUTS AFTER FIXING FOCUS BUG
                    if !self.surface_ids.is_empty() {
                        log::error!("COVER ALL OUTPUTS AFTER FIXING FOCUS BUG");
                        return Command::none();
                    }

                    let surface_id = self.next_surface_id;
                    self.next_surface_id.0 += 1;

                    match self.surface_ids.insert(output.clone(), surface_id) {
                        Some(old_surface_id) => {
                            //TODO: remove old surface?
                            log::warn!(
                                "output {}: already had surface ID {}",
                                output.id(),
                                old_surface_id.0
                            );
                        }
                        None => {}
                    }

                    return Command::batch([
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
                            exclusive_zone: 0,
                            size_limits: iced::Limits::NONE.min_width(1.0).min_height(1.0),
                        }),
                        widget::text_input::focus(self.text_input_id.clone()),
                    ]);
                }
                OutputEvent::Removed => {
                    log::info!("output {}: removed", output.id());
                    match self.surface_ids.remove(&output) {
                        Some(surface_id) => {
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
            },
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

                let mut commands = Vec::new();
                for (_output, surface_id) in self.surface_ids.drain() {
                    commands.push(destroy_layer_surface(surface_id));
                }
                //TODO: cleaner method to exit?
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
    fn view_window(&self, _id: SurfaceId) -> Element<Self::Message> {
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

            //TODO: get actual status
            let status_row = iced::widget::row![
                widget::icon::from_name("network-wireless-signal-ok-symbolic",),
                iced::widget::row![
                    widget::icon::from_name("battery-level-50-symbolic"),
                    widget::text("50%"),
                ]
            ]
            .padding(16.0)
            .spacing(12.0);

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
                    column = column.push(
                        widget::container(widget::text::title4(gecos))
                            .width(Length::Fill)
                            .align_x(alignment::Horizontal::Center),
                    );
                }
                None => {}
            }

            match &self.prompt_opt {
                Some((prompt, secret, value)) => {
                    let mut text_input = widget::text_input(&prompt, &value)
                        .id(self.text_input_id.clone())
                        .leading_icon(widget::icon::from_name("system-lock-screen-symbolic").into())
                        .trailing_icon(
                            widget::icon::from_name("document-properties-symbolic").into(),
                        )
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

            widget::container(column)
                .align_x(alignment::Horizontal::Center)
                .width(Length::Fill)
        };

        crate::image_container::ImageContainer::new(
            widget::container(
                widget::cosmic_container::container(
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
                        appearance.border_radius = 16.0.into();
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
        .image(self.flags.background.clone())
        .into()
    }

    //TODO: subscription for date/time
    fn subscription(&self) -> Subscription<Self::Message> {
        if self.exited {
            return Subscription::none();
        }

        struct SomeWorker;

        //TODO: how to avoid cloning this on every time subscription is called?
        let username = self.flags.current_user.name.clone();
        Subscription::batch([
            subscription::events_with(|event, _| match event {
                iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
                    WaylandEvent::Output(output_event, output),
                )) => Some(Message::OutputEvent(output_event, output)),
                _ => None,
            }),
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
            ),
        ])
    }
}
