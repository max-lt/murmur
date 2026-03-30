//! Status and recent files views.

use iced::widget::{column, container, text, text_input};
use iced::{Color, Element, Length};

use crate::app::App;
use crate::helpers::{format_uptime, truncate_hex};
use crate::message::Message;
use crate::style::*;

impl App {
    pub(crate) fn view_status(&self) -> Element<'_, Message> {
        let mut col = column![text("Status").size(24).color(Color::WHITE),].spacing(16);

        // Network info card
        let mut info = column![text("Network").size(16).color(TEXT_SECONDARY)].spacing(4);
        info = info.push(
            text(format!(
                "Network ID: {}",
                truncate_hex(&self.status_network_id, 16)
            ))
            .size(13)
            .color(Color::WHITE),
        );
        info = info.push(
            text(format!(
                "Device: {} ({})",
                self.status_device_name,
                truncate_hex(&self.status_device_id, 16)
            ))
            .size(13)
            .color(Color::WHITE),
        );
        info = info.push(
            text(format!(
                "Peers: {}  |  DAG: {}  |  Folders: {}  |  Conflicts: {}",
                self.status_peer_count,
                self.status_dag_entries,
                self.folders.len(),
                self.conflicts.len()
            ))
            .size(13)
            .color(Color::WHITE),
        );
        info = info.push(
            text(format!(
                "Uptime: {}",
                format_uptime(self.status_uptime_secs)
            ))
            .size(12)
            .color(TEXT_MUTED),
        );
        if let Some(ref e) = self.daemon_error {
            info = info.push(text(format!("Last error: {e}")).color(ERROR).size(12));
        }
        col = col.push(
            container(info)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        );

        // Event log card
        let mut log_section = column![text("Event Log").size(16).color(TEXT_SECONDARY)].spacing(4);
        if self.event_log.is_empty() {
            log_section = log_section.push(text("No events yet.").size(13).color(TEXT_MUTED));
        } else {
            for ev in self.event_log.iter().rev().take(50) {
                log_section = log_section.push(text(ev).size(12).color(TEXT_SECONDARY));
            }
        }
        col = col.push(
            container(log_section)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        );
        col.into()
    }

    pub(crate) fn view_recent_files(&self) -> Element<'_, Message> {
        let mut col = column![
            text("Recent Files").size(24).color(Color::WHITE),
            text_input("Search across all folders...", &self.search_query)
                .on_input(Message::SearchQueryChanged)
                .padding(10),
        ]
        .spacing(16);
        if self.search_query.is_empty() {
            col = col.push(
                container(
                    text("Type a search term to find files across all folders.")
                        .size(14)
                        .color(TEXT_MUTED),
                )
                .padding(20)
                .width(Length::Fill)
                .style(card_style),
            );
        } else {
            col = col.push(
                container(
                    text(format!("Searching for: {}", self.search_query))
                        .size(14)
                        .color(TEXT_SECONDARY),
                )
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
            );
        }
        col.into()
    }
}
