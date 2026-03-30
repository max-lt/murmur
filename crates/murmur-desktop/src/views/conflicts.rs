//! Conflicts list and file history views.

use iced::widget::{button, column, container, row, text};
use iced::{Color, Element, Length};

use crate::app::{App, Screen};
use crate::helpers::{format_size, truncate_hex};
use crate::message::Message;
use crate::style::*;

impl App {
    pub(crate) fn view_conflicts(&self) -> Element<'_, Message> {
        let mut col = column![text("Conflicts").size(24).color(Color::WHITE),].spacing(16);
        if self.conflicts.is_empty() {
            col = col.push(
                container(text("No active conflicts.").size(14).color(TEXT_MUTED))
                    .padding(20)
                    .width(Length::Fill)
                    .style(card_style),
            );
        } else {
            // Bulk actions per folder
            let mut folder_ids: Vec<String> =
                self.conflicts.iter().map(|c| c.folder_id.clone()).collect();
            folder_ids.dedup();
            for fid in &folder_ids {
                let fname = self
                    .conflicts
                    .iter()
                    .find(|c| c.folder_id == *fid)
                    .map(|c| c.folder_name.as_str())
                    .unwrap_or("unknown");
                col = col.push(
                    row![
                        text(format!("{fname}:"))
                            .size(16)
                            .color(Color::WHITE)
                            .width(Length::Fill),
                        button(text("Keep All Newest").size(13))
                            .on_press(Message::BulkResolve {
                                folder_id: fid.clone(),
                                strategy: "keep_newest".to_string()
                            })
                            .style(primary_btn)
                            .padding(6),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );
            }
            // Individual conflicts
            for conflict in &self.conflicts {
                let mut card_content = column![
                    text(format!("{} / {}", conflict.folder_name, conflict.path))
                        .size(14)
                        .color(Color::WHITE),
                ]
                .spacing(6);
                for v in &conflict.versions {
                    card_content = card_content.push(
                        row![
                            text(format!(
                                "{} ({})  HLC: {}",
                                v.device_name,
                                truncate_hex(&v.blob_hash, 16),
                                v.hlc
                            ))
                            .size(12)
                            .color(TEXT_SECONDARY)
                            .width(Length::Fill),
                            button(text("Keep").size(12))
                                .on_press(Message::ResolveConflict {
                                    folder_id: conflict.folder_id.clone(),
                                    path: conflict.path.clone(),
                                    chosen_hash: v.blob_hash.clone()
                                })
                                .style(primary_btn)
                                .padding(4),
                        ]
                        .spacing(4)
                        .align_y(iced::Alignment::Center),
                    );
                }
                card_content = card_content.push(
                    button(text("Keep Both (dismiss)").size(12))
                        .on_press(Message::DismissConflict {
                            folder_id: conflict.folder_id.clone(),
                            path: conflict.path.clone(),
                        })
                        .style(secondary_btn)
                        .padding(6),
                );
                col = col.push(
                    container(card_content)
                        .padding(14)
                        .width(Length::Fill)
                        .style(card_style),
                );
            }
        }
        col.into()
    }

    pub(crate) fn view_file_history(&self) -> Element<'_, Message> {
        let mut content = column![
            text(format!("History: {}", self.history_path))
                .size(14)
                .color(Color::WHITE)
        ]
        .spacing(8);
        if self.history_versions.is_empty() {
            content = content.push(text("No versions found.").size(14).color(TEXT_MUTED));
        } else {
            for v in &self.history_versions {
                content = content.push(
                    row![
                        text(format!(
                            "{}  by {}  HLC: {}  ({})",
                            truncate_hex(&v.blob_hash, 16),
                            v.device_name,
                            v.modified_at,
                            format_size(v.size)
                        ))
                        .size(13)
                        .color(TEXT_SECONDARY)
                        .width(Length::Fill),
                        button(text("Restore").size(12))
                            .on_press(Message::RestoreVersion {
                                folder_id: self.history_folder_id.clone(),
                                path: self.history_path.clone(),
                                blob_hash: v.blob_hash.clone()
                            })
                            .style(primary_btn)
                            .padding(6),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );
            }
        }

        column![
            row![
                button(text("Back").size(13))
                    .on_press(Message::Navigate(Screen::Folders))
                    .style(secondary_btn)
                    .padding(iced::Padding {
                        top: 6.0,
                        right: 12.0,
                        bottom: 6.0,
                        left: 12.0,
                    }),
                text("File History").size(24).color(Color::WHITE),
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center),
            container(content)
                .padding(16)
                .width(Length::Fill)
                .style(card_style),
        ]
        .spacing(16)
        .into()
    }
}
