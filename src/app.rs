//! 应用状态与事件处理。
//!
//! 架构（见 AGENTS.md「性能优化」）：
//! - UI 线程只负责渲染与处理按键，绝不阻塞；
//! - 搜索（fd/rg）在后台线程以子进程方式运行，stdout 流式读取；
//! - 结果通过 mpsc channel 分批回传，按代号（gen）过滤过期消息；
//! - 取消搜索 = 置取消标志 + kill 子进程（非阻塞，UI 线程调用也不会卡）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Text;
use ratatui::widgets::ListState;

use crate::config;
use crate::preview;
use crate::search::{self, SearchMode, SearchResultItem};
use crate::ui;

/// 文件名列表缓存有效期
const FILE_LIST_TTL: Duration = Duration::from_secs(60);
/// 内容搜索流式回传的背压阈值：累计 100 条 **或** 间隔 16ms（≈60fps）即刷新一次 UI，
/// 两个维度任一达到即发送，避免疯狂推送拖垮界面。
const BATCH_SIZE: usize = 100;
const BATCH_INTERVAL: Duration = Duration::from_millis(16);
/// 单次搜索展示的最大结果数
pub const MAX_RESULTS: usize = 400;
/// 预览缓存最大条目
const PREVIEW_CACHE_MAX: usize = 100;
/// 状态栏消息停留时间
const STATUS_TTL: Duration = Duration::from_secs(5);

/// on_key 返回的动作（需要主循环配合，如挂起终端打开编辑器）
pub enum Action {
    Open(PathBuf),
}

/// 当前打开的编辑弹窗类型
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PopupKind {
    /// 搜索路径（Ctrl+P）
    Path,
    /// 内容搜索额外忽略目录（Ctrl+I）
    IgnoreDirs,
    /// 内容搜索文件大小上限（Ctrl+S）
    MaxSize,
}

/// 后台线程回报消息
pub enum Msg {
    /// 内容搜索的增量批次（流式）
    SearchBatch {
        gen: u64,
        items: Vec<SearchResultItem>,
    },
    SearchDone {
        gen: u64,
        /// true = 内容搜索（结果已通过 SearchBatch 流式到达）
        content: bool,
        items: Vec<SearchResultItem>,
        new_file_list: Option<(PathBuf, Arc<Vec<String>>)>,
    },
    PreviewDone {
        gen: u64,
        path: PathBuf,
        width: u16,
        text: Text<'static>,
    },
}

/// 一次搜索任务的句柄：用于取消（置标志 + kill 子进程）。
/// 字段均为 Arc，clone 廉价；UI 线程与后台线程共享。
#[derive(Clone)]
struct SearchJob {
    cancel: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl SearchJob {
    fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
        }
    }

    /// 取消搜索：置取消标志并 kill 子进程。
    /// kill 会关闭管道，令后台读取线程立刻收到 EOF 退出；整个过程非阻塞。
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Ok(mut g) = self.child.lock() {
            if let Some(c) = g.as_mut() {
                let _ = c.kill();
            }
        }
    }
}

pub struct App {
    pub mode: SearchMode,
    search_path: PathBuf,

    /// 当前打开的弹窗（路径 / 忽略目录 / 大小上限），三者共用一套输入状态
    pub popup: Option<PopupKind>,
    /// 弹窗输入框内容
    pub popup_input: String,
    /// 弹窗光标（按字符计）
    pub popup_cursor: usize,
    /// 弹窗全选状态（打开时旧值整体选中，输入即替换）
    pub popup_select_all: bool,
    /// 弹窗校验错误（展示在弹窗内，编辑时自动清除）
    pub popup_error: Option<String>,

    pub input: String,
    /// 光标位置（按字符计）
    pub cursor: usize,
    /// 顶部搜索输入框是否展开（Ctrl+H 切换）：
    /// 展开时高度最大为页面 1/3（超出滚动查看）；折叠时单行高度、超出以省略号截断
    pub input_expanded: bool,
    /// 展开态下输入框内容超出可视区时的滚动偏移（顶部可见折行号，由绘制层维护）
    pub input_scroll: usize,
    /// 搜索输入框内容区宽度（由绘制层维护）：↑/↓ 按视觉折行上下移动光标时需要它来计算折行
    pub input_inner_width: usize,

    pub results: Vec<SearchResultItem>,
    pub list_state: ListState,
    /// 文件列表滚动窗口起点（由绘制层维持）
    pub list_offset: usize,

    pub preview: Option<Rc<Text<'static>>>,
    pub preview_path: Option<PathBuf>,
    pub preview_scroll: u16,
    pub preview_max_scroll: u16,
    pub preview_loading: bool,
    /// 最近一次绘制时预览区内宽（决定 bat 的换行宽度）
    pub preview_width: u16,
    pub size_changed: bool,

    pub status: Option<(String, Instant)>,
    pub should_quit: bool,
    /// 搜索是否进行中（fd 建索引中 / rg 结果流式到达中）
    pub searching: bool,
    /// 产生当前结果集的查询词（输入框内容可能已被改动、尚未再搜索）
    pub last_query: String,

    /// 内容搜索额外忽略目录（默认必忽略目录之外；Ctrl+I 编辑，持久化）
    pub ignore_dirs: Vec<String>,
    /// 内容搜索单文件大小上限（MB；Ctrl+S 编辑，持久化）
    pub max_file_size_mb: f64,
    /// 配置文件路径（`~/.config/sift/config.toml`）
    config_path: Option<PathBuf>,

    search_gen: u64,
    preview_gen: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    file_lists: HashMap<PathBuf, (Instant, Arc<Vec<String>>)>,
    preview_cache: HashMap<(PathBuf, u16), Rc<Text<'static>>>,
    /// 当前搜索任务句柄（用于取消）
    job: Option<SearchJob>,
}

impl App {
    pub fn new() -> Self {
        // 默认搜索路径：启动程序时所在的当前目录
        let search_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let (tx, rx) = mpsc::channel();
        let cfg = config::load();
        Self {
            mode: SearchMode::FileName,
            search_path,
            popup: None,
            popup_input: String::new(),
            popup_cursor: 0,
            popup_select_all: false,
            popup_error: None,
            input: String::new(),
            cursor: 0,
            input_expanded: true,
            input_scroll: 0,
            input_inner_width: 1,
            results: Vec::new(),
            list_state: ListState::default(),
            list_offset: 0,
            preview: None,
            preview_path: None,
            preview_scroll: 0,
            preview_max_scroll: 0,
            preview_loading: false,
            preview_width: 80,
            size_changed: false,
            status: None,
            should_quit: false,
            searching: false,
            last_query: String::new(),
            ignore_dirs: cfg.ignore_dirs,
            max_file_size_mb: cfg.max_file_size_mb,
            config_path: config::config_path(),
            search_gen: 0,
            preview_gen: 0,
            tx,
            rx,
            file_lists: HashMap::new(),
            preview_cache: HashMap::new(),
            job: None,
        }
    }

    pub fn current_path(&self) -> &PathBuf {
        &self.search_path
    }

