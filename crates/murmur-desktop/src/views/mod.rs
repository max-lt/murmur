//! View (rendering) methods for the desktop app screens.

mod conflicts;
mod devices;
mod folders;
mod network_health;
mod settings;
mod status;

use iced::widget::{Space, button, column, container, row, rule, scrollable, text, text_input};
use iced::{Background, Color, Element, Length, Theme};

use crate::app::{App, Screen, SetupStep};
use crate::message::Message;
use crate::style::*;

impl App {
    pub fn view(&self) -> Element<'_, Message> {
        match self.screen {
            Screen::DaemonCheck => self.view_daemon_check(),
            Screen::Setup => self.view_setup(),
            _ => self.view_main(),
        }
    }

    // -- Daemon check / Setup --

    fn view_daemon_check(&self) -> Element<'_, Message> {
        let status_el: Element<'_, Message> = match self.daemon_running {
            None => text("Connecting to murmurd...")
                .size(16)
                .color(TEXT_SECONDARY)
                .into(),
            Some(false) => text("Starting murmurd...")
                .size(16)
                .color(TEXT_SECONDARY)
                .into(),
            Some(true) => text("Connected!").size(16).color(ACCENT).into(),
        };

        let mut content = column![
            text("Murmur").size(28).color(Color::WHITE),
            text("Private Device Sync Network")
                .size(14)
                .color(TEXT_MUTED),
            Space::new().height(16),
            status_el,
        ]
        .spacing(6);

        if let Some(ref e) = self.daemon_error {
            content = content
                .push(Space::new().height(8))
                .push(text(format!("Error: {e}")).color(ERROR).size(13))
                .push(Space::new().height(12))
                .push(
                    row![
                        button(text("Retry"))
                            .on_press(Message::RetryDaemonCheck)
                            .style(primary_btn)
                            .padding(8),
                        button(text("Setup new network"))
                            .on_press(Message::Navigate(Screen::Setup))
                            .style(secondary_btn)
                            .padding(8),
                    ]
                    .spacing(8),
                );
        }

        let card = container(content)
            .padding(40)
            .max_width(420)
            .style(card_style);

        container(card)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(SIDEBAR_BG)),
                ..Default::default()
            })
            .into()
    }

    fn view_setup(&self) -> Element<'_, Message> {
        let inner: Element<'_, Message> = match self.setup_step {
            SetupStep::ChooseMode => column![
                text("Murmur").size(28).color(Color::WHITE),
                text("Private Device Sync Network")
                    .size(14)
                    .color(TEXT_MUTED),
                Space::new().height(20),
                button(text("Create Network").width(Length::Fill))
                    .width(Length::Fill)
                    .on_press(Message::SetupChooseCreate)
                    .style(primary_btn)
                    .padding(12),
                button(text("Join Network").width(Length::Fill))
                    .width(Length::Fill)
                    .on_press(Message::SetupChooseJoin)
                    .style(secondary_btn)
                    .padding(12),
                Space::new().height(8),
                button(text("Back to daemon check").color(TEXT_MUTED))
                    .on_press(Message::RetryDaemonCheck)
                    .style(|_: &Theme, _: button::Status| btn(None, TEXT_MUTED))
                    .padding(8),
            ]
            .spacing(10)
            .into(),
            SetupStep::Form => {
                let title = if self.join_mode {
                    "Join Network"
                } else {
                    "Create Network"
                };
                let mut col = column![
                    button(text("Back").color(TEXT_SECONDARY))
                        .on_press(Message::SetupBack)
                        .style(secondary_btn)
                        .padding(8),
                    text(title).size(22).color(Color::WHITE),
                    Space::new().height(8),
                    text_input("Device name", &self.device_name)
                        .on_input(Message::DeviceNameChanged)
                        .padding(10),
                ]
                .spacing(10);
                if self.join_mode {
                    col = col.push(
                        text_input("Enter mnemonic phrase...", &self.mnemonic_input)
                            .on_input(Message::MnemonicInputChanged)
                            .padding(10),
                    );
                }
                let can = !self.device_name.is_empty()
                    && (!self.join_mode || !self.mnemonic_input.is_empty());
                let label = if self.join_mode {
                    "Start daemon & join"
                } else {
                    "Start daemon & create"
                };
                let mut submit = button(text(label).width(Length::Fill))
                    .width(Length::Fill)
                    .style(primary_btn)
                    .padding(12);
                if can {
                    submit = submit.on_press(Message::StartDaemon);
                }
                col = col.push(Space::new().height(4)).push(submit);
                if let Some(ref e) = self.setup_error {
                    col = col.push(text(format!("Error: {e}")).color(ERROR).size(13));
                }
                col.into()
            }
        };

        let card = container(inner)
            .padding(40)
            .max_width(480)
            .style(card_style);

        container(card)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(SIDEBAR_BG)),
                ..Default::default()
            })
            .into()
    }

    // -- Main layout: sidebar + content panel --

    fn view_main(&self) -> Element<'_, Message> {
        let conflict_label = if self.conflicts.is_empty() {
            "Conflicts".to_string()
        } else {
            format!("Conflicts ({})", self.conflicts.len())
        };
        let devices_label = if self.pending.is_empty() {
            "Devices".to_string()
        } else {
            format!("Devices ({})", self.pending.len())
        };

        let (status_color, status_text) = if self.sync_paused {
            (WARNING, "Paused")
        } else {
            (ACCENT, "Syncing")
        };

        let sidebar = container(
            column![
                // Brand
                row![
                    text("Murmur").size(22).color(Color::WHITE),
                    Space::new().width(Length::Fill),
                    text("\u{2022}").size(10).color(status_color),
                    text(status_text).size(11).color(TEXT_MUTED),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
                // Spacer
                Space::new().height(28),
                // Primary nav
                self.nav_button("\u{25A6}", "Folders", Screen::Folders),
                self.nav_button("\u{26A0}", &conflict_label, Screen::Conflicts),
                self.nav_button("\u{2B21}", &devices_label, Screen::Devices),
                self.nav_button("\u{2630}", "Recent Files", Screen::RecentFiles),
                self.nav_button("\u{2261}", "Status", Screen::Status),
                // Push bottom section down
                Space::new().height(Length::Fill),
                // Secondary nav
                self.nav_button("\u{2316}", "Network Health", Screen::NetworkHealth),
                self.nav_button("\u{2699}", "Settings", Screen::Settings),
                // Separator
                rule::horizontal(1),
                // Sync toggle
                button(
                    row![
                        text(if self.sync_paused {
                            "\u{25B6}"
                        } else {
                            "\u{23F8}"
                        })
                        .size(14),
                        text(if self.sync_paused {
                            "Resume Sync"
                        } else {
                            "Pause Sync"
                        })
                        .size(13),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .width(Length::Fill),
                )
                .on_press(Message::ToggleGlobalSync)
                .width(Length::Fill)
                .padding(iced::Padding {
                    top: 10.0,
                    right: 14.0,
                    bottom: 10.0,
                    left: 14.0,
                })
                .style(secondary_btn),
            ]
            .spacing(2)
            .padding(iced::Padding {
                top: 24.0,
                right: 12.0,
                bottom: 16.0,
                left: 12.0,
            })
            .width(230)
            .height(Length::Fill),
        )
        .style(sidebar_style)
        .height(Length::Fill);

        let content: Element<Message> = match self.screen {
            Screen::Folders => self.view_folders(),
            Screen::FolderDetail => self.view_folder_detail(),
            Screen::Conflicts => self.view_conflicts(),
            Screen::FileHistory => self.view_file_history(),
            Screen::Devices => self.view_devices(),
            Screen::Status => self.view_status(),
            Screen::RecentFiles => self.view_recent_files(),
            Screen::Settings => self.view_settings(),
            Screen::NetworkHealth => self.view_network_health(),
            Screen::DaemonCheck | Screen::Setup => unreachable!(),
        };

        let panel = container(scrollable(
            container(content)
                .padding(iced::Padding {
                    top: 28.0,
                    right: 32.0,
                    bottom: 28.0,
                    left: 32.0,
                })
                .width(Length::Fill),
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(panel_style);

        container(row![sidebar, panel].height(Length::Fill))
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(SIDEBAR_BG)),
                ..Default::default()
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn nav_button(&self, icon: &str, label: &str, target: Screen) -> Element<'_, Message> {
        let is_active = self.screen == target;
        let label = label.to_string();
        let icon = icon.to_string();
        let icon_color = if is_active { Color::WHITE } else { TEXT_MUTED };
        let mut b = button(
            row![
                text(icon).size(18).color(icon_color).width(28),
                text(label).size(15),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill),
        )
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 10.0,
            right: 14.0,
            bottom: 10.0,
            left: 14.0,
        })
        .style(nav_btn_style(is_active));
        if !is_active {
            b = b.on_press(Message::Navigate(target));
        }
        b.into()
    }
}
