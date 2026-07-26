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

/// 常见图片扩展名：图片不支持预览，直接给出提示，
/// 避免二进制内容经 bat / 兜底读取输出到终端造成花屏
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "icns", "svg", "tif", "tiff",
    "avif", "heic", "heif", "jxl", "psd", "raw", "cr2", "nef", "arw", "dng", "ppm",
    "pgm", "pbm", "xpm", "exr", "hdr",
];

pub fn render(path: &Path, width: u16) -> Text<'static> {
    // 图片不支持预览：直接提示（也省去读文件与拉起 bat 的开销）
    if is_image(path) {
        return Text::from(Line::from("（图片文件，暂不支持预览）"));
    }
    let mut text = render_text(path, width);
    sanitize_control_chars(&mut text);
    text
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
    // 前 8KB 内含有 NUL 字节则视为二进制文件
    if head[..head.len().min(8192)].contains(&0) {
        return Text::from(Line::from("（二进制文件，不提供预览）"));
    }
    if head.is_empty() {
        // 读失败（权限/不存在）或空文件
        return fallback_read(path);
    }
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

/// 只读文件头部 max 字节；返回 (内容, 是否被截断)。截取点回退到 UTF-8 边界。
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
    // 截取点可能落在多字节字符中间，回退到合法边界
    if let Err(e) = std::str::from_utf8(&buf) {
        buf.truncate(e.valid_up_to());
    }
    (buf, truncated)
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
    use std::fs;

    #[test]
    fn renders_file_with_bat_or_fallback() {
        let dir = std::env::temp_dir().join(format!("sift-test-preview-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("demo.txt");
        fs::write(&file, "hello sift\nsecond line\n").unwrap();

        let text = render(&file, 80);
        let joined: String = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
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

        let text = render(&file, 80);
        let joined: String = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
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
        let text = render(&file, 80);
        let elapsed = start.elapsed();
        let joined: String = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
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

        let text = render(&file, 80);
        let joined: String = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
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

        let text = render(&file, 60);
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
        let text = render(Path::new("/nonexistent/path/xyz.txt"), 80);
        let joined: String = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        assert!(!joined.is_empty());
    }
}
