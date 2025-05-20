// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use carnelian::app::ViewCreationParameters;
use carnelian::color::Color;
use carnelian::drawing::{DisplayRotation, FontFace};
use carnelian::render::rive::load_rive;
use carnelian::scene::facets::{
    RiveFacet, TextFacetOptions, TextHorizontalAlignment, TextVerticalAlignment,
};
use carnelian::scene::layout::{
    CrossAxisAlignment, Flex, FlexOptions, MainAxisAlignment, MainAxisSize,
};
use carnelian::scene::scene::{Scene, SceneBuilder};
use carnelian::{
    input, App, AppAssistant, AppAssistantPtr, AppSender, IntPoint, Point, Size, ViewAssistant,
    ViewAssistantContext, ViewAssistantPtr, ViewKey,
};
use euclid::size2;
use fidl_fuchsia_input_report::ConsumerControlButton;
use fuchsia_async as fasync;

mod menu;
use menu::Menu;
mod fdr;
mod power;
mod view_sender;
use view_sender::ViewSender;

const LOGO_IMAGE_PATH: &str = "/system-recovery-config/logo.riv";
const BG_COLOR: Color = Color::new(); // Black
const HEADER_COLOR: Color = Color { r: 249, g: 194, b: 0, a: 255 };
const MESSAGE_COLOR: Color = Color { r: 247, g: 0, b: 6, a: 255 };
const MENU_COLOR: Color = Color { r: 0, g: 106, b: 157, a: 255 };
const MENU_ACTIVE_BG_COLOR: Color = Color { r: 0, g: 156, b: 100, a: 255 };
const MENU_SELECTED_COLOR: Color = Color::white();

struct RecoveryAppAssistant {
    display_rotation: DisplayRotation,
}

impl RecoveryAppAssistant {
    fn new(display_rotation: DisplayRotation) -> Self {
        Self { display_rotation }
    }
}

impl AppAssistant for RecoveryAppAssistant {
    fn setup(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn create_view_assistant_with_parameters(
        &mut self,
        params: ViewCreationParameters,
    ) -> Result<ViewAssistantPtr, Error> {
        Ok(Box::new(RecoveryViewAssistant::new(params.view_key, params.app_sender)?))
    }

    fn filter_config(&mut self, config: &mut carnelian::app::Config) {
        config.view_mode = carnelian::app::ViewMode::Direct;
        config.display_rotation = self.display_rotation;
    }
}

struct RecoveryViewAssistant {
    view_sender: ViewSender,
    font_face: FontFace,
    logo_file: Option<rive_rs::File>,
    build_info: String,
    scene: Option<Scene>,
    menu: Menu,
    logs: Option<Vec<String>>,
    wheel_diff: i32,
    message: Option<&'static str>,
    waiting_for_confirmation: bool,
    // tuple of touch contact id, start location, current location
    active_contact: Option<(input::touch::ContactId, IntPoint, IntPoint)>,
}

impl RecoveryViewAssistant {
    fn new(view_key: ViewKey, app_sender: AppSender) -> Result<RecoveryViewAssistant, Error> {
        let font_face = recovery_ui::font::get_default_font_face().clone();
        let logo_file = load_rive(LOGO_IMAGE_PATH).ok();
        let product = std::fs::read_to_string("/config/build-info/product").unwrap_or_default();
        let board = std::fs::read_to_string("/config/build-info/board").unwrap_or_default();
        let product_version =
            std::fs::read_to_string("/config/build-info/product_version").unwrap_or_default();
        let platform_version =
            std::fs::read_to_string("/config/build-info/platform_version").unwrap_or_default();
        let build_info = format!("{product}/{board}:{product_version}/{platform_version}");
        let menu = Menu::new(menu::MAIN_MENU);

        Ok(RecoveryViewAssistant {
            view_sender: ViewSender::new(app_sender, view_key),
            font_face,
            logo_file,
            build_info,
            scene: None,
            menu,
            logs: None,
            wheel_diff: 0,
            message: None,
            waiting_for_confirmation: false,
            active_contact: None,
        })
    }

    fn log(&mut self, log: impl Into<String>) {
        let log = log.into();
        log::info!("log: {log}");
        self.logs.get_or_insert_default().push(log);

        self.request_render();
    }

    fn request_render(&mut self) {
        self.wheel_diff = 0;
        self.scene = None;
        self.view_sender.request_render();
    }

