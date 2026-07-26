//! 用系统默认编辑器打开文件；权限不足时通过 sudo 提升权限。

use std::env;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

/// 探测可用编辑器：$VISUAL -> $EDITOR -> nvim -> vim -> code -> nano -> vi
pub fn detect_editor() -> Option<String> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(v) = env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    for cand in ["nvim", "vim", "code", "nano", "vi"] {
        if which(cand).is_some() {
            return Some(cand.to_string());
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for dir in env::split_paths(&paths) {
        let full = dir.join(name);
        if full.is_file() && is_executable(&full) {
            return Some(full);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 读或写权限不足时需要 sudo
pub fn needs_sudo(path: &Path) -> bool {
    File::open(path).is_err() || OpenOptions::new().write(true).open(path).is_err()
}

/// 打开文件（编辑器字符串可能带参数，如 "code --wait"）。
/// 调用前需先恢复终端，返回后再进入 TUI。
pub fn open(editor: &str, path: &Path, sudo: bool) -> io::Result<ExitStatus> {
    let mut parts = editor.split_whitespace();
    let bin = parts.next().unwrap_or(editor);
    let mut cmd = if sudo {
        let mut c = Command::new("sudo");
        c.arg(bin);
        c
    } else {
        Command::new(bin)
    };
    cmd.args(parts).arg(path.as_os_str());
    cmd.status()
}
