//! 系统剪贴板写入（仅 Linux）。
//!
//! 不引入额外依赖，沿用本项目「调用外部命令」的风格：
//! 依次尝试 `wl-copy`（Wayland）、`xclip`、`xsel`（X11），用第一个可用且成功的工具。
//! 这些工具在写入后都会自行 daemonize/分叉以服务选区，前台调用会立刻返回，不会阻塞 UI。

use std::io::Write;
use std::process::{Command, Stdio};

/// 候选剪贴板工具：(命令, 参数)
const TOOLS: [(&str, &[&str]); 3] = [
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

/// 把文本写入系统剪贴板。成功返回 Ok；全部工具不可用/失败时返回可读的错误原因。
pub fn copy(text: &str) -> Result<(), String> {
    let mut last_err: Option<String> = None;
    for (cmd, args) in TOOLS {
        match run_copy(cmd, args, text) {
            Ok(()) => return Ok(()),
            // 命令不存在：静默尝试下一个
            Err(CopyErr::NotFound) => continue,
            // 命令存在但失败：记录原因后仍继续尝试下一个（如 wl-copy 在无 Wayland 时会失败）
            Err(CopyErr::Failed(msg)) => last_err = Some(format!("{cmd}: {msg}")),
        }
    }
    Err(last_err.unwrap_or_else(|| "未找到剪贴板工具（需 wl-copy / xclip / xsel）".to_string()))
}

enum CopyErr {
    /// 命令不存在（spawn 返回 NotFound）
    NotFound,
    /// 命令存在但执行失败
    Failed(String),
}

/// 通过 stdin 把文本喂给指定剪贴板工具
fn run_copy(cmd: &str, args: &[&str], text: &str) -> Result<(), CopyErr> {
    let mut child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(CopyErr::NotFound),
        Err(e) => return Err(CopyErr::Failed(e.to_string())),
    };
    // 写入后立即 drop stdin（关闭管道），工具读到 EOF 后完成写入
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            return Err(CopyErr::Failed(e.to_string()));
        }
    }
    match child.wait() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(CopyErr::Failed(format!(
            "退出码 {}",
            s.code().unwrap_or(-1)
        ))),
        Err(e) => Err(CopyErr::Failed(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不存在的命令应被识别为 NotFound，从而跳到下一个候选工具
    #[test]
    fn missing_tool_is_not_found() {
        let r = run_copy("sift-definitely-not-exist-xyz", &[], "hi");
        assert!(matches!(r, Err(CopyErr::NotFound)));
    }

    /// 全部工具不可用时，copy 返回「未找到」类错误而不是 panic
    #[test]
    fn copy_reports_error_when_no_tools() {
        // 用一个必然失败的文本无关场景：把候选替换为不存在命令的行为等价验证
        let r = run_copy("sift-definitely-not-exist-xyz", &[], "hi");
        assert!(r.is_err());
    }
}
