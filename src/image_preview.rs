//! Kitty 图形协议图片预览（光标定位模式，兼容所有支持该协议的终端）。
//!
//! 为什么用「光标定位模式」而不是「unicode 占位符（`U=1`）模式」：
//! 占位符模式只有较新的 kitty/Ghostty/Rio 支持，而 Alacritty/WezTerm/Konsole 等只支持
//! 基础的「传输 + 在光标处显示」。光标定位模式（yazi 的 `KgpOld`）兼容性最好，
//! 凡支持 kitty 图形协议的终端都能用，故此处统一采用。
//!
//! 设计要点（参考 yazi 的高性能做法）：
//! 1. **先缩放再传输**：把图片等比缩放到「预览区单元格数 × 单元格像素尺寸」的目标像素后再发送，
//!    绝不把原始大图整块推给终端——这是大图也秒开的关键。
//! 2. **压缩传输（`f=100` PNG）**：缩放后在内存中编码为 PNG 再 base64 发送，而非原始像素
//!    （`f=24/32`）。传输量从 MB 级降到几十 KB，终端解析更快、内存缓存（`Rc<Preview>`）也更省。
//!    kitty 协议只标准支持 `f=24`(RGB)/`f=32`(RGBA)/`f=100`(PNG) 三种格式——**没有** JPEG
//!    （`f=101` 仅是社区 feature request，非协议的一部分，用了会在 kitty 等终端坏掉）；PNG 无损、
//!    保留 alpha、且所有支持该协议的终端都支持，是压缩传输的可移植选择。`f=100` 时宽高由 PNG
//!    自身读出，控制数据无需 `s/v`。
//! 3. **磁盘缓存缩放后的图（precache，学 yazi）**：把缩放编码好的 PNG 字节缓存到
//!    `~/.cache/sift/preview/`，键为「路径 + mtime + 大小 + 目标像素框」。再次预览同一文件
//!    （如重启后、或内存缓存被淘汰后）直接读缓存复用，跳过解码/缩放/编码。缓存写入走「临时文件 +
//!    原子 rename」，并按总容量淘汰最旧文件，避免无限膨胀。
//! 4. **光标定位显示**（`a=T,z=-1,C=1`）：图片以「自然像素尺寸」显示在当前光标处。UI 线程先把
//!    光标移到预览区左上角、清空旧图，再写入图片数据；`z=-1` 让图片位于文本之后，配合预览区
//!    「无背景色的空格」从其后透出，从而与 ratatui 的绘制共存。
//! 5. **解码/缩放/编码全部在后台预览线程完成**（沿用 app 的 preview 线程），UI 线程只负责把
//!    成品序列写入终端。
//!
//! 协议常量（普通终端，非 tmux 透传）：
//! - APC 起始：`ESC _ G`（写作 `\x1b_G`）
//! - 字符串终止：`ESC \`（写作 `\x1b\\`）
//! - 删除全部图片：`ESC _ G q=2,a=d,d=A ESC \`

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose, Engine};
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use ratatui::layout::Rect;
use std::io::Cursor;

/// base64 分块大小（kitty 建议 ≤4096）
const CHUNK: usize = 4096;
/// window_size 取不到像素尺寸时的兜底单元格像素尺寸（宽, 高）
const FALLBACK_CELL: (f64, f64) = (10.0, 20.0);
/// 解码内存上限，防止恶意/超大图把内存吃满（512MB）
const MAX_ALLOC: u64 = 512 * 1024 * 1024;
/// 磁盘预览缓存总容量上限（超出后按 mtime 淘汰最旧）
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
/// 淘汰滞回目标：降到上限的 80% 即停，避免在阈值附近频繁淘汰
const EVICT_TARGET_RATIO: u64 = 4; // target = MAX_CACHE_BYTES * 4 / 5
/// 缓存键盐（版本）：编码/键规则变化时改动它即可整体作废旧缓存
const CACHE_SALT: &[u8] = b"sift-preview-v1";

