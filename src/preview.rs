//! 文件预览：调用 `bat` 输出带 ANSI 颜色的文本，再转成 ratatui 的 Text。
//! 大文件只读取头部固定字节数经 stdin 交给 bat，保证渲染耗时有界。

use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

use ansi_to_tui::IntoText;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

/// 最多渲染的行数，防止超大文件拖慢界面
const MAX_LINES: usize = 5000;
/// 送给 bat 的最大字节数：大文件只预览头部，耗时有界
const PREVIEW_MAX_BYTES: usize = 512 * 1024;
/// 兜底读取时的最大字节数
const FALLBACK_MAX_BYTES: usize = 256 * 1024;
/// 二进制/乱码探测扫描的字节数：信号（非文本字节占比）在文件头部即可稳定判定，
/// 只扫前 16KB 即可，避免对大文本文件做全量扫描拖慢预览
const BINARY_SCAN_BYTES: usize = 16 * 1024;
/// 非文本字节占比超过该阈值即判为二进制/乱码：
/// 实测纯文本/代码/CJK ≈ 0.000，含少量 latin-1 杂字节的文本 ≈ 0.097，
/// 而各类二进制（ELF/gzip/PNG/加密 tile/无 NUL 随机数据）≥ 0.348，阈值取 0.20 间隔充足
const BINARY_BAD_FRACTION: f64 = 0.20;

/// 常见图片扩展名：图片不支持预览，直接给出提示，
/// 避免二进制内容经 bat / 兜底读取输出到终端造成花屏
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "icns", "svg", "tif", "tiff", "avif",
    "heic", "heif", "jxl", "psd", "raw", "cr2", "nef", "arw", "dng", "ppm", "pgm", "pbm", "xpm",
    "exr", "hdr",
];

/// 预览结果：纯文本（bat 渲染）或图片（kitty 图形传输序列）
pub enum Preview {
    /// 文本预览
    Text(Text<'static>),
    /// 图片预览（支持 kitty 图形协议的终端）：`transmit` 为发送给终端的 kitty 传输序列，
    /// 由 UI 线程拼接光标定位后写入终端，图片显示在预览区
    Image { transmit: Vec<u8> },
}

/// 渲染文件预览。`width`/`height` 为预览区内容区的单元格列数/行数（图片按此估算缩放目标）。
pub fn render(path: &Path, width: u16, height: u16) -> Preview {
    if is_image(path) {
        // 支持 kitty 图形协议的终端（kitty/Alacritty/Ghostty/WezTerm/Konsole…）：走光标定位模式；
        // 其它终端回退到文字提示
        if crate::image_preview::kitty_supported() {
            if let Some(p) = crate::image_preview::prepare(path, width.max(1), height.max(1)) {
                return Preview::Image { transmit: p.transmit };
            }
            // 是图片但解码失败（损坏/格式不支持）
            return Preview::Text(Text::from(Line::from("（图片文件，无法解码预览）")));
        }
        return Preview::Text(Text::from(Line::from("（图片文件，当前终端不支持图片预览）")));
    }
    let mut text = render_text(path, width);
    sanitize_control_chars(&mut text);
    Preview::Text(text)
}

/// 按扩展名（大小写不敏感）判断是否图片文件
fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// 剔除输出文本中的终端控制字符：制表符展开为空格，其余（ESC/BEL/SO/SI/CR…）丢弃。
/// 否则二进制内容里的转义序列会被终端直接执行，造成花屏、内容画出界面框外。
fn sanitize_control_chars(text: &mut Text<'static>) {
    for line in &mut text.lines {
        for span in &mut line.spans {
            if !span.content.chars().any(|c| c.is_control() && c != '\n') {
                continue;
            }
            let mut cleaned = String::with_capacity(span.content.len());
            for c in span.content.chars() {
                match c {
                    '\t' => cleaned.push_str("    "),
                    '\n' => cleaned.push(c),
                    c if c.is_control() => {}
                    c => cleaned.push(c),
                }
            }
            span.content = Cow::Owned(cleaned);
        }
    }
}

fn render_text(path: &Path, width: u16) -> Text<'static> {
    let (head, truncated) = read_head(path, PREVIEW_MAX_BYTES);
    // 二进制/乱码探测：NUL 字节 或 非文本字节占比过高（UTF-8 感知）。
    // 仅扫描头部 BINARY_SCAN_BYTES，耗时有界；像 sumdb tile 这类高熵加密/压缩数据
    // （即便没有早期 NUL）也会因非文本字节占比极高而被正确判定，避免输出乱码花屏
    if is_binary_content(&head[..head.len().min(BINARY_SCAN_BYTES)]) {
        return Text::from(Line::from("（二进制文件，不提供预览）"));
    }
    if head.is_empty() {
        // 读失败（权限/不存在）或空文件
        return fallback_read(path);
    }
    // 确认为文本后再截断到 UTF-8 边界（避免断开多字节字符）交给 bat
    let head = truncate_to_utf8_boundary(head);
    match run_bat(&head, path, width) {
        Some(bytes) if !bytes.is_empty() => {
            let mut text = bytes.into_text().unwrap_or_else(|_| fallback_read(path));
            if truncated {
                text.lines.push(Line::from(Span::styled(
                    "…… 文件过大，预览已截断 ……",
                    Style::default().add_modifier(Modifier::ITALIC),
                )));
            }
            text
        }
        _ => fallback_read(path),
    }
}

