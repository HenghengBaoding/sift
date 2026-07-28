//! 搜索模块：基于 `fd`（文件名）与 `rg`（文件内容）。
//!
//! 文件名匹配规则（见 AGENTS.md）：仅针对**文件名**（不含目录路径）匹配，
//! 输入 `test.sh` 应匹配 `xxxxtest.sh`、`test.shxxxx`、`xxxtest.shxxxx`、`tesh.sh`，
//! 其他情况不匹配，不做拆词、不做子序列匹配、不看路径成分。
//! 因此匹配器按优先级依次尝试：
//!   1. 子串包含（大小写不敏感）
//!   2. typo 容错：有界编辑距离 + 结构约束（见 fuzzy_score）
//!
//! 内容搜索（rg）：
//!   - 查询支持 `\n` / `\t` / `\\` 转义，含换行时自动开启 --multiline
//!   - rg 以 current_dir=搜索根 运行，输出相对路径
//!   - 固定忽略内置系统目录（/proc /sys … 与 .git），并支持用户额外忽略目录
//!   - `--max-filesize` 跳过超大文件；二进制文件 rg 默认即跳过

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(test)]
use std::sync::Arc;

/// 单次搜索返回的最大结果数
const MAX_RESULTS: usize = 400;

/// 内置必忽略目录（绝对路径）。当搜索根包含它们时自动排除，
/// 避免 rg/fd 直接扫 /proc /sys 等虚拟/庞大目录（见 AGENTS.md）。
pub const MANDATORY_IGNORES: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/tmp",
    "/var/tmp",
    "/var/cache",
    "/mnt",
    "/media",
    "/var/lib",
    "/snap",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchMode {
    FileName,
    Content,
}

impl SearchMode {
    pub fn toggle(&mut self) {
        *self = match self {
            SearchMode::FileName => SearchMode::Content,
            SearchMode::Content => SearchMode::FileName,
        };
    }

