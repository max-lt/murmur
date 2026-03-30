//! Settings screen view.

use iced::widget::{button, column, container, row, text};
use iced::{Color, Element, Length, Theme};

use crate::app::App;
use crate::helpers::{format_size, truncate_hex};
use crate::message::Message;
use crate::style::*;

impl App {
    pub(crate) fn view_settings(&self) -> Element<'_, Message> {
        let mut col = column![text("Settings").size(24).color(Color::WHITE),].spacing(16);

        // Device card
        let device_card = container(
            column![
                text("Device").size(16).color(TEXT_SECONDARY),
                text(format!("Name: {}", self.status_device_name))
                    .size(14)
                    .color(Color::WHITE),
                text(format!("ID: {}", truncate_hex(&self.status_device_id, 32)))
                    .size(11)
                    .color(TEXT_MUTED),
            ]
            .spacing(6),
        )
        .padding(14)
        .width(Length::Fill)
        .style(card_style);
        col = col.push(device_card);

        // Network card
        let network_card = container(
            column![
                text("Network").size(16).color(TEXT_SECONDARY),
                row![
                    text("Auto-approve new devices:")
                        .size(14)
                        .color(Color::WHITE)
                        .width(Length::Fill),
                    button(text(if self.cfg_auto_approve { "ON" } else { "OFF" }).size(13))
                        .on_press(Message::ToggleAutoApprove)
                        .style(if self.cfg_auto_approve {
                            primary_btn as fn(&Theme, button::Status) -> button::Style
                        } else {
                            secondary_btn
                        })
                        .padding(6),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
                row![
                    text("mDNS LAN discovery:")
                        .size(14)
                        .color(Color::WHITE)
                        .width(Length::Fill),
                    button(text(if self.cfg_mdns { "ON" } else { "OFF" }).size(13))
                        .on_press(Message::ToggleMdns)
                        .style(if self.cfg_mdns {
                            primary_btn as fn(&Theme, button::Status) -> button::Style
                        } else {
                            secondary_btn
                        })
                        .padding(6),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            ]
            .spacing(8),
        )
        .padding(14)
        .width(Length::Fill)
        .style(card_style);
        col = col.push(network_card);

        // Bandwidth card
        let up_label = if self.cfg_upload_throttle == 0 {
            "Unlimited".to_string()
        } else {
            format!("{}/s", format_size(self.cfg_upload_throttle))
        };
        let down_label = if self.cfg_download_throttle == 0 {
            "Unlimited".to_string()
        } else {
            format!("{}/s", format_size(self.cfg_download_throttle))
        };
        let bw_card = container(
            column![
                text("Bandwidth").size(16).color(TEXT_SECONDARY),
                text(format!("Upload: {up_label}  |  Download: {down_label}"))
                    .size(14)
                    .color(Color::WHITE),
                row![
                    button(text("Unlimited").size(13))
                        .on_press(Message::SetThrottle(0, 0))
                        .style(secondary_btn)
                        .padding(6),
                    button(text("1 MB/s").size(13))
                        .on_press(Message::SetThrottle(1_048_576, 1_048_576))
                        .style(secondary_btn)
                        .padding(6),
                    button(text("5 MB/s").size(13))
                        .on_press(Message::SetThrottle(5_242_880, 5_242_880))
                        .style(secondary_btn)
                        .padding(6),
                    button(text("10 MB/s").size(13))
                        .on_press(Message::SetThrottle(10_485_760, 10_485_760))
                        .style(secondary_btn)
                        .padding(6),
                ]
                .spacing(4),
            ]
            .spacing(8),
        )
        .padding(14)
        .width(Length::Fill)
        .style(card_style);
        col = col.push(bw_card);

        // Storage card
        let mut storage_items = column![text("Storage").size(16).color(TEXT_SECONDARY)].spacing(4);
        if let Some(ref stats) = self.storage_stats {
            storage_items = storage_items.push(
                text(format!(
                    "Blobs: {} ({})",
                    stats.total_blob_count,
                    format_size(stats.total_blob_bytes)
                ))
                .size(14)
                .color(Color::WHITE),
            );
            storage_items = storage_items.push(
                text(format!(
                    "Orphaned: {} ({})",
                    stats.orphaned_blob_count,
                    format_size(stats.orphaned_blob_bytes)
                ))
                .size(14)
                .color(Color::WHITE),
            );
            storage_items = storage_items.push(
                text(format!("DAG entries: {}", stats.dag_entry_count))
                    .size(14)
                    .color(Color::WHITE),
            );
        }
        storage_items = storage_items.push(
            button(text("Reclaim Orphaned Blobs").size(13))
                .on_press(Message::ReclaimOrphanedBlobs)
                .style(secondary_btn)
                .padding(6),
        );
        if let Some(ref toast) = self.settings_toast {
            storage_items = storage_items.push(text(toast).color(ACCENT).size(12));
        }
        col = col.push(
            container(storage_items)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        );

        // Sync card
        let sync_card = container(
            column![
                text("Sync").size(16).color(TEXT_SECONDARY),
                row![
                    text("Global sync:")
                        .size(14)
                        .color(Color::WHITE)
                        .width(Length::Fill),
                    button(
                        text(if self.sync_paused {
                            "PAUSED - Resume"
                        } else {
                            "Active - Pause"
                        })
                        .size(13)
                    )
                    .on_press(Message::ToggleGlobalSync)
                    .style(secondary_btn)
                    .padding(6),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            ]
            .spacing(6),
        )
        .padding(14)
        .width(Length::Fill)
        .style(card_style);
        col = col.push(sync_card);

        // Danger zone card
        let mut danger_items = column![text("Danger Zone").size(16).color(ERROR)].spacing(8);
        if self.leave_network_confirm {
            danger_items = danger_items.push(
                text("This will delete all Murmur data (config, keys, DAG, blobs). Files in synced folders on disk will NOT be deleted. The daemon will shut down.")
                    .size(13)
                    .color(WARNING),
            );
            danger_items = danger_items.push(
                row![
                    button(text("Yes, leave and wipe").size(13))
                        .on_press(Message::LeaveNetworkConfirm)
                        .style(destructive_btn)
                        .padding(8),
                    button(text("Cancel").size(13))
                        .on_press(Message::LeaveNetworkCancel)
                        .style(secondary_btn)
                        .padding(8),
                ]
                .spacing(8),
            );
        } else {
            danger_items = danger_items.push(
                button(text("Leave Network & Wipe Data").size(13))
                    .on_press(Message::LeaveNetworkStart)
                    .style(destructive_btn)
                    .padding(8),
            );
        }
        col = col.push(
            container(danger_items)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        );

        col.into()
    }
}
