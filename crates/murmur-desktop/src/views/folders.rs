//! Folders list and folder detail views.

use iced::widget::{button, column, container, row, text, text_input};
use iced::{Background, Border, Color, Element, Length, Theme};

use crate::app::{App, Screen, SortField};
use crate::helpers::format_size;
use crate::message::Message;
use crate::style::*;

impl App {
    pub(crate) fn view_folders(&self) -> Element<'_, Message> {
        let mut col = column![
            row![
                text("Folders")
                    .size(24)
                    .color(Color::WHITE)
                    .width(Length::Fill),
                button(text("New Folder"))
                    .on_press(Message::CreateFolderFromPicker)
                    .style(primary_btn)
                    .padding(iced::Padding {
                        top: 6.0,
                        right: 16.0,
                        bottom: 6.0,
                        left: 16.0,
                    }),
            ]
            .align_y(iced::Alignment::Center)
        ]
        .spacing(16);

        // Subscribed folders
        let subscribed: Vec<_> = self.folders.iter().filter(|f| f.subscribed).collect();
        if !subscribed.is_empty() {
            let mut section = column![].spacing(6);
            for f in &subscribed {
                let path_text = f.local_path.as_deref().unwrap_or("(no local path)");
                let info = column![
                    text(&f.name).size(15).color(Color::WHITE),
                    text(path_text).size(11).color(TEXT_MUTED),
                ]
                .spacing(2)
                .width(Length::Fill);
                section = section.push(
                    button(
                        row![info, text(&f.sync_status).size(12).color(TEXT_SECONDARY),]
                            .spacing(8)
                            .align_y(iced::Alignment::Center),
                    )
                    .on_press(Message::SelectFolder((*f).clone()))
                    .width(Length::Fill)
                    .padding(12)
                    .style(secondary_btn),
                );
            }
            col = col.push(
                container(section)
                    .padding(16)
                    .width(Length::Fill)
                    .style(card_style),
            );
        }

