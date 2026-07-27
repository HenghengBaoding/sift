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
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, PopupKind, MAX_RESULTS};
use crate::search::{sanitize_display, SearchMode, SearchResultItem};
use crate::theme;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // 顶部搜索输入框：查询过长时按宽度硬折行，高度按折行数动态计算
    // （最多占约 1/3 屏高，避免长查询挤掉结果列表）
    let input_inner_w = area.width.saturating_sub(2).max(1) as usize;
    let input_wrapped = wrap_by_width(&app.input, input_inner_w);
    let input_max_content = (area.height / 3).clamp(1, 8) as usize;
    let input_h = input_wrapped.len().clamp(1, input_max_content) as u16 + 2;

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

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let mode_color = match app.mode {
        SearchMode::FileName => theme::SKY,
        SearchMode::Content => theme::MAUVE,
    };
    // 输入内容有未搜索的改动时，右上角放醒目的 "Enter 搜索" chip
    let dirty = app.input_dirty();
    let chip_w = if dirty { 12 } else { 0 }; // " Enter 搜索 " 11 列 + 间隔
    let titles_w = area.width.saturating_sub(2) as usize;
    let fixed_w = UnicodeWidthStr::width(" 搜索 | 模式: ")
        + UnicodeWidthStr::width(app.mode.label())
        + UnicodeWidthStr::width(" (Tab) | 路径: ")
        + UnicodeWidthStr::width(" (Ctrl+P) ");
    // 路径按剩余宽度截断，保证不与右侧 chip 重叠
    let path_budget = titles_w.saturating_sub(fixed_w + chip_w + 1).max(4);
    let path = truncate_width(&sanitize_display(&app.current_path_display()), path_budget);
    let title = Line::from(vec![
        Span::styled(" 搜索 | 模式: ", theme::title_style()),
        Span::styled(
            app.mode.label(),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" (Tab)", Style::default().fg(theme::OVERLAY1)),
        Span::styled(" | 路径: ", theme::title_style()),
        Span::styled(
            path,
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" (Ctrl+P)", Style::default().fg(theme::OVERLAY1)),
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
                " \\n 换行 \\\\ 反斜杠 ",
                Style::default().fg(theme::OVERLAY1),
            ))
            .alignment(Alignment::Right),
        );
    }
    // 按宽度硬折行（字符级、宽字符不拆分），与 draw() 里计算高度的折行保持一致
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let lines: Vec<Line> = wrap_by_width(&app.input, inner_w)
        .into_iter()
        .map(|l| Line::from(Span::raw(l)))
        .collect();
    let input = Paragraph::new(lines)
        .block(block)
        .style(Style::default().fg(theme::TEXT));
    f.render_widget(input, area);

    // 光标（支持宽字符与折行）：定位到光标所在折行的行/列
    if app.popup.is_none() {
        let (line, col_w) = cursor_line_col(&app.input, app.cursor, inner_w);
        // 输入框过高被 clamp 时，光标行可能超出可见区域，收敛到最后一行
        let visible_lines = area.height.saturating_sub(2).max(1) as usize;
        let line = line.min(visible_lines.saturating_sub(1));
        let x = area.x + 1 + col_w as u16;
        let max_x = area.x + area.width.saturating_sub(2);
        f.set_cursor_position((x.min(max_x), area.y + 1 + line as u16));
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
/// 多行查询以转义形态（与输入框一致的 `\n`）单行展示，过长按宽度截断加省略号，
/// 保证标题只占一行、不挤压结果列表，且右侧计数始终可见。
fn list_title_left(app: &App, titles_w: usize, right_w: usize) -> Line<'static> {
    if app.last_query.is_empty() {
        return theme::t(" 文件列表 ");
    }
    let mode_color = match app.mode {
        SearchMode::FileName => theme::SKY,
        SearchMode::Content => theme::MAUVE,
    };
    let prefix_w = 1 + UnicodeWidthStr::width(app.mode.label()) + 2; // " " + 模式 + ": "
    let budget = titles_w.saturating_sub(right_w + prefix_w + 1).max(1);
    let query = truncate_width(&app.last_query, budget);
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            app.mode.label(),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
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
    if app.last_query.is_empty() {
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
    let block = theme::panel(theme::t(" 预览 "));
    let inner = block.inner(area);
    app.preview_width = inner.width.max(1);

    // 防御：预览内容必须与当前选中项一致。流式搜索结果重排序可能换掉选中项，
    // 若出现错位则丢弃旧预览并重新请求，避免“路径已变、内容未变”
    let selected_path = app.selected_item().map(|i| i.path.clone());
    if app.preview.is_some() && app.preview_path != selected_path {
        app.preview = None;
        app.request_preview();
    }

    if let Some(text) = &app.preview {
        let total = text.lines.len();
        let max_scroll = total.saturating_sub(inner.height as usize) as u16;
        app.preview_max_scroll = max_scroll;
        if app.preview_scroll > max_scroll {
            app.preview_scroll = max_scroll;
        }
        let paragraph = Paragraph::new((**text).clone())
            .block(block)
            .scroll((app.preview_scroll, 0));
        f.render_widget(paragraph, area);
    } else {
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

fn draw_path(f: &mut Frame, area: Rect, full_path: &str) {
    let paragraph = Paragraph::new(full_path)
        .block(theme::panel(theme::t(" 完整路径 ")))
        .style(Style::default().fg(theme::SKY))
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

/// 底部快捷键条目：(按键, 说明)
fn footer_items() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Tab", ":切换模式"),
        ("Ctrl+P", ":路径"),
        ("Ctrl+I", ":忽略目录"),
        ("Ctrl+S", ":大小上限"),
        ("↑/↓", ":选择"),
        ("Ctrl+J/K", ":滚动预览"),
        ("Enter", ":搜索"),
        ("Ctrl+G", ":打开"),
        ("Esc", ":取消/退出"),
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

/// 编辑弹窗：路径（Ctrl+P）/ 忽略目录（Ctrl+I）/ 大小上限（Ctrl+S）共用一套渲染
fn draw_popup(f: &mut Frame, app: &App) {
    let Some(kind) = app.popup else { return };
    let area = f.area();
    let width = (area.width * 3 / 5).clamp(24, 72).min(area.width);
    // 有校验错误时弹窗加高一行展示错误信息
    let height = (if app.popup_error.is_some() { 4 } else { 3 }).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let (title_main, hint, color): (&str, &str, Color) = match kind {
        PopupKind::Path => (
            " 搜索路径 ",
            "(Enter 确认 / Esc 取消，支持 ~)",
            theme::MAUVE,
        ),
        PopupKind::IgnoreDirs => (
            " 忽略目录 ",
            "(逗号分隔，Enter 确认 / Esc 取消)",
            theme::PEACH,
        ),
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
    // 全选状态下整行高亮，提示用户输入即替换
    let input_line = if app.popup_select_all {
        Line::from(Span::styled(app.popup_input.as_str(), theme::selected()))
    } else {
        Line::from(app.popup_input.as_str())
    };
    let mut lines = vec![input_line];
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

    // 弹窗内光标
    let prefix: String = app.popup_input.chars().take(app.popup_cursor).collect();
    let x = popup.x + 1 + UnicodeWidthStr::width(prefix.as_str()) as u16;
    let max_x = popup.x + popup.width.saturating_sub(2);
    f.set_cursor_position((x.min(max_x), popup.y + 1));
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

/// 计算光标（字符索引 cursor）在折行文本中的（行号, 列显示宽度）。
/// 折行规则与 wrap_by_width 完全一致，保证光标落在正确的行/列。
fn cursor_line_col(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    let mut line = 0usize;
    let mut col_w = 0usize;
    for (i, c) in text.chars().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
        // 多行查询以转义形态单行展示在标题上（与输入框一致）
        app.last_query = "foo\\nbar\\nbaz".to_string();
        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains("文件内容: foo\\nbar\\nbaz"), "{s}");
        // 内容模式：输入框底边框出现多行语法提示
        assert!(s.contains("\\n 换行"), "{s}");

        // 超长查询被截断为单行并加省略号，标题不会换行挤压列表
        app.last_query = "a".repeat(500);
        let s = render_to_string(&mut app, 100, 30);
        assert!(s.contains('…'), "{s}");
        assert!(!s.contains(&"a".repeat(100)), "{s}");
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
}

/// 按显示宽度截断，超出时末尾加省略号
fn truncate_width(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let mut w = 0;
    let mut out = String::new();
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}
