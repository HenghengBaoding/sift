//! Catppuccin Macchiato 配色方案与通用组件样式。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

// ------------------------------------------------------------ Catppuccin Macchiato 色板

/// 正文
pub const TEXT: Color = Color::Rgb(0xca, 0xd3, 0xf5);
/// 次要标题
pub const SUBTEXT0: Color = Color::Rgb(0xa5, 0xad, 0xcb);
/// 弱化提示文字
pub const OVERLAY1: Color = Color::Rgb(0x80, 0x87, 0xa2);
/// 选中行背景
pub const SURFACE1: Color = Color::Rgb(0x49, 0x4d, 0x64);
/// 面板边框
pub const SURFACE2: Color = Color::Rgb(0x5b, 0x60, 0x78);
/// 最深背景（chip 按钮上的反色文字用）
pub const BASE: Color = Color::Rgb(0x24, 0x27, 0x3a);

pub const MAUVE: Color = Color::Rgb(0xc6, 0xa0, 0xf6);
pub const BLUE: Color = Color::Rgb(0x8a, 0xad, 0xf4);
pub const SKY: Color = Color::Rgb(0x91, 0xd7, 0xe3);
pub const GREEN: Color = Color::Rgb(0xa6, 0xda, 0x95);
pub const YELLOW: Color = Color::Rgb(0xee, 0xd4, 0x9f);
pub const PEACH: Color = Color::Rgb(0xf5, 0xa9, 0x7f);
pub const RED: Color = Color::Rgb(0xed, 0x87, 0x96);

// ------------------------------------------------------------ 语义色

/// 未聚焦面板边框
pub const BORDER: Color = SURFACE2;
/// 聚焦组件边框
pub const BORDER_FOCUS: Color = BLUE;
/// 弹窗边框
pub const BORDER_POPUP: Color = MAUVE;
/// 选中行背景
pub const SELECT_BG: Color = SURFACE1;

// ------------------------------------------------------------ 组件样式

/// 圆角面板
pub fn panel(title: Line<'static>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title(title)
}

/// 聚焦中的圆角面板（高亮边框）
pub fn panel_focused(title: Line<'static>) -> Block<'static> {
    panel(title).border_style(Style::default().fg(BORDER_FOCUS))
}

/// 弹窗面板
pub fn popup(title: Line<'static>) -> Block<'static> {
    panel(title).border_style(Style::default().fg(BORDER_POPUP))
}

/// 普通小标题
pub fn t(title: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(title.into(), title_style()))
}

pub fn title_style() -> Style {
    Style::default().fg(SUBTEXT0)
}

/// 占位提示（预览为空等）
pub fn placeholder() -> Style {
    Style::default().fg(OVERLAY1).add_modifier(Modifier::ITALIC)
}

/// 列表选中行
pub fn selected() -> Style {
    Style::default()
        .fg(TEXT)
        .bg(SELECT_BG)
        .add_modifier(Modifier::BOLD)
}