/// 判断一段内容是否为二进制/乱码（不宜当文本预览）。
/// 两个信号任一命中即判为二进制：
/// 1. 含 NUL 字节（经典信号，ELF/gzip/PNG 等大多命中）；
/// 2. 「非文本字节」占比超过阈值——按 UTF-8 感知扫描：
///    可打印 ASCII、常见空白（\t \n \r）、合法 UTF-8 多字节序列 记为文本；
///    其余控制字符与非法 UTF-8 字节 记为非文本。
///    高熵数据（加密/压缩，如 go sumdb 的 tile 文件）几乎每个字节都是非法 UTF-8 或控制字符，
///    占比极高 => 命中；而正常文本（含 CJK、少量 latin-1 杂字节）占比很低 => 不误伤。
fn is_binary_content(head: &[u8]) -> bool {
    if head.is_empty() {
        return false;
    }
    if head.contains(&0) {
        return true;
    }
    let mut bad = 0usize;
    let mut i = 0usize;
    let n = head.len();
    while i < n {
        let b = head[i];
        if (0x20..=0x7e).contains(&b) || b == b'\t' || b == b'\n' || b == b'\r' {
            i += 1;
        } else if b >= 0x80 {
            // 尝试按合法 UTF-8 多字节序列整体前进；否则记一个非文本字节
            let len = utf8_seq_len(b);
            if len > 1 && i + len <= n && std::str::from_utf8(&head[i..i + len]).is_ok() {
                i += len;
            } else {
                bad += 1;
                i += 1;
            }
        } else {
            // 其它控制字符（0x01-0x08, 0x0b, 0x0c, 0x0e-0x1f, 0x7f）
            bad += 1;
            i += 1;
        }
    }
    (bad as f64) / (n as f64) > BINARY_BAD_FRACTION
}

