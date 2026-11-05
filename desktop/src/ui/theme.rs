use gpui::{Font, FontWeight, Hsla, font, rgb};

pub const BACKGROUND: u32 = 0x0a0a0a;
pub const BAR: u32 = 0x111113;
pub const BORDER: u32 = 0x27272a;
pub const ACCENT: u32 = 0x10b981;
pub const TEXT: u32 = 0xd4d4d8;
pub const MUTED: u32 = 0x71717a;
pub const DANGER: u32 = 0xfb7185;
pub const SELECTION: u32 = 0x1e3a8a;
pub const HOVER: u32 = 0x3f3f46;
pub const CLOSE_HOVER: u32 = 0xc42b1c;
pub const TITLEBAR_HEIGHT: f32 = 34.0;
pub const RESIZE_STRIP_HEIGHT: f32 = 4.0;
pub const DRAG_AREA_MIN_WIDTH: f32 = 140.0;
pub const TAB_MIN_WIDTH: f32 = 64.0;
pub const FONT_SIZE: f32 = 13.0;
pub const LINE_HEIGHT: f32 = 17.0;
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;
pub const TEXT_PADDING: f32 = 8.0;

pub fn line_height(font_size: f32) -> f32 {
    (font_size * LINE_HEIGHT / FONT_SIZE).round()
}

pub fn font_family() -> &'static str {
    if cfg!(target_os = "windows") {
        "Cascadia Mono"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "monospace"
    }
}

pub fn terminal_font(bold: bool) -> Font {
    let base = font(font_family());
    if bold { Font { weight: FontWeight::BOLD, ..base } } else { base }
}

pub fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn parse_hex(value: &str) -> Option<Hsla> {
    let digits = value.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    u32::from_str_radix(digits, 16).ok().map(color)
}
