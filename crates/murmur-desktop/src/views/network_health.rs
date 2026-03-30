//! Network health / diagnostics screen view.

use iced::widget::{button, column, container, row, text};
use iced::{Color, Element, Length};

use crate::app::App;
use crate::helpers::{format_relative_time, format_size, truncate_hex};
use crate::message::Message;
use crate::style::*;

impl App {
    pub(crate) fn view_network_health(&self) -> Element<'_, Message> {
        let mut col = column![text("Network Health").size(24).color(Color::WHITE),].spacing(16);

        // Peers card
        let mut peers_section = column![text("Peers").size(16).color(TEXT_SECONDARY)].spacing(6);
        if self.peers.is_empty() {
            peers_section =
                peers_section.push(text("No peers connected.").size(14).color(TEXT_MUTED));
        } else {
            peers_section = peers_section.push(
                row![
                    text("Name").size(12).color(TEXT_MUTED).width(Length::Fill),
                    text("Connection")
                        .size(12)
                        .color(TEXT_MUTED)
                        .width(Length::Fixed(80.0)),
                    text("Last Seen")
                        .size(12)
                        .color(TEXT_MUTED)
                        .width(Length::Fixed(120.0)),
                    text("ID")
                        .size(12)
                        .color(TEXT_MUTED)
                        .width(Length::Fixed(140.0)),
                ]
                .spacing(8),
            );
            for p in &self.peers {
                peers_section = peers_section.push(
                    row![
                        text(&p.device_name)
                            .size(13)
                            .color(Color::WHITE)
                            .width(Length::Fill),
                        text(&p.connection_type)
                            .size(13)
                            .color(TEXT_SECONDARY)
                            .width(Length::Fixed(80.0)),
                        text(if p.last_seen_unix > 0 {
                            format_relative_time(p.last_seen_unix)
                        } else {
                            "Never".to_string()
                        })
                        .size(13)
                        .color(TEXT_SECONDARY)
                        .width(Length::Fixed(120.0)),
                        text(truncate_hex(&p.device_id, 16))
                            .size(11)
                            .color(TEXT_MUTED)
                            .width(Length::Fixed(140.0)),
                    ]
                    .spacing(8),
                );
            }
        }
        col = col.push(
            container(peers_section)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        );

        // Storage card
        let mut storage_section =
            column![text("Storage").size(16).color(TEXT_SECONDARY)].spacing(4);
        if let Some(ref stats) = self.storage_stats {
            storage_section = storage_section.push(
                text(format!(
                    "Total blobs: {} ({})",
                    stats.total_blob_count,
                    format_size(stats.total_blob_bytes)
                ))
                .size(14)
                .color(Color::WHITE),
            );
            storage_section = storage_section.push(
                text(format!(
                    "Orphaned blobs: {} ({})",
                    stats.orphaned_blob_count,
                    format_size(stats.orphaned_blob_bytes)
                ))
                .size(14)
                .color(Color::WHITE),
            );
            storage_section = storage_section.push(
                text(format!("DAG entries: {}", stats.dag_entry_count))
                    .size(14)
                    .color(Color::WHITE),
            );
        } else {
            storage_section = storage_section.push(text("Loading...").size(14).color(TEXT_MUTED));
        }
        col = col.push(
            container(storage_section)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        );

        // Connectivity card
        let mut conn_section =
            column![text("Connectivity").size(16).color(TEXT_SECONDARY)].spacing(6);
        conn_section = conn_section.push(
            button(text("Run Connectivity Check").size(13))
                .on_press(Message::RunConnectivityCheck)
                .style(primary_btn)
                .padding(8),
        );
        if let Some((reachable, latency)) = &self.connectivity_result {
            let status = if *reachable {
                "Reachable"
            } else {
                "Unreachable"
            };
            let latency_str = latency.map(|ms| format!(" ({ms} ms)")).unwrap_or_default();
            let color = if *reachable { ACCENT } else { ERROR };
            conn_section = conn_section.push(
                text(format!("Relay: {status}{latency_str}"))
                    .size(14)
                    .color(color),
            );
        }
        col = col.push(
            container(conn_section)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        );

        // Export
        col = col.push(
            button(text("Export Diagnostics").size(13))
                .on_press(Message::ExportDiagnostics)
                .style(secondary_btn)
                .padding(8),
        );

        col.into()
    }
}