    /// 输入框内容自上次搜索后是否有改动（决定是否显示 "Enter 搜索" 提示）
    pub fn input_dirty(&self) -> bool {
        self.input.trim() != self.last_query.as_str()
    }

    /// 展示用路径：家目录前缀缩成 ~
    pub fn current_path_display(&self) -> String {
        let p = self.current_path();
        if let Ok(home) = std::env::var("HOME") {
            let s = p.to_string_lossy();
            if s.as_ref() == home {
                return "~".to_string();
            }
            if let Some(rest) = s.strip_prefix(&(home.clone() + "/")) {
                return format!("~/{rest}");
            }
        }
        p.display().to_string()
    }

    pub fn selected_item(&self) -> Option<&SearchResultItem> {
        self.list_state.selected().and_then(|i| self.results.get(i))
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// 文件大小上限换算为字节（rg --max-filesize 只接受整数字节）
    fn max_filesize_bytes(&self) -> u64 {
        (self.max_file_size_mb.max(0.0) * 1024.0 * 1024.0) as u64
    }

    // ------------------------------------------------------------ 按键处理

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        // 弹窗打开时，按键全部交给弹窗处理
        if self.popup.is_some() {
            self.on_popup_key(key);
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // Esc：搜索进行中 -> 取消当前搜索；否则退出程序
            KeyCode::Esc => self.cancel_or_quit(),
            // Ctrl+C：复制选中文件的完整路径到系统剪贴板（不再用于取消/退出，退出用 Esc）
            KeyCode::Char('c') if ctrl => self.copy_selected_path(),
            // Tab 切换模式：即便搜索进行中也会先取消旧搜索再切换重搜（不再阻塞等待）
            KeyCode::Tab => {
                self.mode.toggle();
                self.trigger_search_now();
            }
            KeyCode::Char('p') if ctrl => self.open_popup(PopupKind::Path),
            // Ctrl+H 展开/折叠顶部搜索输入框（折叠=单行截断，展开=最高 1/3 屏、可滚动）
            KeyCode::Char('h') if ctrl => {
                self.input_expanded = !self.input_expanded;
                self.input_scroll = 0;
            }
            // 忽略目录 / 大小上限对文件名搜索（fd）与内容搜索（rg）同样生效
            KeyCode::Char('i') if ctrl => self.open_popup(PopupKind::IgnoreDirs),
            KeyCode::Char('s') if ctrl => self.open_popup(PopupKind::MaxSize),
            KeyCode::Char('j') if ctrl => self.scroll_preview(3),
            KeyCode::Char('k') if ctrl => self.scroll_preview(-3),
            KeyCode::PageDown => self.scroll_preview(10),
            KeyCode::PageUp => self.scroll_preview(-10),
            // Alt+J / Alt+K：选择下一个 / 上一个文件。
            // 多数终端上报为小写+ALT，少数上报为大写字母+ALT，两者都兼容。
            KeyCode::Char('j') if alt => self.move_selection(1),
            KeyCode::Char('k') if alt => self.move_selection(-1),
            KeyCode::Char('J') if alt => self.move_selection(1),
            KeyCode::Char('K') if alt => self.move_selection(-1),
            // ↑/↓：展开态下在搜索输入框中按视觉折行上下移动光标（折叠态不处理）
            KeyCode::Down => self.move_input_cursor_vertical(1),
            KeyCode::Up => self.move_input_cursor_vertical(-1),
            // Shift+Enter 插入真实换行（多行查询；内容搜索自动启用 --multiline）
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let byte = char_byte(&self.input, self.cursor);
                self.input.insert(byte, '\n');
                self.cursor += 1;
            }
            // Enter 触发搜索：若上一次还在跑，会先 kill 旧进程再搜新关键词
            KeyCode::Enter => self.trigger_search_now(),
            KeyCode::Char('g') if ctrl => {
                if let Some(item) = self.selected_item() {
                    return Some(Action::Open(item.path.clone()));
                }
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let byte = char_byte(&self.input, self.cursor);
                    self.input.remove(byte);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let byte = char_byte(&self.input, self.cursor);
                    self.input.remove(byte);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let byte = char_byte(&self.input, self.cursor);
                self.input.insert(byte, c);
                self.cursor += 1;
            }
            _ => {}
        }
        None
    }

    /// Esc / Ctrl+C：搜索中取消搜索（kill 子进程），否则退出程序
    fn cancel_or_quit(&mut self) {
        if self.searching {
            self.cancel_current_job();
            self.set_status("已取消当前搜索");
        } else {
            self.should_quit = true;
        }
    }

    /// Ctrl+C：把选中文件的完整路径复制到系统剪贴板（wl-copy/xclip/xsel）。
    /// 无选中文件或复制失败时以 toast 提示原因。
    fn copy_selected_path(&mut self) {
        let Some(item) = self.selected_item() else {
            self.set_status("无选中文件，无法复制路径");
            return;
        };
        let path = item.path.display().to_string();
        match crate::clipboard::copy(&path) {
            Ok(()) => self.set_status("已复制文件路径到剪贴板"),
            Err(e) => self.set_status(format!("复制失败：{e}")),
        }
    }

    /// 粘贴事件（bracketed paste）：原始文本可能含换行/制表符/反斜杠，
    /// 编码为查询转义形式后插入（多行文本 -> \n 序列，自动启用 --multiline）
    pub fn on_paste(&mut self, text: &str) {
        if self.popup.is_some() {
            // 忽略目录为多行输入：保留换行（\r\n / \r 归一化为 \n），其余控制字符丢弃；
            // 路径 / 大小上限为单行，换行及其他控制字符（ESC 等）直接丢弃，防止花屏
            let multiline = matches!(self.popup, Some(PopupKind::IgnoreDirs));
            let cleaned: String = if multiline {
                let mut out = String::with_capacity(text.len());
                let mut chars = text.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        '\n' => out.push('\n'),
                        '\r' => {
                            if chars.peek() == Some(&'\n') {
                                chars.next();
                            }
                            out.push('\n');
                        }
                        c if c.is_control() => {}
                        c => out.push(c),
                    }
                }
                out
            } else {
                text.chars().filter(|c| !c.is_control()).collect()
            };
            let n = cleaned.chars().count();
            if self.popup_select_all {
                // 全选状态下粘贴 = 整体替换
                self.popup_input = cleaned;
                self.popup_cursor = n;
                self.popup_select_all = false;
            } else {
                let byte = char_byte(&self.popup_input, self.popup_cursor);
                self.popup_input.insert_str(byte, &cleaned);
                self.popup_cursor += n;
            }
            self.popup_error = None;
            return;
        }
        let encoded = search::encode_paste(text);
        if encoded.is_empty() {
            return;
        }
        let n = encoded.chars().count();
        let byte = char_byte(&self.input, self.cursor);
        self.input.insert_str(byte, &encoded);
        self.cursor += n;
    }

    // ------------------------------------------------------------ 编辑弹窗（路径 / 忽略目录 / 大小上限）

    /// 打开弹窗，按类型预填当前值并全选（方便直接输入替换）
    fn open_popup(&mut self, kind: PopupKind) {
        self.popup_input = match kind {
            PopupKind::Path => self.current_path_display(),
            PopupKind::IgnoreDirs => self.ignore_dirs.join("\n"),
            PopupKind::MaxSize => format_size_mb(self.max_file_size_mb),
        };
        self.popup_cursor = self.popup_input.chars().count();
        self.popup_select_all = true;
        self.popup_error = None;
        self.popup = Some(kind);
    }

    fn on_popup_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // 除 Enter（重新校验）外，任何按键都清除上一次的校验错误
        if key.code != KeyCode::Enter {
            self.popup_error = None;
        }
        // 全选状态下，编辑类按键先作用于整个选区，移动类按键取消选区
        if self.popup_select_all {
            match key.code {
                KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    // 大小上限弹窗只接受数字与小数点
                    if matches!(self.popup, Some(PopupKind::MaxSize))
                        && !(c.is_ascii_digit() || c == '.')
                    {
                        self.popup_select_all = false;
                        return;
                    }
                    self.popup_input.clear();
                    self.popup_input.push(c);
                    self.popup_cursor = 1;
                    self.popup_select_all = false;
                    return;
                }
                KeyCode::Backspace | KeyCode::Delete => {
                    self.popup_input.clear();
                    self.popup_cursor = 0;
                    self.popup_select_all = false;
                    return;
                }
                KeyCode::Left | KeyCode::Home => {
                    self.popup_cursor = 0;
                    self.popup_select_all = false;
                    return;
                }
                KeyCode::Right | KeyCode::End => {
                    self.popup_cursor = self.popup_input.chars().count();
                    self.popup_select_all = false;
                    return;
                }
                // 其余按键（Enter / Esc / Ctrl+U / Ctrl+W 等）：取消全选后走正常逻辑
                _ => self.popup_select_all = false,
            }
        }
        match key.code {
            KeyCode::Esc => self.popup = None,
            // Shift+Enter 插入真实换行（仅忽略目录为多行；路径/大小上限等同 Enter 确认）
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if matches!(self.popup, Some(PopupKind::IgnoreDirs)) {
                    let byte = char_byte(&self.popup_input, self.popup_cursor);
                    self.popup_input.insert(byte, '\n');
                    self.popup_cursor += 1;
                } else {
                    self.confirm_popup();
                }
            }
            KeyCode::Enter => self.confirm_popup(),
            KeyCode::Backspace => {
                if self.popup_cursor > 0 {
                    self.popup_cursor -= 1;
                    let byte = char_byte(&self.popup_input, self.popup_cursor);
                    self.popup_input.remove(byte);
                }
            }
            KeyCode::Delete => {
                if self.popup_cursor < self.popup_input.chars().count() {
                    let byte = char_byte(&self.popup_input, self.popup_cursor);
                    self.popup_input.remove(byte);
                }
            }
            KeyCode::Left => self.popup_cursor = self.popup_cursor.saturating_sub(1),
            KeyCode::Right => {
                self.popup_cursor = (self.popup_cursor + 1).min(self.popup_input.chars().count())
            }
            KeyCode::Home => self.popup_cursor = 0,
            KeyCode::End => self.popup_cursor = self.popup_input.chars().count(),
            KeyCode::Char('u') if ctrl => {
                self.popup_input.clear();
                self.popup_cursor = 0;
            }
            // 删除到上一个分隔符（路径为 '/'，忽略目录为换行）不含分隔符本身
            KeyCode::Char('w') if ctrl => {
                let sep = match self.popup {
                    Some(PopupKind::IgnoreDirs) => '\n',
                    _ => '/',
                };
                while self.popup_cursor > 0 {
                    if self.popup_input.chars().nth(self.popup_cursor - 1) == Some(sep) {
                        break;
                    }
                    self.popup_cursor -= 1;
                    let byte = char_byte(&self.popup_input, self.popup_cursor);
                    self.popup_input.remove(byte);
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                // 大小上限弹窗只接受数字与小数点
                if matches!(self.popup, Some(PopupKind::MaxSize))
                    && !(c.is_ascii_digit() || c == '.')
                {
                    return;
                }
                let byte = char_byte(&self.popup_input, self.popup_cursor);
                self.popup_input.insert(byte, c);
                self.popup_cursor += 1;
            }
            _ => {}
        }
    }

    fn confirm_popup(&mut self) {
        match self.popup {
            Some(PopupKind::Path) => self.confirm_path(),
            Some(PopupKind::IgnoreDirs) => self.confirm_ignore_dirs(),
            Some(PopupKind::MaxSize) => self.confirm_max_size(),
            None => {}
        }
    }

    /// 确认路径：真实存在且为可读目录才切换并重新搜索，否则保持弹窗并提示原因
    fn confirm_path(&mut self) {
        let raw = self.popup_input.trim().to_string();
        if raw.is_empty() {
            self.popup = None;
            self.popup_error = None;
            return;
        }
        match validate_search_path(&raw) {
            Ok(path) => {
                self.search_path = path;
                self.popup = None;
                self.popup_error = None;
                self.trigger_search_now();
            }
            Err(msg) => self.popup_error = Some(msg),
        }
    }

    /// 确认忽略目录：换行分隔（Shift+Enter 或输入 \n），逐个校验为真实存在的目录；空输入 = 清空额外忽略
    fn confirm_ignore_dirs(&mut self) {
        let decoded = search::decode_escapes(&self.popup_input);
        let mut dirs: Vec<String> = Vec::new();
        for part in decoded.split('\n') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            let expanded = expand_tilde(p);
            match std::fs::metadata(&expanded) {
                Ok(md) if md.is_dir() => {
                    let canon = std::fs::canonicalize(&expanded)
                        .unwrap_or_else(|_| PathBuf::from(&expanded));
                    dirs.push(canon.to_string_lossy().into_owned());
                }
                Ok(_) => {
                    self.popup_error = Some(format!("不是目录: {p}"));
                    return;
                }
                Err(_) => {
                    self.popup_error = Some(format!("路径不存在: {p}"));
                    return;
                }
            }
        }
        dirs.dedup();
        self.ignore_dirs = dirs;
        self.popup = None;
        self.popup_error = None;
        self.save_config();
        self.research_after_settings_change();
    }

    /// 确认大小上限：仅接受正整数或正小数（单位 MB），如 10 或 0.2
    fn confirm_max_size(&mut self) {
        let raw = self.popup_input.trim().to_string();
        let dots = raw.bytes().filter(|&b| b == b'.').count();
        let well_formed = !raw.is_empty()
            && raw != "."
            && dots <= 1
            && raw.chars().all(|c| c.is_ascii_digit() || c == '.');
        let val: f64 = raw.parse().unwrap_or(-1.0);
        if !well_formed || !val.is_finite() || val <= 0.0 {
            self.popup_error = Some(format!("无效大小: {raw}（示例：10 或 0.2）"));
            return;
        }
        self.max_file_size_mb = val;
        self.popup = None;
        self.popup_error = None;
        self.save_config();
        self.research_after_settings_change();
    }

    /// 将当前忽略目录与大小上限写入配置文件
    fn save_config(&mut self) {
        let cfg = config::Config {
            max_file_size_mb: self.max_file_size_mb,
            ignore_dirs: self.ignore_dirs.clone(),
        };
        if let Some(path) = &self.config_path {
            if let Err(e) = config::save_to(path, &cfg) {
                self.set_status(format!("配置保存失败: {e}"));
            }
        }
    }

    /// 忽略目录 / 大小上限对文件名搜索（fd）与内容搜索（rg）同样生效。
    /// 变更后：清空文件列表缓存（fd 结果依赖这两项设置），若有查询词则立即重搜生效。
    fn research_after_settings_change(&mut self) {
        self.file_lists.clear();
        if !self.last_query.is_empty() {
            self.trigger_search_now();
        }
    }

    // ------------------------------------------------------------ 主循环节拍

    pub fn tick(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::SearchBatch { gen, items } => {
                    if gen != self.search_gen {
                        continue;
                    }
                    let prev_path = self.selected_item().map(|i| i.path.clone());
                    self.results.extend(items);
                    self.results.sort_by(|a, b| {
                        b.score
                            .cmp(&a.score)
                            .then_with(|| a.priority.cmp(&b.priority))
                            .then_with(|| a.display.cmp(&b.display))
                    });
                    self.results.truncate(MAX_RESULTS);
                    // 截断可能让选中索引越界，收敛到最后一项
                    if let Some(sel) = self.list_state.selected() {
                        if !self.results.is_empty() && sel >= self.results.len() {
                            self.list_state.select(Some(self.results.len() - 1));
                        }
                    }
                    if self.list_state.selected().is_none() && !self.results.is_empty() {
                        self.select_first();
                    } else if self.selected_item().map(|i| i.path.clone()) != prev_path {
                        // 流式结果重排序把选中项“挤”到了别的文件上，
                        // 预览必须跟着刷新，否则出现路径已变、预览内容未变的错位
                        self.preview_scroll = 0;
                        self.request_preview();
                    }
                    // 结果已够，停掉后台 rg
                    if self.results.len() >= MAX_RESULTS {
                        if let Some(j) = &self.job {
                            j.cancel();
                        }
                    }
                }
                Msg::SearchDone {
                    gen,
                    content,
                    items,
                    new_file_list,
                } => {
                    if let Some((root, list)) = new_file_list {
                        self.file_lists.insert(root, (Instant::now(), list));
                    }
                    if gen == self.search_gen {
                        self.searching = false;
                        if !content {
                            self.results = items;
                            self.select_first();
                        }
                    }
                }
                Msg::PreviewDone {
                    gen,
                    path,
                    width,
                    text,
                } => {
                    if self.preview_cache.len() >= PREVIEW_CACHE_MAX {
                        self.preview_cache.clear();
                    }
                    let text = Rc::new(text);
                    self.preview_cache
                        .insert((path.clone(), width), text.clone());
                    if gen == self.preview_gen && width == self.preview_width {
                        self.preview = Some(text);
                        self.preview_path = Some(path);
                        self.preview_loading = false;
                    }
                }
            }
        }
        if let Some((_, t)) = &self.status {
            if t.elapsed() > STATUS_TTL {
                self.status = None;
            }
        }
    }

    /// 终端尺寸变化后调用：用新宽度重新渲染预览
    pub fn after_resize(&mut self) {
        if self.preview_path.is_some() {
            self.request_preview();
        }
    }

    // ------------------------------------------------------------ 搜索

    fn trigger_search_now(&mut self) {
        self.dispatch_search();
    }

    fn dispatch_search(&mut self) {
        // 先停掉可能还在跑的旧搜索（kill 子进程，非阻塞），并令在途消息作废
        self.cancel_current_job();
        self.search_gen += 1;
        let query = self.input.trim().to_string();
        self.last_query = query.clone();
        if query.is_empty() {
            self.searching = false;
            self.results.clear();
            self.select_first();
            return;
        }
        let gen = self.search_gen;
        let tx = self.tx.clone();
        let root = self.current_path().clone();
        let job = SearchJob::new();
        self.job = Some(job.clone());
        match self.mode {
            SearchMode::FileName => {
                // fd 全量扫描可能耗时，期间结果区显示“搜索中…”
                self.searching = true;
                let cached = self
                    .file_lists
                    .get(&root)
                    .and_then(|(t, l)| (t.elapsed() < FILE_LIST_TTL).then(|| l.clone()));
                // 与内容搜索一致：忽略目录（Ctrl+I）与大小上限（Ctrl+S）同样作用于 fd
                let excludes = Arc::new(search::fd_excludes(&root, &self.ignore_dirs));
                let max_bytes = self.max_filesize_bytes();
                thread::spawn(move || {
                    fd_search_job(root, query, cached, excludes, max_bytes, job, gen, tx);
                });
            }
            SearchMode::Content => {
                // 流式搜索：先清空旧结果，结果分批到达
                self.results.clear();
                self.list_state.select(None);
                self.preview = None;
                self.preview_path = None;
                self.searching = true;
                let globs = Arc::new(search::rg_exclude_globs(&root, &self.ignore_dirs));
                let max_bytes = self.max_filesize_bytes();
                thread::spawn(move || {
                    rg_stream(root, query, globs, max_bytes, job, gen, tx);
                });
            }
        }
    }

    /// 停掉正在运行的搜索：置取消标志 + kill 子进程，并令在途消息作废。
    /// 不 join 后台线程（其会因子进程被 kill、管道 EOF 而自行退出），保证 UI 不阻塞。
    fn cancel_current_job(&mut self) {
        self.searching = false;
        self.search_gen += 1;
        if let Some(j) = self.job.take() {
            j.cancel();
        }
    }

    /// 退出前调用：确保后台子进程被清理
    pub fn shutdown(&mut self) {
        self.cancel_current_job();
    }

    // ------------------------------------------------------------ 列表与预览

    fn select_first(&mut self) {
        if self.results.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
        self.list_offset = 0;
        self.preview_scroll = 0;
        self.request_preview();
    }

    /// 保证选中项落在列表可视窗口内（由绘制层按可见高度调用）
    pub fn ensure_list_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        match self.list_state.selected() {
            Some(sel) => {
                if sel < self.list_offset {
                    self.list_offset = sel;
                } else if sel >= self.list_offset + height {
                    self.list_offset = sel + 1 - height;
                }
            }
            None => self.list_offset = 0,
        }
        let max_offset = self.results.len().saturating_sub(height);
        self.list_offset = self.list_offset.min(max_offset);
    }

    fn move_selection(&mut self, delta: i64) {
        if self.results.is_empty() {
            return;
        }
        let len = self.results.len() as i64;
        let cur = self.list_state.selected().unwrap_or(0) as i64;
        let next = (cur + delta).clamp(0, len - 1) as usize;
        if Some(next) != self.list_state.selected() {
            self.list_state.select(Some(next));
            self.preview_scroll = 0;
            self.request_preview();
        }
    }

    /// ↑/↓：展开态下在搜索输入框中按视觉折行上下移动光标（尽量保持原列）；折叠态不处理。
    fn move_input_cursor_vertical(&mut self, delta: isize) {
        if !self.input_expanded {
            return;
        }
        let width = self.input_inner_width.max(1);
        self.cursor = ui::move_cursor_vertical(&self.input, self.cursor, width, true, delta);
    }

    fn scroll_preview(&mut self, delta: i32) {
        let cur = self.preview_scroll as i32;
        let max = self.preview_max_scroll as i32;
        self.preview_scroll = (cur + delta).clamp(0, max) as u16;
    }

    pub(crate) fn request_preview(&mut self) {
        self.preview_gen += 1;
        let Some(item) = self.selected_item() else {
            self.preview = None;
            self.preview_path = None;
            self.preview_loading = false;
            return;
        };
        let path = item.path.clone();
        let width = self.preview_width.max(10);
        if let Some(t) = self.preview_cache.get(&(path.clone(), width)) {
            self.preview = Some(t.clone());
            self.preview_path = Some(path);
            self.preview_loading = false;
            return;
        }
        self.preview = None;
        self.preview_loading = true;
        self.preview_path = Some(path.clone());
        let gen = self.preview_gen;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let text = preview::render(&path, width);
            let _ = tx.send(Msg::PreviewDone {
                gen,
                path,
                width,
                text,
            });
        });
    }
}

