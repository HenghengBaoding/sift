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
//!   - rg 以 current_dir=搜索根 运行，输出相对路径；排除 glob 用 '/' 锚定到搜索根
//!   - 搜索中出现的 Permission denied 路径可聚合为排除 glob，下次搜索直接跳过

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// 单次搜索返回的最大结果数
const MAX_RESULTS: usize = 400;

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
}

/// 文件名搜索：返回 (结果, 新构建的文件列表缓存)。
/// `cached` 为已有缓存时直接在内存中过滤，避免每次按键都拉起 fd。
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

    let root_str = root.to_string_lossy();
    let query_lower = query.to_lowercase();
    let mut items: Vec<SearchResultItem> = list
        .iter()
        .filter_map(|p| score_file(&root_str, p, &query_lower))
        .collect();
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.display.cmp(&b.display)));
    items.truncate(MAX_RESULTS);
    (items, new_list)
}

fn run_fd(root: &Path) -> Vec<String> {
    let output = Command::new("fd")
        .arg("--type")
        .arg("f")
        .arg("--absolute-path")
        .arg("--color=never")
        .arg(".")
        .arg(root.as_os_str())
        .output();
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
    let name = Path::new(path).file_name()?.to_str()?.to_lowercase();
    let score = fuzzy_score(query_lower, &name)?;
    let display = path
        .strip_prefix(root_str)
        .map(|s| s.trim_start_matches('/'))
        .unwrap_or(path)
        .to_string();
    Some(SearchResultItem {
        path: PathBuf::from(path),
        display,
        matches: 0,
        score,
    })
}

/// 内容搜索（阻塞式，主要用于测试；App 内走流式 rg_stream）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn rg_search(root: &Path, query: &str) -> Vec<SearchResultItem> {
    let output = rg_cmd(root, query, &[]).output();
    let Ok(out) = output else { return Vec::new() };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut items: Vec<SearchResultItem> = stdout
        .lines()
        .filter_map(|line| parse_count_line(root, line))
        .collect();
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.display.cmp(&b.display)));
    items.truncate(MAX_RESULTS);
    items
}

/// 构建 rg 命令：以搜索根为工作目录、相对路径搜索，输出 `./path:count`。
/// 查询中的 `\n` 解码为换行后自动开启 --multiline。
pub fn rg_cmd(root: &Path, query: &str, exclude_globs: &[String]) -> Command {
    let decoded = decode_escapes(query);
    let mut cmd = Command::new("rg");
    cmd.arg("--count-matches")
        .arg("--fixed-strings")
        .arg("--smart-case")
        .arg("--color=never")
        .arg("--line-buffered");
    if decoded.contains('\n') {
        cmd.arg("--multiline");
    }
    for g in exclude_globs {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--")
        .arg(decoded)
        .arg(".")
        .current_dir(root);
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
    Some(SearchResultItem {
        path: root.join(rel),
        display: rel.to_string(),
        matches,
        score: matches as i64,
    })
}

/// 解析 stderr 中的权限错误：`rg: ./path: Permission denied (os error 13)`，
/// 返回相对路径。其他类型错误（如 ENOENT 竞态）不缓存，避免误排。
pub fn parse_error_line(line: &str) -> Option<String> {
    let rest = line.strip_prefix("rg: ")?;
    let (path, _) = rest.rsplit_once(": Permission denied")?;
    let rel = path.strip_prefix("./").unwrap_or(path);
    if rel.is_empty() || rel == "." {
        None
    } else {
        Some(rel.to_string())
    }
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
            c => out.push(c),
        }
    }
    out
}

