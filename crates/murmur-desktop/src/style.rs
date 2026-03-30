//! Color palette and widget style functions for the Murmur desktop UI.

use iced::widget::{button, container};
use iced::{Background, Border, Color, Theme};

// ---------------------------------------------------------------------------
// Color palette — inspired by Liana's dark theme
// ---------------------------------------------------------------------------

pub const SIDEBAR_BG: Color = Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};
pub const PANEL_BG: Color = Color {
    r: 0.078,
    g: 0.078,
    b: 0.078,
    a: 1.0,
};
pub const CARD_BG: Color = Color {
    r: 0.125,
    g: 0.125,
    b: 0.125,
    a: 1.0,
};
pub const ACCENT: Color = Color {
    r: 0.0,
    g: 1.0,
    b: 0.4,
    a: 1.0,
};
pub const TEXT_SECONDARY: Color = Color {
    r: 0.8,
    g: 0.8,
    b: 0.8,
    a: 1.0,
};
pub const TEXT_MUTED: Color = Color {
    r: 0.443,
    g: 0.443,
    b: 0.443,
    a: 1.0,
};
pub const ERROR: Color = Color {
    r: 0.886,
    g: 0.306,
    b: 0.106,
    a: 1.0,
};
pub const WARNING: Color = Color {
    r: 1.0,
    g: 0.655,
    b: 0.0,
    a: 1.0,
};
pub const HOVER_BG: Color = Color {
    r: 0.10,
    g: 0.10,
    b: 0.10,
    a: 1.0,
};

// ---------------------------------------------------------------------------
// Button / container style helpers
// ---------------------------------------------------------------------------

/// Build a button style with the given background and text color.
pub fn btn(bg: Option<Color>, text_color: Color) -> button::Style {
    button::Style {
        background: bg.map(Background::Color),
        text_color,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    }
}

pub fn primary_btn(_theme: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Hovered => btn(
            Some(Color {
                r: 0.0,
                g: 0.85,
                b: 0.34,
                a: 1.0,
            }),
            Color::BLACK,
        ),
        button::Status::Pressed => btn(
            Some(Color {
                r: 0.0,
                g: 0.7,
                b: 0.28,
                a: 1.0,
            }),
            Color::BLACK,
        ),
        button::Status::Disabled => btn(
            Some(Color {
                r: 0.2,
                g: 0.2,
                b: 0.2,
                a: 1.0,
            }),
            TEXT_MUTED,
        ),
        _ => btn(Some(ACCENT), Color::BLACK),
    }
}

pub fn secondary_btn(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Color {
            r: 0.18,
            g: 0.18,
            b: 0.18,
            a: 1.0,
        },
        button::Status::Pressed => Color {
            r: 0.15,
            g: 0.15,
            b: 0.15,
            a: 1.0,
        },
        _ => CARD_BG,
    };
    btn(Some(bg), TEXT_SECONDARY)
}

pub fn destructive_btn(_theme: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Hovered => btn(Some(ERROR), Color::WHITE),
        button::Status::Pressed => btn(
            Some(Color {
                r: 0.7,
                g: 0.2,
                b: 0.1,
                a: 1.0,
            }),
            Color::WHITE,
        ),
        _ => btn(Some(CARD_BG), ERROR),
    }
}

pub fn nav_btn_style(is_active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme: &Theme, status: button::Status| {
        if is_active {
            button::Style {
                background: Some(Background::Color(HOVER_BG)),
                text_color: Color::WHITE,
                border: Border {
                    color: ACCENT,
                    width: 2.0,
                    radius: iced::border::Radius {
                        top_left: 0.0,
                        top_right: 8.0,
                        bottom_right: 8.0,
                        bottom_left: 0.0,
                    },
                },
                ..Default::default()
            }
        } else {
            match status {
                button::Status::Hovered => btn(Some(HOVER_BG), Color::WHITE),
                button::Status::Pressed => btn(Some(HOVER_BG), Color::WHITE),
                _ => btn(None, TEXT_MUTED),
            }
        }
    }
}

pub fn sidebar_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SIDEBAR_BG)),
        ..Default::default()
    }
}

pub fn panel_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PANEL_BG)),
        border: Border {
            radius: iced::border::Radius {
                top_left: 20.0,
                top_right: 0.0,
                bottom_right: 0.0,
                bottom_left: 0.0,
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn card_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(CARD_BG)),
        border: Border {
            radius: 12.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}
