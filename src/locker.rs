// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use color_eyre::eyre::WrapErr;
use cosmic::app::{Core, Settings, Task};
use cosmic::cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity;
use cosmic::iced::core::text::{Ellipsize, EllipsizeHeightLimit};
use cosmic::iced::event::wayland::{OutputEvent, SessionLockEvent};
use cosmic::iced::futures::{self, SinkExt};
use cosmic::iced::platform_specific::shell::wayland::commands::session_lock::{
    destroy_lock_surface, lock, unlock,
};
use cosmic::iced::runtime::core::window::Id as SurfaceId;
use cosmic::iced::runtime::platform_specific::wayland::subsurface::SctkSubsurfaceSettings;
use cosmic::iced::{
    self, Alignment, Background, Border, Length, Point, Rectangle, Size, Subscription,
};
use cosmic::surface::action::LiveSettings;
use cosmic::{Element, executor, surface, theme, widget};
use cosmic_config::CosmicConfigEntry;
use cosmic_greeter_daemon::{TimeAppletConfig, UserData};
use std::any::TypeId;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs, process};
use tokio::sync::mpsc;
use tracing::level_filters::LevelFilter;
use tracing::warn;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};
use wayland_client::Proxy;
use wayland_client::protocol::wl_output::WlOutput;

use crate::common::{self, Common, DEFAULT_MENU_ITEM_HEIGHT, MAX_WIDTH};
use crate::fl;

/// Minimum time duration of a PAM attempt.
///
/// If a failed PAM attempt takes less time than this, wait out the rest of
/// this duration before retrying.
const PAM_MIN_ATTEMPT_DURATION: Duration = Duration::from_millis(500);

fn lockfile_opt() -> Option<PathBuf> {
    let runtime_dir = dirs::runtime_dir()?;
    let session_id = env::var("XDG_SESSION_ID").ok()?;
    Some(runtime_dir.join(format!("cosmic-greeter-{}.lock", session_id)))
}

pub fn main(user: pwd::Passwd) -> Result<(), Box<dyn std::error::Error>> {
    color_eyre::install().wrap_err("failed to install color_eyre error handler")?;

    let trace = tracing_subscriber::registry();
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .from_env_lossy();

    #[cfg(feature = "systemd")]
    if let Ok(journald) = tracing_journald::layer() {
        trace
            .with(journald)
            .with(env_filter)
            .try_init()
            .wrap_err("failed to initialize logger")?;
    } else {
        trace
            .with(fmt::layer())
            .with(env_filter)
            .try_init()
            .wrap_err("failed to initialize logger")?;
        warn!("failed to connect to journald")
    }

    #[cfg(not(feature = "systemd"))]
    trace
        .with(fmt::layer())
        .with(env_filter)
        .try_init()
        .wrap_err("failed to initialize logger")?;

    crate::localize::localize();

    let mut user_data = UserData::from(user);
    // We are already the user at this point
    user_data.load_config_as_user();

    // Read the fingerprint gate from the user's greeter config. Defaults to off.
    let (greeter_config, _greeter_config_handler) = cosmic_greeter_config::Config::load();

    let flags = Flags {
        user_icon: user_data
            .icon_opt
            .take()
            .map(widget::image::Handle::from_bytes),
        user_data,
        lockfile_opt: lockfile_opt(),
        #[cfg(feature = "logind")]
        logind_available: crate::logind::is_available(),
        #[cfg(not(feature = "logind"))]
        logind_available: false,
        fingerprint_enabled: greeter_config.enable_fingerprint_authentication,
    };

    let settings = Settings::default().no_main_window(true);

    cosmic::app::run::<App>(settings, flags)?;

    Ok(())
}

/// Outcome of running one out-of-process PAM stack.
enum WorkerOutcome {
    /// The stack authenticated successfully.
    Success,
    /// The stack rejected the attempt; carries an already-localized message.
    Failure(String),
    /// The worker was killed or its pipe closed before producing a verdict
    /// (e.g. the other stack won the race, or the UI went away).
    Aborted,
}

