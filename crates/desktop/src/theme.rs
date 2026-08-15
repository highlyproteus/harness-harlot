use hh_protocol::TerminalColor;

/// Complete visual contract for the desktop shell and terminal palette.
/// Additional built-in or user themes can implement the same structure
/// without changing terminal/session state.
#[derive(Clone, Copy, Debug)]
pub struct AppTheme {
    pub name: &'static str,
    pub window: u32,
    pub sidebar: u32,
    pub terminal: u32,
    pub surface: u32,
    pub elevated: u32,
    pub border: u32,
    pub border_strong: u32,
    pub foreground: u32,
    pub muted: u32,
    pub dim: u32,
    pub accent: u32,
    pub accent_soft: u32,
    pub selection: u32,
    pub danger: u32,
    pub ansi: [u32; 16],
}

impl AppTheme {
    /// Original built-in dark theme. Its restrained contrast and crisp
    /// typography hierarchy were informed by observing modern native code
    /// editors, including Zed, without reusing any product palette or assets.
    pub const HARBOR_NIGHT: Self = Self {
        name: "Harbor Night",
        window: 0x14161b,
        sidebar: 0x15171c,
        terminal: 0x101217,
        surface: 0x191c22,
        elevated: 0x22262f,
        border: 0x292e37,
        border_strong: 0x3b424f,
        foreground: 0xe2e5eb,
        muted: 0x9aa2af,
        dim: 0x68717f,
        accent: 0x62adff,
        accent_soft: 0x243b55,
        selection: 0x294766,
        danger: 0xef6b73,
        ansi: [
            0x20242b, 0xef717a, 0x95cc7f, 0xe4bd72, 0x6faaf2, 0xc990e5, 0x67c8c6, 0xd2d7df,
            0x6e7785, 0xff858d, 0xaddf95, 0xf4d087, 0x87bdff, 0xdaa6f0, 0x80d9d6, 0xf5f7fa,
        ],
    };

    pub fn terminal_color(self, color: TerminalColor, bold: bool, dim: bool) -> u32 {
        let color = match color {
            TerminalColor::DefaultForeground => self.foreground,
            TerminalColor::DefaultBackground => self.terminal,
            TerminalColor::Ansi { index } => {
                let mut index = usize::from(index.min(15));
                if bold && index < 8 {
                    index += 8;
                }
                self.ansi[index]
            }
            TerminalColor::Indexed { index } if index < 16 => self.ansi[usize::from(index)],
            TerminalColor::Indexed { index } if index < 232 => {
                let value = index - 16;
                let red = value / 36;
                let green = (value % 36) / 6;
                let blue = value % 6;
                rgb_channels(cube(red), cube(green), cube(blue))
            }
            TerminalColor::Indexed { index } => {
                let gray = 8_u8.saturating_add((index - 232).saturating_mul(10));
                rgb_channels(gray, gray, gray)
            }
            TerminalColor::Rgb { red, green, blue } => rgb_channels(red, green, blue),
        };
        if dim {
            blend(color, self.terminal, 58)
        } else {
            color
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltInTheme {
    HarborNight,
}

impl BuiltInTheme {
    pub const fn theme(self) -> AppTheme {
        match self {
            Self::HarborNight => AppTheme::HARBOR_NIGHT,
        }
    }
}

const fn cube(value: u8) -> u8 {
    if value == 0 { 0 } else { 55 + value * 40 }
}

const fn rgb_channels(red: u8, green: u8, blue: u8) -> u32 {
    ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
}

fn blend(foreground: u32, background: u32, foreground_percent: u32) -> u32 {
    let channel = |shift: u32| {
        let foreground = (foreground >> shift) & 0xff_u32;
        let background = (background >> shift) & 0xff_u32;
        (foreground * foreground_percent + background * (100 - foreground_percent) + 50) / 100
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_bold_ansi_and_truecolor_without_mutating_terminal_state() {
        let theme = AppTheme::HARBOR_NIGHT;
        assert_eq!(
            theme.terminal_color(TerminalColor::Ansi { index: 1 }, true, false),
            theme.ansi[9]
        );
        assert_eq!(
            theme.terminal_color(
                TerminalColor::Rgb {
                    red: 12,
                    green: 34,
                    blue: 56
                },
                false,
                false
            ),
            0x0c2238
        );
    }

    #[test]
    fn built_in_themes_are_named_and_selectable_without_session_state() {
        assert_eq!(BuiltInTheme::HarborNight.theme().name, "Harbor Night");
    }

    #[test]
    fn hermes_256_color_indices_follow_the_xterm_palette() {
        let theme = AppTheme::HARBOR_NIGHT;
        assert_eq!(
            theme.terminal_color(TerminalColor::DefaultForeground, false, false),
            theme.foreground
        );
        assert_eq!(
            theme.terminal_color(TerminalColor::Indexed { index: 136 }, false, false),
            0xaf8700
        );
        assert_eq!(
            theme.terminal_color(TerminalColor::Indexed { index: 220 }, false, false),
            0xffd700
        );
        assert_eq!(
            theme.terminal_color(TerminalColor::Indexed { index: 234 }, false, false),
            0x1c1c1c
        );
    }
}