/// 第 char_idx 个字符的字节偏移
fn char_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

/// 大小（MB）展示文本：整数去掉小数点（10.0 -> "10"），小数原样（0.2 -> "0.2"）
fn format_size_mb(mb: f64) -> String {
    if mb.fract() == 0.0 && mb.is_finite() {
        format!("{}", mb as u64)
    } else {
        format!("{mb}")
    }
}

/// 文件名搜索线程：命中缓存则纯内存过滤；否则拉起 fd 流式读取文件列表。
/// 被取消时丢弃不完整列表、不写缓存。
#[allow(clippy::too_many_arguments)]
fn fd_search_job(
    root: PathBuf,
    query: String,
    cached: Option<Arc<Vec<String>>>,
    excludes: Arc<Vec<String>>,
    max_bytes: u64,
    job: SearchJob,
    gen: u64,
    tx: Sender<Msg>,
) {
    if let Some(list) = cached {
        let items = search::filter_fd_list(&root, &query, &list);
        let _ = tx.send(Msg::SearchDone {
            gen,
            content: false,
            items,
            new_file_list: None,
        });
        return;
    }
    let mut child = match search::fd_cmd(&root, &excludes, max_bytes)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            let _ = tx.send(Msg::SearchDone {
                gen,
                content: false,
                items: Vec::new(),
                new_file_list: None,
            });
            return;
        }
    };
    let stdout = child.stdout.take();
    if let Ok(mut g) = job.child.lock() {
        *g = Some(child);
    }
    let mut files: Vec<String> = Vec::new();
    let mut cancelled = false;
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if job.cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            if !line.is_empty() {
                files.push(line);
            }
        }
    }
    // 回收子进程
    if let Ok(mut g) = job.child.lock() {
        if let Some(mut c) = g.take() {
            let _ = c.wait();
        }
    }
    if cancelled {
        return;
    }
    let arc = Arc::new(files);
    let items = search::filter_fd_list(&root, &query, &arc);
    let _ = tx.send(Msg::SearchDone {
        gen,
        content: false,
        items,
        new_file_list: Some((root, arc)),
    });
}