    fn on_menu_select(&mut self) {
        match self.menu.current_item() {
            menu::MenuItem::Reboot => {
                self.log("Rebooting...");
                self.view_sender.queue_message(RecoveryMessages::Reboot);
            }
            menu::MenuItem::RebootBootloader => {
                self.log("Rebooting to bootloader...");
                self.view_sender.queue_message(RecoveryMessages::RebootBootloader);
            }
            menu::MenuItem::PowerOff => {
                self.log("Powering off...");
                self.view_sender.queue_message(RecoveryMessages::PowerOff);
            }
            menu::MenuItem::WipeData => {
                self.menu = Menu::new(menu::WIPE_DATA_MENU);
                self.message = Some("Wipe all user data?\n  THIS CAN NOT BE UNDONE!");
                self.request_render();
            }
            menu::MenuItem::WipeDataCancel => {
                self.menu = Menu::new(menu::MAIN_MENU);
                self.message = None;
                self.request_render();
            }
            menu::MenuItem::WipeDataConfirm => {
                self.log("Wiping data...");
                self.view_sender.queue_message(RecoveryMessages::WipeData);
            }
            menu_item => {
                self.log(format!("Not implemented: {menu_item:?}"));
                self.view_sender.queue_message(RecoveryMessages::TaskDone);
            }
        }
    }
}

impl ViewAssistant for RecoveryViewAssistant {
    fn setup(&mut self, _context: &ViewAssistantContext) -> Result<(), Error> {
        Ok(())
    }

    fn get_scene(&mut self, size: Size) -> Option<&mut Scene> {
        Some(self.scene.get_or_insert_with(|| {
            let mut builder =
                SceneBuilder::new().background_color(BG_COLOR).round_scene_corners(true);
            builder.group().column().max_size().main_align(MainAxisAlignment::Start).contents(
                |builder| {
                    if let Some(logo_file) = &self.logo_file {
                        // Centre the logo
                        builder.start_group(
                            "logo_row",
                            Flex::with_options_ptr(FlexOptions::row(
                                MainAxisSize::Max,
                                MainAxisAlignment::Center,
                                CrossAxisAlignment::End,
                            )),
                        );

                        let logo_size: Size = size2(50.0, 50.0);
                        let facet = RiveFacet::new_from_file(logo_size, &logo_file, None)
                            .expect("facet_from_file");
                        builder.facet(Box::new(facet));
                        builder.end_group(); // logo_row
                    }

                    builder.space(size2(size.width, 10.0));

                    let text_size = 25.0;
                    builder.text(
                        self.font_face.clone(),
                        "Android Recovery",
                        text_size,
                        Point::zero(),
                        TextFacetOptions {
                            horizontal_alignment: TextHorizontalAlignment::Center,
                            color: HEADER_COLOR,
                            ..TextFacetOptions::default()
                        },
                    );

                    builder.space(size2(size.width, 10.0));

                    builder
                        .group()
                        .row()
                        .max_size()
                        .cross_align(CrossAxisAlignment::Start)
                        .contents(|builder| {
                            builder.space(size2(size.width * 0.1, text_size));
                            builder.text(
                                self.font_face.clone(),
                                &self.build_info,
                                text_size,
                                Point::zero(),
                                TextFacetOptions {
                                    color: HEADER_COLOR,
                                    horizontal_alignment: TextHorizontalAlignment::Left,
                                    max_width: Some(size.width * 0.8),
                                    ..TextFacetOptions::default()
                                },
                            );
                        });

                    builder.space(size2(size.width, 30.0));

                    builder
                        .group()
                        .column()
                        .max_size()
                        .main_align(MainAxisAlignment::Start)
                        .cross_align(CrossAxisAlignment::Start)
                        .contents(|builder| {
                            if let Some(logs) = &self.logs {
                                builder
                                    .group()
                                    .row()
                                    .max_size()
                                    .cross_align(CrossAxisAlignment::Start)
                                    .contents(|builder| {
                                        // padding on the left of the log
                                        builder.space(size2(size.width * 0.1, text_size));
                                        builder.text(
                                            self.font_face.clone(),
                                            &logs.join("\n"),
                                            text_size,
                                            Point::zero(),
                                            TextFacetOptions {
                                                color: Color::white(),
                                                horizontal_alignment: TextHorizontalAlignment::Left,
                                                max_width: Some(size.width * 0.8),
                                                ..TextFacetOptions::default()
                                            },
                                        );
                                    });
                                return;
                            }

                            if let Some(message) = &self.message {
                                builder
                                    .group()
                                    .row()
                                    .max_size()
                                    .cross_align(CrossAxisAlignment::Start)
                                    .contents(|builder| {
                                        // padding on the left of the message
                                        builder.space(size2(size.width * 0.1, text_size));
                                        builder.text(
                                            self.font_face.clone(),
                                            message,
                                            text_size,
                                            Point::zero(),
                                            TextFacetOptions {
                                                color: MESSAGE_COLOR,
                                                horizontal_alignment: TextHorizontalAlignment::Left,
                                                max_width: Some(size.width * 0.8),
                                                ..TextFacetOptions::default()
                                            },
                                        );
                                    });
                            }

                            const MENU_ITEM_HEIGHT: f32 = 30.0;

                            for item in self.menu.items() {
                                builder.group().stack().contents(|builder| {
                                    builder
                                        .group()
                                        .row()
                                        .max_size()
                                        .cross_align(CrossAxisAlignment::Start)
                                        .contents(|builder| {
                                            // padding on the left of the menu text
                                            builder
                                                .space(size2(size.width * 0.1, MENU_ITEM_HEIGHT));
                                            builder.text(
                                                self.font_face.clone(),
                                                item.title(),
                                                text_size,
                                                Point::zero(),
                                                TextFacetOptions {
                                                    horizontal_alignment:
                                                        TextHorizontalAlignment::Left,
                                                    vertical_alignment:
                                                        TextVerticalAlignment::Center,
                                                    color: if self.menu.current_item() == item {
                                                        MENU_SELECTED_COLOR
                                                    } else {
                                                        MENU_COLOR
                                                    },
                                                    max_width: Some(size.width * 0.8),
                                                    ..TextFacetOptions::default()
                                                },
                                            );
                                        });

                                    let rect_size = size2(size.width, MENU_ITEM_HEIGHT);
                                    if self.menu.current_item() == item {
                                        builder.rectangle(
                                            rect_size,
                                            if self.menu.is_active() {
                                                MENU_ACTIVE_BG_COLOR
                                            } else {
                                                MENU_COLOR
                                            },
                                        );
                                    } else {
                                        builder.space(rect_size);
                                    }
                                });
                            }
                        });
                },
            );

            builder.build()
        }))
    }