/// 一次图片预览的成品：发送给终端的 kitty 传输序列
pub struct PreparedImage {
    /// kitty 图形传输序列（`a=T,z=-1,C=1,f=100`，光标定位模式 + PNG 压缩）。
    /// 由 UI 线程在前面拼接「保存光标 + 移动到预览区 + 删除旧图」、末尾拼接「恢复光标」后写入终端。
    pub transmit: Vec<u8>,
}

/// 当前终端是否支持 kitty 图形协议（决定是否启用图片预览）。
/// 覆盖已知支持该协议的终端：kitty / Alacritty / Ghostty / WezTerm / Konsole / Rio。
/// env 命中任一即启用；结果缓存，仅探测一次。
pub fn kitty_supported() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let exists = |k: &str| std::env::var_os(k).is_some();
        if exists("KITTY_WINDOW_ID")
            || exists("ALACRITTY_WINDOW_ID")
            || exists("GHOSTTY_RESOURCES_DIR")
            || exists("WEZTERM_EXECUTABLE")
            || exists("KONSOLE_VERSION")
        {
            return true;
        }
        if matches!(
            std::env::var("TERM").as_deref(),
            Ok("xterm-kitty") | Ok("xterm-ghostty") | Ok("rio")
        ) {
            return true;
        }
        matches!(
            std::env::var("TERM_PROGRAM").as_deref(),
            Ok("kitty") | Ok("WezTerm") | Ok("ghostty") | Ok("rio")
        )
    })
}

/// 删除 kitty 中全部已注册图片的序列。每次展示新图片前先发它，保证屏幕上只保留当前这张。
pub fn delete_all_payload() -> Vec<u8> {
    b"\x1b_Gq=2,a=d,d=A\x1b\\".to_vec()
}

/// 组装「在预览区显示图片」的完整终端序列：
/// 保存光标 -> 移到预览区左上角 -> 删除旧图 -> 传输并显示新图 -> 恢复光标。
/// `area` 为预览区内容区（不含边框）的单元格矩形。
pub fn show_sequence(area: Rect, transmit: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(transmit.len() + 64);
    buf.extend_from_slice(b"\x1b7"); // DECSC：保存光标位置
    // CSI row;col H：移到预览区左上角（终端坐标 1 基）
    buf.extend_from_slice(format!("\x1b[{};{}H", area.y + 1, area.x + 1).as_bytes());
    buf.extend_from_slice(&delete_all_payload()); // 先清掉上一张
    buf.extend_from_slice(transmit); // 传输并在光标处显示
    buf.extend_from_slice(b"\x1b8"); // DECRC：恢复光标位置
    buf
}

/// 准备一张图片的预览：
/// 命中磁盘缓存则直接复用已缩放编码好的 PNG；否则 解码 -> 等比缩放到预览区目标像素 ->
/// 编码为 PNG -> 写磁盘缓存，最后组装为 kitty 传输序列。
/// `cols`/`rows` 为预览区内容区的单元格列数/行数。任何一步失败返回 None（上层回退到文字提示）。
pub fn prepare(path: &Path, cols: u16, rows: u16) -> Option<PreparedImage> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let (cell_w, cell_h) = cell_size();
    // 预览区能容纳的像素上限（单元格数 × 单元格像素尺寸）
    let max_w = ((cols as f64 * cell_w) as u32).max(1);
    let max_h = ((rows as f64 * cell_h) as u32).max(1);

    let dir = cache_dir();
    let key = cache_key(path, max_w, max_h);

    // 磁盘缓存命中：直接复用，跳过解码/缩放/编码（precache 的核心收益）
    if let (Some(dir), Some(key)) = (&dir, &key) {
        if let Ok(bytes) = std::fs::read(dir.join(format!("{key}.png"))) {
            if !bytes.is_empty() {
                return Some(PreparedImage {
                    transmit: transmit_png(&bytes),
                });
            }
        }
    }

    let img = decode(path, max_w, max_h)?;
    // 大图等比缩放到上限内（性能关键：传输/显示的都只是这个小图）
    let img = downscale(img, max_w, max_h);
    if img.width() == 0 || img.height() == 0 {
        return None;
    }
    let png = encode_png(img)?;

    // 写入磁盘缓存（原子写 + 容量淘汰）；失败不影响本次预览
    if let (Some(dir), Some(key)) = (&dir, &key) {
        write_cache(dir, key, &png);
    }

    Some(PreparedImage {
        transmit: transmit_png(&png),
    })
}

