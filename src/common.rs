use cosmic::app::{Core, Task};
use cosmic::iced::core::SmolStr;
use cosmic::iced::event::wayland::{Event as WaylandEvent, OutputEvent, SessionLockEvent};
use cosmic::iced::event::{self};
use cosmic::iced::keyboard::{Event as KeyEvent, Key, Modifiers};
use cosmic::iced::platform_specific::shell::commands::blur::blur;
use cosmic::iced::runtime::core::window::Id as SurfaceId;
use cosmic::iced::{self, Rectangle, Size, Subscription};
use cosmic::widget::rectangle_tracker::{RectangleUpdate, rectangle_tracker_subscription};
use cosmic::widget::{self, RectangleTracker};
use cosmic_config::{ConfigSet, CosmicConfigEntry};
use cosmic_greeter_daemon::{BgSource, CosmicCompConfig, UserData};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use wayland_client::protocol::wl_output::WlOutput;

pub const DEFAULT_MENU_ITEM_HEIGHT: f32 = 36.;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ActiveLayout {
    pub layout: String,
    pub description: String,
    pub variant: String,
}

pub struct Common<M> {
    pub active_layouts: Vec<ActiveLayout>,
    pub active_surface_id_opt: Option<SurfaceId>,
    pub on_battery: bool,
    pub battery_percent: f64,
    pub charging_limit: Option<bool>,
    pub caps_lock: bool,
    pub comp_config_handler: Option<cosmic_config::Config>,
    pub core: Core,
    pub error_opt: Option<String>,
    pub fallback_background: widget::image::Handle,
    pub layouts_opt: Option<Arc<xkb_data::KeyboardLayouts>>,
    pub network_icon_opt: Option<widget::Icon>,
    pub on_output_event: Option<Box<dyn Fn(OutputEvent, WlOutput) -> M>>,
    pub on_session_lock_event: Option<Box<dyn Fn(SessionLockEvent) -> M>>,
    pub output_names: HashMap<WlOutput, String>,
    pub power_info_opt: Option<(widget::Icon, f64)>,
    pub prompt_opt: Option<(String, bool, Option<String>)>,
    pub rectangle_tracker: Option<RectangleTracker<(SurfaceId, u8)>>,
    pub rectangles: HashMap<(SurfaceId, u8), iced::Rectangle>,
    pub include_menu: bool,
    pub last_blur_rects: HashMap<SurfaceId, Vec<iced::Rectangle>>,
    pub subsurface_rects: HashMap<WlOutput, Rectangle>,
    pub surface_ids: HashMap<WlOutput, SurfaceId>,
    pub subsurface_outputs: HashMap<SurfaceId, WlOutput>,
    pub surface_images: HashMap<SurfaceId, widget::image::Handle>,
    pub surface_names: HashMap<SurfaceId, String>,
    pub text_input_ids: HashMap<String, widget::Id>,
    pub time: crate::time::Time,
    pub window_size: HashMap<SurfaceId, Size>,
    /// When true, wallpapers are loaded sharp (no blur/darken) for lock screen.
    /// When false, wallpapers use frosted blur for the greeter.
    pub use_sharp_wallpaper: bool,
}