    fn handle_mouse_event(
        &mut self,
        _context: &mut ViewAssistantContext,
        _event: &input::Event,
        mouse_event: &input::mouse::Event,
    ) -> Result<(), Error> {
        if self.logs.is_some() {
            return Ok(());
        }
        match mouse_event.phase {
            input::mouse::Phase::Wheel(vector) => {
                self.wheel_diff += vector.y;
                if self.wheel_diff > 80 {
                    self.menu.move_up();
                    self.request_render();
                } else if self.wheel_diff < -80 {
                    self.menu.move_down();
                    self.request_render();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_touch_event(
        &mut self,
        context: &mut ViewAssistantContext,
        _event: &input::Event,
        touch_event: &input::touch::Event,
    ) -> Result<(), Error> {
        if self.logs.is_some() {
            return Ok(());
        }
        match *touch_event.contacts {
            [input::touch::Contact {
                contact_id,
                phase: input::touch::Phase::Down(location, _size),
            }] => {
                self.active_contact = Some((contact_id, location, location));
            }
            [input::touch::Contact {
                contact_id,
                phase: input::touch::Phase::Moved(location, _size),
            }] => {
                let start_location = if let Some((active_contact_id, start_location, _)) =
                    self.active_contact
                    && contact_id == active_contact_id
                {
                    start_location
                } else {
                    location
                };

                self.active_contact = Some((contact_id, start_location, location));
            }
            [input::touch::Contact { contact_id, phase: input::touch::Phase::Up }] => {
                if let Some((active_contact_id, start_location, current_location)) =
                    self.active_contact
                    && contact_id == active_contact_id
                {
                    let delta = current_location - start_location;
                    let x = delta.x.abs() as f32;
                    let y = delta.y.abs() as f32;
                    if y > context.size.height * 0.4 && y > x * 2.0 {
                        if delta.y > 0 {
                            self.menu.move_down();
                        } else {
                            self.menu.move_up();
                        }
                        self.request_render();
                    } else if x > context.size.width * 0.4 && x > y * 2.0 {
                        self.menu.set_active(true);
                        self.on_menu_select();
                    }
                }
                self.active_contact = None;
            }
            _ => {
                self.active_contact = None;
            }
        }
        Ok(())
    }

    fn handle_consumer_control_event(
        &mut self,
        _context: &mut ViewAssistantContext,
        _event: &input::Event,
        consumer_control_event: &input::consumer_control::Event,
    ) -> Result<(), Error> {
        if self.logs.is_some() {
            if self.waiting_for_confirmation
                && consumer_control_event.phase == input::consumer_control::Phase::Up
            {
                self.waiting_for_confirmation = false;
                self.logs = None;
                self.menu = Menu::new(menu::MAIN_MENU);
                self.message = None;
                self.request_render();
            }
            return Ok(());
        }
        match consumer_control_event {
            input::consumer_control::Event {
                button: ConsumerControlButton::Power,
                phase: input::consumer_control::Phase::Down,
            } => {
                self.menu.set_active(true);
                self.request_render();
            }
            input::consumer_control::Event {
                button: ConsumerControlButton::Function,
                phase: input::consumer_control::Phase::Up,
            } => {
                self.menu.move_down();
                self.request_render();
            }
            input::consumer_control::Event {
                button: ConsumerControlButton::Power,
                phase: input::consumer_control::Phase::Up,
            } => self.on_menu_select(),
            _ => {
                return Ok(());
            }
        }
        Ok(())
    }

    fn handle_message(&mut self, message: carnelian::Message) {
        let Some(message) = message.downcast_ref::<RecoveryMessages>() else {
            return;
        };
        match message {
            RecoveryMessages::Log(log) => {
                self.log(log);
            }
            RecoveryMessages::TaskDone => {
                self.waiting_for_confirmation = true;
            }
            RecoveryMessages::Reboot => {
                let view_sender = self.view_sender.clone();
                fasync::Task::local(async move {
                    if let Err(e) = power::reboot().await {
                        view_sender.queue_message(RecoveryMessages::Log(format!(
                            "Failed to reboot: {e:#}"
                        )));
                    }
                    view_sender.queue_message(RecoveryMessages::TaskDone);
                })
                .detach();
            }
            RecoveryMessages::RebootBootloader => {
                let view_sender = self.view_sender.clone();
                fasync::Task::local(async move {
                    if let Err(e) = power::reboot_to_bootloader().await {
                        view_sender.queue_message(RecoveryMessages::Log(format!(
                            "Failed to reboot to bootloader: {e:#}"
                        )));
                    }
                    view_sender.queue_message(RecoveryMessages::TaskDone);
                })
                .detach();
            }
            RecoveryMessages::PowerOff => {
                let view_sender = self.view_sender.clone();
                fasync::Task::local(async move {
                    if let Err(e) = power::power_off().await {
                        view_sender.queue_message(RecoveryMessages::Log(format!(
                            "Failed to power off: {e:#}"
                        )));
                    }
                    view_sender.queue_message(RecoveryMessages::TaskDone);
                })
                .detach();
            }
            RecoveryMessages::WipeData => {
                let view_sender = self.view_sender.clone();
                fasync::Task::local(async move {
                    if let Err(e) = fdr::factory_data_reset().await {
                        view_sender.queue_message(RecoveryMessages::Log(format!(
                            "Failed to factory data reset: {e:#}"
                        )));
                    }
                    view_sender.queue_message(RecoveryMessages::TaskDone);
                })
                .detach();
            }
        }
    }
}

enum RecoveryMessages {
    Log(String),
    TaskDone,
    Reboot,
    RebootBootloader,
    PowerOff,
    WipeData,
}

#[fuchsia::main]
fn main() -> Result<(), Error> {
    log::info!("recovery-android started.");

    let config = recovery_ui_config::Config::take_from_startup_handle();
    let display_rotation = match config.display_rotation {
        0 => DisplayRotation::Deg0,
        180 => DisplayRotation::Deg180,
        // Carnelian uses an inverted z-axis for rotation
        90 => DisplayRotation::Deg270,
        270 => DisplayRotation::Deg90,
        val => {
            log::error!("Invalid display_rotation {}, defaulting to 0 degrees", val);
            DisplayRotation::Deg0
        }
    };

    App::run(Box::new(move |_| {
        Box::pin(async move {
            let assistant = Box::new(RecoveryAppAssistant::new(display_rotation));
            Ok::<AppAssistantPtr, Error>(assistant)
        })
    }))
}
