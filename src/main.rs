//! sift — 基于 fd / rg / bat 的终端文件搜索工具（仅 Linux）。

mod app;
mod config;
mod editor;
mod preview;
mod search;
mod theme;
mod ui;

use std::io::{self, Stdout};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{Action, App};

fn main() -> Result<()> {
    let mut terminal = setup_terminal()?;
    let mut app = App::new();
    let result = run_app(&mut terminal, &mut app);
    app.shutdown(); // 清理可能还在运行的后台 rg
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // 让支持 kitty 键盘协议的终端能区分 Ctrl+J 与 Enter；
    // bracketed paste 让多行粘贴作为单个 Event::Paste 到达（保留换行）
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        EnableBracketedPaste
    );
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    let _ = execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    terminal.show_cursor()?;
    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(Action::Open(path)) = app.on_key(key) {
                        open_in_editor(terminal, app, &path);
                    }
                }
                Event::Paste(s) => app.on_paste(&s),
                Event::Resize(..) => app.size_changed = true,
                _ => {}
            }
        }

        app.tick();

        if app.size_changed {
            app.size_changed = false;
            app.after_resize();
        }
    }
    Ok(())
}

/// 挂起 TUI -> 打开编辑器 -> 恢复 TUI
fn open_in_editor(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App, path: &Path) {
    let Some(ed) = editor::detect_editor() else {
        app.set_status("未找到可用编辑器（nvim/vim/code…）");
        return;
    };
    let sudo = editor::needs_sudo(path);

    let _ = restore_terminal(terminal);
    let result = editor::open(&ed, path, sudo);
    // 重新进入 TUI
    let _ = enable_raw_mode();
    // 编辑器（如 vim）可能自己开关过 bracketed paste，需重新启用
    let _ = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        EnableBracketedPaste
    );
    let _ = terminal.clear();

    match result {
        Ok(status) if status.success() => app.set_status(format!("已关闭编辑器（{ed}）")),
        Ok(status) => app.set_status(format!("编辑器退出码: {}", status.code().unwrap_or(-1))),
        Err(e) => app.set_status(format!("打开失败: {e}")),
    }
}