    pub fn label(&self) -> &'static str {
        match self {
            SearchMode::FileName => "文件名",
            SearchMode::Content => "文件内容",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchResultItem {
    pub path: PathBuf,
    /// 相对搜索根路径的展示文本
    pub display: String,
    /// 内容模式下该文件的命中次数
    pub matches: u64,
    pub score: i64,
    /// 路径优先级：0=优先（/home /etc /usr/local），2=延后（/usr /var），1=其他。
    /// 作为 score 之后的次级排序键，让大范围搜索时常用目录结果先出现。
    pub priority: u8,
}

/// 结果排序：分数降序 → 路径优先级升序 → 展示文本升序
fn cmp_items(a: &SearchResultItem, b: &SearchResultItem) -> std::cmp::Ordering {
    b.score
        .cmp(&a.score)
        .then_with(|| a.priority.cmp(&b.priority))
        .then_with(|| a.display.cmp(&b.display))
}

/// 路径优先级（见 SearchResultItem::priority）
pub fn path_priority(path: &Path) -> u8 {
    const PREFERRED: &[&str] = &["/home", "/etc", "/usr/local"];
    const DELAYED: &[&str] = &["/usr", "/var"];
    let s = path.to_string_lossy();
    let under = |p: &str| s.as_ref() == p || s.starts_with(&format!("{p}/"));
    if PREFERRED.iter().any(|p| under(p)) {
        0
    } else if DELAYED.iter().any(|p| under(p)) {
        2
    } else {
        1
    }
}

/// 计算搜索根 root 之下需要忽略的目录相对路径（内置 + 用户额外）。
/// 仅返回真正位于 root 之下的目录；root 本身不会被排除。
pub fn ignore_rels(root: &Path, extra: &[String]) -> Vec<String> {
    let mut rels: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for abs in MANDATORY_IGNORES
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().cloned())
    {
        if let Ok(rel) = Path::new(&abs).strip_prefix(root) {
            let r = rel.to_string_lossy().into_owned();
            if !r.is_empty() && seen.insert(r.clone()) {
                rels.push(r);
            }
        }
    }
    rels
}

/// rg 排除 glob：始终排除 .git（--hidden 会把 .git 纳入搜索），
/// 再加上 root 之下的忽略目录（'/' 前缀锚定到搜索根）。
pub fn rg_exclude_globs(root: &Path, extra: &[String]) -> Vec<String> {
    let mut globs = vec!["!.git".to_string()];
    for r in ignore_rels(root, extra) {
        globs.push(format!("!/{r}"));
    }
    globs
}

/// fd 排除项：root 之下的忽略目录（'/' 前缀锚定到搜索根）。
/// fd 默认即跳过隐藏目录与 .git，无需额外排除。
///
/// 与内容搜索（rg）一致：用户额外忽略目录（Ctrl+I）同样作用于文件名搜索，
/// 故 App 调用时 extra 传用户忽略目录。
pub fn fd_excludes(root: &Path, extra: &[String]) -> Vec<String> {
    ignore_rels(root, extra)
        .into_iter()
        .map(|r| format!("/{r}"))
        .collect()
}

/// 文件名搜索：返回 (结果, 新构建的文件列表缓存)。
/// `cached` 为已有缓存时直接在内存中过滤，避免每次按键都拉起 fd。
/// （App 内走流式 fd_search_job；此函数仅供测试/复用。）
#[cfg(test)]
pub fn fd_search(
    root: &Path,
    query: &str,
    cached: Option<Arc<Vec<String>>>,
) -> (Vec<SearchResultItem>, Option<Arc<Vec<String>>>) {
    let (list, new_list) = match cached {
        Some(l) => (l, None),
        None => {
            let files = run_fd(root);
            let arc = Arc::new(files);
            (arc.clone(), Some(arc))
        }
    };
    (filter_fd_list(root, query, &list), new_list)
}

/// 在已构建的文件列表上做内存过滤与打分（fd 流式读取完成后复用）
pub fn filter_fd_list(root: &Path, query: &str, list: &[String]) -> Vec<SearchResultItem> {
    let root_str = root.to_string_lossy();
    let query_lower = query.to_lowercase();
    let mut items: Vec<SearchResultItem> = list
        .iter()
        .filter_map(|p| score_file(&root_str, p, &query_lower))
        .collect();
    items.sort_by(cmp_items);
    items.truncate(MAX_RESULTS);
    items
}

/// 构建 fd 命令：列出 root 下所有文件的绝对路径。
///
/// 与内容搜索（rg）保持一致：
/// - `--exclude` 排除忽略目录（内置必忽略 + 用户额外）
/// - `--size -<N>b` 跳过超过大小上限的文件（`-N` 表示 <= N 字节）
pub fn fd_cmd(root: &Path, excludes: &[String], max_filesize_bytes: u64) -> Command {
    let mut cmd = Command::new("fd");
    cmd.arg("--type")
        .arg("f")
        .arg("--absolute-path")
        .arg("--color=never")
        .arg("--size")
        .arg(format!("-{max_filesize_bytes}b"));
    for e in excludes {
        cmd.arg("--exclude").arg(e);
    }
    cmd.arg(".").arg(root.as_os_str());
    cmd
}

#[cfg(test)]
fn run_fd(root: &Path) -> Vec<String> {
    let output = fd_cmd(root, &[], u64::MAX).output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn score_file(root_str: &str, path: &str, query_lower: &str) -> Option<SearchResultItem> {
    let p = Path::new(path);
    let name = p.file_name()?.to_str()?.to_lowercase();
    let score = fuzzy_score(query_lower, &name)?;
    let display = sanitize_display(
        path.strip_prefix(root_str)
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or(path),
    );
    Some(SearchResultItem {
        path: PathBuf::from(path),
        display,
        matches: 0,
        score,
        priority: path_priority(p),
    })
}

/// 内容搜索（阻塞式，主要用于测试；App 内走流式 rg_stream）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn rg_search(root: &Path, query: &str) -> Vec<SearchResultItem> {
    let globs = rg_exclude_globs(root, &[]);
    let output = rg_cmd(root, query, &globs, 10 * 1024 * 1024).output();
    let Ok(out) = output else { return Vec::new() };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut items: Vec<SearchResultItem> = stdout
        .lines()
        .filter_map(|line| parse_count_line(root, line))
        .collect();
    items.sort_by(cmp_items);
    items.truncate(MAX_RESULTS);
    items
}

/// 构建 rg 命令：以搜索根为工作目录、相对路径搜索，输出 `./path:count`。
/// 查询中的 `\n` 解码为换行后自动开启 --multiline。
///
/// 性能/安全参数（见 AGENTS.md）：
/// - `--hidden` 包含隐藏文件（配合 `!.git` 排除 .git）
/// - `--no-messages` 抑制 stderr 的权限等错误，避免干扰
/// - `-j 0` 线程数自动（=CPU 核数）
/// - `--max-filesize <bytes>` 跳过超大文件（字节数，由 MB 换算而来）
/// - 二进制文件 rg 默认即跳过（rg 无 grep 的 --binary-files 选项）
pub fn rg_cmd(
    root: &Path,
    query: &str,
    exclude_globs: &[String],
    max_filesize_bytes: u64,
) -> Command {
    let decoded = decode_escapes(query);
    let mut cmd = Command::new("rg");
    // 内容搜索要求精准匹配：--fixed-strings 字面量匹配（不拆词、不当正则），
    // 且不加 --smart-case / -i，保持大小写敏感，避免小写查询误命中大写内容
    // （如 ceshi 误匹配 SPACESHIP）。
    cmd.arg("--count-matches")
        .arg("--fixed-strings")
        .arg("--case-sensitive")
        .arg("--color=never")
        .arg("--line-buffered")
        .arg("--hidden")
        .arg("--no-messages")
        .arg("-j")
        .arg("0")
        .arg("--max-filesize")
        .arg(max_filesize_bytes.to_string());
    if decoded.contains('\n') {
        cmd.arg("--multiline");
    }
    for g in exclude_globs {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--").arg(decoded).arg(".").current_dir(root);
    cmd
}

/// 解析一行 `./path:count`，返回结果项（path 拼回绝对路径）
pub fn parse_count_line(root: &Path, line: &str) -> Option<SearchResultItem> {
    let (path, count) = line.rsplit_once(':')?;
    let matches: u64 = count.trim().parse().ok()?;
    if matches == 0 {
        return None;
    }
    let rel = path.strip_prefix("./").unwrap_or(path);
    if rel.is_empty() {
        return None;
    }
    let abs = root.join(rel);
    let priority = path_priority(&abs);
    Some(SearchResultItem {
        path: abs,
        display: sanitize_display(rel),
        matches,
        score: matches as i64,
        priority,
    })
}

/// 展示用文本清洗：文件/路径名中的控制字符（ESC/BEL/换行等）替换为 U+FFFD，
/// 避免终端把它当作转义序列执行，造成花屏、内容画出界面框外。
pub fn sanitize_display(s: &str) -> String {
    if !s.chars().any(|c| c.is_control()) {
        return s.to_string();
    }
    s.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// 转义解码：`\n` -> 换行，`\t` -> 制表符，`\r` -> 回车，`\\` -> 反斜杠；
/// 其他反斜杠序列原样保留。搜索源码中的字面 `\n` 请输入 `\\n`。
pub fn decode_escapes(q: &str) -> String {
    if !q.contains('\\') {
        return q.to_string();
    }
    let mut out = String::with_capacity(q.len());
    let mut chars = q.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 粘贴文本编码为查询转义形式（与 decode_escapes 对应）：
/// 换行/制表符 -> `\n` `\t`，反斜杠 -> `\\`；`\r\n` 与单独 `\r` 归一化为换行。
/// 其余控制字符（ESC/BEL 等）直接丢弃，防止进入输入框后注入终端造成花屏。
/// 这样粘贴多行原文进输入框后可直接命中 --multiline 搜索。
pub fn encode_paste(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => {
                // \r\n 视为一个换行
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\\n");
            }
            // 其余控制字符（ESC/BEL 等）丢弃，防止注入终端造成花屏
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// 文件名模糊打分，返回 None 表示不匹配。
///
/// 规则（见 AGENTS.md）：
/// 1. 子串包含：文件名中包含查询词（前/后/两侧扩展均可）
/// 2. typo 容错：编辑距离 <= allowed，且同时满足
///    - 首字符、尾字符与查询词相同（避免 test.shj -> test.sh 这类“打超了”）
///    - 文件名只使用查询词中出现过的字符（避免 test.sh -> text.sh 这类引入陌生字符）
///
///    如 test.sh -> tesh.sh（h 是查询词中已有的字符，首尾一致）
pub fn fuzzy_score(query: &str, name: &str) -> Option<i64> {
    if query.is_empty() || name.is_empty() {
        return None;
    }
    // 1. 子串包含
    if let Some(pos) = name.find(query) {
        return Some(1000 - pos as i64 * 4 - (name.len() - query.len()) as i64);
    }
    let qlen = query.chars().count();
    let nlen = name.chars().count();
    // 2. typo 容错：有界编辑距离 + 结构约束
    let allowed = (qlen / 5).clamp(1, 3);
    if qlen >= 4
        && nlen.abs_diff(qlen) <= allowed
        && same_ends(query, name)
        && chars_within(query, name)
    {
        if let Some(d) = levenshtein_within(query, name, allowed) {
            return Some(800 - d as i64 * 60 - nlen.abs_diff(qlen) as i64 * 10);
        }
    }
    None
}

/// 首字符与尾字符是否一致
fn same_ends(a: &str, b: &str) -> bool {
    a.chars().next() == b.chars().next() && a.chars().last() == b.chars().last()
}

/// name 的字符是否全部在 query 中出现过（字符集合包含）
fn chars_within(query: &str, name: &str) -> bool {
    name.chars().all(|c| query.contains(c))
}

/// 带上限的 Levenshtein 距离；超过 max 返回 None。
pub fn levenshtein_within(a: &str, b: &str, max: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    (d <= max).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn fuzzy_match_agents_md_examples() {
        let q = "test.sh";
        assert!(fuzzy_score(q, "xxxxtest.sh").is_some());
        assert!(fuzzy_score(q, "test.shxxxx").is_some());
        assert!(fuzzy_score(q, "xxxtest.shxxxx").is_some());
        // 缺一个字符的情况（编辑距离为 1）
        assert!(fuzzy_score(q, "tesh.sh").is_some());
        // 其他情况不匹配
        assert!(fuzzy_score(q, "abc.txt").is_none());
        assert!(fuzzy_score(q, "readme.md").is_none());
        assert!(fuzzy_score(q, "stash").is_none());
        // 不做拆词/子序列匹配：tsh 虽是 test.sh 的子序列，但不应匹配
        assert!(fuzzy_score("tsh", "test.sh").is_none());
        assert!(fuzzy_score("t_s_t", "test.sh").is_none());
    }

    #[test]
    fn typo_tolerance_rejects_false_positives() {
        // 引入了查询中不存在的字符（x），虽编辑距离为 1，也不匹配
        assert!(fuzzy_score("test.sh", "text.sh").is_none());
        // 查询词“打超了”（文件名只是查询的前缀，尾字符不同），不匹配
        assert!(fuzzy_score("test.shj", "test.sh").is_none());
        // 首字符不同，不匹配
        assert!(fuzzy_score("test.sh", "xest.sh").is_none());
        // 尾字符不同，不匹配
        assert!(fuzzy_score("test.sh", "test.sx").is_none());
        // 合规 typo：错字用的是查询中已有的字符、首尾一致，匹配
        assert!(fuzzy_score("test.sh", "tesh.sh").is_some());
        assert!(fuzzy_score("test.sh", "tett.sh").is_some());
        // 中间漏字（删除），首尾一致且字符不越界，匹配
        assert!(fuzzy_score("test.sh", "tes.sh").is_some());
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein_within("test.sh", "tesh.sh", 3), Some(1));
        assert_eq!(levenshtein_within("abc", "abc", 1), Some(0));
        assert_eq!(levenshtein_within("abc", "xyz", 2), None);
    }

    fn make_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sift-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fd_search_finds_files() {
        let dir = make_temp_dir("fd");
        fs::write(dir.join("xxxxtest.sh"), "echo hi").unwrap();
        fs::write(dir.join("tesh.sh"), "echo hi").unwrap();
        fs::write(dir.join("unrelated.txt"), "nothing").unwrap();
        // 目录名中含关键字、但文件名本身不含：不应匹配（只匹配文件名）
        let sub = dir.join("test.sh.d");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("plain.txt"), "nothing").unwrap();

        let (items, cache) = fd_search(&dir, "test.sh", None);
        assert!(cache.is_some());
        let names: Vec<_> = items
            .iter()
            .map(|i| i.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"xxxxtest.sh".to_string()), "got {names:?}");
        assert!(names.contains(&"tesh.sh".to_string()), "got {names:?}");
        assert!(!names.contains(&"unrelated.txt".to_string()));
        assert!(!names.contains(&"plain.txt".to_string()), "got {names:?}");

        // 使用缓存路径再搜一次
        let (items2, cache2) = fd_search(&dir, "test.sh", cache);
        assert!(cache2.is_none());
        assert_eq!(items.len(), items2.len());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fd_search_sanitizes_control_chars_in_names() {
        let dir = make_temp_dir("fdctl");
        // 文件名含 ESC（终端转义序列）：展示文本必须被清洗
        fs::write(dir.join("ctrl_\u{1b}[31m_test.sh"), "echo hi").unwrap();

        let (items, _) = fd_search(&dir, "test.sh", None);
        assert_eq!(items.len(), 1, "got {items:?}");
        assert!(
            !items[0].display.chars().any(|c| c.is_control()),
            "got {:?}",
            items[0].display
        );
        assert!(items[0].display.contains('\u{FFFD}'));
        // path 字段保留原始路径，保证打开文件正常
        assert!(items[0].path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rg_search_finds_content() {
        let dir = make_temp_dir("rg");
        fs::write(dir.join("a.txt"), "hello world\nhello again\n").unwrap();
        fs::write(dir.join("b.txt"), "no match here").unwrap();

        let items = rg_search(&dir, "hello");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].matches, 2);
        assert!(items[0].display.ends_with("a.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rg_search_is_exact_case_sensitive() {
        let dir = make_temp_dir("rgexact");
        // 大写内容中包含小写查询的子串（如 SPACESHIP 含 CESHI）：
        // 精准匹配要求大小写敏感，小写查询 ceshi 不应命中
        fs::write(dir.join("a.txt"), "ZEND_SPACESHIP_SPEC\n").unwrap();
        fs::write(dir.join("b.txt"), "this has ceshi inside\n").unwrap();

        let items = rg_search(&dir, "ceshi");
        assert_eq!(items.len(), 1, "got {items:?}");
        assert!(items[0].display.ends_with("b.txt"), "got {items:?}");

        // 不拆词：ceshi 不应匹配被其他字符隔开的 c.e.s.h.i
        fs::write(dir.join("c.txt"), "c e s h i\n").unwrap();
        let items = rg_search(&dir, "ceshi");
        assert_eq!(items.len(), 1, "got {items:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rg_search_multiline() {
        let dir = make_temp_dir("rgml");
        fs::write(dir.join("a.txt"), "foo a\nb bar\nzzz\n").unwrap();
        fs::write(dir.join("b.txt"), "a\n\nb\n").unwrap();

        // 查询中的 \n 转义序列应匹配跨行文本
        let items = rg_search(&dir, "a\\nb");
        assert_eq!(items.len(), 1, "got {items:?}");
        assert!(items[0].display.ends_with("a.txt"));

        // 搜索字面 \\n 序列本身不应命中任何内容
        let items = rg_search(&dir, "\\\\n");
        assert!(items.is_empty(), "got {items:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// rg 应跳过 .git 目录与超过大小上限的文件
    #[test]
    fn rg_search_skips_git_and_oversized() {
        let dir = make_temp_dir("rgskip");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/config"), "hello\n").unwrap();
        fs::write(dir.join("keep.txt"), "hello\n").unwrap();
        // 一个明显超过 10M 上限的文件（稀疏写入，快速生成）
        let big = dir.join("big.log");
        {
            use std::io::Write;
            let mut f = fs::File::create(&big).unwrap();
            let chunk = "hello\n".repeat(4096); // 24KB
            for _ in 0..500 {
                f.write_all(chunk.as_bytes()).unwrap(); // ~12MB
            }
        }

        let items = rg_search(&dir, "hello");
        let names: Vec<_> = items.iter().map(|i| i.display.clone()).collect();
        assert!(
            names.iter().any(|n| n.ends_with("keep.txt")),
            "got {names:?}"
        );
        assert!(!names.iter().any(|n| n.contains(".git")), "got {names:?}");
        assert!(
            !names.iter().any(|n| n.ends_with("big.log")),
            "got {names:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_escapes_basics() {
        assert_eq!(decode_escapes("a\\nb"), "a\nb");
        assert_eq!(decode_escapes("a\\tb"), "a\tb");
        assert_eq!(decode_escapes("\\\\n"), "\\n");
        // 未知转义原样保留
        assert_eq!(decode_escapes("a\\xb"), "a\\xb");
        // 末尾孤单反斜杠原样保留
        assert_eq!(decode_escapes("a\\"), "a\\");
        assert_eq!(decode_escapes("plain"), "plain");
    }

    #[test]
    fn encode_paste_basics() {
        assert_eq!(
            encode_paste("ceshi\ntest\n这是/usr/share"),
            "ceshi\\ntest\\n这是/usr/share"
        );
        assert_eq!(encode_paste("a\r\nb"), "a\\nb");
        assert_eq!(encode_paste("a\rb"), "a\\nb");
        assert_eq!(encode_paste("a\tb"), "a\\tb");
        // 原文中的字面反斜杠转义为 \\，避免被 decode_escapes 误解码
        assert_eq!(encode_paste("C:\\Users"), "C:\\\\Users");
        assert_eq!(encode_paste("plain"), "plain");
        assert_eq!(encode_paste(""), "");
        // 其他控制字符（ESC/BEL/SO/SI…）直接丢弃
        assert_eq!(encode_paste("a\u{7}b\u{1b}c\u{e}d"), "abcd");
        // 与 decode_escapes 互逆（CR 归一化为 \n 除外）
        assert_eq!(decode_escapes(&encode_paste("x\ny\\z\tw")), "x\ny\\z\tw");
    }

    #[test]
    fn sanitize_display_replaces_control_chars() {
        assert_eq!(sanitize_display("normal/file.txt"), "normal/file.txt");
        // ESC 等控制字符替换为 U+FFFD，且不影响真实路径（path 字段不动，仅清洗展示文本）
        assert_eq!(sanitize_display("a\u{1b}[31mb.png"), "a\u{FFFD}[31mb.png");
        assert_eq!(sanitize_display("x\u{7}y\u{e}z"), "x\u{FFFD}y\u{FFFD}z");
        assert_eq!(sanitize_display(""), "");
    }

    #[test]
    fn parse_count_lines() {
        let root = Path::new("/data");
        let item = parse_count_line(root, "./a/b.txt:3").unwrap();
        assert_eq!(item.display, "a/b.txt");
        assert_eq!(item.path, PathBuf::from("/data/a/b.txt"));
        assert_eq!(item.matches, 3);
        assert!(parse_count_line(root, "./a.txt:0").is_none());
        assert!(parse_count_line(root, "garbage").is_none());
    }

    #[test]
    fn path_priority_tiers() {
        assert_eq!(path_priority(Path::new("/home/heng/a.txt")), 0);
        assert_eq!(path_priority(Path::new("/etc/hosts")), 0);
        assert_eq!(path_priority(Path::new("/usr/local/bin/x")), 0);
        // /usr/local 优先于 /usr（最长前缀）
        assert_eq!(path_priority(Path::new("/usr/share/x")), 2);
        assert_eq!(path_priority(Path::new("/var/log/x")), 2);
        assert_eq!(path_priority(Path::new("/opt/app/x")), 1);
        assert_eq!(path_priority(Path::new("/home")), 0);
        assert_eq!(path_priority(Path::new("/usr")), 2);
    }

    #[test]
    fn ignore_rels_and_globs() {
        let root = Path::new("/");
        let rels = ignore_rels(root, &[]);
        assert!(rels.contains(&"proc".to_string()), "got {rels:?}");
        assert!(rels.contains(&"sys".to_string()), "got {rels:?}");
        assert!(rels.contains(&"var/tmp".to_string()), "got {rels:?}");

        let globs = rg_exclude_globs(root, &["/data/logs".to_string()]);
        assert!(globs.contains(&"!.git".to_string()), "got {globs:?}");
        assert!(globs.contains(&"!/proc".to_string()), "got {globs:?}");
        assert!(globs.contains(&"!/data/logs".to_string()), "got {globs:?}");

        // root=/home 时 /proc 不在其下，不应出现；用户额外目录若不在 root 下也忽略
        let rels2 = ignore_rels(Path::new("/home"), &["/proc".to_string()]);
        assert!(!rels2.contains(&"proc".to_string()), "got {rels2:?}");

        // root=/var 时 /var/tmp -> tmp、/var/cache -> cache、/var/lib -> lib
        let rels3 = ignore_rels(Path::new("/var"), &[]);
        assert!(rels3.contains(&"tmp".to_string()), "got {rels3:?}");
        assert!(rels3.contains(&"cache".to_string()), "got {rels3:?}");
        assert!(rels3.contains(&"lib".to_string()), "got {rels3:?}");

        // fd 排除项带 '/' 前缀锚定
        let fde = fd_excludes(root, &[]);
        assert!(fde.contains(&"/proc".to_string()), "got {fde:?}");
    }

    #[test]
    fn fd_cmd_contains_size_and_excludes() {
        let cmd = fd_cmd(Path::new("/"), &["/proc".to_string()], 1048576);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // 大小上限：--size -<字节>b（与 rg --max-filesize 一致）
        let size_pos = args.iter().position(|a| a == "--size").expect("--size");
        assert_eq!(args[size_pos + 1], "-1048576b");
        // 忽略目录：--exclude /proc
        let ex_pos = args
            .iter()
            .position(|a| a == "--exclude")
            .expect("--exclude");
        assert_eq!(args[ex_pos + 1], "/proc");
    }

    #[test]
    fn rg_cmd_contains_perf_flags() {
        let cmd = rg_cmd(Path::new("/tmp"), "q", &["!.git".to_string()], 1048576);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for flag in [
            "--hidden",
            "--no-messages",
            "--max-filesize",
            "1048576",
            "-j",
            "0",
        ] {
            assert!(args.iter().any(|a| a == flag), "missing {flag} in {args:?}");
        }
    }
}
