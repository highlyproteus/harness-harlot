use rust_mux_protocol::TerminalColor;

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
    pub cursor: u32,
    pub danger: u32,
    pub ansi: [u32; 16],
}

impl AppTheme {
    /// Original built-in dark theme. Its restrained contrast and crisp
    /// typography hierarchy were informed by observing modern native code
    /// editors, including Zed, without reusing any product palette or assets.
    pub const HARBOR_NIGHT: Self = Self {
        name: "Harbor Night",
        window: 0x17191e,
        sidebar: 0x181a1f,
        terminal: 0x15171b,
        surface: 0x1d2026,
        elevated: 0x242832,
        border: 0x2c313b,
        border_strong: 0x3a414e,
        foreground: 0xd9dce3,
        muted: 0x9299a6,
        dim: 0x626975,
        accent: 0x58a9ff,
        accent_soft: 0x253b55,
        selection: 0x294563,
        cursor: 0x70b7ff,
        danger: 0xef6b73,
        ansi: [
            0x1d2026, 0xef6b73, 0x91c77b, 0xe2b96b, 0x68a6ed, 0xc58be2, 0x61c2c0, 0xcbd0d8,
            0x69717f, 0xff838b, 0xa9dc90, 0xf3ce83, 0x82baff, 0xd7a2ef, 0x7bd7d3, 0xf2f4f7,
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
}