/// 内容搜索线程：流式读取 rg 输出，按背压阈值（100 条或 16ms）分批回传结果。
/// 取消时 kill rg（由 UI 线程经 job.child 触发），本线程读到 EOF 后退出。
fn rg_stream(
    root: PathBuf,
    query: String,
    globs: Arc<Vec<String>>,
    max_bytes: u64,
    job: SearchJob,
    gen: u64,
    tx: Sender<Msg>,
) {
    let mut child = match search::rg_cmd(&root, &query, &globs, max_bytes)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            let _ = tx.send(Msg::SearchDone {
                gen,
                content: true,
                items: Vec::new(),
                new_file_list: None,
            });
            return;
        }
    };
    let stdout = child.stdout.take();
    if let Ok(mut g) = job.child.lock() {
        *g = Some(child);
    }

    let mut batch: Vec<SearchResultItem> = Vec::new();
    let mut last_flush = Instant::now();
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if job.cancel.load(Ordering::Relaxed) {
                break;
            }
            if let Some(item) = search::parse_count_line(&root, &line) {
                batch.push(item);
            }
            if batch.len() >= BATCH_SIZE
                || (!batch.is_empty() && last_flush.elapsed() >= BATCH_INTERVAL)
            {
                if tx
                    .send(Msg::SearchBatch {
                        gen,
                        items: std::mem::take(&mut batch),
                    })
                    .is_err()
                {
                    break;
                }
                last_flush = Instant::now();
            }
        }
    }
    if !batch.is_empty() {
        let _ = tx.send(Msg::SearchBatch {
            gen,
            items: std::mem::take(&mut batch),
        });
    }
    if let Ok(mut g) = job.child.lock() {
        if let Some(mut c) = g.take() {
            let _ = c.wait();
        }
    }
    let _ = tx.send(Msg::SearchDone {
        gen,
        content: true,
        items: Vec::new(),
        new_file_list: None,
    });
}

