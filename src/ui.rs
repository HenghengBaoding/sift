//! 界面渲染（Catppuccin Macchiato 配色 + 圆角边框）：
//! ╭──────────────────────────────────────────────╮
//! │ 搜索输入框                                   │
//! ├───────────────┬──────────────────────────────┤
//! │ 文件列表       │ 文件内容预览（bat）           │
//! │               ├──────────────────────────────┤
//! │               │ 文件完整路径（自动换行）       │
//! ├───────────────┴──────────────────────────────┤
//! │ 快捷键提示                                   │
//! ╰──────────────────────────────────────────────╯

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, PopupKind, MAX_RESULTS};
use crate::preview::Preview;
use crate::search::{decode_escapes, sanitize_display, SearchMode, SearchResultItem};
use crate::theme;

// 面板标题图标（Nerd Font）：图标颜色始终与紧随其后的标题文字一致。
const ICON_FILE_LIST: &str = "\u{F022}"; // 文件列表（nf-fa-rectangle_list）
const ICON_FILE_NAME: &str = "\u{F0C7C}"; // 文件名（nf-md-file_outline）
const ICON_FILE_CONTENT: &str = "\u{F13B8}"; // 文件内容（nf-md-file_document_outline）
const ICON_PREVIEW: &str = "\u{F0208}"; // 预览（nf-md-eye，待确认）
const ICON_FULL_PATH: &str = "\u{F0216}"; // 完整路径

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // 顶部搜索输入框：Ctrl+H 展开/折叠。
    // - 展开：查询过长时按宽度硬折行，高度最高占 1/3 屏高；超出可视区时滚动查看（带滚动条）。
    // - 折叠：单行高度，超出宽度以省略号截断。
    // 换行符（Shift+Enter 插入的真实换行 / 输入的 \n 转义）解析为真正的换行显示。
    let input_inner_w = area.width.saturating_sub(2).max(1) as usize;
    // 记录输入框内容区宽度：↑/↓ 按视觉折行上下移动光标时需要
    app.input_inner_width = input_inner_w;
    let input_h = if app.input_expanded {
        let input_wrapped = wrap_input(&app.input, input_inner_w, true);
        let input_max_content = (area.height / 3).max(1) as usize;
        input_wrapped.len().clamp(1, input_max_content) as u16 + 2
    } else {
        3 // 折叠态：单行内容 + 上下边框
    };

    // 底部快捷键提示栏：一行放不下时自动折行，高度按折行数动态计算
    let footer_lines = build_footer_lines(area.width);
    let footer_h = (footer_lines.len() as u16).clamp(1, 4) + 2;

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(input_h),  // 搜索输入框（可折行）
            Constraint::Min(3),           // 中部
            Constraint::Length(footer_h), // 快捷键提示（可折行）
        ])
        .split(area);

    draw_input(f, app, root[0]);

    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(root[1]);

    draw_list(f, app, middle[0]);

    // 路径框高度按折行数动态计算；文件名可能含控制字符，清洗后再展示/测量
    let full_path = app
        .selected_item()
        .map(|i| sanitize_display(&i.path.display().to_string()))
        .unwrap_or_default();
    let inner_w = middle[1].width.saturating_sub(2).max(1) as usize;
    let path_lines = wrap_lines(&full_path, inner_w).clamp(1, 6) as u16;

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(path_lines + 2)])
        .split(middle[1]);

    draw_preview(f, app, right[0]);
    draw_path(f, right[1], &full_path);
    draw_footer(f, root[2], footer_lines);

    // 编辑弹窗（路径 / 忽略目录 / 大小上限）
    if app.popup.is_some() {
        draw_popup(f, app);
    }

    // 状态提示弹框（toast，最顶层）：取消搜索 / 编辑器 / 配置错误等临时消息，
    // 居中展示、到期自动消失（取代旧的底栏内嵌提示，底栏太窄放不下）
    if app.status.is_some() {
        draw_toast(f, app);
    }
}

