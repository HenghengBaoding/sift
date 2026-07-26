//! 应用状态与事件处理。

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Text;
use ratatui::widgets::ListState;

use crate::preview;
use crate::search::{self, SearchMode, SearchResultItem};

/// 文件名列表缓存有效期
const FILE_LIST_TTL: Duration = Duration::from_secs(60);
/// rg 不可读路径缓存有效期
const RG_BAD_TTL: Duration = Duration::from_secs(600);
/// 内容搜索流式回传的批次大小/间隔
const BATCH_SIZE: usize = 100;
const BATCH_INTERVAL: Duration = Duration::from_millis(50);
/// 单次搜索展示的最大结果数
pub const MAX_RESULTS: usize = 400;
/// 预览缓存最大条目
const PREVIEW_CACHE_MAX: usize = 100;
/// 状态栏消息停留时间
const STATUS_TTL: Duration = Duration::from_secs(5);
/// “搜索进行中”提示弹窗的停留时间
const BUSY_POPUP_TTL: Duration = Duration::from_secs(3);

/// on_key 返回的动作（需要主循环配合，如挂起终端打开编辑器）
pub enum Action {
    Open(PathBuf),
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
        /// rg 新发现的不可读相对路径（所属 root）
        new_bad: Option<(PathBuf, Vec<String>)>,
    },
    PreviewDone {
        gen: u64,
        path: PathBuf,
        width: u16,
        text: Text<'static>,
    },
}

/// rg 不可读路径缓存：聚合为排除 glob，下次搜索直接跳过
struct BadCache {
    updated: Instant,
    /// 原始不可读相对路径
    bad: Vec<String>,
    /// 聚合后的排除 glob
    globs: Arc<Vec<String>>,
}

pub struct App {
    pub mode: SearchMode,
    search_path: PathBuf,

    /// 路径编辑弹窗是否打开
    pub editing_path: bool,
    /// 路径输入框内容
    pub path_input: String,
    /// 路径输入框光标（按字符计）
    pub path_cursor: usize,
    /// 路径校验失败的错误信息（展示在弹窗内，编辑时自动清除）
    pub path_error: Option<String>,

    pub input: String,
    /// 光标位置（按字符计）
    pub cursor: usize,

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
    /// “搜索进行中，请勿重复触发”提示弹窗的弹出时刻（3 秒后自动消失）
    pub busy_popup_since: Option<Instant>,
    pub should_quit: bool,
    /// 搜索是否进行中（fd 建索引中 / rg 结果流式到达中）
    pub searching: bool,
    /// 产生当前结果集的查询词（输入框内容可能已被改动、尚未再搜索）
    pub last_query: String,

    search_gen: u64,
    preview_gen: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    file_lists: HashMap<PathBuf, (Instant, Arc<Vec<String>>)>,
    preview_cache: HashMap<(PathBuf, u16), Rc<Text<'static>>>,
    rg_bad: HashMap<PathBuf, BadCache>,
    content_cancel: Option<Arc<AtomicBool>>,
    content_handle: Option<JoinHandle<()>>,
}