        // Available on network
        let available: Vec<_> = self
            .network_folders
            .iter()
            .filter(|f| !f.subscribed)
            .collect();
        if !available.is_empty() {
            let mut section =
                column![text("Available on Network").size(16).color(TEXT_SECONDARY),].spacing(8);
            for f in available {
                section = section.push(
                    container(
                        row![
                            text(&f.name)
                                .size(14)
                                .color(Color::WHITE)
                                .width(Length::Fill),
                            text(format!("{} subs", f.subscriber_count))
                                .size(12)
                                .color(TEXT_MUTED),
                            button(text("Subscribe"))
                                .on_press(Message::SubscribeFolder(
                                    f.folder_id.clone(),
                                    f.name.clone()
                                ))
                                .style(primary_btn)
                                .padding(iced::Padding {
                                    top: 4.0,
                                    right: 12.0,
                                    bottom: 4.0,
                                    left: 12.0,
                                }),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                    )
                    .padding(8)
                    .width(Length::Fill)
                    .style(|_: &Theme| container::Style {
                        background: Some(Background::Color(Color {
                            r: 0.09,
                            g: 0.09,
                            b: 0.09,
                            a: 1.0,
                        })),
                        border: Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            col = col.push(
                container(section)
                    .padding(16)
                    .width(Length::Fill)
                    .style(card_style),
            );
        }

        if subscribed.is_empty() && self.network_folders.is_empty() {
            col = col.push(
                container(
                    text("No folders yet. Create one to get started.")
                        .size(14)
                        .color(TEXT_MUTED),
                )
                .padding(20)
                .width(Length::Fill)
                .style(card_style),
            );
        }
        col.into()
    }

    pub(crate) fn view_folder_detail(&self) -> Element<'_, Message> {
        let folder = match &self.selected_folder {
            Some(f) => f,
            None => return text("No folder selected.").color(TEXT_MUTED).into(),
        };
        let pause_label = if self.folder_paused {
            "Resume"
        } else {
            "Pause"
        };

        // Header: Back + name (or rename) + action buttons
        let is_renaming = self.renaming_folder_id.as_deref() == Some(&folder.folder_id);
        let name_el: Element<'_, Message> = if is_renaming {
            row![
                text_input("Folder name", &self.rename_input)
                    .on_input(Message::RenameInputChanged)
                    .on_submit(Message::SubmitRenameFolder)
                    .padding(6)
                    .width(Length::Fill),
                button(text("Save"))
                    .on_press(Message::SubmitRenameFolder)
                    .style(primary_btn)
                    .padding(6),
                button(text("Cancel"))
                    .on_press(Message::CancelRenameFolder)
                    .style(secondary_btn)
                    .padding(6),
            ]
            .spacing(4)
            .into()
        } else {
            row![
                text(&folder.name)
                    .size(22)
                    .color(Color::WHITE)
                    .width(Length::Fill),
                button(text("Rename").size(13))
                    .on_press(Message::StartRenameFolder(
                        folder.folder_id.clone(),
                        folder.name.clone()
                    ))
                    .style(secondary_btn)
                    .padding(6),
            ]
            .spacing(4)
            .into()
        };

        let header = row![
            button(text("Back").size(13))
                .on_press(Message::Navigate(Screen::Folders))
                .style(secondary_btn)
                .padding(iced::Padding {
                    top: 6.0,
                    right: 12.0,
                    bottom: 6.0,
                    left: 12.0,
                }),
            name_el,
            button(text(pause_label).size(13))
                .on_press(Message::ToggleFolderSync(folder.folder_id.clone()))
                .style(secondary_btn)
                .padding(6),
            button(text("Unsub").size(13))
                .on_press(Message::UnsubscribeFolder(folder.folder_id.clone()))
                .style(destructive_btn)
                .padding(6),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        // Info card
        let mut info_items = column![
            text(format!(
                "ID: {}  |  {} files  |  Mode: {}  |  {}",
                &folder.folder_id[..16],
                folder.file_count,
                folder.mode.as_deref().unwrap_or("--"),
                folder.local_path.as_deref().unwrap_or("(no local path)")
            ))
            .size(12)
            .color(TEXT_MUTED)
        ]
        .spacing(4);
        if !self.folder_subscribers.is_empty() {
            let sub_text = self
                .folder_subscribers
                .iter()
                .map(|s| format!("{} [{}]", s.device_name, s.mode))
                .collect::<Vec<_>>()
                .join(", ");
            info_items = info_items.push(
                text(format!("Subscribers: {sub_text}"))
                    .size(11)
                    .color(TEXT_MUTED),
            );
        }

        // Ignore patterns
        let fid = folder.folder_id.clone();
        let ignore_section = column![
            text("Ignore Patterns").size(14).color(TEXT_SECONDARY),
            row![
                text_input(".murmurignore patterns", &self.folder_ignore_patterns)
                    .on_input(Message::FolderIgnorePatternsChanged)
                    .padding(6)
                    .width(Length::Fill),
                button(text("Save"))
                    .on_press(Message::SaveIgnorePatterns(fid))
                    .style(primary_btn)
                    .padding(6),
            ]
            .spacing(4),
        ]
        .spacing(6);

        // Search & sort
        let search_sort = row![
            text_input("Search files...", &self.search_query)
                .on_input(Message::SearchQueryChanged)
                .padding(8)
                .width(Length::Fill),
            button(text("Name").size(13))
                .on_press(Message::SortBy(SortField::Name))
                .style(secondary_btn)
                .padding(6),
            button(text("Size").size(13))
                .on_press(Message::SortBy(SortField::Size))
                .style(secondary_btn)
                .padding(6),
            button(text("Type").size(13))
                .on_press(Message::SortBy(SortField::Type))
                .style(secondary_btn)
                .padding(6),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);

        // Files
        let mut files: Vec<_> = self
            .folder_files
            .iter()
            .filter(|f| {
                self.search_query.is_empty()
                    || f.path
                        .to_lowercase()
                        .contains(&self.search_query.to_lowercase())
            })
            .collect();
        match self.sort_field {
            SortField::Name => files.sort_by(|a, b| {
                if self.sort_ascending {
                    a.path.cmp(&b.path)
                } else {
                    b.path.cmp(&a.path)
                }
            }),
            SortField::Size => files.sort_by(|a, b| {
                if self.sort_ascending {
                    a.size.cmp(&b.size)
                } else {
                    b.size.cmp(&a.size)
                }
            }),
            SortField::Type => files.sort_by(|a, b| {
                let at = a.mime_type.as_deref().unwrap_or("");
                let bt = b.mime_type.as_deref().unwrap_or("");
                if self.sort_ascending {
                    at.cmp(bt)
                } else {
                    bt.cmp(at)
                }
            }),
        }

        let mut file_list = column![
            row![
                text("Path").size(12).color(TEXT_MUTED).width(Length::Fill),
                text("Size")
                    .size(12)
                    .color(TEXT_MUTED)
                    .width(Length::Fixed(80.0)),
                text("Type")
                    .size(12)
                    .color(TEXT_MUTED)
                    .width(Length::Fixed(100.0)),
                text("Actions")
                    .size(12)
                    .color(TEXT_MUTED)
                    .width(Length::Fixed(140.0)),
            ]
            .spacing(8),
        ]
        .spacing(4);
        if files.is_empty() {
            file_list = file_list.push(text("No files match.").size(14).color(TEXT_MUTED));
        } else {
            for file in files {
                file_list = file_list.push(
                    row![
                        text(&file.path)
                            .size(13)
                            .color(Color::WHITE)
                            .width(Length::Fill),
                        text(format_size(file.size))
                            .size(13)
                            .color(TEXT_SECONDARY)
                            .width(Length::Fixed(80.0)),
                        text(file.mime_type.as_deref().unwrap_or("--"))
                            .size(13)
                            .color(TEXT_SECONDARY)
                            .width(Length::Fixed(100.0)),
                        row![
                            button(text("History").size(12))
                                .on_press(Message::ViewFileHistory {
                                    folder_id: file.folder_id.clone(),
                                    path: file.path.clone()
                                })
                                .style(secondary_btn)
                                .padding(4),
                            button(text("Del").size(12))
                                .on_press(Message::DeleteFile {
                                    folder_id: file.folder_id.clone(),
                                    path: file.path.clone()
                                })
                                .style(destructive_btn)
                                .padding(4),
                        ]
                        .spacing(4)
                        .width(Length::Fixed(140.0)),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );
            }
        }

        column![
            header,
            container(info_items)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
            container(ignore_section)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
            search_sort,
            container(file_list)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        ]
        .spacing(12)
        .into()
    }
}