fn draw_input(f: &mut Frame, app: &mut App, area: Rect) {
    let mode_color = match app.mode {
        SearchMode::FileName => theme::SKY,
        SearchMode::Content => theme::MAUVE,
    };
    // 输入内容有未搜索的改动时，右上角放醒目的 "Enter 搜索" chip
    let dirty = app.input_dirty();
    let chip_w = if dirty { 12 } else { 0 }; // " Enter 搜索 " 11 列 + 间隔
    // 展开/折叠状态标识（置于标题最左，清晰指示当前状态）：
    // 展开 = nf-oct-unfold（U+F42D，绿），收起 = nf-oct-fold（U+F48C，橙）
    // 注：为 Nerd Font 图标，终端需使用 Nerd Font 字体才能正常显示
    let (expand_icon, expand_word, expand_color) = if app.input_expanded {
        ("\u{F42D}", "展开", theme::GREEN)
    } else {
        ("\u{F48C}", "收起", theme::PEACH)
    };
    let expand_text = format!(" {expand_icon} {expand_word}");
    // 模式图标（Nerd Font）：文件名 = nf-md-file_outline（U+F0C7C），文件内容 = nf-md-file_document_outline（U+F13B8）
    // 颜色与各自模式文字一致
    let mode_icon = match app.mode {
        SearchMode::FileName => "\u{F0C7C}",
        SearchMode::Content => "\u{F13B8}",
    };
    // 路径图标（Nerd Font）：nf-md-folder_outline（U+F0968），颜色与路径文字一致
    const PATH_ICON: &str = "\u{F0968}";
    let titles_w = area.width.saturating_sub(2) as usize;
    let fixed_w = UnicodeWidthStr::width(expand_text.as_str())
        + UnicodeWidthStr::width(" ")
        + UnicodeWidthStr::width(mode_icon)
        + UnicodeWidthStr::width(" ")
        + UnicodeWidthStr::width(app.mode.label())
        + UnicodeWidthStr::width(" ")
        + UnicodeWidthStr::width(PATH_ICON)
        + UnicodeWidthStr::width(" ")
        + UnicodeWidthStr::width(" ");
    // 路径按剩余宽度截断，保证不与右侧 chip 重叠
    let path_budget = titles_w.saturating_sub(fixed_w + chip_w + 1).max(4);
    let path = truncate_width(&sanitize_display(&app.current_path_display()), path_budget);
    let title = Line::from(vec![
        Span::styled(
            expand_text,
            Style::default()
                .fg(expand_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            mode_icon,
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            app.mode.label(),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            PATH_ICON,
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            path,
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    // 弹窗打开时，焦点在弹窗上，输入框边框降回普通色
    let mut block = if app.popup.is_some() {
        theme::panel(title)
    } else {
        theme::panel_focused(title)
    };
    if dirty {
        block = block.title_top(
            Line::from(Span::styled(
                " Enter 搜索 ",
                Style::default()
                    .fg(theme::BASE)
                    .bg(theme::PEACH)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Right),
        );
    }
    // 内容搜索模式：底边框右侧常驻多行语法提示（低调）
    if app.mode == SearchMode::Content {
        block = block.title_bottom(
            Line::from(Span::styled(
                " Shift+Enter 或 \\n 换行 \\\\ 反斜杠 ",
                Style::default().fg(theme::OVERLAY1),
            ))
            .alignment(Alignment::Right),
        );
    }
    // 内容区尺寸（去掉左右/上下边框）
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let visible_h = area.height.saturating_sub(2).max(1) as usize;

    if app.input_expanded {
        // 展开态：按宽度硬折行（字符级、宽字符不拆分，与 draw() 计算高度的折行一致），
        // 并把换行符（真实换行 / \n 转义）解析为换行。内容超出可视区时滚动使光标行可见。
        let content = wrap_input(&app.input, inner_w, true);
        let (cur_line, cur_col) = input_cursor_line_col(&app.input, app.cursor, inner_w, true);
        let scroll = if content.len() > visible_h {
            let mut s = app.input_scroll;
            if cur_line < s {
                s = cur_line;
            } else if cur_line >= s + visible_h {
                s = cur_line + 1 - visible_h;
            }
            s.min(content.len() - visible_h)
        } else {
            0
        };
        app.input_scroll = scroll;

        let lines: Vec<Line> = content
            .iter()
            .skip(scroll)
            .take(visible_h)
            .map(|l| Line::from(Span::raw(l.clone())))
            .collect();
        let input = Paragraph::new(lines)
            .block(block)
            .style(Style::default().fg(theme::TEXT));
        f.render_widget(input, area);

        // 内容超出可视区：在右侧渲染滚动条，指示当前视窗在全部内容中的位置
        if content.len() > visible_h {
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: visible_h as u16,
            };
            let mut state = ScrollbarState::default()
                .content_length(content.len())
                .viewport_content_length(visible_h)
                .position(scroll);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                inner,
                &mut state,
            );
        }

        // 光标（支持宽字符、折行与换行符）：行 = 光标折行 - 滚动偏移，列 = 折行内显示宽度
        if app.popup.is_none() {
            let line = cur_line
                .saturating_sub(scroll)
                .min(visible_h.saturating_sub(1));
            let x = area.x + 1 + cur_col as u16;
            let max_x = area.x + area.width.saturating_sub(2);
            f.set_cursor_position((x.min(max_x), area.y + 1 + line as u16));
        }
    } else {
        // 折叠态：单行展示，换行以 ⏎ 标记，超出宽度以省略号截断
        let (disp, col) = collapsed_input_display(&app.input, app.cursor, inner_w);
        let input = Paragraph::new(Line::from(Span::raw(disp)))
            .block(block)
            .style(Style::default().fg(theme::TEXT));
        f.render_widget(input, area);

        if app.popup.is_none() {
            let x = area.x + 1 + col as u16;
            let max_x = area.x + area.width.saturating_sub(2);
            f.set_cursor_position((x.min(max_x), area.y + 1));
        }
    }
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let titles_w = area.width.saturating_sub(2) as usize;
    let right = list_title_right(app);
    let right_w = right.as_ref().map_or(0, |l| l.width());
    let left = list_title_left(app, titles_w, right_w);

    let mut block = theme::panel(left);
    if let Some(r) = right {
        block = block.title_top(r.alignment(Alignment::Right));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    let height = inner.height as usize;
    let width = inner.width as usize;
    if height == 0 || width < 4 {
        return;
    }
    app.ensure_list_visible(height);

    if app.results.is_empty() {
        let lines = if app.searching {
            loading_lines("搜索中", inner.height)
        } else {
            let msg = if app.last_query.is_empty() {
                "暂无结果"
            } else {
                "无匹配结果"
            };
            placeholder_lines(msg, inner.height)
        };
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let selected = app.list_state.selected();
    let lines: Vec<Line> = app
        .results
        .iter()
        .enumerate()
        .skip(app.list_offset)
        .take(height)
        .map(|(i, it)| item_line(it, selected == Some(i), width))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// 列表左标题：说明当前结果是对什么内容的搜索。
/// 多行查询先解码转义、再把换行替换为 ⏎（与折叠态输入框一致）单行展示，
/// 过长按宽度截断并以 `...` 替代，保证标题只占一行、不挤压结果列表，且右侧计数始终可见。
fn list_title_left(app: &App, titles_w: usize, right_w: usize) -> Line<'static> {
    if app.last_query.trim().is_empty() {
        return theme::t_icon(ICON_FILE_LIST, "文件列表");
    }
    let (mode_color, mode_icon) = match app.mode {
        SearchMode::FileName => (theme::SKY, ICON_FILE_NAME),
        SearchMode::Content => (theme::MAUVE, ICON_FILE_CONTENT),
    };
    let label_style = Style::default().fg(mode_color).add_modifier(Modifier::BOLD);
    let icon_text = format!(" {mode_icon} ");
    let prefix_w = UnicodeWidthStr::width(icon_text.as_str())
        + UnicodeWidthStr::width(app.mode.label())
        + 2; // 图标+空格 + 模式 + ": "
    let budget = titles_w.saturating_sub(right_w + prefix_w + 1).max(1);
    // 查询可能含真实换行（Shift+Enter）或 `\n` 转义：标题只占一行，
    // 解码后把换行统一展示为 ⏎（与折叠态输入框一致），过长按宽度截断并以 ` ...` 替代（省略号左侧留一个空格）
    let query = truncate_width_with(&decoded_single_line(&app.last_query), budget, " ...");
    Line::from(vec![
        Span::styled(icon_text, label_style),
        Span::styled(app.mode.label(), label_style),
        Span::styled(": ", theme::title_style()),
        Span::styled(
            query,
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ])
}

/// 列表右标题：结果计数（搜索中显示动态提示，达到上限显示 400+）
fn list_title_right(app: &App) -> Option<Line<'static>> {
    if app.last_query.trim().is_empty() {
        return None;
    }
    let n = app.results.len();
    let count = if !app.searching && n >= MAX_RESULTS {
        format!("{n}+")
    } else {
        n.to_string()
    };
    let line = if app.searching {
        Line::from(vec![
            Span::styled(" 搜索中 ", Style::default().fg(theme::YELLOW)),
            Span::styled(
                format!("{} ", spinner()),
                Style::default().fg(theme::YELLOW),
            ),
            Span::styled(format!("{count} 项 "), theme::title_style()),
        ])
    } else {
        Line::from(vec![Span::styled(
            format!(" {count} 项 "),
            theme::title_style(),
        )])
    };
    Some(line)
}

/// 居中占位提示（水平居中、垂直大致居中）
fn placeholder_lines(msg: &str, height: u16) -> Vec<Line<'static>> {
    let pad = usize::from(height / 2);
    let mut lines = vec![Line::default(); pad];
    lines.push(
        Line::from(Span::styled(msg.to_string(), theme::placeholder()))
            .alignment(Alignment::Center),
    );
    lines
}

/// 动态加载标识：基于当前时间在 braille 帧间循环，形成旋转动画（非静止符号）。
/// 主循环每轮都会重绘（无事件时约 60ms 一次），故动画能持续转动。
fn spinner() -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES[(ms / 80) as usize % FRAMES.len()]
}

/// 居中加载提示：文案 + 动态 spinner（取代旧的静态 “…” 省略号）。
fn loading_lines(label: &str, height: u16) -> Vec<Line<'static>> {
    let pad = usize::from(height / 2);
    let mut lines = vec![Line::default(); pad];
    lines.push(
        Line::from(vec![
            Span::styled(format!("{label} "), theme::placeholder()),
            Span::styled(spinner().to_string(), Style::default().fg(theme::YELLOW)),
        ])
        .alignment(Alignment::Center),
    );
    lines
}

/// 单条文件项：选中行整行背景高亮，普通行同样缩进一格对齐
fn item_line(item: &SearchResultItem, selected: bool, width: usize) -> Line<'static> {
    let badge = if item.matches > 0 {
        format!(" [{}]", item.matches)
    } else {
        String::new()
    };
    let badge_w = UnicodeWidthStr::width(badge.as_str());
    // 文本前统一缩进一格
    let text_max = width.saturating_sub(1);
    let name = truncate_width(&item.display, text_max.saturating_sub(badge_w));
    let used = 1 + UnicodeWidthStr::width(name.as_str()) + badge_w;
    let pad = width.saturating_sub(used);

    if selected {
        let hl = theme::selected();
        Line::from(vec![
            Span::styled(format!(" {name}"), hl),
            Span::styled(
                badge,
                Style::default()
                    .fg(theme::GREEN)
                    .bg(theme::SELECT_BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(pad), hl),
        ])
    } else {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(name, Style::default().fg(theme::TEXT)),
            Span::styled(badge, Style::default().fg(theme::GREEN)),
        ])
    }
}