/// 由 UTF-8 首字节推断序列长度；非法首字节返回 0
fn utf8_seq_len(first: u8) -> usize {
    match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

/// 只读文件头部 max 字节；返回 (原始内容, 是否被截断)。
/// 注意：这里返回**原始字节**，不做 UTF-8 截断——二进制探测必须在原始字节上进行，
/// 否则高熵数据会在第一个非法 UTF-8 字节处被截成几乎为空，导致探测失效（这正是
/// sumdb tile 之类文件漏网、输出乱码的根因）。UTF-8 边界截断只在确认是文本后、
/// 交给 bat 之前做（见 truncate_to_utf8_boundary）。
fn read_head(path: &Path, max: usize) -> (Vec<u8>, bool) {
    let Ok(f) = File::open(path) else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::new();
    let n = match f.take(max as u64 + 1).read_to_end(&mut buf) {
        Ok(n) => n,
        Err(_) => return (Vec::new(), false),
    };
    let truncated = n > max;
    buf.truncate(max);
    (buf, truncated)
}

/// 把字节缓冲截断到合法 UTF-8 边界（截取点落在多字节字符中间时回退），避免交给 bat 时断开字符。
fn truncate_to_utf8_boundary(mut buf: Vec<u8>) -> Vec<u8> {
    if let Err(e) = std::str::from_utf8(&buf) {
        buf.truncate(e.valid_up_to());
    }
    buf
}

/// 通过 stdin 把文件头部交给 bat（--file-name 保留语法高亮检测）
fn run_bat(head: &[u8], path: &Path, width: u16) -> Option<Vec<u8>> {
    let mut cmd = Command::new("bat");
    cmd.arg("--paging=never")
        .arg("--color=always")
        .arg("--style=numbers");
    if bat_has_macchiato() {
        cmd.arg("--theme=Catppuccin Macchiato");
    }
    let mut child = cmd
        .arg("--wrap=character")
        .arg("--terminal-width")
        .arg(width.max(10).to_string())
        .arg("--line-range")
        .arg(format!(":{MAX_LINES}"))
        .arg("--file-name")
        .arg(path.as_os_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let data = head.to_vec();
    // 单独线程写 stdin，避免管道积满互相等待
    thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let output = child.wait_with_output().ok()?;
    Some(output.stdout)
}

/// bat 是否带有 Catppuccin Macchiato 主题（只检测一次）
fn bat_has_macchiato() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        Command::new("bat")
            .arg("--list-themes")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("Catppuccin Macchiato"))
            .unwrap_or(false)
    })
}