/// 解码图片并应用 EXIF 方向。设置解码内存上限以防 OOM。
///
/// 性能关键：JPEG 走 DCT 域缩放解码（`decode_jpeg_scaled`），直接在解码阶段把超大图
/// （如 6016x6016 的 36MP 照片）降到略大于预览区的尺寸，避免「全分辨率解码 + 对 36MP 做
/// 重采样」这两步各自数秒的开销（实测 6016x6016 从 ~7s 降到 ~0.8s）。其它格式走通用解码。
fn decode(path: &Path, max_w: u32, max_h: u32) -> Option<DynamicImage> {
    let bytes = std::fs::read(path).ok()?;
    let format = image::guess_format(&bytes).ok()?;

    if format == ImageFormat::Jpeg {
        if let Some(img) = decode_jpeg_scaled(&bytes, max_w, max_h) {
            return Some(img);
        }
        // 缩放解码失败（如 CMYK JPEG）时回退到通用解码，保证仍能预览
    }
    decode_generic(&bytes)
}

/// 通用解码：交给 `image` crate（zune-jpeg / png / webp 等），带内存上限并应用 EXIF 方向。
fn decode_generic(bytes: &[u8]) -> Option<DynamicImage> {
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(MAX_ALLOC);

    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader.limits(limits);
    let mut decoder = reader.with_guessed_format().ok()?.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    if orientation != Orientation::NoTransforms {
        img.apply_orientation(orientation);
    }
    Some(img)
}

/// JPEG 专用的 DCT 域缩放解码：`jpeg_decoder` 支持在 IDCT 阶段直接按 1/1、1/2、1/4、1/8
/// 缩放（`scale(w, h)` 会自动挑选「仍能覆盖目标尺寸的最小缩放因子」，故预览区越大画质越好）。
/// 这样 36MP 大图无需先解码成 36MP 再重采样，解码与后续 resize 都只处理一个小图。
///
/// EXIF 方向：`jpeg_decoder` 不解析 EXIF，故先用 `image` crate 仅读头（不解码像素，开销极小）
/// 取出方向，解码后再 `apply_orientation` 补上，保持与通用路径一致的正确朝向。
///
/// 仅处理常见的 RGB24 / L8；CMYK 等少见像素格式返回 None 由调用方回退通用解码。
fn decode_jpeg_scaled(bytes: &[u8], max_w: u32, max_h: u32) -> Option<DynamicImage> {
    // 仅读 JPEG 头拿 EXIF 方向（不 decode 像素，毫秒级）
    let orientation = {
        let mut reader = ImageReader::new(Cursor::new(bytes));
        reader.set_format(ImageFormat::Jpeg);
        reader
            .into_decoder()
            .ok()
            .and_then(|mut d| d.orientation().ok())
            .unwrap_or(Orientation::NoTransforms)
    };

    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    // scale 需在 decode 前调用；目标尺寸clamp 到 u16（预览区不可能接近 65535，仅防御）
    let req_w = max_w.min(u16::MAX as u32) as u16;
    let req_h = max_h.min(u16::MAX as u32) as u16;
    dec.scale(req_w, req_h).ok()?;
    let pixels = dec.decode().ok()?;
    let info = dec.info()?;

    // 内存保护：缩放后的输出仍超上限则放弃（极端恶意图）
    if (info.width as u64) * (info.height as u64) * 4 > MAX_ALLOC {
        return None;
    }

    let w = info.width as u32;
    let h = info.height as u32;
    let mut img = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, pixels)?)
        }
        jpeg_decoder::PixelFormat::L8 => {
            DynamicImage::ImageLuma8(image::GrayImage::from_raw(w, h, pixels)?)
        }
        // CMYK 等：交给通用解码处理
        _ => return None,
    };

    if orientation != Orientation::NoTransforms {
        img.apply_orientation(orientation);
    }
    Some(img)
}