fn draw_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let block = theme::panel(theme::t_icon(ICON_PREVIEW, "预览"));
    let inner = block.inner(area);
    app.preview_width = inner.width.max(1);
    app.preview_height = inner.height.max(1);

    // 防御：预览内容必须与当前选中项一致。流式搜索结果重排序可能换掉选中项，
    // 若出现错位则丢弃旧预览并重新请求，避免“路径已变、内容未变”
    let selected_path = app.selected_item().map(|i| i.path.clone());
    if app.preview.is_some() && app.preview_path != selected_path {
        app.preview = None;
        app.request_preview();
    }

    match app.preview.as_deref() {
        Some(Preview::Text(text)) => {
            app.image_area = None;
            let total = text.lines.len();
            let max_scroll = total.saturating_sub(inner.height as usize) as u16;
            app.preview_max_scroll = max_scroll;
            if app.preview_scroll > max_scroll {
                app.preview_scroll = max_scroll;
            }
            let paragraph = Paragraph::new(text.clone())
                .block(block)
                .scroll((app.preview_scroll, 0));
            f.render_widget(paragraph, area);
        }
        Some(Preview::Image { .. }) => {
            // 光标定位模式：图片由主循环在绘制后以光标定位写入终端（z=-1 置于文本之后）。
            // 这里只绘制边框，并把内容区清成「无背景色的空格」，让图片从其后透出；不滚动。
            app.preview_max_scroll = 0;
            app.preview_scroll = 0;
            f.render_widget(block, area);
            f.render_widget(Clear, inner);
            app.image_area = Some(inner);
        }
        None => {
            app.image_area = None;
            f.render_widget(block, area);
            if inner.height > 0 && inner.width > 0 {
                let lines = if app.preview_loading {
                    loading_lines("加载中", inner.height)
                } else if app.selected_item().is_some() {
                    placeholder_lines("无法预览该文件", inner.height)
                } else if app.searching {
                    loading_lines("搜索中", inner.height)
                } else if !app.last_query.is_empty() {
                    placeholder_lines("无匹配结果", inner.height)
                } else {
                    placeholder_lines("暂无预览", inner.height)
                };
                f.render_widget(Paragraph::new(lines), inner);
            }
        }
    }
}

fn draw_path(f: &mut Frame, area: Rect, full_path: &str) {
    let paragraph = Paragraph::new(full_path)
        .block(theme::panel(theme::t_icon(ICON_FULL_PATH, "完整路径")))
        .style(Style::default().fg(theme::SKY))
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

/// 底部快捷键条目：(按键, 说明)
fn footer_items() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Tab", ":切换模式"),
        ("Ctrl+H", ":展开/折叠输入框"),
        ("Ctrl+P", ":搜索路径"),
        ("Ctrl+I", ":忽略目录"),
        ("Ctrl+S", ":文件大小上限"),
        ("Alt+J/K", ":文件选择"),
        ("Ctrl+J/K", ":滚动预览"),
        ("Ctrl+G", ":打开文件"),
        ("Ctrl+C", ":复制路径"),
        ("Esc", ":取消/退出"),
        ("Enter", ":搜索"),
    ]
}

