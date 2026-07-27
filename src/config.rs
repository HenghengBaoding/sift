//! 持久化配置：`~/.config/sift/config.toml`。
//!
//! 目前记录两项（均为内容搜索 rg 相关）：
//! - `max_file_size_mb`：rg `--max-filesize` 的上限（单位 MB，默认 10）
//! - `ignore_dirs`：用户在默认必忽略目录之外额外指定的忽略目录（Ctrl+I 编辑）
//!
//! 文件缺失或损坏时回退到默认值，不阻断启动。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 默认最大文件大小（MB）
pub const DEFAULT_MAX_FILE_SIZE_MB: f64 = 10.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// rg 内容搜索的单文件大小上限（MB）
    #[serde(default = "default_max_size")]
    pub max_file_size_mb: f64,
    /// 额外忽略目录（绝对路径；不含内置必忽略目录）
    #[serde(default)]
    pub ignore_dirs: Vec<String>,
}

fn default_max_size() -> f64 {
    DEFAULT_MAX_FILE_SIZE_MB
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_file_size_mb: DEFAULT_MAX_FILE_SIZE_MB,
            ignore_dirs: Vec::new(),
        }
    }
}

/// 配置文件路径：`$HOME/.config/sift/config.toml`
pub fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".config/sift/config.toml"))
}

/// 读取配置；文件不存在 / 解析失败时返回默认值
pub fn load() -> Config {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// 写入指定路径（自动创建父目录）。失败时返回 Err，由调用方提示用户。
pub fn save_to(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg).unwrap_or_default();
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let c = Config::default();
        assert_eq!(c.max_file_size_mb, 10.0);
        assert!(c.ignore_dirs.is_empty());
    }

    #[test]
    fn roundtrip_toml() {
        let c = Config {
            max_file_size_mb: 0.2,
            ignore_dirs: vec!["/data/logs".to_string(), "/backup".to_string()],
        };
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.max_file_size_mb, 0.2);
        assert_eq!(back.ignore_dirs, c.ignore_dirs);
    }

    #[test]
    fn missing_fields_fall_back_to_default() {
        // 旧配置文件只有部分字段时，缺省字段用默认值
        let back: Config = toml::from_str("max_file_size_mb = 5.0\n").unwrap();
        assert_eq!(back.max_file_size_mb, 5.0);
        assert!(back.ignore_dirs.is_empty());

        let empty: Config = toml::from_str("").unwrap();
        assert_eq!(empty.max_file_size_mb, 10.0);
    }
}