/// Spawn one PAM stack as a child process and drive its conversation, relaying
/// prompts/messages to the UI through `msg_tx` and feeding typed input from
/// `value_rx` (password stack only; `None` for fingerprint). The child is
/// `kill_on_drop`, so dropping this future SIGKILLs the worker.
async fn run_pam_worker(
    service: &str,
    username: &str,
    msg_tx: &mut futures::channel::mpsc::Sender<cosmic::Action<Message>>,
    mut value_rx: Option<mpsc::Receiver<String>>,
    is_fingerprint: bool,
) -> WorkerOutcome {
    use crate::pam_worker::WorkerMsg;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut child = match tokio::process::Command::new("/proc/self/exe")
        .arg("--pam-conversation")
        .arg(service)
        .arg(username)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::error!("failed to spawn PAM worker for {service}: {err}");
            return WorkerOutcome::Aborted;
        }
    };

    let stdout = child.stdout.take().expect("worker stdout piped");
    let mut stdin = child.stdin.take().expect("worker stdin piped");
    let mut lines = BufReader::new(stdout).lines();

    let mut standing_prompt: Option<String> = None;
    let mut saw_scan_result = false;

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            // EOF: the worker exited without a verdict (typically killed).
            Ok(None) => return WorkerOutcome::Aborted,
            Err(err) => {
                tracing::warn!("PAM worker read error ({service}): {err}");
                return WorkerOutcome::Aborted;
            }
        };

        let msg = match ron::from_str::<WorkerMsg>(&line) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!("invalid PAM worker message {line:?}: {err}");
                continue;
            }
        };

        let (prompt, secret) = match msg {
            WorkerMsg::PromptEchoOff(prompt) => (prompt, true),
            WorkerMsg::PromptEchoOn(prompt) => (prompt, false),
            WorkerMsg::Info(text) => {
                // Fingerprint sensor text gets its own status line; password
                // info replaces the prompt label (matching the old behavior).
                let action = if is_fingerprint {
                    if standing_prompt.is_none() {
                        standing_prompt = Some(text.clone());
                    }
                    Message::FingerprintInfo(Some(text))
                } else {
                    common::Message::Prompt(text, false, None).into()
                };
                let _ = msg_tx.send(cosmic::Action::App(action)).await;
                continue;
            }
            WorkerMsg::Error(text) => {
                let action = if is_fingerprint {
                    saw_scan_result = true;
                    Message::FingerprintInfo(Some(text))
                } else {
                    Message::Error(text)
                };
                let _ = msg_tx.send(cosmic::Action::App(action)).await;
                continue;
            }
            WorkerMsg::Success => return WorkerOutcome::Success,
            WorkerMsg::Failure(message) => {
                if is_fingerprint && !saw_scan_result {
                    let _ = msg_tx
                        .send(cosmic::Action::App(Message::FingerprintInfo(
                            standing_prompt.take(),
                        )))
                        .await;
                }
                return WorkerOutcome::Failure(message);
            }
        };

        // A prompt needs typed input. Only the password stack has an input
        // channel, the fingerprint stack should never reach here.
        let Some(value_rx) = value_rx.as_mut() else {
            tracing::warn!("unexpected input prompt in fingerprint worker");
            // Unblock the worker with an empty value rather than deadlocking it.
            let _ = write_host_input(&mut stdin, String::new()).await;
            continue;
        };

        let _ = msg_tx
            .send(cosmic::Action::App(
                common::Message::Prompt(prompt, secret, Some(String::new())).into(),
            ))
            .await;

        let value = match value_rx.recv().await {
            Some(value) => value,
            // UI input channel closed.
            None => return WorkerOutcome::Aborted,
        };

        if write_host_input(&mut stdin, value).await.is_err() {
            return WorkerOutcome::Aborted;
        }
    }
}