/// bat 不可用/失败时的兜底：直接读取文本内容。
fn fallback_read(path: &Path) -> Text<'static> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes = &bytes[..bytes.len().min(FALLBACK_MAX_BYTES)];
            let content = String::from_utf8_lossy(bytes);
            Text::from(content.into_owned())
        }
        Err(e) => Text::from(Line::from(format!("无法读取文件: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取出预览中的文本内容（图片预览无文本，返回空），便于断言
    fn text_of(p: Preview) -> Text<'static> {
        match p {
            Preview::Text(t) => t,
            Preview::Image { .. } => Text::default(),
        }
    }
    use std::fs;

    #[test]
    fn renders_file_with_bat_or_fallback() {
        let dir = std::env::temp_dir().join(format!("sift-test-preview-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("demo.txt");
        fs::write(&file, "hello sift\nsecond line\n").unwrap();

        let text = text_of(render(&file, 80, 24));
        let joined: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("hello sift"), "got: {joined}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_file_shows_hint() {
        let dir = std::env::temp_dir().join(format!("sift-test-bin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("blob.bin");
        fs::write(&file, b"PK\x00\x01binarystuff").unwrap();

        let text = text_of(render(&file, 80, 24));
        let joined: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(joined.contains("二进制文件"), "got: {joined}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn huge_file_is_truncated() {
        let dir = std::env::temp_dir().join(format!("sift-test-huge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("huge.log");
        // 写入超过 PREVIEW_MAX_BYTES 的内容
        let line = "0123456789abcdef\n";
        let mut content = String::new();
        while content.len() < PREVIEW_MAX_BYTES + 4096 {
            content.push_str(line);
        }
        fs::write(&file, content).unwrap();

        let start = std::time::Instant::now();
        let text = text_of(render(&file, 80, 24));
        let elapsed = start.elapsed();
        let joined: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("已截断"), "marker missing");
        assert!(elapsed.as_secs() < 5, "preview too slow: {elapsed:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_file_shows_hint() {
        let dir = std::env::temp_dir().join(format!("sift-test-img-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 即使内容不含 NUL（能绕过二进制探测），图片扩展名也直接命中提示；
        // 扩展名大小写不敏感
        let file = dir.join("photo.PNG");
        fs::write(&file, b"no nul bytes in this fake image").unwrap();

        let text = text_of(render(&file, 80, 24));
        let joined: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(joined.contains("图片"), "got: {joined}");
        assert!(!joined.contains("no nul bytes"), "got: {joined}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn control_chars_never_reach_output() {
        let dir = std::env::temp_dir().join(format!("sift-test-ctl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("weird.dat");
        // 无 NUL 的二进制：ESC 序列、BEL、SO/SI、CR、TAB 全混进去
        let mut data: Vec<u8> = (1u8..=255).collect();
        data.extend_from_slice(b"\x1b[31m\x0e\x0f\x07\x08\x0c\r\ttail");
        fs::write(&file, &data).unwrap();

        let text = text_of(render(&file, 60, 24));
        for line in &text.lines {
            for span in &line.spans {
                for c in span.content.chars() {
                    assert!(
                        !c.is_control() || c == '\n',
                        "control char U+{:04X} leaked into preview",
                        c as u32
                    );
                }
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_does_not_panic() {
        let text = text_of(render(Path::new("/nonexistent/path/xyz.txt"), 80, 24));
        let joined: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(!joined.is_empty());
    }

    /// 无 NUL 的高熵二进制（如 go sumdb 的 tile 文件：加密/压缩数据）也应被判为二进制，
    /// 给出「不提供预览」提示，而不是输出乱码花屏。旧的「仅查 NUL」探测会漏掉这类文件。
    #[test]
    fn nul_free_high_entropy_shows_binary_hint() {
        let dir = std::env::temp_dir().join(format!("sift-test-entropy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tile.dat");
        // 确定性伪随机字节（xorshift），剔除 NUL，模拟高熵且无 NUL 的二进制
        let mut x: u32 = 0x1234_5678;
        let mut data = Vec::with_capacity(4096);
        while data.len() < 4096 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            for b in x.to_le_bytes() {
                if b != 0 {
                    data.push(b);
                }
            }
        }
        assert!(!data.contains(&0), "样本不应含 NUL（才能验证非 NUL 路径）");
        fs::write(&file, &data).unwrap();

        let text = text_of(render(&file, 80, 24));
        let joined: String = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        assert!(joined.contains("二进制文件"), "高熵无 NUL 文件应判为二进制: {joined}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 含少量 latin-1 杂字节的文本不应被误判为二进制（避免假阳性），仍应正常预览出文本内容。
    #[test]
    fn text_with_few_stray_bytes_is_not_binary() {
        let dir = std::env::temp_dir().join(format!("sift-test-stray-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        // 大段 ASCII 文本中零星插入几个高字节（非法 UTF-8），占比远低于阈值
        let mut data = Vec::new();
        for i in 0..2000usize {
            if i % 100 == 0 {
                data.push(0xe9u8); // 孤立的 latin-1 é（非法 UTF-8 起始）
            } else {
                data.push(b'a' + (i % 26) as u8);
            }
        }
        fs::write(&file, &data).unwrap();

        assert!(!is_binary_content(&data), "少量杂字节不应判为二进制");
        let text = text_of(render(&file, 80, 24));
        let joined: String = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        assert!(!joined.contains("二进制文件"), "不应误判: {joined}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 直接对用户给出的 sumdb tile 文件验证（若该文件存在）：必须判为二进制。
    #[test]
    fn real_sumdb_tile_is_binary() {
        let p = Path::new(
            "/home/heng/go/pkg/mod/cache/download/sumdb/sum.golang.org/tile/8/1/877.p/101",
        );
        if !p.exists() {
            return; // 环境里没有该文件则跳过
        }
        let (head, _) = read_head(p, PREVIEW_MAX_BYTES);
        assert!(
            is_binary_content(&head[..head.len().min(BINARY_SCAN_BYTES)]),
            "sumdb tile 应判为二进制"
        );
    }
}