/// 校验用户输入的搜索路径，成功返回规范化后的目录路径，失败返回错误原因
fn validate_search_path(raw: &str) -> Result<PathBuf, String> {
    let expanded = expand_tilde(raw);
    // 纯斜杠输入（"//"、"/////"…）：Linux 会将其归一化为 "/" 而误判合法，直接拒绝
    if expanded.len() > 1 && expanded.chars().all(|c| c == '/') {
        return Err(format!("路径无效: {raw}"));
    }
    let path = PathBuf::from(&expanded);
    match std::fs::metadata(&path) {
        Ok(md) if !md.is_dir() => Err(format!("不是目录: {raw}")),
        Ok(_) => {
            // 顶层目录不可读时搜索毫无意义，提前告知
            if let Err(e) = std::fs::read_dir(&path) {
                return Err(format!("无法读取目录: {raw} ({e})"));
            }
            // 规范化：解析 ..、符号链接与多余斜杠，避免同一目录产生多份缓存
            Ok(std::fs::canonicalize(&path).unwrap_or(path))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("路径不存在: {raw}")),
        Err(e) => Err(format!("路径无效: {raw} ({e})")),
    }
}

/// 展开路径开头的 ~ 为家目录
fn expand_tilde(s: &str) -> String {
    let home = || std::env::var("HOME").ok();
    if s == "~" {
        return home().unwrap_or_else(|| s.to_string());
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = home() {
            return format!("{home}/{rest}");
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, display: &str, score: i64) -> SearchResultItem {
        SearchResultItem {
            path: PathBuf::from(path),
            display: display.to_string(),
            matches: 1,
            score,
            priority: search::path_priority(std::path::Path::new(path)),
        }
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn shift_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
    }

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    /// 搜索进行中按 Enter：取消旧搜索并立即重搜（gen 递增、仍处于搜索中）
    #[test]
    fn enter_while_searching_cancels_and_restarts() {
        let mut app = App::new();
        app.input = "test".to_string();
        app.searching = true;
        let gen = app.search_gen;
        app.on_key(enter());
        assert!(app.search_gen > gen, "应派发新搜索");
        assert!(app.searching, "重新搜索后应处于搜索中");
        assert_eq!(app.last_query, "test");
    }

    /// 搜索进行中按 Tab：取消旧搜索、切换模式并重新搜索（不再阻塞等待）
    #[test]
    fn tab_while_searching_toggles_and_restarts() {
        let mut app = App::new();
        app.input = "test".to_string();
        app.searching = true;
        let gen = app.search_gen;
        let mode = app.mode;
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_ne!(app.mode, mode, "Tab 应切换模式");
        assert!(app.search_gen > gen, "Tab 应派发新搜索");
    }

    /// Esc：搜索中取消搜索（不退出）；非搜索时退出程序
    #[test]
    fn esc_cancels_search_or_quits() {
        let mut app = App::new();
        app.searching = true;
        app.on_key(esc());
        assert!(!app.should_quit, "搜索中 Esc 不应退出");
        assert!(!app.searching, "搜索中 Esc 应取消搜索");

        app.on_key(esc());
        assert!(app.should_quit, "非搜索时 Esc 应退出");
    }

    /// Ctrl+C 用于复制选中文件路径：任何情况下都不退出程序，也不取消搜索
    #[test]
    fn ctrl_c_copies_path_and_never_quits() {
        // 无选中文件：不退出，提示无法复制
        let mut app = App::new();
        app.on_key(ctrl_c());
        assert!(!app.should_quit, "Ctrl+C 不应退出程序");
        assert!(app.status.is_some(), "无选中文件应给出提示");

        // 有选中文件：不退出，且给出 toast（成功/失败取决于环境，都算有反馈）
        let mut app = App::new();
        app.results = vec![item("/home/heng/test.sh", "home/heng/test.sh", 10)];
        app.list_state.select(Some(0));
        app.on_key(ctrl_c());
        assert!(!app.should_quit, "Ctrl+C 不应退出程序");
        assert!(app.status.is_some(), "复制后应有 toast 反馈");
    }

    /// Ctrl+C 不再取消搜索（取消搜索改用 Esc）：搜索中按 Ctrl+C 仅复制路径
    #[test]
    fn ctrl_c_does_not_cancel_search() {
        let mut app = App::new();
        app.searching = true;
        app.results = vec![item("/home/heng/test.sh", "home/heng/test.sh", 10)];
        app.list_state.select(Some(0));
        app.on_key(ctrl_c());
        assert!(app.searching, "Ctrl+C 不应取消搜索");
        assert!(!app.should_quit, "Ctrl+C 不应退出");
    }

    /// 流式批次到达后结果重排序，选中项被“挤”到别的文件时预览必须跟着刷新，
    /// 否则出现选中路径已变、预览内容还是旧文件的错位
    #[test]
    fn search_batch_resort_refreshes_preview() {
        let mut app = App::new();
        app.searching = true;
        // 第一批：只有 usr/share/test.sh，被选中且预览已展示其内容
        app.results = vec![item("/usr/share/test.sh", "usr/share/test.sh", 1)];
        app.list_state.select(Some(0));
        app.preview = Some(Rc::new(Text::from("old content")));
        app.preview_path = Some(PathBuf::from("/usr/share/test.sh"));

        // 第二批：分数更高的 home/heng/test.sh 到达，重排后成为新的第 0 项
        let gen = app.search_gen;
        app.tx
            .send(Msg::SearchBatch {
                gen,
                items: vec![item("/home/heng/test.sh", "home/heng/test.sh", 10)],
            })
            .unwrap();
        app.tick();

        assert_eq!(
            app.selected_item().map(|i| i.path.as_path()),
            Some(std::path::Path::new("/home/heng/test.sh"))
        );
        // 预览目标必须切换到新选中项，旧内容不得继续展示
        assert_eq!(
            app.preview_path.as_deref(),
            Some(std::path::Path::new("/home/heng/test.sh"))
        );
        assert!(app.preview.is_none(), "旧预览内容必须被丢弃");
        assert!(app.preview_loading);
    }

    /// 重排序后选中项未变化时，不应重复请求预览（避免闪烁）
    #[test]
    fn search_batch_same_selection_keeps_preview() {
        let mut app = App::new();
        app.searching = true;
        app.results = vec![item("/home/heng/test.sh", "home/heng/test.sh", 10)];
        app.list_state.select(Some(0));
        app.preview = Some(Rc::new(Text::from("current content")));
        app.preview_path = Some(PathBuf::from("/home/heng/test.sh"));

        let gen = app.search_gen;
        app.tx
            .send(Msg::SearchBatch {
                gen,
                items: vec![item("/usr/share/test.sh", "usr/share/test.sh", 1)],
            })
            .unwrap();
        app.tick();

        // 第 0 项未变，预览原样保留
        assert!(app.preview.is_some());
        assert!(!app.preview_loading);
    }

    /// 取消搜索会递增 gen，使在途的旧批次消息作废，不会把已取消的结果又塞回列表
    #[test]
    fn cancel_invalidates_inflight_batches() {
        let mut app = App::new();
        app.searching = true;
        let old_gen = app.search_gen;
        // 模拟一个在途批次（使用旧 gen）
        let stale = Msg::SearchBatch {
            gen: old_gen,
            items: vec![item("/home/heng/a.txt", "home/heng/a.txt", 5)],
        };
        // 用户按 Esc 取消：gen 递增
        app.on_key(esc());
        assert!(app.search_gen > old_gen);
        // 此后再收到旧 gen 的批次应被忽略
        app.tx.send(stale).unwrap();
        app.tick();
        assert!(app.results.is_empty(), "已取消搜索的在途结果不应进入列表");
    }

    /// 路径确认：纯斜杠（"/////"）在 Linux 上会被归一化为 "/"，必须被拒绝
    #[test]
    fn confirm_path_rejects_slash_only_input() {
        let mut app = App::new();
        let original = app.current_path().clone();
        app.open_popup(PopupKind::Path);
        app.popup_input = "/////".to_string();
        app.on_key(enter());
        assert_eq!(app.popup, Some(PopupKind::Path), "非法路径不应关闭弹窗");
        assert!(app.popup_error.is_some());
        assert_eq!(app.current_path(), &original, "非法路径不应切换搜索目录");

        // 单个 "/" 是合法的根目录
        app.popup_input = "/".to_string();
        app.popup_error = None;
        app.on_key(enter());
        assert_eq!(app.popup, None);
        assert_eq!(app.current_path(), std::path::Path::new("/"));
    }

    /// 路径确认：不存在的路径 / 文件而非目录，都要拒绝并提示
    #[test]
    fn confirm_path_rejects_nonexistent_and_file() {
        let mut app = App::new();
        app.open_popup(PopupKind::Path);

        app.popup_input = "/nonexistent/dir/xxxx".to_string();
        app.on_key(enter());
        assert_eq!(app.popup, Some(PopupKind::Path));
        assert!(app.popup_error.as_deref().unwrap().contains("路径不存在"));

        // 存在但不是目录
        let file = std::env::temp_dir().join(format!("sift-test-{}", std::process::id()));
        std::fs::write(&file, "x").unwrap();
        app.popup_input = file.to_string_lossy().to_string();
        app.popup_error = None;
        app.on_key(enter());
        assert_eq!(app.popup, Some(PopupKind::Path));
        assert!(app.popup_error.as_deref().unwrap().contains("不是目录"));
        std::fs::remove_file(&file).ok();
    }

    /// 路径确认：合法目录接受并规范化（解析 .. 与多余斜杠），编辑时错误自动清除
    #[test]
    fn confirm_path_accepts_valid_dir_and_clears_error_on_edit() {
        let mut app = App::new();
        app.open_popup(PopupKind::Path);
        app.popup_input = "/nonexistent/xxxx".to_string();
        app.on_key(enter());
        assert!(app.popup_error.is_some());

        // 任意编辑按键清除错误
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.popup_error.is_none());

        // "/tmp/../tmp/" 规范化后为 "/tmp"（或 /tmp 的真实路径）
        app.popup_input = "/tmp/../tmp/".to_string();
        app.on_key(enter());
        assert_eq!(app.popup, None, "合法目录应关闭弹窗");
        let expected = std::fs::canonicalize("/tmp").unwrap();
        assert_eq!(app.current_path(), &expected);
    }

    /// 打开路径弹窗时旧路径应处于全选状态：直接输入即整体替换
    #[test]
    fn path_popup_opens_with_select_all_and_typing_replaces() {
        let mut app = App::new();
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.popup, Some(PopupKind::Path));
        assert!(app.popup_select_all, "打开弹窗时应全选旧路径");
        assert_eq!(app.popup_input, app.current_path_display());

        // 直接输入一个字符：旧路径被整体替换
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.popup_input, "/");
        assert_eq!(app.popup_cursor, 1);
        assert!(!app.popup_select_all);

        // 选区已取消，后续输入为正常插入
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.popup_input, "/t");
    }

    /// 全选状态下：Backspace/Delete 清空，移动键取消选区，粘贴整体替换
    #[test]
    fn popup_select_all_editing_and_movement() {
        let mut app = App::new();
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        let old_len = app.popup_input.chars().count();

        // 左移：取消选区，光标到开头，内容不变
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!app.popup_select_all);
        assert_eq!(app.popup_cursor, 0);
        assert_eq!(app.popup_input.chars().count(), old_len);

        // 重新全选后 Backspace：清空
        app.popup_select_all = true;
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(app.popup_input.is_empty());
        assert_eq!(app.popup_cursor, 0);
        assert!(!app.popup_select_all);

        // 重新全选后粘贴：整体替换
        app.popup_input = "/old/path".to_string();
        app.popup_select_all = true;
        app.on_paste("/new/path");
        assert_eq!(app.popup_input, "/new/path");
        assert_eq!(app.popup_cursor, "/new/path".chars().count());
        assert!(!app.popup_select_all);
    }

    /// 大小上限弹窗：输入框只接受数字与小数点；确认时校验为正数
    #[test]
    fn maxsize_popup_input_filter_and_validation() {
        let mut app = App::new();
        app.config_path =
            Some(std::env::temp_dir().join(format!("sift-cfg-ms-{}.toml", std::process::id())));
        app.open_popup(PopupKind::MaxSize);
        // 非法字符被输入框直接拒绝
        app.popup_select_all = false;
        app.popup_input.clear();
        app.popup_cursor = 0;
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(!app.popup_input.contains('a'), "字母不应被接受");

        // 合法小数 0.2
        app.popup_input = "0.2".to_string();
        app.on_key(enter());
        assert_eq!(app.popup, None);
        assert!((app.max_file_size_mb - 0.2).abs() < 1e-9);

        // 0 / 多个小数点 / 空 都非法
        for bad in ["0", "1.2.3", "."] {
            app.open_popup(PopupKind::MaxSize);
            app.popup_input = bad.to_string();
            app.on_key(enter());
            assert_eq!(app.popup, Some(PopupKind::MaxSize), "{bad} 应被拒绝");
            assert!(app.popup_error.is_some(), "{bad} 应报错");
        }
        let _ = std::fs::remove_file(app.config_path.as_ref().unwrap());
    }

    /// 忽略目录弹窗：换行分隔（Shift+Enter 或 \n）、逐个校验真实目录；空输入清空；写入配置文件
    #[test]
    fn ignore_dirs_popup_validates_and_persists() {
        let mut app = App::new();
        let cfg_file =
            std::env::temp_dir().join(format!("sift-cfg-ig-{}.toml", std::process::id()));
        app.config_path = Some(cfg_file.clone());
        app.open_popup(PopupKind::IgnoreDirs);

        // 不存在的路径 -> 错误，弹窗不关
        app.popup_input = "/nonexistent/xxxx".to_string();
        app.on_key(enter());
        assert_eq!(app.popup, Some(PopupKind::IgnoreDirs));
        assert!(app.popup_error.is_some());

        // 合法目录（换行分隔，含空行与行首尾空格）-> 接受并保存
        let tmp = std::env::temp_dir();
        app.popup_input = format!("{}\n \n", tmp.display());
        app.popup_error = None;
        app.on_key(enter());
        assert_eq!(app.popup, None);
        let canon = std::fs::canonicalize(&tmp)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(app.ignore_dirs, vec![canon], "应去重并规范化");
        assert!(cfg_file.exists(), "应写入配置文件");

        // 配置文件内容可被读回
        let loaded: config::Config =
            toml::from_str(&std::fs::read_to_string(&cfg_file).unwrap()).unwrap();
        assert_eq!(loaded.ignore_dirs, app.ignore_dirs);

        let _ = std::fs::remove_file(&cfg_file);
    }

    /// 忽略目录改用换行分隔：打开弹窗时以换行连接；输入的 \n 转义与 Shift+Enter 真实换行都生效
    #[test]
    fn ignore_dirs_newline_separated() {
        let mut app = App::new();
        app.config_path =
            Some(std::env::temp_dir().join(format!("sift-cfg-ignl-{}.toml", std::process::id())));
        app.ignore_dirs = vec!["/home".to_string(), "/etc".to_string()];

        // 打开弹窗：旧值以换行连接（不再用逗号）
        app.open_popup(PopupKind::IgnoreDirs);
        assert_eq!(app.popup_input, "/home\n/etc");
        assert!(!app.popup_input.contains(','), "不应再用逗号分隔");

        // 输入的 \n 转义被解析为换行分隔
        let tmp = std::env::temp_dir();
        app.popup_input = format!("{}\\n{}", tmp.display(), tmp.display());
        app.popup_error = None;
        app.on_key(enter());
        assert_eq!(app.popup, None, "\\n 转义应被解析为换行分隔");
        let canon = std::fs::canonicalize(&tmp)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(app.ignore_dirs, vec![canon], "应去重");

        let _ = std::fs::remove_file(app.config_path.as_ref().unwrap());
    }

    /// Ctrl+H 切换顶部搜索输入框展开/折叠（默认展开），并重置滚动偏移
    #[test]
    fn ctrl_h_toggles_input_expand() {
        let mut app = App::new();
        assert!(app.input_expanded, "默认应为展开态");
        app.input_scroll = 5;
        app.on_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert!(!app.input_expanded, "Ctrl+H 应折叠");
        assert_eq!(app.input_scroll, 0, "切换时应重置滚动偏移");
        app.on_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert!(app.input_expanded, "再按 Ctrl+H 应展开");
    }

    /// Shift+Enter 在搜索输入框插入真实换行（不触发搜索）
    #[test]
    fn shift_enter_inserts_newline_in_search_input() {
        let mut app = App::new();
        app.input = "foobar".to_string();
        app.cursor = 3;
        let gen = app.search_gen;
        app.on_key(shift_enter());
        assert_eq!(app.input, "foo\nbar", "Shift+Enter 应插入换行");
        assert_eq!(app.cursor, 4);
        assert_eq!(app.search_gen, gen, "Shift+Enter 不应触发搜索");

        // 普通 Enter 仍触发搜索
        app.on_key(enter());
        assert!(app.search_gen > gen);
    }

    /// Shift+Enter 在忽略目录弹窗插入真实换行（不确认）；路径/大小上限弹窗等同 Enter 确认
    #[test]
    fn shift_enter_in_popups() {
        // 忽略目录：插入换行，弹窗不关
        let mut app = App::new();
        app.open_popup(PopupKind::IgnoreDirs);
        app.popup_select_all = false;
        app.popup_input = "/home".to_string();
        app.popup_cursor = 5;
        app.on_key(shift_enter());
        assert_eq!(app.popup_input, "/home\n");
        assert_eq!(app.popup, Some(PopupKind::IgnoreDirs), "不应确认关闭");

        // 大小上限：Shift+Enter 等同 Enter 确认（合法值关闭弹窗）
        let mut app2 = App::new();
        app2.config_path =
            Some(std::env::temp_dir().join(format!("sift-cfg-se-{}.toml", std::process::id())));
        app2.open_popup(PopupKind::MaxSize);
        app2.popup_input = "5".to_string();
        app2.on_key(shift_enter());
        assert_eq!(app2.popup, None, "大小上限 Shift+Enter 应确认");
        assert!((app2.max_file_size_mb - 5.0).abs() < 1e-9);
        let _ = std::fs::remove_file(app2.config_path.as_ref().unwrap());
    }

    /// 忽略目录弹窗粘贴：保留换行（\r\n 归一化），路径弹窗粘贴仍丢弃换行
    #[test]
    fn popup_paste_newline_handling() {
        let mut app = App::new();
        app.open_popup(PopupKind::IgnoreDirs);
        app.popup_select_all = false;
        app.popup_input.clear();
        app.popup_cursor = 0;
        app.on_paste("/a\r\n/b\n/c");
        assert_eq!(app.popup_input, "/a\n/b\n/c", "忽略目录粘贴应保留换行");

        let mut app2 = App::new();
        app2.open_popup(PopupKind::Path);
        app2.popup_select_all = false;
        app2.popup_input.clear();
        app2.popup_cursor = 0;
        app2.on_paste("/a\n/b");
        assert_eq!(app2.popup_input, "/a/b", "路径粘贴应丢弃换行");
    }

    #[test]
    fn format_size_mb_variants() {
        assert_eq!(format_size_mb(10.0), "10");
        assert_eq!(format_size_mb(0.2), "0.2");
        assert_eq!(format_size_mb(10.5), "10.5");
    }

    /// Ctrl+I / Ctrl+S 在文件名搜索与内容搜索下都打开编辑弹窗（两项设置对 fd/rg 同样生效）
    #[test]
    fn ctrl_i_s_open_popup_in_both_modes() {
        for mode in [SearchMode::FileName, SearchMode::Content] {
            let mut app = App::new();
            app.mode = mode;

            app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
            assert_eq!(app.popup, Some(PopupKind::IgnoreDirs), "mode={mode:?}");

            app.popup = None;
            app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
            assert_eq!(app.popup, Some(PopupKind::MaxSize), "mode={mode:?}");
        }
    }

    /// 在 root 下以文件名模式搜索 query，返回结果文件名列表（等待后台 fd 完成）
    fn run_filename_search(
        root: &std::path::Path,
        query: &str,
        ignore_dirs: &[String],
        max_mb: f64,
    ) -> Vec<String> {
        let mut app = App::new();
        app.mode = SearchMode::FileName;
        app.search_path = root.to_path_buf();
        app.ignore_dirs = ignore_dirs.to_vec();
        app.max_file_size_mb = max_mb;
        app.input = query.to_string();
        app.on_key(enter());

        let deadline = Instant::now() + Duration::from_secs(5);
        while app.searching && Instant::now() < deadline {
            app.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        app.results
            .iter()
            .map(|i| i.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// 文件名搜索（fd）与内容搜索一致：用户额外忽略目录（Ctrl+I）生效，被忽略目录下的文件搜不到
    #[test]
    fn filename_search_respects_user_ignore_dirs() {
        let root = std::env::temp_dir().join(format!("sift-fnig-{}", std::process::id()));
        let sub = root.join("sub");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("test.sh"), "echo hi").unwrap();
        std::fs::write(root.join("keep_test.sh"), "echo hi").unwrap();

        // 忽略 sub 后：sub/test.sh 搜不到，root/keep_test.sh 仍能搜到
        let names = run_filename_search(
            &root,
            "test.sh",
            &[sub.to_string_lossy().into_owned()],
            10.0,
        );
        assert!(!names.contains(&"test.sh".to_string()), "got {names:?}");
        assert!(names.contains(&"keep_test.sh".to_string()), "got {names:?}");

        // 不忽略时两者都能搜到
        let names = run_filename_search(&root, "test.sh", &[], 10.0);
        assert!(names.contains(&"test.sh".to_string()), "got {names:?}");
        assert!(names.contains(&"keep_test.sh".to_string()), "got {names:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 文件名搜索（fd）与内容搜索一致：文件大小上限（Ctrl+S）生效，超过上限的文件搜不到
    #[test]
    fn filename_search_respects_max_filesize() {
        let root = std::env::temp_dir().join(format!("sift-fnsz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("small_test.sh"), "echo hi").unwrap(); // 8 字节
        std::fs::write(root.join("big_test.sh"), "a".repeat(3000)).unwrap(); // 3000 字节

        // 上限 ~1KB（0.001MB≈1048B）：big_test.sh 被跳过，small_test.sh 保留
        let names = run_filename_search(&root, "test.sh", &[], 0.001);
        assert!(
            names.contains(&"small_test.sh".to_string()),
            "got {names:?}"
        );
        assert!(!names.contains(&"big_test.sh".to_string()), "got {names:?}");

        // 上限足够大：两者都能搜到
        let names = run_filename_search(&root, "test.sh", &[], 10.0);
        assert!(
            names.contains(&"small_test.sh".to_string()),
            "got {names:?}"
        );
        assert!(names.contains(&"big_test.sh".to_string()), "got {names:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