impl App {
    pub fn new() -> Self {
        // 默认搜索路径：启动程序时所在的当前目录
        let search_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let (tx, rx) = mpsc::channel();
        Self {
            mode: SearchMode::FileName,
            search_path,
            editing_path: false,
            path_input: String::new(),
            path_cursor: 0,
            path_error: None,
            input: String::new(),
            cursor: 0,
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
            busy_popup_since: None,
            should_quit: false,
            searching: false,
            last_query: String::new(),
            search_gen: 0,
            preview_gen: 0,
            tx,
            rx,
            file_lists: HashMap::new(),
            preview_cache: HashMap::new(),
            rg_bad: HashMap::new(),
            content_cancel: None,
            content_handle: None,
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

    // ------------------------------------------------------------ 按键处理

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        // 路径编辑弹窗打开时，按键全部交给弹窗处理
        if self.editing_path {
            self.on_path_key(key);
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Tab => {
                self.mode.toggle();
                self.trigger_search_now();
            }
            KeyCode::Char('p') if ctrl => self.start_path_edit(),
            KeyCode::Char('j') if ctrl => self.scroll_preview(3),
            KeyCode::Char('k') if ctrl => self.scroll_preview(-3),
            KeyCode::PageDown => self.scroll_preview(10),
            KeyCode::PageUp => self.scroll_preview(-10),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            // Enter 触发搜索（输入过程中不即时搜索）；
            // 上一次搜索还没执行完时只弹提示框，不并发派发新搜索
            KeyCode::Enter => {
                if self.searching {
                    self.busy_popup_since = Some(Instant::now());
                } else {
                    self.trigger_search_now();
                }
            }
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
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count())
            }
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

    /// 粘贴事件（bracketed paste）：原始文本可能含换行/制表符/反斜杠，
    /// 编码为查询转义形式后插入（多行文本 -> \n 序列，自动启用 --multiline）
    pub fn on_paste(&mut self, text: &str) {
        if self.editing_path {
            // 路径输入框：单行，换行/回车无意义直接丢弃
            let cleaned: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            let n = cleaned.chars().count();
            let byte = char_byte(&self.path_input, self.path_cursor);
            self.path_input.insert_str(byte, &cleaned);
            self.path_cursor += n;
            self.path_error = None;
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

    // ------------------------------------------------------------ 路径编辑弹窗

    /// 打开路径编辑弹窗（预填当前路径）
    fn start_path_edit(&mut self) {
        self.path_input = self.current_path_display();
        self.path_cursor = self.path_input.chars().count();
        self.path_error = None;
        self.editing_path = true;
    }

    fn on_path_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // 除 Enter（重新校验）外，任何按键都清除上一次的校验错误
        if key.code != KeyCode::Enter {
            self.path_error = None;
        }
        match key.code {
            KeyCode::Esc => self.editing_path = false,
            KeyCode::Enter => self.confirm_path(),
            KeyCode::Backspace => {
                if self.path_cursor > 0 {
                    self.path_cursor -= 1;
                    let byte = char_byte(&self.path_input, self.path_cursor);
                    self.path_input.remove(byte);
                }
            }
            KeyCode::Delete => {
                if self.path_cursor < self.path_input.chars().count() {
                    let byte = char_byte(&self.path_input, self.path_cursor);
                    self.path_input.remove(byte);
                }
            }
            KeyCode::Left => self.path_cursor = self.path_cursor.saturating_sub(1),
            KeyCode::Right => {
                self.path_cursor = (self.path_cursor + 1).min(self.path_input.chars().count())
            }
            KeyCode::Home => self.path_cursor = 0,
            KeyCode::End => self.path_cursor = self.path_input.chars().count(),
            KeyCode::Char('u') if ctrl => {
                self.path_input.clear();
                self.path_cursor = 0;
            }
            // 删除到上一个路径分隔符（不含 '/'）
            KeyCode::Char('w') if ctrl => {
                while self.path_cursor > 0 {
                    if self.path_input.chars().nth(self.path_cursor - 1) == Some('/') {
                        break;
                    }
                    self.path_cursor -= 1;
                    let byte = char_byte(&self.path_input, self.path_cursor);
                    self.path_input.remove(byte);
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let byte = char_byte(&self.path_input, self.path_cursor);
                self.path_input.insert(byte, c);
                self.path_cursor += 1;
            }
            _ => {}
        }
    }

    /// 确认路径：真实存在且为可读目录才切换并重新搜索，否则保持弹窗并提示原因
    fn confirm_path(&mut self) {
        let raw = self.path_input.trim().to_string();
        if raw.is_empty() {
            self.editing_path = false;
            self.path_error = None;
            return;
        }
        match validate_search_path(&raw) {
            Ok(path) => {
                self.search_path = path;
                self.editing_path = false;
                self.path_error = None;
                self.trigger_search_now();
            }
            Err(msg) => self.path_error = Some(msg),
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
                        b.score.cmp(&a.score).then_with(|| a.display.cmp(&b.display))
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
                        if let Some(c) = &self.content_cancel {
                            c.store(true, Ordering::Relaxed);
                        }
                    }
                }
                Msg::SearchDone {
                    gen,
                    content,
                    items,
                    new_file_list,
                    new_bad,
                } => {
                    if let Some((root, list)) = new_file_list {
                        self.file_lists.insert(root, (Instant::now(), list));
                    }
                    if let Some((root, bad)) = new_bad {
                        if !bad.is_empty() {
                            self.merge_rg_bad(root, bad);
                        }
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
                    self.preview_cache.insert((path.clone(), width), text.clone());
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
        // “搜索进行中”提示弹窗 3 秒后自动消失
        if let Some(t) = &self.busy_popup_since {
            if t.elapsed() > BUSY_POPUP_TTL {
                self.busy_popup_since = None;
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
        self.search_gen += 1;
        let query = self.input.trim().to_string();
        self.last_query = query.clone();
        // 任何新搜索都先停掉可能还在跑的内容搜索
        self.cancel_content_search();
        if query.is_empty() {
            self.searching = false;
            self.results.clear();
            self.select_first();
            return;
        }
        let gen = self.search_gen;
        let tx = self.tx.clone();
        let root = self.current_path().clone();
        match self.mode {
            SearchMode::FileName => {
                // fd 全量扫描可能耗时，期间结果区显示“搜索中…”
                self.searching = true;
                let cached = self.file_lists.get(&root).and_then(|(t, l)| {
                    (t.elapsed() < FILE_LIST_TTL).then(|| l.clone())
                });
                thread::spawn(move || {
                    let (items, new_list) = search::fd_search(&root, &query, cached);
                    let _ = tx.send(Msg::SearchDone {
                        gen,
                        content: false,
                        items,
                        new_file_list: new_list.map(|l| (root, l)),
                        new_bad: None,
                    });
                });
            }
            SearchMode::Content => {
                // 流式搜索：先清空旧结果，结果分批到达
                self.results.clear();
                self.list_state.select(None);
                self.preview = None;
                self.preview_path = None;
                self.searching = true;
                let cancel = Arc::new(AtomicBool::new(false));
                self.content_cancel = Some(cancel.clone());
                let globs = self.rg_globs_for(&root);
                self.content_handle = Some(thread::spawn(move || {
                    rg_stream(root, query, globs, gen, cancel, tx);
                }));
            }
        }
    }

    /// 停掉正在运行的内容搜索（置取消标志并 kill rg，等待线程退出）
    fn cancel_content_search(&mut self) {
        self.searching = false;
        if let Some(c) = self.content_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        if let Some(h) = self.content_handle.take() {
            let _ = h.join();
        }
    }

    /// 退出前调用：确保后台 rg 进程被清理
    pub fn shutdown(&mut self) {
        self.cancel_content_search();
    }

    /// 当前搜索根的排除 glob（缓存未过期时）
    fn rg_globs_for(&self, root: &PathBuf) -> Arc<Vec<String>> {
        self.rg_bad
            .get(root)
            .filter(|c| c.updated.elapsed() < RG_BAD_TTL)
            .map(|c| c.globs.clone())
            .unwrap_or_default()
    }

    /// 合并新发现的不可读路径，并重新聚合排除 glob
    fn merge_rg_bad(&mut self, root: PathBuf, new_bad: Vec<String>) {
        let entry = self.rg_bad.entry(root.clone()).or_insert_with(|| BadCache {
            updated: Instant::now(),
            bad: Vec::new(),
            globs: Arc::new(Vec::new()),
        });
        let mut set: std::collections::HashSet<String> = entry.bad.drain(..).collect();
        let grew = new_bad.into_iter().any(|p| set.insert(p));
        entry.bad = set.into_iter().collect();
        if grew {
            entry.globs = Arc::new(search::aggregate_excludes(&root, &entry.bad));
        }
        entry.updated = Instant::now();
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

/// 内容搜索线程：流式读取 rg 输出，分批回传结果；
/// cancel 置位时 kill rg 尽快退出；stderr 中的权限错误聚合后回传。
fn rg_stream(
    root: PathBuf,
    query: String,
    globs: Arc<Vec<String>>,
    gen: u64,
    cancel: Arc<AtomicBool>,
    tx: Sender<Msg>,
) {
    let mut child = match search::rg_cmd(&root, &query, &globs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            let _ = tx.send(Msg::SearchDone {
                gen,
                content: true,
                items: Vec::new(),
                new_file_list: None,
                new_bad: None,
            });
            return;
        }
    };

    // stderr 收集线程，防止管道积满阻塞 rg
    let err_handle = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut bad = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(p) = search::parse_error_line(&line) {
                    bad.push(p);
                }
            }
            bad
        })
    });

    let mut batch: Vec<SearchResultItem> = Vec::new();
    let mut last_flush = Instant::now();
    let mut cancelled = false;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                cancelled = true;
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
                    let _ = child.kill();
                    cancelled = true;
                    break;
                }
                last_flush = Instant::now();
            }
        }
    }
    if !cancelled && !batch.is_empty() {
        let _ = tx.send(Msg::SearchBatch {
            gen,
            items: std::mem::take(&mut batch),
        });
    }
    let _ = child.wait();
    let bad = err_handle.and_then(|h| h.join().ok()).unwrap_or_default();
    let _ = tx.send(Msg::SearchDone {
        gen,
        content: true,
        items: Vec::new(),
        new_file_list: None,
        new_bad: Some((root, bad)),
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
        }
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    /// 搜索进行中按 Enter：只弹提示框，不派发新搜索；弹窗 3 秒后自动消失
    #[test]
    fn enter_while_searching_shows_busy_popup_without_dispatching() {
        let mut app = App::new();
        app.input = "test".to_string();
        app.searching = true;
        let gen = app.search_gen;
        app.on_key(enter());
        assert!(app.busy_popup_since.is_some());
        assert_eq!(app.search_gen, gen, "搜索中按 Enter 不应派发新搜索");

        // 未搜索中时 Enter 正常派发
        app.searching = false;
        app.busy_popup_since = None;
        app.on_key(enter());
        assert!(app.busy_popup_since.is_none());
        assert_eq!(app.search_gen, gen + 1);

        // 弹窗超过 3 秒自动消失
        app.busy_popup_since = Some(Instant::now() - BUSY_POPUP_TTL - Duration::from_secs(1));
        app.tick();
        assert!(app.busy_popup_since.is_none());
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

    /// 路径确认：纯斜杠（"/////"）在 Linux 上会被归一化为 "/"，必须被拒绝
    #[test]
    fn confirm_path_rejects_slash_only_input() {
        let mut app = App::new();
        let original = app.current_path().clone();
        app.editing_path = true;
        app.path_input = "/////".to_string();
        app.on_key(enter());
        assert!(app.editing_path, "非法路径不应关闭弹窗");
        assert!(app.path_error.is_some());
        assert_eq!(app.current_path(), &original, "非法路径不应切换搜索目录");

        // 单个 "/" 是合法的根目录
        app.path_input = "/".to_string();
        app.path_error = None;
        app.on_key(enter());
        assert!(!app.editing_path);
        assert_eq!(app.current_path(), std::path::Path::new("/"));
    }

    /// 路径确认：不存在的路径 / 文件而非目录，都要拒绝并提示
    #[test]
    fn confirm_path_rejects_nonexistent_and_file() {
        let mut app = App::new();
        app.editing_path = true;

        app.path_input = "/nonexistent/dir/xxxx".to_string();
        app.on_key(enter());
        assert!(app.editing_path);
        assert!(app.path_error.as_deref().unwrap().contains("路径不存在"));

        // 存在但不是目录
        let file = std::env::temp_dir().join(format!("frsearch-test-{}", std::process::id()));
        std::fs::write(&file, "x").unwrap();
        app.path_input = file.to_string_lossy().to_string();
        app.path_error = None;
        app.on_key(enter());
        assert!(app.editing_path);
        assert!(app.path_error.as_deref().unwrap().contains("不是目录"));
        std::fs::remove_file(&file).ok();
    }

    /// 路径确认：合法目录接受并规范化（解析 .. 与多余斜杠），编辑时错误自动清除
    #[test]
    fn confirm_path_accepts_valid_dir_and_clears_error_on_edit() {
        let mut app = App::new();
        app.editing_path = true;
        app.path_input = "/nonexistent/xxxx".to_string();
        app.on_key(enter());
        assert!(app.path_error.is_some());

        // 任意编辑按键清除错误
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.path_error.is_none());

        // "/tmp/../tmp/" 规范化后为 "/tmp"（或 /tmp 的真实路径）
        app.path_input = "/tmp/../tmp/".to_string();
        app.on_key(enter());
        assert!(!app.editing_path, "合法目录应关闭弹窗");
        let expected = std::fs::canonicalize("/tmp").unwrap();
        assert_eq!(app.current_path(), &expected);
    }
}