#[derive(Clone, Debug)]
pub enum Message {
    CapsLock(bool),
    Focus(SurfaceId),
    Key(Modifiers, Key, Option<SmolStr>),
    NetworkIcon(Option<&'static str>),
    SubsurfaceOpened(SurfaceId),
    OutputEvent(OutputEvent, WlOutput),
    PowerInfo(Option<(f64, bool, bool)>),
    Prompt(String, bool, Option<String>),
    SessionLockEvent(SessionLockEvent),
    Tick,
    Tz(jiff::tz::TimeZone),
    Rectangle(RectangleUpdate<(SurfaceId, u8)>),
}

pub fn circular_avatar_handle(bytes: &[u8], size: u32) -> widget::image::Handle {
    match image::load_from_memory(bytes) {
        Ok(dyn_img) => {
            let min_dim = dyn_img.width().min(dyn_img.height());
            let cropped = dyn_img.crop_imm(
                (dyn_img.width() - min_dim) / 2,
                (dyn_img.height() - min_dim) / 2,
                min_dim,
                min_dim,
            );
            let mut rgba = cropped
                .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
                .to_rgba8();
            let center = (size as f32) / 2.0;
            let radius = center - 1.0;
            for (x, y, pixel) in rgba.enumerate_pixels_mut() {
                let dx = x as f32 + 0.5 - center;
                let dy = y as f32 + 0.5 - center;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist >= radius + 1.0 {
                    pixel[3] = 0;
                } else if dist > radius - 1.0 {
                    let alpha_factor = (radius + 1.0 - dist) / 2.0;
                    pixel[3] = (pixel[3] as f32 * alpha_factor) as u8;
                }
            }
            widget::image::Handle::from_rgba(size, size, rgba.into_raw())
        }
        Err(err) => {
            tracing::warn!("Failed to process image for circular avatar: {err}");
            widget::image::Handle::from_bytes(bytes.to_vec())
        }
    }
}

fn fast_box_blur(buf: &mut [u8], width: u32, height: u32, radius: u32) {
    if width < 2 || height < 2 || radius == 0 {
        return;
    }
    let w = width as usize;
    let h = height as usize;
    let r = radius as usize;
    let mut temp = vec![0u8; buf.len()];

    temp.par_chunks_exact_mut(w * 4)
        .zip(buf.par_chunks_exact(w * 4))
        .for_each(|(temp_row, buf_row)| {
            let window_size = (2 * r + 1) as u32;

            let mut sum_r = (r as u32 + 1) * buf_row[0] as u32;
            let mut sum_g = (r as u32 + 1) * buf_row[1] as u32;
            let mut sum_b = (r as u32 + 1) * buf_row[2] as u32;
            let mut sum_a = (r as u32 + 1) * buf_row[3] as u32;

            for i in 1..=r {
                let clamped_x = i.min(w - 1);
                let idx = clamped_x * 4;
                sum_r += buf_row[idx] as u32;
                sum_g += buf_row[idx + 1] as u32;
                sum_b += buf_row[idx + 2] as u32;
                sum_a += buf_row[idx + 3] as u32;
            }

            for x in 0..w {
                let dst_idx = x * 4;
                temp_row[dst_idx] = (sum_r / window_size) as u8;
                temp_row[dst_idx + 1] = (sum_g / window_size) as u8;
                temp_row[dst_idx + 2] = (sum_b / window_size) as u8;
                temp_row[dst_idx + 3] = (sum_a / window_size) as u8;

                let left_x = x.saturating_sub(r);
                let right_x = (x + r + 1).min(w - 1);

                let left_idx = left_x * 4;
                let right_idx = right_x * 4;

                sum_r = sum_r + buf_row[right_idx] as u32 - buf_row[left_idx] as u32;
                sum_g = sum_g + buf_row[right_idx + 1] as u32 - buf_row[left_idx + 1] as u32;
                sum_b = sum_b + buf_row[right_idx + 2] as u32 - buf_row[left_idx + 2] as u32;
                sum_a = sum_a + buf_row[right_idx + 3] as u32 - buf_row[left_idx + 3] as u32;
            }
        });

    for x in 0..w {
        let window_size = (2 * r + 1) as u32;

        let mut sum_r = (r as u32 + 1) * temp[x * 4] as u32;
        let mut sum_g = (r as u32 + 1) * temp[x * 4 + 1] as u32;
        let mut sum_b = (r as u32 + 1) * temp[x * 4 + 2] as u32;
        let mut sum_a = (r as u32 + 1) * temp[x * 4 + 3] as u32;

        for i in 1..=r {
            let clamped_y = i.min(h - 1);
            let idx = (clamped_y * w + x) * 4;
            sum_r += temp[idx] as u32;
            sum_g += temp[idx + 1] as u32;
            sum_b += temp[idx + 2] as u32;
            sum_a += temp[idx + 3] as u32;
        }

        for y in 0..h {
            let dst_idx = (y * w + x) * 4;
            buf[dst_idx] = (sum_r / window_size) as u8;
            buf[dst_idx + 1] = (sum_g / window_size) as u8;
            buf[dst_idx + 2] = (sum_b / window_size) as u8;
            buf[dst_idx + 3] = (sum_a / window_size) as u8;

            let top_y = y.saturating_sub(r);
            let bottom_y = (y + r + 1).min(h - 1);

            let top_idx = (top_y * w + x) * 4;
            let bottom_idx = (bottom_y * w + x) * 4;

            sum_r = sum_r + temp[bottom_idx] as u32 - temp[top_idx] as u32;
            sum_g = sum_g + temp[bottom_idx + 1] as u32 - temp[top_idx + 1] as u32;
            sum_b = sum_b + temp[bottom_idx + 2] as u32 - temp[top_idx + 2] as u32;
            sum_a = sum_a + temp[bottom_idx + 3] as u32 - temp[top_idx + 3] as u32;
        }
    }
}

pub fn frosted_handle(path: &str, bytes: &[u8]) -> widget::image::Handle {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    if let Ok(metadata) = std::fs::metadata(path)
        && let Ok(modified) = metadata.modified()
    {
        modified.hash(&mut hasher);
    }
    let path_hash = hasher.finish();

    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("cosmic-greeter");
    let _ = std::fs::create_dir_all(&cache_dir);
    let cache_file = cache_dir.join(format!("frosted_{:x}.png", path_hash));

    if let Ok(cached_bytes) = std::fs::read(&cache_file) {
        return widget::image::Handle::from_bytes(cached_bytes);
    }

    match image::load_from_memory(bytes) {
        Ok(dyn_img) => {
            let (w, h) = (dyn_img.width().max(4) / 4, dyn_img.height().max(4) / 4);
            let small = dyn_img.resize_exact(w, h, image::imageops::FilterType::Triangle);
            let mut blurred = small.to_rgba8();
            let (width, height) = blurred.dimensions();
            let raw = blurred.as_mut();
            fast_box_blur(raw, width, height, 6);
            fast_box_blur(raw, width, height, 6);
            fast_box_blur(raw, width, height, 6);

            raw.par_chunks_exact_mut(4).for_each(|pixel| {
                pixel[0] = (pixel[0] as f32 * 0.55) as u8;
                pixel[1] = (pixel[1] as f32 * 0.55) as u8;
                pixel[2] = (pixel[2] as f32 * 0.55) as u8;
            });

            if let Err(e) = image::save_buffer(
                &cache_file,
                blurred.as_raw(),
                width,
                height,
                image::ColorType::Rgba8,
            ) {
                tracing::warn!("Failed to save cached frosted background: {}", e);
            }

            widget::image::Handle::from_rgba(width, height, blurred.into_raw())
        }
        Err(err) => {
            tracing::warn!("Failed to process image for frosted blur: {err}");
            widget::image::Handle::from_bytes(bytes.to_vec())
        }
    }
}

/// Load wallpaper at full resolution without blur or darkening.
/// Used by the lock screen where the wallpaper is the visual hero.
pub fn sharp_handle(bytes: &[u8]) -> widget::image::Handle {
    widget::image::Handle::from_bytes(bytes.to_vec())
}

impl<M: From<Message> + Send + 'static> Common<M> {
    pub fn init(mut core: Core, use_sharp_wallpaper: bool) -> (Self, Task<M>) {
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        // XXX must be false or define custom style to have transparent bg
        core.window.sharp_corners = false;
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.window.use_template = false;

        let comp_config_handler = match cosmic_config::Config::new(
            "com.system76.CosmicComp",
            CosmicCompConfig::VERSION,
        ) {
            Ok(config_handler) => Some(config_handler),
            Err(err) => {
                tracing::error!("failed to create cosmic-comp config handler: {}", err);
                None
            }
        };

        let layouts_opt = match xkb_data::all_keyboard_layouts() {
            Ok(ok) => Some(Arc::new(ok)),
            Err(err) => {
                tracing::warn!("failed to load keyboard layouts: {}", err);
                None
            }
        };

        let bg_bytes = include_bytes!("../res/background.jpg").as_slice();
        let fallback_background = if use_sharp_wallpaper {
            sharp_handle(bg_bytes)
        } else {
            frosted_handle("fallback", bg_bytes)
        };

        let app = Self {
            active_layouts: Vec::new(),
            active_surface_id_opt: None,
            caps_lock: false,
            comp_config_handler,
            core,
            error_opt: None,
            fallback_background,
            layouts_opt,
            network_icon_opt: None,
            on_output_event: None,
            on_session_lock_event: None,
            output_names: HashMap::new(),
            power_info_opt: None,
            prompt_opt: None,
            subsurface_rects: HashMap::new(),
            surface_ids: HashMap::new(),
            surface_images: HashMap::new(),
            surface_names: HashMap::new(),
            text_input_ids: HashMap::new(),
            time: crate::time::Time::new(),
            window_size: HashMap::new(),
            battery_percent: 0.0,
            on_battery: false,
            charging_limit: None,
            subsurface_outputs: HashMap::new(),
            rectangle_tracker: None,
            rectangles: HashMap::new(),
            include_menu: false,
            last_blur_rects: HashMap::new(),
            use_sharp_wallpaper,
        };
        (
            app,
            Task::batch(vec![
                crate::time::tick().map(|_| cosmic::Action::App(Message::Tick.into())),
                crate::time::tz_updates().map(|tz| cosmic::Action::App(Message::Tz(tz).into())),
            ]),
        )
    }