/// 将不可读路径聚合为尽量少的排除 glob（相对 root）：
/// - 目录本身不可读 -> 排除整个目录
/// - 某目录下所有条目都出错 -> 排除整个目录
/// - 自底向上收敛：某目录的全部条目均已被排除 -> 排除该目录
/// - 其余 -> 排除单个文件
/// 宁可少排也不多排，避免漏掉该搜的目录。
pub fn aggregate_excludes(root: &Path, bad: &[String]) -> Vec<String> {
    let mut excluded: HashSet<String> = HashSet::new();
    // 目录 -> 出错文件数
    let mut dir_errs: HashMap<String, usize> = HashMap::new();
    for rel in bad {
        if glob_unusable(rel) {
            continue;
        }
        if root.join(rel).is_dir() {
            excluded.insert(rel.clone());
        } else if let Some(parent) = Path::new(rel).parent() {
            let p = parent.to_string_lossy().into_owned();
            if !p.is_empty() {
                *dir_errs.entry(p).or_default() += 1;
            }
        }
    }
    // 目录下条目全部出错 -> 整目录排除
    for (dir, cnt) in &dir_errs {
        if excluded.contains(dir) {
            continue;
        }
        if let Ok(rd) = fs::read_dir(root.join(dir)) {
            let total = rd.count();
            if total > 0 && *cnt >= total {
                excluded.insert(dir.clone());
            }
        }
    }
    // 未被整目录吸收的出错文件 -> 单文件排除
    for rel in bad {
        if glob_unusable(rel) || excluded.contains(rel) || root.join(rel).is_dir() {
            continue;
        }
        if !has_excluded_ancestor(&excluded, rel) {
            excluded.insert(rel.clone());
        }
    }
    // 自底向上收敛：目录的全部条目均被排除 -> 排除该目录
    loop {
        let parents: HashSet<String> = excluded
            .iter()
            .filter_map(|p| {
                let s = Path::new(p).parent()?.to_string_lossy().into_owned();
                (!s.is_empty()).then_some(s)
            })
            .collect();
        let mut changed = false;
        for p in parents {
            if excluded.contains(&p) || glob_unusable(&p) {
                continue;
            }
            if let Ok(rd) = fs::read_dir(root.join(&p)) {
                let children: Vec<String> = rd
                    .filter_map(|e| {
                        let e = e.ok()?;
                        let name = e.file_name().into_string().ok()?;
                        Some(format!("{p}/{name}"))
                    })
                    .collect();
                if !children.is_empty() && children.iter().all(|c| excluded.contains(c)) {
                    excluded.insert(p.clone());
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    // 去掉已被祖先覆盖的冗余项
    let snapshot = excluded.clone();
    let mut rels: Vec<String> = excluded
        .into_iter()
        .filter(|r| !has_excluded_ancestor(&snapshot, r))
        .collect();
    rels.sort();
    rels.iter().filter_map(|r| exclude_glob(r)).collect()
}

/// rel 是否有已被排除的祖先目录
fn has_excluded_ancestor(excluded: &HashSet<String>, rel: &str) -> bool {
    let mut anc = Path::new(rel).parent();
    while let Some(a) = anc {
        let s = a.to_string_lossy();
        if s.is_empty() {
            break;
        }
        if excluded.contains(s.as_ref()) {
            return true;
        }
        anc = a.parent();
    }
    false
}

/// 含 glob 元字符的路径不做排除（避免误排），宁可下次继续报错误
fn glob_unusable(rel: &str) -> bool {
    rel.is_empty()
        || rel == "."
        || rel.starts_with('-')
        || rel.starts_with('#')
        || rel
            .chars()
            .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '!' | '\\'))
}

/// 相对路径 -> 排除 glob。'/' 前缀锚定到搜索根（要求 rg 以 current_dir=root 运行）。
fn exclude_glob(rel: &str) -> Option<String> {
    if glob_unusable(rel) {
        return None;
    }
    Some(format!("!/{rel}"))
}

/// 文件名模糊打分，返回 None 表示不匹配。
///
/// 规则（见 AGENTS.md）：
/// 1. 子串包含：文件名中包含查询词（前/后/两侧扩展均可）
/// 2. typo 容错：编辑距离 <= allowed，且同时满足
///    - 首字符、尾字符与查询词相同（避免 test.shj -> test.sh 这类“打超了”）
///    - 文件名只使用查询词中出现过的字符（避免 test.sh -> text.sh 这类引入陌生字符）
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
        let dir = std::env::temp_dir().join(format!(
            "sift-test-{}-{}",
            tag,
            std::process::id()
        ));
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
        assert_eq!(encode_paste("ceshi\ntest\n这是/usr/share"), "ceshi\\ntest\\n这是/usr/share");
        assert_eq!(encode_paste("a\r\nb"), "a\\nb");
        assert_eq!(encode_paste("a\rb"), "a\\nb");
        assert_eq!(encode_paste("a\tb"), "a\\tb");
        // 原文中的字面反斜杠转义为 \\，避免被 decode_escapes 误解码
        assert_eq!(encode_paste("C:\\Users"), "C:\\\\Users");
        assert_eq!(encode_paste("plain"), "plain");
        assert_eq!(encode_paste(""), "");
        // 与 decode_escapes 互逆（CR 归一化为 \n 除外）
        assert_eq!(decode_escapes(&encode_paste("x\ny\\z\tw")), "x\ny\\z\tw");
    }

    #[test]
    fn parse_lines() {
        let root = Path::new("/data");
        let item = parse_count_line(root, "./a/b.txt:3").unwrap();
        assert_eq!(item.display, "a/b.txt");
        assert_eq!(item.path, PathBuf::from("/data/a/b.txt"));
        assert_eq!(item.matches, 3);
        assert!(parse_count_line(root, "./a.txt:0").is_none());
        assert!(parse_count_line(root, "garbage").is_none());

        assert_eq!(
            parse_error_line("rg: ./closed: Permission denied (os error 13)"),
            Some("closed".to_string())
        );
        assert_eq!(
            parse_error_line("rg: ./a/b c.txt: Permission denied (os error 13)"),
            Some("a/b c.txt".to_string())
        );
        // 其他错误不缓存
        assert!(parse_error_line("rg: ./gone: No such file or directory (os error 2)").is_none());
        assert!(parse_error_line("./a.txt:3").is_none());
    }

    #[test]
    fn aggregate_excludes_rollup() {
        let dir = make_temp_dir("agg");
        // a 目录全部文件出错 -> 整目录排除
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::write(dir.join("a/f1"), "1").unwrap();
        fs::write(dir.join("a/f2"), "2").unwrap();
        // b 目录只有部分文件出错 -> 单文件排除
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::write(dir.join("b/f1"), "1").unwrap();
        fs::write(dir.join("b/f2"), "2").unwrap();
        // c/d/e 嵌套全部出错 -> 自底向上收敛到 c
        fs::create_dir_all(dir.join("c/d")).unwrap();
        fs::write(dir.join("c/d/e"), "3").unwrap();
        // 目录本身不可读
        fs::create_dir_all(dir.join("closed")).unwrap();

        let bad = vec![
            "a/f1".to_string(),
            "a/f2".to_string(),
            "b/f1".to_string(),
            "c/d/e".to_string(),
            "closed".to_string(),
        ];
        let globs = aggregate_excludes(&dir, &bad);
        assert!(globs.contains(&"!/a".to_string()), "got {globs:?}");
        assert!(globs.contains(&"!/b/f1".to_string()), "got {globs:?}");
        assert!(globs.contains(&"!/c".to_string()), "got {globs:?}");
        assert!(globs.contains(&"!/closed".to_string()), "got {globs:?}");
        // 不应误排未出错的 b/f2 或 b 本身
        assert!(!globs.contains(&"!/b".to_string()), "got {globs:?}");
        assert!(!globs.contains(&"!/b/f2".to_string()), "got {globs:?}");
        // 收敛后不应保留冗余子项
        assert!(!globs.contains(&"!/c/d".to_string()), "got {globs:?}");
        assert!(!globs.contains(&"!/c/d/e".to_string()), "got {globs:?}");

        let _ = fs::remove_dir_all(&dir);
    }
}