fn footer_key(k: &'static str) -> Span<'static> {
    Span::styled(
        k,
        Style::default()
            .fg(theme::YELLOW)
            .add_modifier(Modifier::BOLD),
    )
}

fn footer_desc(d: &'static str) -> Span<'static> {
    Span::styled(d, Style::default().fg(theme::OVERLAY1))
}

/// 按可用宽度把快捷键条目贪心打包成多行（整条不拆分），实现折行。
fn build_footer_lines(width: u16) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(1) as usize; // 去掉左右边框
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for (k, d) in footer_items() {
        let item_w = UnicodeWidthStr::width(k) + UnicodeWidthStr::width(d);
        let need = if cur.is_empty() { item_w } else { item_w + 2 }; // 2 = 分隔空格
        if !cur.is_empty() && cur_w + need > inner {
            lines.push(Line::from(std::mem::take(&mut cur)));
            cur_w = 0;
        }
        if !cur.is_empty() {
            cur.push(Span::raw("  "));
            cur_w += 2;
        }
        cur.push(footer_key(k));
        cur.push(footer_desc(d));
        cur_w += item_w;
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn draw_footer(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    let footer = Paragraph::new(lines).block(theme::panel(Line::default()));
    f.render_widget(footer, area);
}

/// 编辑弹窗：路径（Ctrl+P）/ 忽略目录（Ctrl+I）/ 大小上限（Ctrl+S）共用一套渲染。
/// 输入内容按宽度折行；忽略目录额外把换行符（Shift+Enter / \n）解析为真正的换行，
/// 弹窗高度随内容行动态增长（超出可用高度时滚动，保证光标可见）。
fn draw_popup(f: &mut Frame, app: &App) {
    let Some(kind) = app.popup else { return };
    let area = f.area();
    let width = (area.width * 3 / 5).clamp(24, 72).min(area.width);
    let inner_w = width.saturating_sub(2).max(1) as usize;
    // 忽略目录把 \n 转义解析为换行；路径/大小上限仅折行不解析转义
    let decode = matches!(kind, PopupKind::IgnoreDirs);
    let content = wrap_input(&app.popup_input, inner_w, decode);

    let (title_main, hint, color): (&str, &str, Color) = match kind {
        PopupKind::Path => (
            " 搜索路径 ",
            "(Enter 确认 / Esc 取消，支持 ~)",
            theme::MAUVE,
        ),
        PopupKind::IgnoreDirs => (" 忽略目录 ", "(每行一个 / Enter 确认)", theme::PEACH),
        PopupKind::MaxSize => (
            " 文件大小上限(M) ",
            "(数字，Enter 确认 / Esc 取消)",
            theme::GREEN,
        ),
    };
    let title = Line::from(vec![
        Span::styled(
            title_main,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(hint, Style::default().fg(theme::OVERLAY1)),
        Span::raw(" "),
    ]);

    // 高度动态：内容行 + 上下边框 + （可选）错误行，不超过可用高度
    let err_lines = usize::from(app.popup_error.is_some());
    let max_total = area.height.saturating_sub(2).clamp(3, 18) as usize;
    let max_content = max_total.saturating_sub(2 + err_lines).max(1);
    let visible = content.len().min(max_content);
    let height = (visible + 2 + err_lines) as u16;
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    // 光标（支持折行与换行符）；内容超出可视区时滚动使光标所在行可见
    let (cur_line, cur_col) =
        input_cursor_line_col(&app.popup_input, app.popup_cursor, inner_w, decode);
    let offset = if content.len() > max_content {
        cur_line
            .saturating_sub(max_content - 1)
            .min(content.len() - max_content)
    } else {
        0
    };

    // 全选状态下整块高亮，提示用户输入即替换
    let style = if app.popup_select_all {
        theme::selected()
    } else {
        Style::default().fg(theme::TEXT)
    };
    let mut lines: Vec<Line> = content
        .iter()
        .skip(offset)
        .take(max_content)
        .map(|l| Line::from(Span::styled(l.clone(), style)))
        .collect();
    if let Some(err) = &app.popup_error {
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(theme::RED),
        )));
    }
    let input = Paragraph::new(lines)
        .block(theme::popup(title))
        .style(Style::default().fg(theme::TEXT));
    f.render_widget(Clear, popup);
    f.render_widget(input, popup);

    // 弹窗内光标：行 = 光标折行 - 滚动偏移，列 = 折行内显示宽度
    let vis_line = cur_line
        .saturating_sub(offset)
        .min(visible.saturating_sub(1));
    let x = popup.x + 1 + cur_col as u16;
    let max_x = popup.x + popup.width.saturating_sub(2);
    let y = popup.y + 1 + vis_line as u16;
    f.set_cursor_position((x.min(max_x), y));
}

/// 状态提示弹框（toast）：居中展示 app.status 消息，到期由 app.tick() 自动清除。
/// 取代旧的底栏内嵌提示（底栏太窄放不下完整消息）。
fn draw_toast(f: &mut Frame, app: &App) {
    let Some((msg, _)) = &app.status else { return };
    let area = f.area();
    let msg_w = UnicodeWidthStr::width(msg.as_str()) as u16;
    // 宽度贴合文本（含边框与左右留白），并限制在可用区域内
    let width = (msg_w + 4)
        .clamp(24, 72)
        .min(area.width.saturating_sub(2))
        .max(area.width.min(24));
    let inner_w = width.saturating_sub(2).max(1) as usize;
    // 文本超出宽度时自动换行，弹框随之加高
    let lines = wrap_lines(msg, inner_w) as u16;
    let height = (lines + 2).clamp(3, area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let title = Line::from(Span::styled(
        " 提示 ",
        Style::default()
            .fg(theme::PEACH)
            .add_modifier(Modifier::BOLD),
    ));
    let paragraph = Paragraph::new(msg.as_str())
        .block(theme::popup(title))
        .style(Style::default().fg(theme::TEXT))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(paragraph, popup);
}

/// 估算折行数（按显示宽度）
fn wrap_lines(text: &str, width: usize) -> usize {
    if text.is_empty() {
        return 1;
    }
    let w = UnicodeWidthStr::width(text);
    (w / width) + 1
}

/// 按显示宽度把文本硬折行（字符级，宽字符不拆分），至少返回一行。
/// 用于搜索输入框：与终端输入一致的可预期折行，便于光标定位。
fn wrap_by_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for c in text.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_w + cw > width {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(c);
        cur_w += cw;
    }
    lines.push(cur);
    lines
}

/// 输入框折行：先按换行符拆行（真实换行始终生效；`decode` 为 true 时额外把
/// `\n`/`\t`/`\\` 等转义解码，令输入的 `\n` 也呈现为真正换行），再逐行按宽度硬折行。
/// 至少返回一行。
fn wrap_input(text: &str, width: usize, decode: bool) -> Vec<String> {
    let decoded;
    let source: &str = if decode {
        decoded = decode_escapes(text);
        &decoded
    } else {
        text
    };
    let mut lines: Vec<String> = Vec::new();
    for seg in source.split('\n') {
        lines.extend(wrap_by_width(seg, width));
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 计算光标（字符索引 cursor）在输入框折行文本中的（行号, 列显示宽度）。
/// `decode` 为 true 时先对文本与光标前缀做转义解码（与 wrap_input 一致），
/// 保证输入的 `\n` 与 Shift+Enter 插入的真实换行都能正确定位光标。
fn input_cursor_line_col(text: &str, cursor: usize, width: usize, decode: bool) -> (usize, usize) {
    if !decode {
        return cursor_line_col(text, cursor, width);
    }
    let prefix: String = text.chars().take(cursor).collect();
    let decoded_cursor = decode_escapes(&prefix).chars().count();
    let decoded = decode_escapes(text);
    cursor_line_col(&decoded, decoded_cursor, width)
}

/// 折叠态单行展示：先做转义解码（与展开态 wrap_input 一致），再把换行替换为 ⏎、
/// 其余控制字符替换为 U+FFFD，按宽度截断（超出加省略号）。
/// 返回（截断后的单行文本, 光标在其中的列显示宽度）。
fn collapsed_input_display(text: &str, cursor: usize, width: usize) -> (String, usize) {
    // 光标按原始字符计；解码会改变字符数，先用前缀解码算出解码后的光标位置
    let prefix: String = text.chars().take(cursor).collect();
    let decoded_cursor = decode_escapes(&prefix).chars().count();
    let decoded = decode_escapes(text);

    let mut full = String::with_capacity(decoded.len());
    let mut col = 0usize;
    let mut cursor_col = None;
    for (i, c) in decoded.chars().enumerate() {
        if i == decoded_cursor {
            cursor_col = Some(col);
        }
        match c {
            '\n' => {
                full.push('⏎');
                col += 1;
            }
            c if c.is_control() => {
                full.push('\u{FFFD}');
                col += 1;
            }
            c => {
                full.push(c);
                col += UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
    }
    let cursor_col = cursor_col.unwrap_or(col);
    // 折叠态超长截断：省略号左右两侧各留一个空格（`prefix ... `）
    let truncated = truncate_width_with(&full, width, " ... ");
    let vis_w = UnicodeWidthStr::width(truncated.as_str());
    (truncated, cursor_col.min(vis_w))
}

/// 列表标题单行展示用：先做转义解码（与展开态 wrap_input / 折叠态一致），
/// 再把换行替换为 ⏎、其余控制字符替换为 U+FFFD。不含截断。
fn decoded_single_line(s: &str) -> String {
    let decoded = decode_escapes(s);
    let mut out = String::with_capacity(decoded.len());
    for c in decoded.chars() {
        match c {
            '\n' => out.push('⏎'),
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// 计算光标（字符索引 cursor）在折行文本中的（行号, 列显示宽度）。
/// 折行规则与 wrap_by_width 完全一致，并把真实换行符（\n）视为换行，保证光标落在正确的行/列。
fn cursor_line_col(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    let mut line = 0usize;
    let mut col_w = 0usize;
    for (i, c) in text.chars().enumerate() {
        if c == '\n' {
            // 光标恰在换行符上：停留在当前行行尾
            if i == cursor {
                return (line, col_w);
            }
            line += 1;
            col_w = 0;
            continue;
        }
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        // 与 wrap_by_width 一致：当前行放不下则先换行，
        // 保证光标恰好落在折行边界时位于下一行行首（与下一个字符位置一致）
        if col_w + cw > width {
            line += 1;
            col_w = 0;
        }
        if i == cursor {
            return (line, col_w);
        }
        col_w += cw;
    }
    (line, col_w)
}

/// 展开态输入框中按视觉行上下移动光标，返回新的原始字符索引（raw cursor）。
/// 折行与转义解码规则与 wrap_input / input_cursor_line_col 完全一致：
/// 先构建解码后的字符序列（并记录每个解码字符对应的原始索引），在解码文本中
/// 按折行计算当前（行, 列），移动到目标行后尽量保持原列，最后回填为原始光标索引。
pub fn move_cursor_vertical(
    text: &str,
    cursor: usize,
    width: usize,
    decode: bool,
    delta: isize,
) -> usize {
    let width = width.max(1);
    // 解码字符序列 + 每个解码字符对应的原始字符索引（用于最后回填 raw cursor）。
    let raw: Vec<char> = text.chars().collect();
    let mut decoded: Vec<char> = Vec::new();
    let mut raw_of: Vec<usize> = Vec::new();
    if decode {
        let mut r = 0usize;
        while r < raw.len() {
            let c = raw[r];
            if c != '\\' {
                decoded.push(c);
                raw_of.push(r);
                r += 1;
            } else {
                match raw.get(r + 1) {
                    Some('n') => {
                        decoded.push('\n');
                        raw_of.push(r);
                        r += 2;
                    }
                    Some('t') => {
                        decoded.push('\t');
                        raw_of.push(r);
                        r += 2;
                    }
                    Some('r') => {
                        decoded.push('\r');
                        raw_of.push(r);
                        r += 2;
                    }
                    Some('\\') => {
                        decoded.push('\\');
                        raw_of.push(r);
                        r += 2;
                    }
                    Some(&other) => {
                        decoded.push('\\');
                        raw_of.push(r);
                        decoded.push(other);
                        raw_of.push(r + 1);
                        r += 2;
                    }
                    None => {
                        decoded.push('\\');
                        raw_of.push(r);
                        r += 1;
                    }
                }
            }
        }
    } else {
        for (i, &c) in raw.iter().enumerate() {
            decoded.push(c);
            raw_of.push(i);
        }
    }
    let decoded_str: String = decoded.iter().collect();

    // 当前 raw cursor 对应的解码光标位置：解码字符中原始索引 < cursor 的个数。
    let dc = raw_of.iter().filter(|&&ri| ri < cursor).count();
    let (line, col) = cursor_line_col(&decoded_str, dc, width);

    let num_lines = wrap_input(&decoded_str, width, false).len().max(1);
    let target_line = if delta >= 0 {
        (line + delta as usize).min(num_lines - 1)
    } else {
        line.saturating_sub((-delta) as usize)
    };

    let target_decoded = decoded_pos_at_line_col(&decoded_str, width, target_line, col);
    match raw_of.get(target_decoded) {
        Some(&ri) => ri,
        None => raw.len(),
    }
}

/// 在折行文本中找到 target_line 上、列显示宽度不超过 target_col 且最接近它的位置（解码字符索引）。
/// 折行规则与 wrap_by_width / cursor_line_col 一致；真实换行符视为换行。
fn decoded_pos_at_line_col(text: &str, width: usize, target_line: usize, target_col: usize) -> usize {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    let mut line = 0usize;
    let mut col_w = 0usize;
    let mut best: Option<usize> = None; // target_line 上 col_w <= target_col 的最大位置
    let mut line_start = 0usize; // target_line 行首位置（兄底）
    let mut reached = false;
    for p in 0..=chars.len() {
        if line == target_line {
            if !reached {
                reached = true;
                line_start = p;
            }
            if col_w <= target_col {
                best = Some(p);
            }
        }
        if p == chars.len() {
            break;
        }
        let c = chars[p];
        if c == '\n' {
            if line == target_line {
                break; // 目标行已扫描完
            }
            line += 1;
            col_w = 0;
            continue;
        }
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if col_w + cw > width {
            if line == target_line {
                break; // 折行进入下一行，目标行已扫描完
            }
            line += 1;
            col_w = 0;
        }
        col_w += cw;
    }
    best.unwrap_or(line_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn vertical_cursor_move_real_newline() {
        // "abc\ndef"，光标在 'd'（raw=4），上移 -> 行首 'a'（raw=0）
        assert_eq!(move_cursor_vertical("abc\ndef", 4, 80, true, -1), 0);
        // 从 'a'（raw=0）下移 -> 'd'（raw=4）
        assert_eq!(move_cursor_vertical("abc\ndef", 0, 80, true, 1), 4);
        // 第一行再上移不越界
        assert_eq!(move_cursor_vertical("abc\ndef", 0, 80, true, -1), 0);
        // 已在最后一行，下移为 no-op（保持原位置 raw=6）
        assert_eq!(move_cursor_vertical("abc\ndef", 6, 80, true, 1), 6);
    }

    #[test]
    fn vertical_cursor_move_wrapped_keeps_column() {
        // "abcdef" 宽 3 折为 "abc"/"def"；光标在 'f'（raw=5，行1列2），上移保持列 -> 'c'（raw=2）
        assert_eq!(move_cursor_vertical("abcdef", 5, 3, false, -1), 2);
        // 从 'b'（raw=1，行0列1）下移 -> 'e'（raw=4，行1列1）
        assert_eq!(move_cursor_vertical("abcdef", 1, 3, false, 1), 4);
    }

    #[test]
    fn vertical_cursor_move_decoded_escape() {
        // 原始 "a\\nb"（a,\,n,b）解码为 "a\nb"；光标在 'b'（raw=3），上移 -> 'a'（raw=0）
        assert_eq!(move_cursor_vertical("a\\nb", 3, 80, true, -1), 0);
        // 从 'a'（raw=0）下移 -> 'b'（raw=3）
        assert_eq!(move_cursor_vertical("a\\nb", 0, 80, true, 1), 3);
    }

    fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // 宽字符在 buffer 中占多个 cell（后续 cell 被 reset 为空格），
        // 按符号显示宽度跳过这些占位 cell，还原真实文本
        let mut out = String::new();
        for y in 0..h {
            let mut x = 0;
            while x < w {
                let symbol = buf[(x, y)].symbol();
                out.push_str(symbol);
                x += UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn enter_hint_shown_only_when_input_dirty() {
        let mut app = App::new();
        // 初始状态：无提示，展示中性占位
        let s = render_to_string(&mut app, 100, 30);
        assert!(!s.contains("Enter 搜索"), "{s}");
        assert!(s.contains("暂无结果"), "{s}");
        assert!(s.contains("暂无预览"), "{s}");

        // 初次输入内容后出现提示
        app.input = "test".to_string();
        app.cursor = 4;
        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains("Enter 搜索"), "{s}");

        // 模拟搜索已派发（last_query 与输入一致）后提示消失
        app.last_query = "test".to_string();
        let s = render_to_string(&mut app, 100, 30);
        assert!(!s.contains("Enter 搜索"), "{s}");

        // 再次改动内容，提示复现
        app.input = "test2".to_string();
        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains("Enter 搜索"), "{s}");
    }

    #[test]
    fn list_title_shows_query_context_and_count() {
        let mut app = App::new();
        app.last_query = "foo".to_string();
        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains("文件名: foo"), "{s}");
        assert!(s.contains("0 项"), "{s}");
        // 搜过但没结果：占位提示为“无匹配结果”
        assert!(s.contains("无匹配结果"), "{s}");
    }

    #[test]
    fn list_title_multiline_query_stays_single_line() {
        let mut app = App::new();
        app.mode = SearchMode::Content;
        // 多行查询解码后把换行展示为 ⏎（与折叠态输入框一致），单行展示在标题上
        app.last_query = "foo\\nbar\\nbaz".to_string();
        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains("文件内容: foo⏎bar⏎baz"), "{s}");
        // 内容模式：输入框底边框出现多行语法提示
        assert!(s.contains("\\n 换行"), "{s}");

        // 超长查询被截断为单行并以 ... 替代，标题不会换行挤压列表
        app.last_query = "a".repeat(500);
        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains("..."), "{s}");
        assert!(!s.contains(&"a".repeat(100)), "{s}");
    }

    #[test]
    fn list_title_matches_collapsed_input_trailing_newlines() {
        // 折叠态输入框与文件列表标题对同一查询的展示必须一致：
        // 尾部换行（⏎）不能在标题中被丢掉
        let mut app = App::new();
        app.mode = SearchMode::Content;
        app.input = "ceshi \nceshi \n\n".to_string();
        app.cursor = app.input.chars().count();
        // dispatch_search 会把原始输入存入 last_query（仅对 fd/rg 传参做 trim）
        app.last_query = app.input.clone();
        let s = render_to_string(&mut app, 120, 30);
        assert!(s.contains("文件内容: ceshi ⏎ceshi ⏎⏎"), "{s}");
    }

    /// 文件名含 ESC 等控制字符时，整个界面（列表/路径框）不得把控制字符写进终端
    #[test]
    fn control_chars_in_names_never_reach_screen() {
        let mut app = App::new();
        app.last_query = "test".to_string();
        app.results = vec![SearchResultItem {
            path: std::path::PathBuf::from("/tmp/ctrl_\u{1b}[31m_test.sh"),
            display: sanitize_display("ctrl_\u{1b}[31m_test.sh"),
            matches: 0,
            score: 100,
            priority: 1,
        }];
        app.list_state.select(Some(0));

        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains('\u{FFFD}'), "{s}");
        for c in s.chars() {
            assert!(
                !c.is_control() || c == '\n',
                "control char U+{:04X} reached screen",
                c as u32
            );
        }
    }

    /// 状态消息（如取消搜索）以居中 toast 弹框展示，不再塞进狭窄的底栏
    #[test]
    fn status_shown_as_centered_toast() {
        let mut app = App::new();
        app.set_status("已取消当前搜索");
        let s = render_to_string(&mut app, 100, 30);
        // toast 弹框标题与完整消息都展示在屏幕上
        assert!(s.contains("提示"), "{s}");
        assert!(s.contains("已取消当前搜索"), "{s}");
        // 底栏不再有内嵌的 "|  消息" 形式
        assert!(!s.contains("|  已取消当前搜索"), "{s}");
    }

    /// 搜索输入框：真实换行（Shift+Enter）与输入的 \n 转义都显示为真正的换行（两行）
    #[test]
    fn search_input_renders_newlines_as_line_breaks() {
        // 真实换行
        let mut app = App::new();
        app.input = "foo\nbar".to_string();
        app.cursor = 7;
        let s = render_to_string(&mut app, 100, 30);
        let rows: Vec<&str> = s.lines().collect();
        // 输入框占前两行内容：foo 与 bar 分别出现在不同行
        assert!(rows[1].contains("foo"), "{s}");
        assert!(rows[2].contains("bar"), "{s}");
        assert!(!rows[1].contains("bar"), "foo 与 bar 应分行: {s}");

        // 输入的 \n 转义同样解析为换行显示
        let mut app2 = App::new();
        app2.input = "foo\\nbar".to_string();
        app2.cursor = 8;
        let s2 = render_to_string(&mut app2, 100, 30);
        let rows2: Vec<&str> = s2.lines().collect();
        assert!(rows2[1].contains("foo"), "{s2}");
        assert!(rows2[2].contains("bar"), "{s2}");
        // 不应再显示字面 \n
        assert!(!rows2[1].contains("\\n"), "不应显示字面 \\n: {s2}");
    }

    /// 输入框标题显示展开/折叠状态标识：
    /// 展开 = nf-oct-unfold（U+F42D）+ “展开”，收起 = nf-oct-fold（U+F48C）+ “收起”
    #[test]
    fn input_title_shows_expand_state_indicator() {
        const UNFOLD: char = '\u{F42D}'; // nf-oct-unfold
        const FOLD: char = '\u{F48C}'; // nf-oct-fold

        let mut app = App::new();
        app.input_expanded = true;
        let s = render_to_string(&mut app, 100, 30);
        let title_row = s.lines().next().unwrap();
        // 图标唯一出现在标题：展开态有 unfold、无 fold
        assert!(s.contains(UNFOLD), "展开态应显示 nf-oct-unfold 图标: {s}");
        assert!(!s.contains(FOLD), "{s}");
        // 标题行（首行）同时展示“展开”文字（底栏快捷键提示也含“展开”，故只看标题行）
        assert!(title_row.contains("展开"), "标题行应含“展开”: {s}");

        app.input_expanded = false;
        let s = render_to_string(&mut app, 100, 30);
        let title_row = s.lines().next().unwrap();
        assert!(s.contains(FOLD), "折叠态应显示 nf-oct-fold 图标: {s}");
        assert!(!s.contains(UNFOLD), "{s}");
        assert!(title_row.contains("收起"), "标题行应含“收起”: {s}");
    }

    /// 折叠态（Ctrl+H）：输入框只占单行，换行以 ⏎ 标记，超长内容以省略号截断
    #[test]
    fn collapsed_input_single_line_with_ellipsis() {
        // 多行内容折叠为单行，真实换行显示为 ⏎
        let mut app = App::new();
        app.input_expanded = false;
        app.input = "foo\nbar".to_string();
        app.cursor = 7;
        let s = render_to_string(&mut app, 100, 30);
        let rows: Vec<&str> = s.lines().collect();
        assert!(rows[1].contains("foo⏎bar"), "折叠态应单行展示换行: {s}");
        // 输入框只占一行内容（row 1），row 2 已是边框/列表区，不应再出现 bar
        assert!(!rows[2].contains("bar"), "折叠态应只占单行: {s}");

        // 超长内容被截断并加省略号
        let mut app2 = App::new();
        app2.input_expanded = false;
        app2.input = "a".repeat(300);
        app2.cursor = 300;
        let s2 = render_to_string(&mut app2, 100, 30);
        let rows2: Vec<&str> = s2.lines().collect();
        assert!(rows2[1].contains("..."), "超长折叠应加省略号: {s2}");
        assert!(!rows2[1].contains(&"a".repeat(100)), "应被截断: {s2}");
    }

    /// 展开态：内容超过 1/3 屏高时出现滚动条，并随光标滚动查看全部内容
    #[test]
    fn expanded_input_scrolls_when_overflowing() {
        let mut app = App::new();
        app.input_expanded = true;
        // 20 行内容；屏高 30 => 输入框最高 1/3=10 行，内容溢出
        app.input = (1..=20)
            .map(|i| format!("line{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.cursor = app.input.chars().count(); // 光标在末尾（最后一行）

        let s = render_to_string(&mut app, 100, 30);
        // 光标在末尾 => 视窗滚到底部：最后一行可见、第一行不可见
        assert!(s.contains("line20"), "应滚动到光标所在行: {s}");
        assert!(!s.contains("line01"), "滚出视窗的行不应显示: {s}");
        // 溢出时出现滚动条（默认 thumb 符号 █）
        assert!(s.contains('█'), "溢出时应显示滚动条: {s}");
        assert_eq!(app.input_scroll, 10, "滚动偏移应使光标行可见");

        // 光标回到开头 => 视窗滚回顶部，第一行重新可见
        app.cursor = 0;
        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains("line01"), "光标回顶部应滚回第一行: {s}");
        assert!(!s.contains("line20"), "顶部视窗不应见末行: {s}");
        assert_eq!(app.input_scroll, 0);
    }

    /// 忽略目录弹窗：多行内容逐行展示，弹窗随内容加高
    #[test]
    fn ignore_dirs_popup_renders_multiple_lines() {
        let mut app = App::new();
        app.ignore_dirs = vec!["/home".to_string(), "/etc".to_string(), "/usr".to_string()];
        app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
        let s = render_to_string(&mut app, 100, 30);
        // 三个目录都展示在屏幕上
        assert!(s.contains("/home"), "{s}");
        assert!(s.contains("/etc"), "{s}");
        assert!(s.contains("/usr"), "{s}");
        // 提示不再提及逗号分隔
        assert!(s.contains("每行一个"), "{s}");
        assert!(!s.contains("逗号分隔"), "{s}");
    }

    #[test]
    fn input_dirty_logic() {
        let mut app = App::new();
        assert!(!app.input_dirty());
        app.input = " x ".to_string();
        assert!(app.input_dirty());
        app.last_query = "x".to_string();
        // trim 后一致 => 不算脏
        assert!(!app.input_dirty());
    }

    #[test]
    fn wrap_by_width_ascii_and_wide() {
        // 空文本至少一行
        assert_eq!(wrap_by_width("", 10), vec![""]);
        // ASCII 按宽度硬折行
        assert_eq!(wrap_by_width("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_by_width("abcdefg", 3), vec!["abc", "def", "g"]);
        // 宽字符（中文=2 列）不被拆分：第 3 列放不下整个字就换行
        assert_eq!(wrap_by_width("中文搜索", 3), vec!["中", "文", "搜", "索"]);
        assert_eq!(wrap_by_width("中文搜索", 4), vec!["中文", "搜索"]);
        // 混合：ab(2) + 中(2) = 4 刚好填满宽 4
        assert_eq!(wrap_by_width("ab中文", 4), vec!["ab中", "文"]);
    }

    #[test]
    fn cursor_line_col_tracks_wrap() {
        // "abcdef" 宽 3 折为 ["abc","def"]
        assert_eq!(cursor_line_col("abcdef", 0, 3), (0, 0));
        assert_eq!(cursor_line_col("abcdef", 3, 3), (1, 0)); // 光标在第二行行首
        assert_eq!(cursor_line_col("abcdef", 6, 3), (1, 3)); // 末尾
                                                             // 宽字符："中文搜索" 宽 4 折为 ["中文","搜索"]，光标在 "搜" 前 => 第 2 行行首
        assert_eq!(cursor_line_col("中文搜索", 2, 4), (1, 0));
        assert_eq!(cursor_line_col("中文搜索", 4, 4), (1, 4));
    }

    /// 真实换行符被 cursor_line_col 视为换行
    #[test]
    fn cursor_line_col_handles_newline() {
        // "ab\ncd" => ["ab","cd"]
        assert_eq!(cursor_line_col("ab\ncd", 0, 10), (0, 0));
        assert_eq!(cursor_line_col("ab\ncd", 2, 10), (0, 2)); // 光标在换行符上：当前行行尾
        assert_eq!(cursor_line_col("ab\ncd", 3, 10), (1, 0)); // 换行后：下一行行首
        assert_eq!(cursor_line_col("ab\ncd", 5, 10), (1, 2)); // 末尾
                                                              // 换行 + 折行组合
        assert_eq!(cursor_line_col("ab\ncdef", 5, 3), (1, 2));
    }

    #[test]
    fn wrap_input_splits_newlines_and_decodes() {
        // 真实换行始终拆行（decode=false 也生效）
        assert_eq!(wrap_input("ab\ncd", 10, false), vec!["ab", "cd"]);
        // decode=true：输入的 \n 转义被解析为换行
        assert_eq!(wrap_input("ab\\ncd", 10, true), vec!["ab", "cd"]);
        // decode=false：\n 转义原样保留为单行
        assert_eq!(wrap_input("ab\\ncd", 20, false), vec!["ab\\ncd"]);
        // 换行 + 按宽折行组合
        assert_eq!(wrap_input("abcdef\nxy", 3, false), vec!["abc", "def", "xy"]);
        // 空文本至少一行；尾部换行保留空行
        assert_eq!(wrap_input("", 10, true), vec![""]);
        assert_eq!(wrap_input("ab\n", 10, true), vec!["ab", ""]);
    }

    #[test]
    fn input_cursor_line_col_decodes_escapes() {
        // 输入的 \n 转义（ab\ncd，6 个原始字符）解析为换行后定位
        // 原始索引：0a 1b 2\\ 3n 4c 5d
        assert_eq!(input_cursor_line_col("ab\\ncd", 0, 10, true), (0, 0));
        assert_eq!(input_cursor_line_col("ab\\ncd", 2, 10, true), (0, 2)); // \ 之前
        assert_eq!(input_cursor_line_col("ab\\ncd", 4, 10, true), (1, 0)); // \n 之后、c 之前
        assert_eq!(input_cursor_line_col("ab\\ncd", 6, 10, true), (1, 2)); // 末尾
                                                                           // 真实换行（Shift+Enter）同样定位
        assert_eq!(input_cursor_line_col("ab\ncd", 3, 10, true), (1, 0));
        // decode=false 等同 cursor_line_col
        assert_eq!(input_cursor_line_col("abcdef", 3, 3, false), (1, 0));
    }

    #[test]
    fn decoded_single_line_shows_return_symbol() {
        // 解码转义后把换行展示为 ⏎，其余控制字符替换为 U+FFFD
        assert_eq!(decoded_single_line("a\nb"), "a⏎b");
        assert_eq!(decoded_single_line("a\\nb"), "a⏎b"); // 输入的 \n 解码为换行后展示为 ⏎
        assert_eq!(decoded_single_line("a\tb"), "a\u{FFFD}b");
        assert_eq!(decoded_single_line("a\u{1b}b"), "a\u{FFFD}b");
        assert_eq!(decoded_single_line("plain"), "plain");
    }
}

/// 按显示宽度截断，超出时末尾加省略号
fn truncate_width(s: &str, max: usize) -> String {
    truncate_width_with(s, max, "…")
}

/// 按显示宽度截断，超出部分以自定义省略标记（如 `…` 或 `...`）替代。
fn truncate_width_with(s: &str, max: usize, ellipsis: &str) -> String {
    let ew = UnicodeWidthStr::width(ellipsis);
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max <= ew {
        return ellipsis.to_string();
    }
    let mut w = 0;
    let mut out = String::new();
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max - ew {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push_str(ellipsis);
    out
}