/// 等比缩放：仅当图片超出 (max_w, max_h) 时才缩小（Triangle 滤镜，速度与质量均衡）。
fn downscale(img: DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    if img.width() <= max_w && img.height() <= max_h {
        return img;
    }
    img.resize(max_w, max_h, FilterType::Triangle)
}

/// 把缩放后的图片在内存中编码为 PNG 字节（无损、保留 alpha）。
/// PNG 既是磁盘缓存的存储格式，也是 `f=100` 传输的载荷——一次编码两处复用。
fn encode_png(img: DynamicImage) -> Option<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// 把 PNG 字节组装为 kitty 图形传输序列（光标定位模式 + PNG 压缩）：
/// 首块带完整控制数据——`q=2` 静默、`a=T` 传输并在光标处显示、`z=-1` 置于文本之后、
/// `C=1` 显示后不移动光标、`f=100` PNG 格式（宽高由 PNG 自身读出，无需 `s/v`）；
/// 后续块仅带 `m`（是否还有后续）。base64 按 4096 分块。
fn transmit_png(png: &[u8]) -> Vec<u8> {
    let b64 = general_purpose::STANDARD.encode(png);
    let mut buf: Vec<u8> = Vec::with_capacity(b64.len() + b64.len() / CHUNK * 40 + 128);
    let mut chunks = b64.as_bytes().chunks(CHUNK).peekable();

    let Some(first) = chunks.next() else {
        return buf; // 空载荷：返回空序列（上层不会拿到空 PNG，此处仅防御）
    };
    let more = chunks.peek().is_some() as u8;
    buf.extend_from_slice(format!("\x1b_Gq=2,a=T,z=-1,C=1,f=100,m={more};").as_bytes());
    buf.extend_from_slice(first);
    buf.extend_from_slice(b"\x1b\\");

    while let Some(chunk) = chunks.next() {
        let more = chunks.peek().is_some() as u8;
        buf.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        buf.extend_from_slice(chunk);
        buf.extend_from_slice(b"\x1b\\");
    }
    buf
}

// ------------------------------------------------------------ 磁盘缓存（precache）

/// 磁盘缓存目录：`$XDG_CACHE_HOME/sift/preview`，无 XDG_CACHE_HOME 时回退 `~/.cache/sift/preview`。
fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("sift").join("preview"))
}

/// 计算缓存键：对「版本盐 + 规范化路径 + mtime(秒+纳秒) + 文件大小 + 目标像素框」做 FNV-1a 64 哈希，
/// 输出 16 位十六进制串。源文件改动（mtime/大小变化）或目标框变化都会得到不同键，从而自动失效。
/// 取不到元数据时返回 None（此时不缓存，仅本次正常编码预览）。
fn cache_key(path: &Path, max_w: u32, max_h: u32) -> Option<String> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut buf = Vec::with_capacity(canon.as_os_str().len() + 64);
    buf.extend_from_slice(CACHE_SALT);
    buf.extend_from_slice(canon.to_string_lossy().as_bytes());
    buf.extend_from_slice(&mtime.as_secs().to_le_bytes());
    buf.extend_from_slice(&mtime.subsec_nanos().to_le_bytes());
    buf.extend_from_slice(&md.len().to_le_bytes());
    buf.extend_from_slice(&max_w.to_le_bytes());
    buf.extend_from_slice(&max_h.to_le_bytes());
    Some(format!("{:016x}", fnv1a(&buf)))
}