/// Write one `HostMsg::Input` line to the worker's stdin.
async fn write_host_input(
    stdin: &mut tokio::process::ChildStdin,
    value: String,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let line = ron::to_string(&crate::pam_worker::HostMsg::Input(value))
        .unwrap_or_else(|_| String::from("Input(\"\")"));
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

#[derive(Clone)]
pub struct Flags {
    user_data: UserData,
    user_icon: Option<widget::image::Handle>,
    lockfile_opt: Option<PathBuf>,
    logind_available: bool,
    fingerprint_enabled: bool,
}

///TODO: this is custom code that should be better handled by libcosmic
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dropdown {
    Keyboard,
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    None,
    Common(common::Message),
    OutputEvent(OutputEvent, WlOutput),
    SessionLockEvent(SessionLockEvent),
    Channel(mpsc::Sender<String>),
    BackgroundState(cosmic_bg_config::state::State),
    DropdownToggle(Dropdown),
    KeyboardLayout(usize),
    Inhibit(Arc<OwnedFd>),
    Submit(String),
    Surface(surface::Action<Message>),
    Suspend,
    TimeAppletConfig(TimeAppletConfig),
    Error(String),
    FingerprintInfo(Option<String>),
    Lock,
    Unlock,
}

impl From<common::Message> for Message {
    fn from(message: common::Message) -> Self {
        Self::Common(message)
    }
}

#[derive(Clone, Debug)]
enum State {
    Locking,
    Locked {
        task_handle: cosmic::iced::task::Handle,
    },
    Unlocking,
    Unlocked,
}

impl Drop for State {
    fn drop(&mut self) {
        // Abort the locked task when the state is changed.
        if let Self::Locked { task_handle } = self {
            tracing::info!("dropping lockscreen tasks");
            task_handle.abort();
        }
    }
}

/// The [`App`] stores application-specific state.
pub struct App {
    common: Common<Message>,
    flags: Flags,
    state: State,
    dropdown_opt: Option<Dropdown>,
    inhibit_opt: Option<Arc<OwnedFd>>,
    value_tx_opt: Option<mpsc::Sender<String>>,
    authenticating: bool,
    fingerprint_info_opt: Option<String>,
}

impl App {
    fn menu(&self, surface_id: SurfaceId) -> Element<'_, Message> {
        let window_width = self
            .common
            .window_size
            .get(&surface_id)
            .map(|s| s.width)
            .unwrap_or(MAX_WIDTH);
        let menu_width = if window_width > MAX_WIDTH {
            MAX_WIDTH
        } else {
            window_width
        };
        let left_element = {
            let military_time = self.flags.user_data.time_applet_config.military_time;
            let date_time_column = self.common.time.date_time_widget(military_time);

            let mut status_row = widget::row::with_capacity(2).padding(16.0).spacing(12.0);

            if let Some(network_icon) = self.common.network_icon_opt.as_ref() {
                status_row = status_row.push(network_icon.clone());
            }

            if let Some((power_icon, power_percent)) = &self.common.power_info_opt {
                status_row = status_row.push(
                    iced::widget::row![
                        power_icon.clone(),
                        widget::text(format!("{:.0}%", power_percent)),
                    ]
                    .align_y(Alignment::Center),
                );
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
                            widget::space::horizontal()
                                .width(Length::Fixed(17.0))
                                .into()
                        },
                        widget::space::horizontal().width(Length::Fixed(8.0)).into(),
                        widget::text(label)
                            .align_x(iced::alignment::Horizontal::Left)
                            .into(),
                    ])
                    .on_press(message),
                )
            };
            let dropdown_menu = |items: Vec<_>| {
                let item_cnt = items.len();

                let items = widget::menu::menu_column::MenuColumn::with_children(items);
                let items = if item_cnt > 7 {
                    Element::from(
                        widget::scrollable(items)
                            .height(Length::Fixed(DEFAULT_MENU_ITEM_HEIGHT * 7.)),
                    )
                } else {
                    Element::from(items)
                };

                let menu: widget::Container<'_, Message, cosmic::prelude::Theme> =
                    widget::container(items)
                        .padding(1)
                        //TODO: move style to libcosmic
                        .class(theme::Container::custom(|theme| {
                            let cosmic = theme.cosmic();
                            let component = &cosmic.background(theme.transparent).component;
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
                        .width(Length::Fixed(240.0));

                if let Some(t) = self.common.rectangle_tracker.as_ref() {
                    Element::from(t.container((surface_id, true), menu))
                } else {
                    menu.into()
                }
            };

            let mut input_button = widget::popover(
                widget::container(
                    widget::button::custom(
                        widget::text(
                            self.common
                                .active_layouts
                                .get(self.common.current_keyboard_layout)
                                .map(|x| x.name())
                                .unwrap_or_default(),
                        )
                        .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                        .height(16)
                        .center(),
                    )
                    .padding(12.0)
                    .on_press(Message::DropdownToggle(Dropdown::Keyboard)),
                )
                .max_width(80),
            )
            .position(widget::popover::Position::Bottom);
            if matches!(self.dropdown_opt, Some(Dropdown::Keyboard)) {
                let mut items = Vec::with_capacity(self.common.active_layouts.len());
                for (i, layout) in self.common.active_layouts.iter().enumerate() {
                    items.push(menu_checklist(
                        &layout.description,
                        i == self.common.current_keyboard_layout,
                        Message::KeyboardLayout(i),
                    ));
                }
                input_button = input_button.popup(dropdown_menu(items));
            }

            //TODO: implement these buttons
            let mut button_row = iced::widget::row![
                /*TODO: greeter accessibility options
                widget::button::custom(widget::icon::from_name(
                    "applications-accessibility-symbolic"
                ))
                .padding(12.0)
                .on_press(Message::None),
                */
                widget::tooltip(
                    widget::button::custom(widget::icon::from_name("input-keyboard-symbolic"))
                        .padding(12.0)
                        .on_press(Message::Common(common::Message::OnScreenKeyboard)),
                    widget::text(fl!("on-screen-keyboard")),
                    widget::tooltip::Position::Top,
                ),
                widget::tooltip(
                    input_button,
                    widget::text(fl!("keyboard-layout")),
                    widget::tooltip::Position::Top
                ),
            ];
            if cfg!(feature = "logind") && self.flags.logind_available {
                button_row = button_row.push(widget::tooltip(
                    widget::button::custom(widget::icon::from_name("system-suspend-symbolic"))
                        .padding(12.0)
                        .on_press(Message::Suspend),
                    widget::text(fl!("suspend")),
                    widget::tooltip::Position::Top,
                ));
            }
            let button_row = button_row.padding([16.0, 0.0, 0.0, 0.0]).spacing(8.0);

            widget::container(iced::widget::column![
                date_time_column,
                widget::divider::horizontal::default().width(Length::Fixed(menu_width / 2. - 16.)),
                status_row,
                widget::divider::horizontal::default().width(Length::Fixed(menu_width / 2. - 16.)),
                button_row,
            ])
            .align_x(Alignment::Start)
        };

        let right_element = {
            let mut column = widget::column::with_capacity(5)
                .spacing(12.0)
                .max_width(280.0);

            let military_time = self.flags.user_data.time_applet_config.military_time;
            let space_height = match military_time {
                true => 63.0,
                false => 10.0,
            };

            // Add top spacing for better visual appearance
            // Bottom of the password text input field should align with bottom of time widget
            column = column.push(widget::space::vertical().height(Length::Fixed(space_height)));

            // Display user icon or empty transparent box
            if let Some(icon_handle) = &self.flags.user_icon {
                column = column.push(
                    widget::container(
                        widget::image(icon_handle)
                            .width(Length::Fixed(78.0))
                            .height(Length::Fixed(78.0))
                            .content_fit(iced::ContentFit::Fill),
                    )
                    .padding(0.0)
                    .width(Length::Fill)
                    .height(Length::Fixed(78.0))
                    .align_x(Alignment::Center),
                );
            } else {
                // Empty transparent box for users without icons
                column = column.push(
                    widget::container(widget::space::horizontal().width(Length::Fixed(78.0)))
                        .padding(0.0)
                        .width(Length::Fill)
                        .height(Length::Fixed(78.0))
                        .align_x(Alignment::Center),
                );
            }

            column = column.push(
                widget::container(widget::text::title4(&self.flags.user_data.full_name))
                    .width(Length::Fill)
                    .align_x(Alignment::Center),
            );

            if let Some((prompt, secret, value_opt)) = &self.common.prompt_opt {
                match value_opt {
                    Some(value) => {
                        let text_input_id = self
                            .common
                            .surface_names
                            .get(&surface_id)
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
                        .id(text_input_id);

                        // Don't allow input when authenticating
                        if !self.authenticating {
                            text_input = text_input
                                .on_input(|input| {
                                    common::Message::Prompt(prompt.clone(), *secret, Some(input))
                                        .into()
                                })
                                .on_submit(Message::Submit);
                        }

                        if *secret {
                            text_input = text_input.password()
                        }

                        column = column.push(text_input);

                        if self.common.caps_lock && !self.authenticating {
                            column = column.push(widget::text(fl!("caps-lock")));
                        } else if self.common.error_opt.is_none() {
                            column = column.push(widget::text(""));
                        }
                    }
                    None => {
                        column = column.push(widget::text(prompt));
                    }
                }
            }

            if let Some(info) = &self.fingerprint_info_opt {
                column = column.push(widget::text(info));
            }

            // Show either authenticating message or error message in the same location
            if self.authenticating {
                column = column.push(
                    widget::container(
                        widget::row::with_capacity(2)
                            .spacing(8.0)
                            .align_y(Alignment::Center)
                            .push(widget::indeterminate_circular().size(16.0))
                            .push(widget::text(fl!("authenticating"))),
                    )
                    .width(Length::Fill)
                    .align_x(Alignment::Center),
                );
            } else if let Some(error) = &self.common.error_opt {
                column = column.push(
                    widget::text(error)
                        .class(theme::Text::Color(iced::Color::from_rgb(1.0, 0.0, 0.0))),
                );
                if !self.common.caps_lock {
                    column = column.push(widget::text(""));
                }
            } else {
                column = column.push(widget::text(""));
            }

            widget::container(column)
                .align_x(Alignment::Center)
                .width(Length::Fill)
        };
        let menu = widget::layer_container(
            iced::widget::row![left_element, right_element].align_y(Alignment::Start),
        )
        .layer(cosmic::cosmic_theme::Layer::Background)
        .padding(16)
        .class(cosmic::theme::Container::Custom(Box::new(
            |theme: &cosmic::Theme| {
                // Use background appearance as the base
                let mut appearance =
                    widget::container::Catalog::style(theme, &cosmic::theme::Container::Background);
                let c: iced::Color = theme.cosmic().background(theme.transparent).base.into();
                appearance.background = Some(iced::Background::Color(c));
                appearance.border = iced::Border::default().rounded(16.0);
                appearance
            },
        )))
        .width(Length::Fill)
        .height(Length::Shrink);
        let menu = if let Some(t) = self.common.rectangle_tracker.as_ref() {
            Element::from(t.container((surface_id, false), menu))
        } else {
            menu.into()
        };
        widget::container(widget::column::with_children(vec![
            widget::space::vertical()
                .height(Length::FillPortion(1))
                .into(),
            menu,
            widget::space::vertical()
                .height(Length::FillPortion(4))
                .into(),
        ]))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .class(cosmic::theme::Container::Transparent)
        .into()
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
    fn init(mut core: Core, flags: Self::Flags) -> (Self, Task<Self::Message>) {
        core.set_app_type(cosmic::core::AppType::System);

        let (mut common, common_task) = Common::init(core);
        common.on_output_event = Some(Box::new(|output_event, output| {
            Message::OutputEvent(output_event, output)
        }));
        common.on_session_lock_event = Some(Box::new(Message::SessionLockEvent));
        common.update_user_data(&flags.user_data);

        let already_locked = match flags.lockfile_opt {
            Some(ref lockfile) => lockfile.exists(),
            None => false,
        };

        let mut app = App {
            common,
            flags,
            state: State::Unlocked,
            dropdown_opt: None,
            inhibit_opt: None,
            value_tx_opt: None,
            authenticating: false,
            fingerprint_info_opt: None,
        };

        let task = if cfg!(feature = "logind") && app.flags.logind_available {
            if already_locked {
                // Recover previously locked state
                tracing::info!("recovering previous locked state");
                app.state = State::Locking;
                lock()
            } else {
                // When logind is available, wait for lock signal
                Task::none()
            }
        } else {
            // When logind is not available, lock immediately
            tracing::info!("locking immediately");
            app.state = State::Locking;
            lock()
        };

        (app, Task::batch([task, common_task]))
    }

    /// Handle application events here.
    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::None => {}
            Message::Common(common_message) => {
                if matches!(&common_message, common::Message::Prompt(_, _, Some(_)))
                    && self.authenticating
                {
                    self.authenticating = false;
                    self.common.prompt_opt = None;
                }
                return self.common.update(common_message);
            }
            Message::OutputEvent(output_event, output) => {
                match output_event {
                    OutputEvent::Created(output_info_opt) => {
                        tracing::info!("output {}: created", output.id());

                        let surface_id = SurfaceId::unique();
                        let subsurface_id = SurfaceId::unique();

                        if let Some(old_surface_id) =
                            self.common.surface_ids.insert(output.clone(), surface_id)
                        {
                            //TODO: remove old surface?
                            tracing::warn!(
                                "output {}: already had surface ID {:?}",
                                output.id(),
                                old_surface_id
                            );
                            return Task::none();
                        }

                        self.common
                            .subsurface_outputs
                            .insert(subsurface_id, output.clone());

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
                                        .output_names
                                        .insert(output.clone(), output_name.clone());
                                    self.common
                                        .surface_names
                                        .insert(surface_id, output_name.clone());
                                    self.common
                                        .surface_names
                                        .insert(subsurface_id, output_name.clone());
                                    self.common.surface_images.remove(&surface_id);
                                    self.common.update_wallpapers(&self.flags.user_data);
                                    let text_input_id =
                                        widget::Id::new(format!("input-{output_name}",));
                                    self.common
                                        .text_input_ids
                                        .insert(output_name.clone(), text_input_id.clone());
                                }
                                None => {
                                    tracing::warn!("output {}: no output name", output.id());
                                }
                            },
                            None => {
                                tracing::warn!("output {}: no output info", output.id());
                            }
                        }

                        let unwrapped_size = size
                            .map(|s| (s.0.unwrap_or(1920), s.1.unwrap_or(1080)))
                            .unwrap_or((1920, 1080));
                        let (loc, sub_size) = if unwrapped_size.0 as f32 > MAX_WIDTH {
                            (
                                Point::new(unwrapped_size.0 as f32 / 2. - 400., 32.),
                                Size::new(MAX_WIDTH, unwrapped_size.1 as f32 - 32.),
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

                        self.common
                            .subsurface_rects
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

                        if matches!(self.state, State::Locked { .. }) {
                            return cosmic::task::message(cosmic::Action::Surface(
                                cosmic::surface::action::lock(
                                    || LiveSettings {
                                        padding: None,
                                        corners: None,
                                        blur: Some(false),
                                    },
                                    surface_id,
                                    output,
                                    None::<fn() -> cosmic::Element<'static, cosmic::Action<Message>>>,
                                ),
                            ))
                            .chain(cosmic::task::message(cosmic::Action::Surface(msg)));
                        }
                    }
                    OutputEvent::Removed => {
                        tracing::info!("output {}: removed", output.id());
                        match self.common.surface_ids.remove(&output) {
                            Some(surface_id) => {
                                self.common.surface_images.remove(&surface_id);
                                self.common.surface_names.remove(&surface_id);
                                self.common.window_size.remove(&surface_id);
                                if let Some(n) = self.common.surface_names.remove(&surface_id) {
                                    self.common.text_input_ids.remove(&n);
                                }
                                if matches!(self.state, State::Locked { .. }) {
                                    return destroy_lock_surface(surface_id);
                                }
                            }
                            None => {
                                tracing::warn!("output {}: no surface found", output.id());
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
                        let (loc, sub_size) = if unwrapped_size.0 as f32 > MAX_WIDTH {
                            (
                                Point::new(unwrapped_size.0 as f32 / 2. - 400., 32.),
                                Size::new(MAX_WIDTH, unwrapped_size.1 as f32 - 32.),
                            )
                        } else {
                            (Point::ORIGIN, Size::new(1920., 1080.))
                        };
                        self.common
                            .subsurface_rects
                            .insert(output.clone(), Rectangle::new(loc, sub_size));

                        tracing::info!("output {}: info update", output.id());
                    }
                }
            }
            Message::SessionLockEvent(session_lock_event) => match session_lock_event {
                SessionLockEvent::Focused(..) => {}
                SessionLockEvent::Locked => {
                    tracing::info!("session locked");
                    if matches!(self.state, State::Locked { .. }) {
                        return Task::none();
                    }

                    let username = self.flags.user_data.name.clone();
                    let fingerprint_enabled = self.flags.fingerprint_enabled;
                    let (locked_task, locked_handle) =
                        cosmic::task::stream(cosmic::iced::stream::channel(
                            16,
                            move |msg_tx: futures::channel::mpsc::Sender<_>| async move {
                                // Send heartbeat once a second to update time.
                                let heartbeat_future = {
                                    let mut output = msg_tx.clone();
                                    async move {
                                        let mut interval =
                                            tokio::time::interval(Duration::from_secs(1));

                                        loop {
                                            output
                                                .send(cosmic::Action::App(Message::None))
                                                .await
                                                .unwrap();

                                            interval.tick().await;
                                        }
                                    }
                                };

                                let password_future = {
                                    let username = username.clone();
                                    let mut msg_tx = msg_tx.clone();
                                    async move {
                                        loop {
                                            let attempt_start = tokio::time::Instant::now();
                                            let (value_tx, value_rx) = mpsc::channel(16);
                                            msg_tx
                                                .send(cosmic::Action::App(Message::Channel(
                                                    value_tx,
                                                )))
                                                .await
                                                .unwrap();

                                            match run_pam_worker(
                                                // Password-only stack: reusing
                                                // "cosmic-greeter" pulls in
                                                // common-auth, so both workers
                                                // would claim the same reader.
                                                "cosmic-greeter-password",
                                                &username,
                                                &mut msg_tx,
                                                Some(value_rx),
                                                false,
                                            )
                                            .await
                                            {
                                                WorkerOutcome::Success => {
                                                    tracing::info!(
                                                        "successfully authenticated (password)"
                                                    );
                                                    msg_tx
                                                        .send(cosmic::Action::App(Message::Unlock))
                                                        .await
                                                        .unwrap();
                                                    break;
                                                }
                                                WorkerOutcome::Failure(message) => {
                                                    tracing::warn!(
                                                        "password authentication failed: {message}"
                                                    );
                                                    msg_tx
                                                        .send(cosmic::Action::App(Message::Error(
                                                            message,
                                                        )))
                                                        .await
                                                        .unwrap();
                                                }
                                                WorkerOutcome::Aborted => {
                                                    tracing::warn!("password worker aborted");
                                                    tokio::time::sleep(Duration::from_millis(100))
                                                        .await;
                                                }
                                            }

                                            let retry_at = attempt_start + PAM_MIN_ATTEMPT_DURATION;
                                            tokio::time::sleep_until(retry_at).await;
                                        }
                                    }
                                };

                                let fingerprint_future = {
                                    let username = username.clone();
                                    let mut msg_tx = msg_tx.clone();
                                    async move {
                                        let available = fingerprint_enabled
                                            && crate::fprintd::fingerprint_available(&username)
                                                .await;
                                        if !available {
                                            futures::future::pending::<()>().await;
                                        }

                                        tracing::info!("starting fingerprint authentication");
                                        const RETRY_MIN: Duration = Duration::from_secs(1);
                                        const RETRY_MAX: Duration = Duration::from_secs(64);
                                        const ARMED_AFTER: Duration = Duration::from_millis(500);
                                        let mut retry_delay = RETRY_MIN;

                                        loop {
                                            msg_tx
                                                .send(cosmic::Action::App(
                                                    Message::FingerprintInfo(None),
                                                ))
                                                .await
                                                .unwrap();

                                            let started = Instant::now();
                                            let outcome = run_pam_worker(
                                                "cosmic-greeter-fingerprint",
                                                &username,
                                                &mut msg_tx,
                                                None,
                                                true,
                                            )
                                            .await;
                                            let armed = started.elapsed() >= ARMED_AFTER;

                                            match outcome {
                                                WorkerOutcome::Success => {
                                                    tracing::info!(
                                                        "successfully authenticated (fingerprint)"
                                                    );
                                                    msg_tx
                                                        .send(cosmic::Action::App(Message::Unlock))
                                                        .await
                                                        .unwrap();
                                                    break;
                                                }
                                                WorkerOutcome::Failure(message) => {
                                                    tracing::info!(
                                                        "fingerprint attempt failed: {message}"
                                                    );
                                                }
                                                WorkerOutcome::Aborted => {}
                                            }

                                            if armed {
                                                retry_delay = RETRY_MIN;
                                            } else {
                                                tracing::warn!(
                                                    "fingerprint worker gave up after {:?} without arming the reader, retrying in {:?}",
                                                    started.elapsed(),
                                                    retry_delay
                                                );
                                            }
                                            tokio::time::sleep(retry_delay).await;
                                            if !armed {
                                                retry_delay = (retry_delay * 2).min(RETRY_MAX);
                                            }
                                        }
                                    }
                                };

                                let auth_future = async {
                                    futures::pin_mut!(password_future);
                                    futures::pin_mut!(fingerprint_future);
                                    // First stack to win cancels the other.
                                    futures::future::select(password_future, fingerprint_future)
                                        .await;
                                };

                                futures::pin_mut!(heartbeat_future);
                                futures::pin_mut!(auth_future);
                                futures::future::select(heartbeat_future, auth_future).await;
                            },
                        ))
                        .abortable();

                    let mut commands = Vec::with_capacity(self.common.surface_ids.len() + 1);
                    commands.push(locked_task);

                    self.state = State::Locked {
                        task_handle: locked_handle,
                    };

                    // Allow suspend
                    self.inhibit_opt = None;

                    // Create lock surfaces
                    for (output, surface_id) in self.common.surface_ids.iter() {
                        commands.push(cosmic::task::message(cosmic::Action::Surface(
                            cosmic::surface::action::lock(
                                || LiveSettings {
                                    padding: None,
                                    corners: None,
                                    blur: Some(false),
                                },
                                *surface_id,
                                output.clone(),
                                None::<fn() -> cosmic::Element<'static, cosmic::Action<Message>>>,
                            ),
                        )));

                        if let Some((rect, name)) = self
                            .common
                            .subsurface_rects
                            .get(output)
                            .copied()
                            .zip(self.common.output_names.get(output))
                        {
                            let subsurface_id = SurfaceId::unique();
                            self.common
                                .subsurface_outputs
                                .insert(subsurface_id, output.clone());
                            let surface_id = *surface_id;
                            self.common.surface_names.insert(surface_id, name.clone());
                            self.common
                                .surface_names
                                .insert(subsurface_id, name.clone());

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
                            commands.push(cosmic::task::message(cosmic::Action::Surface(msg)));
                        } else {
                            tracing::error!("no rectangle for subsurface creation...");
                        }
                    }
                    return Task::batch(commands);
                }
                SessionLockEvent::Unlocked => {
                    tracing::info!("session unlocked");
                    self.state = State::Unlocked;

                    let mut commands = Vec::new();
                    for surface_id in self.common.surface_ids.values() {
                        self.common.surface_names.remove(surface_id);
                        self.common.window_size.remove(surface_id);
                        commands.push(destroy_lock_surface(*surface_id));
                    }
                    if cfg!(feature = "logind") && self.flags.logind_available {
                        return Task::batch(commands);
                        // When logind is available, stick around for more lock signals
                    } else {
                        // When logind is not available, exit immediately after unlocking
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
            Message::BackgroundState(bg_state) => {
                self.flags.user_data.bg_state = bg_state;
                self.flags.user_data.load_wallpapers_as_user();
                self.common.surface_images.clear();
                self.common.update_wallpapers(&self.flags.user_data);
            }
            Message::DropdownToggle(dropdown) => {
                if self.dropdown_opt == Some(dropdown) {
                    self.dropdown_opt = None;
                    return self.common.dropdown_blur_rects(false);
                } else {
                    self.dropdown_opt = Some(dropdown);
                    return self.common.dropdown_blur_rects(true);
                }
            }
            Message::Inhibit(inhibit) => match self.state {
                State::Locked { .. } => {
                    tracing::info!("no need to inhibit sleep when already locked");
                }
                _ => {
                    self.inhibit_opt = Some(inhibit);
                }
            },
            Message::KeyboardLayout(layout_i) => {
                if let Some(keyboard_layout) = &self.common.keyboard_layout {
                    keyboard_layout.set_group(layout_i as u32);
                }
                self.common.current_keyboard_layout = layout_i;
                if self.dropdown_opt == Some(Dropdown::Keyboard) {
                    self.dropdown_opt = None;
                    return self.common.dropdown_blur_rects(false);
                }
            }
            Message::Submit(value) => {
                if value.is_empty() {
                    return Task::none();
                }
                self.common.error_opt = None;
                self.authenticating = true;
                match self.value_tx_opt.take() {
                    Some(value_tx) => {
                        return cosmic::task::future(async move {
                            value_tx.send(value).await.unwrap();
                            Message::Channel(value_tx)
                        });
                    }
                    None => tracing::warn!("tried to submit when value_tx_opt not set"),
                }
            }
            Message::Suspend => {
                #[cfg(feature = "logind")]
                return cosmic::Task::future(async move { crate::logind::suspend().await.err() })
                    .and_then(|err| {
                        tracing::error!("failed to suspend: {:?}", err);
                        cosmic::task::message(cosmic::Action::App(Message::Error(err.to_string())))
                    });
            }
            Message::TimeAppletConfig(config) => {
                self.flags.user_data.time_applet_config = config;
            }
            Message::Error(error) => {
                self.common.error_opt = Some(error);
                self.authenticating = false;
            }
            Message::FingerprintInfo(info_opt) => {
                self.fingerprint_info_opt = info_opt;
            }
            Message::Lock => match self.state {
                State::Unlocked => {
                    tracing::info!("session locking");
                    self.state = State::Locking;
                    // Clear errors
                    self.common.error_opt = None;
                    // Clear value_tx
                    self.value_tx_opt = None;
                    // Reset authenticating state
                    self.authenticating = false;
                    // Clear fingerprint status
                    self.fingerprint_info_opt = None;
                    // Try to create lockfile when locking
                    if let Some(ref lockfile) = self.flags.lockfile_opt
                        && let Err(err) = fs::File::create(lockfile)
                    {
                        tracing::warn!("failed to create lockfile {:?}: {}", lockfile, err);
                    }
                    // Tell compositor to lock
                    return lock();
                }
                State::Unlocking => {
                    tracing::info!("session still unlocking");
                }
                State::Locking | State::Locked { .. } => {
                    tracing::info!("session already locking or locked");
                }
            },
            Message::Unlock => {
                match self.state {
                    State::Locked { .. } => {
                        tracing::info!("sessing unlocking");
                        self.state = State::Unlocking;
                        // Clear errors
                        self.common.error_opt = None;
                        // Clear value_tx
                        self.value_tx_opt = None;
                        // Stop authenticating
                        self.authenticating = false;
                        // Clear fingerprint status
                        self.fingerprint_info_opt = None;

                        // Try to delete lockfile when unlocking
                        if let Some(ref lockfile) = self.flags.lockfile_opt
                            && let Err(err) = fs::remove_file(lockfile)
                        {
                            tracing::warn!("failed to remove lockfile {:?}: {}", lockfile, err);
                        }

                        // Destroy lock surfaces
                        let mut commands = Vec::with_capacity(self.common.surface_ids.len() + 1);

                        for surface_id in self.common.surface_ids.values() {
                            self.common.surface_names.remove(surface_id);
                            self.common.window_size.remove(surface_id);
                            commands.push(destroy_lock_surface(*surface_id));
                        }

                        // Tell compositor to unlock
                        commands.push(unlock());

                        // Wait to exit until `Unlocked` event, when server has processed unlock
                        return Task::batch(commands);
                    }
                    State::Locking => {
                        tracing::info!("session still locking");
                    }
                    State::Unlocking | State::Unlocked => {
                        tracing::info!("session already unlocking or unlocked");
                    }
                }
            }
            Message::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Surface(a));
            }
        }
        Task::none()
    }

    // Not used for layer surface window
    fn view(&self) -> Element<'_, Self::Message> {
        unimplemented!()
    }

    /// Creates a view after each update.
    fn view_window(&self, _surface_id: SurfaceId) -> Element<'_, Self::Message> {
        widget::space::horizontal().into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subscriptions = Vec::with_capacity(7);

        subscriptions.push(self.common.subscription().map(Message::from));

        struct BackgroundSubscription;
        subscriptions.push(
            cosmic_config::config_state_subscription(
                TypeId::of::<BackgroundSubscription>(),
                cosmic_bg_config::NAME.into(),
                cosmic_bg_config::state::State::version(),
            )
            .map(|res| {
                if !res.errors.is_empty() {
                    tracing::info!("errors loading background state: {:?}", res.errors);
                }
                Message::BackgroundState(res.config)
            }),
        );

        struct TimeAppletSubscription;
        subscriptions.push(
            cosmic_config::config_subscription(
                TypeId::of::<TimeAppletSubscription>(),
                "com.system76.CosmicAppletTime".into(),
                TimeAppletConfig::VERSION,
            )
            .map(|res| {
                if !res.errors.is_empty() {
                    tracing::info!("errors loading background state: {:?}", res.errors);
                }
                Message::TimeAppletConfig(res.config)
            }),
        );

        #[cfg(feature = "logind")]
        if self.flags.logind_available {
            subscriptions.push(crate::logind::subscription());
        }

        Subscription::batch(subscriptions)
    }
}