    pub fn set_xkb_config(&self, user_data: &UserData) {
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
            if let Some(comp_config_handler) = &self.comp_config_handler {
                match comp_config_handler.set("xkb_config", xkb_config) {
                    Ok(()) => tracing::info!("updated cosmic-comp xkb_config"),
                    Err(err) => tracing::error!("failed to update cosmic-comp xkb_config: {}", err),
                }
            }
        }
    }

    pub fn update_wallpapers(&mut self, user_data: &UserData) {
        for (_output, surface_id) in self.surface_ids.iter() {
            if self.surface_images.contains_key(surface_id) {
                continue;
            }

            let Some(output_name) = self.surface_names.get(surface_id) else {
                continue;
            };

            tracing::info!("updating wallpaper for {:?}", output_name);

            for (wallpaper_output_name, wallpaper_source) in user_data.bg_state.wallpapers.iter() {
                if wallpaper_output_name == output_name {
                    match wallpaper_source {
                        BgSource::Path(path) => {
                            match user_data.bg_path_data.get(path) {
                                Some(bytes) => {
                                    let image = if self.use_sharp_wallpaper {
                                        sharp_handle(bytes)
                                    } else {
                                        frosted_handle(&path.to_string_lossy(), bytes)
                                    };
                                    self.surface_images.insert(*surface_id, image);
                                    //TODO: what to do about duplicates?
                                }
                                None => {
                                    tracing::warn!(
                                        "output {}: failed to find wallpaper data for source {:?}",
                                        output_name,
                                        path
                                    );
                                }
                            }
                            break;
                        }
                        BgSource::Color(color) => {
                            //TODO: support color sources
                            tracing::warn!(
                                "output {}: unsupported source {:?}",
                                output_name,
                                color
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn update_user_data(&mut self, user_data: &UserData) {
        self.update_wallpapers(user_data);

        // From cosmic-applet-input-sources
        if let Some(keyboard_layouts) = &self.layouts_opt
            && let Some(xkb_config) = &user_data.xkb_config_opt
        {
            self.active_layouts.clear();
            let config_layouts = xkb_config.layout.split_terminator(',');
            let config_variants = xkb_config
                .variant
                .split_terminator(',')
                .chain(std::iter::repeat(""));
            'outer: for (config_layout, config_variant) in config_layouts.zip(config_variants) {
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
                        continue 'outer;
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
                        continue 'outer;
                    }
                }
            }
            tracing::info!("{:?}", self.active_layouts);
        }
    }

    pub fn update(&mut self, message: Message) -> Task<M> {
        match message {
            Message::CapsLock(caps_lock) => {
                self.caps_lock = caps_lock;
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
            Message::Key(modifiers, key, text) => {
                // Uncaptured keys with only shift modifiers go to the password box
                if !modifiers.logo()
                    && !modifiers.control()
                    && !modifiers.alt()
                    && matches!(key, Key::Character(_))
                {
                    if let Some(text) = text
                        && let Some((_, _, Some(value))) = &mut self.prompt_opt
                    {
                        value.push_str(&text);
                    }

                    if let Some(surface_id) = self.active_surface_id_opt
                        && let Some(text_input_id) = self
                            .surface_names
                            .get(&surface_id)
                            .and_then(|id| self.text_input_ids.get(id))
                    {
                        return widget::text_input::focus(text_input_id.clone());
                    }
                }
            }
            Message::NetworkIcon(network_icon_opt) => {
                self.network_icon_opt =
                    network_icon_opt.map(|name| widget::icon::from_name(name).into());
            }
            Message::OutputEvent(output_event, output) => {
                if let Some(on_output_event) = &self.on_output_event {
                    return Task::done(cosmic::Action::App(on_output_event(output_event, output)));
                }
            }
            Message::PowerInfo(power_info_opt) => {
                if let Some((level, on_battery, threshold_enabled)) = power_info_opt {
                    self.charging_limit = Some(threshold_enabled);
                    self.update_battery(level, on_battery);
                }
            }
            Message::Prompt(prompt, secret, value_opt) => {
                let prompt_was_none = self.prompt_opt.is_none();
                self.prompt_opt = Some((prompt, secret, value_opt));
                if prompt_was_none && let Some(surface_id) = self.active_surface_id_opt {
                    return cosmic::iced::Task::perform(
                        async {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        },
                        move |_| Message::Focus(surface_id),
                    )
                    .map(|m| cosmic::Action::App(m.into()));
                }
            }
            Message::SessionLockEvent(lock_event) => {
                if let Some(on_session_lock_event) = &self.on_session_lock_event {
                    return Task::done(cosmic::Action::App(on_session_lock_event(lock_event)));
                }
            }
            Message::Tick => {
                self.time.tick();
            }
            Message::Tz(tz) => {
                self.time.set_tz(tz);
            }
            Message::SubsurfaceOpened(id) => {
                return self.blur_rects(id);
            }
            Message::Rectangle(u) => match u {
                RectangleUpdate::Rectangle(r) => {
                    let rounded_r = iced::Rectangle {
                        x: r.1.x.round(),
                        y: r.1.y.round(),
                        width: r.1.width.round(),
                        height: r.1.height.round(),
                    };
                    let old_rect = self.rectangles.insert(r.0, rounded_r);
                    // Only re-send blur if the integer-pixel bounding box actually
                    // changed. Sub-pixel jitter from iced layout engine is ignored,
                    // preventing an avalanche of Wayland IPC calls on static screens.
                    let changed = match old_rect {
                        Some(old) => {
                            (old.x - rounded_r.x).abs() >= 1.0
                                || (old.y - rounded_r.y).abs() >= 1.0
                                || (old.width - rounded_r.width).abs() >= 1.0
                                || (old.height - rounded_r.height).abs() >= 1.0
                        }
                        None => true, // First time seeing this rectangle
                    };
                    if changed {
                        return self.blur_rects(r.0.0);
                    }
                }
                RectangleUpdate::Init(tracker) => {
                    self.rectangle_tracker.replace(tracker);
                }
            },
        }
        Task::none()
    }

    pub(crate) fn dropdown_blur_rects(&mut self, enable: bool) -> Task<M> {
        let mut ids = HashSet::new();
        for r in self.rectangles.keys() {
            if r.1 == 1 {
                ids.insert(r.0);
            }
        }
        self.include_menu = enable;
        if !enable {
            self.last_blur_rects.clear();
        }
        Task::batch(
            ids.into_iter()
                .map(|i| self.blur_rects(i))
                .collect::<Vec<_>>(),
        )
    }

    pub fn blur_rects(&mut self, id: SurfaceId) -> Task<M> {
        if !cosmic::theme::active().cosmic().frosted_system_interface || !self.use_sharp_wallpaper {
            if let Some(last) = self.last_blur_rects.get(&id)
                && last.is_empty()
            {
                return Task::none();
            }
            self.last_blur_rects.insert(id, Vec::new());
            return blur(id, None).discard();
        }
        if let Some(output) = self.subsurface_outputs.get(&id)
            && let Some(rect) = self.subsurface_rects.get(output)
        {
            let x = rect.x;
            let y = rect.y;
            let mut rects = Vec::new();
            for (&(surf_id, tag), r) in self.rectangles.iter() {
                if surf_id == id {
                    if tag == 1 && !self.include_menu {
                        continue;
                    }
                    rects.push(iced::Rectangle {
                        x: (r.x + x).round(),
                        y: (r.y + y).round(),
                        width: r.width.round(),
                        height: r.height.round(),
                    });
                }
            }
            if rects.is_empty() {
                if let Some(last) = self.last_blur_rects.get(&id)
                    && last.is_empty()
                {
                    return Task::none();
                }
                self.last_blur_rects.insert(id, Vec::new());
                return blur(id, None).discard();
            }
            // Sort deterministically by spatial coordinates so arbitrary HashMap
            // iteration order does not cause false negatives in the cache check.
            rects.sort_by(|a, b| {
                a.y.partial_cmp(&b.y)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
            });
            if let Some(last) = self.last_blur_rects.get(&id)
                && last == &rects
            {
                return Task::none();
            }
            self.last_blur_rects.insert(id, rects.clone());
            return blur(id, Some(rects)).discard();
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = Vec::with_capacity(3);
        subscriptions
            .push(rectangle_tracker_subscription(0).map(|update| Message::Rectangle(update.1)));
        subscriptions.push(event::listen_with(|event, status, id| match event {
            iced::Event::Keyboard(KeyEvent::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) => match status {
                event::Status::Ignored => Some(Message::Key(modifiers, key, text)),
                event::Status::Captured => None,
            },
            iced::Event::Keyboard(KeyEvent::ModifiersChanged(modifiers)) => {
                Some(Message::CapsLock(modifiers.contains(Modifiers::CAPS_LOCK)))
            }
            iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
                wayland_event,
            )) => match wayland_event {
                WaylandEvent::Output(output_event, output) => {
                    Some(Message::OutputEvent(output_event, output))
                }
                WaylandEvent::SessionLock(lock_event) => {
                    Some(Message::SessionLockEvent(lock_event))
                }
                iced::event::wayland::Event::Subsurface(
                    iced::event::wayland::SubsurfaceEvent::Created,
                ) => Some(Message::SubsurfaceOpened(id)),
                _ => None,
            },
            iced::Event::Window(iced::window::Event::Focused) => Some(Message::Focus(id)),
            _ => None,
        }));

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

impl<M> Common<M> {
    fn update_battery(&mut self, mut percent: f64, on_battery: bool) {
        percent = percent.clamp(0.0, 100.0);
        self.on_battery = on_battery;
        self.battery_percent = percent;
        let battery_percent =
            if self.battery_percent > 95.0 && !self.charging_limit.unwrap_or_default() {
                100
            } else if self.battery_percent > 80.0 && !self.charging_limit.unwrap_or_default() {
                90
            } else if self.battery_percent > 65.0 {
                80
            } else if self.battery_percent > 35.0 {
                50
            } else if self.battery_percent > 20.0 {
                35
            } else if self.battery_percent > 14.0 {
                20
            } else if self.battery_percent > 9.0 {
                10
            } else if self.battery_percent > 5.0 {
                5
            } else {
                0
            };
        let limited = if self.charging_limit.unwrap_or_default() {
            "limited-"
        } else {
            ""
        };
        let charging = if on_battery { "" } else { "charging-" };
        self.power_info_opt = Some((
            widget::icon::from_name(format!(
                "cosmic-applet-battery-level-{battery_percent}-{limited}{charging}symbolic",
            ))
            .into(),
            percent,
        ));
    }
}