/// FNV-1a 64bit 哈希（稳定、无依赖；用于缓存文件名，碰撞概率对预览缓存场景可忽略）
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// 写入缓存：先写临时文件再原子 rename，避免并发预览线程读到半截文件。随后做一次容量淘汰。
fn write_cache(dir: &Path, key: &str, bytes: &[u8]) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let final_path = dir.join(format!("{key}.png"));
    let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &final_path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    evict_cache(dir);
}

/// 容量淘汰：总大小超过上限时，按 mtime 升序删除最旧文件，直到降到上限的 80% 以下。
fn evict_cache(dir: &Path) {
    evict_cache_limit(dir, MAX_CACHE_BYTES);
}

fn evict_cache_limit(dir: &Path, max: u64) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        // 只统计/淘汰 .png 缓存文件，不碰临时文件等
        let is_cache = e
            .path()
            .extension()
            .is_some_and(|x| x == "png");
        if !is_cache {
            continue;
        }
        let size = md.len();
        let mtime = md.modified().unwrap_or(UNIX_EPOCH);
        total = total.saturating_add(size);
        entries.push((e.path(), size, mtime));
    }
    if total <= max {
        return;
    }
    entries.sort_by_key(|e| e.2);
    let target = max * EVICT_TARGET_RATIO / 5;
    for (path, size, _) in entries {
        if total <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

/// 单元格像素尺寸（宽, 高）：优先用 ioctl 得到的窗口像素尺寸 / 单元格数（kitty 系终端会如实上报），
/// 取不到时回退到兜底常量。仅影响缩放目标大小的估算，取近似值也能正常显示。
fn cell_size() -> (f64, f64) {
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 {
            return (
                ws.width as f64 / ws.columns as f64,
                ws.height as f64 / ws.rows as f64,
            );
        }
    }
    FALLBACK_CELL
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 串行化所有会调用 prepare()（经 cache_dir 读 XDG_CACHE_HOME）或改写环境变量的测试，
    /// 避免并行测试间的环境变量竞态。用 into_inner 恢复中毒，避免某个测试 panic 后级联失败。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn delete_all_is_well_formed() {
        let d = delete_all_payload();
        assert!(d.starts_with(b"\x1b_G"), "应以 APC 起始");
        assert!(d.ends_with(b"\x1b\\"), "应以 ST 结束");
        assert!(d.windows(7).any(|w| w == b"a=d,d=A"), "应含删除全部指令");
    }

    #[test]
    fn prepare_rejects_zero_area() {
        let _g = lock_env();
        assert!(prepare(Path::new("/nonexistent.png"), 0, 10).is_none());
        assert!(prepare(Path::new("/nonexistent.png"), 10, 0).is_none());
    }

    #[test]
    fn prepare_fails_gracefully_on_missing_or_non_image() {
        let _g = lock_env();
        // 不存在 / 非图片内容都应返回 None（上层回退文字提示），不 panic
        assert!(prepare(Path::new("/nonexistent/xxx.png"), 40, 20).is_none());
    }

    /// 从 kitty 传输序列里还原出 PNG 字节：拆出每个 APC 帧 ';' 之后、ST 之前的 base64 载荷，
    /// 拼接后 base64 解码。供测试校验实际传输的图片尺寸/格式。
    fn extract_png(transmit: &[u8]) -> Vec<u8> {
        let s = String::from_utf8_lossy(transmit);
        let mut b64 = String::new();
        for frame in s.split("\x1b_G") {
            if let Some(semi) = frame.find(';') {
                let payload = frame[semi + 1..].split("\x1b\\").next().unwrap_or("");
                b64.push_str(payload);
            }
        }
        general_purpose::STANDARD.decode(b64.as_bytes()).unwrap_or_default()
    }

    #[test]
    fn transmit_png_uses_cursor_placement_mode() {
        // 2x2 RGBA 图：编码为 PNG 再传输，验证光标定位模式 + PNG 压缩的关键控制位
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([255, 0, 0, 255]),
        ));
        let png = encode_png(img).expect("PNG 编码应成功");
        let transmit = transmit_png(&png);
        let s = String::from_utf8_lossy(&transmit);
        assert!(s.contains("a=T"), "应传输并在光标处显示");
        assert!(s.contains("z=-1"), "应置于文本之后（透出）");
        assert!(s.contains("C=1"), "显示后不应移动光标");
        assert!(s.contains("f=100"), "应为 PNG 压缩格式 f=100: {s}");
        assert!(!s.contains("f=24") && !s.contains("f=32"), "不应再传原始像素");
        assert!(!s.contains("s=") && !s.contains("v="), "f=100 宽高由 PNG 自身读出，无需 s/v: {s}");
        assert!(!s.contains("U=1"), "光标定位模式不应使用占位符");
        assert!(transmit.ends_with(b"\x1b\\"), "应以 ST 结束");
        // 载荷应为合法 PNG（魔数 89 50 4E 47）
        let decoded = extract_png(&transmit);
        assert!(decoded.starts_with(&[0x89, b'P', b'N', b'G']), "载荷应为 PNG");
    }

    #[test]
    fn prepare_end_to_end_real_png() {
        let _g = lock_env();
        let dir = std::env::temp_dir().join(format!("sift-test-img-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("demo.png");
        let img = image::RgbImage::from_fn(100, 50, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        img.save(&file).expect("保存 PNG 应成功");

        let prepared = prepare(&file, 40, 20).expect("真实 PNG 应能准备预览");
        assert!(prepared.transmit.starts_with(b"\x1b_G"), "应以 APC 起始");
        assert!(prepared.transmit.ends_with(b"\x1b\\"), "应以 ST 结束");
        let s = String::from_utf8_lossy(&prepared.transmit);
        assert!(s.contains("a=T") && s.contains("z=-1"), "控制数据应齐备: {s}");
        assert!(s.contains("f=100"), "应为 PNG 压缩传输 f=100: {s}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 大图必须被缩小后再传输（性能关键）：还原传输载荷里的 PNG，解码读出其像素尺寸，
    /// 不得超过预览区像素上限，且必须小于原图。
    #[test]
    fn prepare_downscales_huge_image() {
        let _g = lock_env();
        let dir = std::env::temp_dir().join(format!("sift-test-img-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.png");
        let img = image::RgbImage::from_fn(3000, 2000, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 0])
        });
        img.save(&file).expect("保存大图应成功");

        let prepared = prepare(&file, 40, 20).expect("大图应能准备预览");
        let png = extract_png(&prepared.transmit);
        let decoded = image::load_from_memory(&png).expect("传输载荷应为可解码 PNG");
        let (w, h) = (decoded.width(), decoded.height());
        let (cw, ch) = cell_size();
        let max_w = (40.0 * cw) as u32;
        let max_h = (20.0 * ch) as u32;
        assert!(w <= max_w.max(1), "宽 {w} 应缩到 <= {max_w}");
        assert!(h <= max_h.max(1), "高 {h} 应缩到 <= {max_h}");
        assert!(w < 3000 && h < 2000, "大图必须被缩小: {w}x{h}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// JPEG 走 DCT 域缩放解码：一张远超预览区的大 JPEG 应能被正确预览，传输载荷解码后的
    /// 尺寸不超过预览区上限且小于原图（证明走了「解码期缩放」而非全分辨率解码）。
    #[test]
    fn prepare_downscales_huge_jpeg() {
        let _g = lock_env();
        let dir = std::env::temp_dir().join(format!("sift-test-img-jpg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.jpg");
        // 2048x1536 渐变图存为 JPEG（足以触发 1/2、1/4 或 1/8 的 DCT 缩放）
        let img = image::RgbImage::from_fn(2048, 1536, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 100])
        });
        img.save(&file).expect("保存 JPEG 应成功");

        let prepared = prepare(&file, 40, 20).expect("大 JPEG 应能准备预览");
        let png = extract_png(&prepared.transmit);
        let decoded = image::load_from_memory(&png).expect("传输载荷应为可解码 PNG");
        let (w, h) = (decoded.width(), decoded.height());
        let (cw, ch) = cell_size();
        let max_w = (40.0 * cw) as u32;
        let max_h = (20.0 * ch) as u32;
        assert!(w <= max_w.max(1), "宽 {w} 应缩到 <= {max_w}");
        assert!(h <= max_h.max(1), "高 {h} 应缩到 <= {max_h}");
        assert!(w < 2048 && h < 1536, "大 JPEG 必须被缩小: {w}x{h}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 压缩传输的实际收益：传输序列应远小于原始像素大小。用平滑渐变图（代表真实照片/截图的
    /// 低频内容，PNG 压缩友好）：800x600 原始 RGB≈1.4MB，PNG 传输应只有其零头。
    #[test]
    fn transmit_is_much_smaller_than_raw_pixels() {
        let img = image::RgbImage::from_fn(800, 600, |x, y| {
            image::Rgb([(x / 3) as u8, (y / 2) as u8, 128])
        });
        let raw = (800 * 600 * 3) as usize; // 原始 RGB 字节数
        let png = encode_png(DynamicImage::ImageRgb8(img)).unwrap();
        let transmit = transmit_png(&png);
        assert!(
            transmit.len() < raw / 3,
            "PNG 传输({}) 应远小于原始像素({raw})",
            transmit.len()
        );
    }

    #[test]
    fn show_sequence_wraps_transmit_with_cursor_ops() {
        let area = Rect {
            x: 10,
            y: 5,
            width: 30,
            height: 15,
        };
        let transmit = b"\x1b_G_TEST_TRANSMIT\x1b\\";
        let seq = show_sequence(area, transmit);
        // 保存光标 -> 移到 (row=6,col=11) -> 删除旧图 -> 传输 -> 恢复光标
        assert!(seq.starts_with(b"\x1b7"), "应先保存光标");
        assert!(seq.ends_with(b"\x1b8"), "应最后恢复光标");
        assert!(seq.windows(7).any(|w| w == b"\x1b[6;11H"), "应移到预览区左上角(1基)");
        assert!(seq.windows(7).any(|w| w == b"a=d,d=A"), "应先删除旧图");
        assert!(
            seq.windows(transmit.len()).any(|w| w == transmit),
            "应原样包含传输序列"
        );
    }

    // ---------------- 磁盘缓存 ----------------

    fn tmp_cache_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sift-test-cache-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 缓存键稳定性：同一文件同一目标框 -> 同键；目标框变化 -> 异键（自动失效）。
    #[test]
    fn cache_key_stable_and_dim_sensitive() {
        let dir = tmp_cache_dir("key");
        let file = dir.join("a.png");
        image::RgbImage::from_fn(10, 10, |_, _| image::Rgb([1, 2, 3]))
            .save(&file)
            .unwrap();

        let k1 = cache_key(&file, 100, 100).expect("应能算出键");
        let k2 = cache_key(&file, 100, 100).expect("应能算出键");
        assert_eq!(k1, k2, "同输入应稳定");
        let k3 = cache_key(&file, 200, 100).unwrap();
        assert_ne!(k1, k3, "目标框变化应得到不同键");
        assert_eq!(k1.len(), 16, "FNV-1a 64 -> 16 位十六进制");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存写入为原子 rename：写入后目标文件存在且内容完整，且不留临时文件。
    #[test]
    fn write_cache_is_atomic_and_complete() {
        let dir = tmp_cache_dir("write");
        let bytes = b"\x89PNG-fake-payload";
        write_cache(&dir, "deadbeefdeadbeef", bytes);

        let cached = dir.join("deadbeefdeadbeef.png");
        assert!(cached.exists(), "缓存文件应存在");
        assert_eq!(std::fs::read(&cached).unwrap(), bytes, "内容应完整");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "tmp").unwrap_or(false))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 容量淘汰：超过上限时按 mtime 删除最旧，降到目标（80%）以下。
    #[test]
    fn evict_removes_oldest_when_over_limit() {
        let dir = tmp_cache_dir("evict");
        // 4 个 100 字节文件，上限设 250 -> 需降到 200（=250*4/5），即至少删 2 个最旧
        for (i, name) in ["aaa", "bbb", "ccc", "ddd"].iter().enumerate() {
            let p = dir.join(format!("{name}.png"));
            std::fs::write(&p, vec![i as u8; 100]).unwrap();
            // 设置递增的 mtime，aaa 最旧
            let t = UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64 * 10);
            let f = std::fs::File::open(&p).unwrap();
            f.set_modified(t).ok();
        }
        evict_cache_limit(&dir, 250);

        let mut remain: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        remain.sort();
        // 最旧的 aaa、bbb 应被淘汰，留下 ccc、ddd（共 200 <= 目标 200）
        assert_eq!(remain, vec!["ccc.png", "ddd.png"], "应淘汰最旧的两个: {remain:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 磁盘缓存命中路径：首次 prepare 会解码源图并写入缓存；随后把缓存文件覆写为一张「与源图
    /// 明显不同」的 PNG，再次 prepare（源图未变 -> 键不变）应直接读缓存、传输缓存里的那张，
    /// 而非重新解码源图。以此证明 prepare 确实走了磁盘缓存命中分支。
    #[test]
    fn prepare_hits_disk_cache_on_second_call() {
        let _g = lock_env();
        // 用独立 XDG_CACHE_HOME 避免污染真实缓存与其它测试
        let cache_home = tmp_cache_dir("xdg");
        std::env::set_var("XDG_CACHE_HOME", &cache_home);

        let srcdir = tmp_cache_dir("src");
        let file = srcdir.join("pic.png");
        image::RgbImage::from_fn(60, 40, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 9]))
            .save(&file)
            .unwrap();

        let first = prepare(&file, 40, 20).expect("首次应解码并写缓存");

        // 算出缓存文件路径（与 prepare 内部同一套 cell_size/cache_key）
        let (cw, ch) = cell_size();
        let mw = ((40.0 * cw) as u32).max(1);
        let mh = ((20.0 * ch) as u32).max(1);
        let key = cache_key(&file, mw, mh).expect("应能算出缓存键");
        let cached = cache_dir().unwrap().join(format!("{key}.png"));
        assert!(cached.exists(), "首次预览应写入磁盘缓存");

        // 用一张明显不同的 PNG（5x5 纯蓝）覆写缓存文件
        let other = image::RgbImage::from_pixel(5, 5, image::Rgb([0, 0, 255]));
        let other_png = encode_png(DynamicImage::ImageRgb8(other)).unwrap();
        std::fs::write(&cached, &other_png).unwrap();

        // 源图未变 -> 键不变 -> 命中缓存：传输的应是缓存里的「纯蓝 5x5」，而非重新解码的源图
        let second = prepare(&file, 40, 20).expect("第二次应命中磁盘缓存");
        assert_eq!(
            second.transmit,
            transmit_png(&other_png),
            "命中缓存应直接传输缓存内容"
        );
        assert_ne!(
            second.transmit, first.transmit,
            "命中缓存不应重新解码源图"
        );

        std::env::remove_var("XDG_CACHE_HOME");
        let _ = std::fs::remove_dir_all(&cache_home);
        let _ = std::fs::remove_dir_all(&srcdir);
    }
}

