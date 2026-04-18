//! Activity feed view (M31).
//!
//! Chronological stream of engine events the desktop has observed this
//! session. Backed by `App::activity` (a ring buffer of last 200 entries)
//! and fed by the existing `SubscribeEvents` stream — the same source that
//! drives the "tray" notifications.

use iced::widget::{column, container, row, text};
use iced::{Color, Element, Length};

use crate::app::App;
use crate::message::Message;
use crate::style::*;

impl App {
    pub(crate) fn view_activity(&self) -> Element<'_, Message> {
        let header = text("Activity")
            .size(24)
            .color(Color::WHITE)
            .width(Length::Fill);

        let mut feed = column![
            row![
                text("When")
                    .size(12)
                    .color(TEXT_MUTED)
                    .width(Length::Fixed(80.0)),
                text("Event")
                    .size(12)
                    .color(TEXT_MUTED)
                    .width(Length::Fixed(220.0)),
                text("Details").size(12).color(TEXT_MUTED),
            ]
            .spacing(8),
        ]
        .spacing(4);

        if self.activity.is_empty() {
            feed = feed.push(
                text("No activity yet. Events will appear here as they happen.")
                    .size(13)
                    .color(TEXT_MUTED),
            );
        } else {
            for entry in self.activity.iter() {
                feed = feed.push(
                    row![
                        text(&entry.when)
                            .size(12)
                            .color(TEXT_SECONDARY)
                            .width(Length::Fixed(80.0)),
                        text(&entry.event_type)
                            .size(12)
                            .color(Color::WHITE)
                            .width(Length::Fixed(220.0)),
                        text(&entry.summary).size(12).color(TEXT_SECONDARY),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );
            }
        }

        column![
            header,
            container(feed)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        ]
        .spacing(12)
        .into()
    }
}
